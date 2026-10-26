//! Schema migrations, the pre-migration backup and the downgrade guard (PLAN §4.15).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;
use rusqlite::backup::Backup;
use rusqlite_migration::{M, Migrations};
use vault::SecretString;

use crate::conn::{self, Access};
use crate::{Error, Result};

const MAIN: &[M<'static>] = &[
    M::up(include_str!("../migrations/main/0001_schema_v1.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/main/0002_account_uuid.sql")),
    M::up(include_str!("../migrations/main/0003_message_list.sql")),
    M::up(include_str!("../migrations/main/0004_compose.sql")),
    M::up(include_str!("../migrations/main/0005_account_secret.sql")),
    M::up(include_str!("../migrations/main/0006_agent_secret.sql")),
    M::up(include_str!("../migrations/main/0007_draft_name.sql")),
    M::up(include_str!(
        "../migrations/main/0008_gmail_label_resync.sql"
    )),
    M::up(include_str!(
        "../migrations/main/0009_draft_support_maze.sql"
    )),
    M::up(include_str!("../migrations/main/0010_imap_only.sql")),
];

const SENDLOG: &[M<'static>] = &[M::up(include_str!(
    "../migrations/sendlog/0001_sendlog_v1.sql"
))];

/// Schema version of `mazemail.db` this build writes.
pub const SCHEMA_VERSION: u32 = MAIN.len() as u32;

/// Schema version of `sendlog.db` this build writes.
pub const SENDLOG_SCHEMA_VERSION: u32 = SENDLOG.len() as u32;

/// How many pre-migration backups are kept.
pub const BACKUPS_KEPT: usize = 2;

/// What opening the main database did to its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Schema version found in the file (0 for a new database).
    pub from: u32,
    /// Schema version after migrating.
    pub to: u32,
    /// The pre-migration backup, if one was made.
    pub backup: Option<PathBuf>,
}

/// The schema version stored in `PRAGMA user_version`.
pub(crate) fn user_version(conn: &Connection) -> Result<u32> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version).map_err(|_| Error::Invalid("negative schema version"))
}

/// Brings the main database to [`SCHEMA_VERSION`].
///
/// - A schema newer than this build is refused ([`Error::SchemaTooNew`]) before
///   anything is written.
/// - An existing database (version ≥ 1) that needs migrating is first copied to
///   `backups/mazemail-<schema>-<app version>.db` with the online backup API, keyed with
///   the same key; only the newest [`BACKUPS_KEPT`] backups are kept.
pub(crate) fn migrate_main(
    conn: &mut Connection,
    key: &SecretString,
    backups_dir: &Path,
    app_version: &str,
) -> Result<MigrationReport> {
    migrate_with_backup(conn, key, backups_dir, app_version, MAIN)
}

fn migrate_with_backup(
    conn: &mut Connection,
    key: &SecretString,
    backups_dir: &Path,
    app_version: &str,
    migrations: &[M<'_>],
) -> Result<MigrationReport> {
    let latest =
        u32::try_from(migrations.len()).map_err(|_| Error::Invalid("too many migrations"))?;
    let from = user_version(conn)?;
    if from > latest {
        return Err(Error::SchemaTooNew {
            found: from,
            supported: latest,
        });
    }
    let mut backup = None;
    if from > 0 && from < latest {
        let name = format!("mazemail-{from}-{}.db", sanitize(app_version));
        let path = backups_dir.join(name);
        backup_to(conn, key, &path)?;
        prune_backups(backups_dir, BACKUPS_KEPT)?;
        tracing::info!(from, to = latest, backup = %path.display(), "migrating the database");
        backup = Some(path);
    }
    if from < latest {
        Migrations::from_slice(migrations).to_latest(conn)?;
    }
    Ok(MigrationReport {
        from,
        to: latest,
        backup,
    })
}

/// Brings the send log to [`SENDLOG_SCHEMA_VERSION`] (never backed up: it must survive a
/// rollback as it is).
pub(crate) fn migrate_sendlog(conn: &mut Connection) -> Result<()> {
    let from = user_version(conn)?;
    if from > SENDLOG_SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: from,
            supported: SENDLOG_SCHEMA_VERSION,
        });
    }
    if from < SENDLOG_SCHEMA_VERSION {
        Migrations::from_slice(SENDLOG).to_latest(conn)?;
    }
    Ok(())
}

/// Copies the database behind `src` into a new SQLCipher file at `dest`, keyed with
/// `key`. The copy is written next to `dest` and renamed into place when complete.
pub(crate) fn backup_to(src: &Connection, key: &SecretString, dest: &Path) -> Result<()> {
    let partial = dest.with_extension("db.partial");
    remove_if_exists(&partial)?;
    {
        let mut dst = conn::open(&partial, key, Access::ReadWrite, 2048)?;
        // A backup is a single file; it doesn't need WAL.
        dst.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
        let backup = Backup::new(src, &mut dst)?;
        backup.run_to_completion(512, std::time::Duration::ZERO, None)?;
    }
    fs::rename(&partial, dest).map_err(Error::io("renaming the database backup"))?;
    Ok(())
}

/// Deletes all but the newest `keep` `mazemail-*.db` backups.
fn prune_backups(dir: &Path, keep: usize) -> Result<()> {
    let mut backups: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(Error::io("listing database backups"))? {
        let entry = entry.map_err(Error::io("listing database backups"))?;
        let path = entry.path();
        let is_backup = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("mazemail-") && n.ends_with(".db"));
        if !is_backup {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        backups.push((modified, path));
    }
    backups.sort();
    let excess = backups.len().saturating_sub(keep);
    for (_, path) in backups.into_iter().take(excess) {
        remove_if_exists(&path)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io("removing an old file")(err)),
    }
}

/// Keeps version strings safe for file names.
fn sanitize(version: &str) -> String {
    let cleaned: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        Migrations::from_slice(MAIN).validate().unwrap();
        Migrations::from_slice(SENDLOG).validate().unwrap();
    }

    fn key(seed: u8) -> SecretString {
        vault::MasterKey::from_bytes(vault::KeyId::FIRST, [seed; vault::KEY_LEN])
            .sqlcipher_pragma_key()
    }

    const NEXT: &str = "CREATE TABLE added_in_next (id INTEGER PRIMARY KEY);";

    #[test]
    fn existing_database_is_backed_up_before_migrating() {
        let tmp = tempfile::tempdir().unwrap();
        let backups = tmp.path().join("backups");
        fs::create_dir(&backups).unwrap();
        let db = tmp.path().join("mazemail.db");
        let key = key(7);

        let mut conn = conn::open(&db, &key, Access::ReadWrite, 1024).unwrap();
        let fresh = migrate_main(&mut conn, &key, &backups, "1.0.0").unwrap();
        assert_eq!(
            fresh,
            MigrationReport {
                from: 0,
                to: SCHEMA_VERSION,
                backup: None
            }
        );
        conn.execute(
            "INSERT INTO account (kind, name) VALUES ('imap', 'Work')",
            [],
        )
        .unwrap();

        // Same version again: nothing to do, no backup.
        let again = migrate_main(&mut conn, &key, &backups, "1.0.0").unwrap();
        assert_eq!(again.backup, None);

        let mut migrations = MAIN.to_vec();
        migrations.push(M::up(NEXT));
        let report = migrate_with_backup(&mut conn, &key, &backups, "1.1.0", &migrations).unwrap();
        let backup = report.backup.clone().unwrap();
        assert_eq!(
            backup,
            backups.join(format!("mazemail-{SCHEMA_VERSION}-1.1.0.db"))
        );
        assert_eq!(report.to, SCHEMA_VERSION + 1);

        // The backup opens with the same key, holds the old schema and the data.
        let copy = conn::open(&backup, &key, Access::ReadOnly, 1024).unwrap();
        assert_eq!(user_version(&copy).unwrap(), SCHEMA_VERSION);
        let name: String = copy
            .query_row("SELECT name FROM account", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Work");
        let has_next: i64 = copy
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'added_in_next'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_next, 0);
        drop(copy);

        // It doesn't open with another key, and it isn't plaintext.
        let other_key = super::tests::key(8);
        assert!(matches!(
            conn::open(&backup, &other_key, Access::ReadOnly, 1024),
            Err(Error::WrongKey { .. })
        ));
        assert!(!fs::read(&backup).unwrap().starts_with(b"SQLite format 3"));
    }

    #[test]
    fn v2_gives_every_account_a_stable_unique_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(4);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..1]).unwrap();
        for name in ["a", "b", "c"] {
            conn.execute(
                "INSERT INTO account (kind, name) VALUES ('imap', ?1)",
                [name],
            )
            .unwrap();
        }
        let report = migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        assert_eq!((report.from, report.to), (1, SCHEMA_VERSION));
        assert!(report.backup.is_some());

        // Inserted without a UUID after the migration: the trigger fills it in.
        conn.execute("INSERT INTO account (kind, name) VALUES ('imap', 'd')", [])
            .unwrap();
        let uuids: Vec<String> = conn
            .prepare("SELECT uuid FROM account ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(uuids.len(), 4);
        for uuid in &uuids {
            let parsed = uuid::Uuid::parse_str(uuid).unwrap();
            assert_eq!(parsed.get_version_num(), 4, "{uuid}");
            assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122, "{uuid}");
            assert_eq!(&parsed.hyphenated().to_string(), uuid);
        }
        let distinct: std::collections::BTreeSet<_> = uuids.iter().collect();
        assert_eq!(distinct.len(), uuids.len());

        // Never changed, never duplicated.
        assert!(
            conn.execute("UPDATE account SET uuid = 'x' WHERE id = 1", [])
                .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO account (kind, name, uuid) VALUES ('imap', 'e', ?1)",
                [&uuids[0]],
            )
            .is_err()
        );
        // Other columns still update.
        conn.execute("UPDATE account SET name = 'renamed' WHERE id = 1", [])
            .unwrap();
    }

    #[test]
    fn v3_copies_date_and_attachment_flag_to_existing_locations() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(5);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..2]).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, kind, name) VALUES (1, 'imap', 'Work');
             INSERT INTO folder (id, account_id, display_name) VALUES (1, 1, 'Inbox');
             INSERT INTO thread (id, account_id) VALUES (1, 1);
             INSERT INTO message (id, account_id, thread_id, date, has_attachments)
               VALUES (1, 1, 1, 1234, 1), (2, 1, 1, 99, 0);
             INSERT INTO message_location (message_id, folder_id) VALUES (1, 1), (2, 1);",
        )
        .unwrap();
        let report = migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        assert_eq!((report.from, report.to), (2, SCHEMA_VERSION));
        let read = |conn: &Connection| -> Vec<(i64, i64, i64)> {
            conn.prepare("SELECT message_id, date, attachment FROM message_location ORDER BY 1")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(read(&conn), vec![(1, 1234, 1), (2, 99, 0)]);
        // The triggers follow later changes of the message.
        conn.execute_batch("UPDATE message SET has_attachments = 1, date = 100 WHERE id = 2;")
            .unwrap();
        assert_eq!(read(&conn), vec![(1, 1234, 1), (2, 100, 1)]);
    }

    #[test]
    fn v5_lists_existing_accounts_for_the_keychain_move() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(6);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..4]).unwrap();
        for name in ["a", "b"] {
            conn.execute(
                "INSERT INTO account (kind, name) VALUES ('imap', ?1)",
                [name],
            )
            .unwrap();
        }
        let report = migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        assert_eq!((report.from, report.to), (4, SCHEMA_VERSION));
        let listed: i64 = conn
            .query_row(
                "SELECT count(*) FROM legacy_secret_account JOIN account ON uuid = account_uuid",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(listed, 2);

        // An account added afterwards never had keychain items.
        conn.execute("INSERT INTO account (kind, name) VALUES ('imap', 'c')", [])
            .unwrap();
        let listed: i64 = conn
            .query_row("SELECT count(*) FROM legacy_secret_account", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(listed, 2);
    }

    #[test]
    fn v8_makes_gmail_all_mail_fetch_its_labels_again() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(7);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..7]).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, kind, name) VALUES (1, 'gmail', 'g'), (2, 'imap', 'i');
             INSERT INTO folder (id, account_id, remote_name, display_name, role, is_label,
                                 highestmodseq)
             VALUES (1, 1, '[Gmail]/All Mail', 'All Mail', 'all', 0, 900),
                    (2, 1, 'INBOX', 'Inbox', 'inbox', 1, NULL),
                    (3, 1, '[Gmail]/Trash', 'Trash', 'trash', 0, 800),
                    (4, 2, 'Archive', 'Archive', 'all', 0, 700),
                    (5, 1, '[Gmail]/Alle Nachrichten', 'All', 'all', 0, NULL);",
        )
        .unwrap();
        migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        let modseqs: Vec<(i64, Option<i64>)> = conn
            .prepare("SELECT id, highestmodseq FROM folder ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Only a synced Gmail All Mail goes back; never-synced folders stay unsynced.
        assert_eq!(
            modseqs,
            [
                (1, Some(1)),
                (2, None),
                (3, Some(800)),
                (4, Some(700)),
                (5, None)
            ]
        );
    }

    #[test]
    fn v9_keeps_sent_with_maze_on_for_existing_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(9);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..8]).unwrap();
        conn.execute_batch(
            "INSERT INTO draft (id, to_json, cc_json, bcc_json, subject, body,
                                references_json, attachments_json, created, updated, name)
             VALUES (1, '[]', '[]', '[]', '', '', '[]', '[]', 1, 1, 'brave-otter');",
        )
        .unwrap();
        let report = migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        assert_eq!((report.from, report.to), (8, SCHEMA_VERSION));
        let on: i64 = conn
            .query_row("SELECT support_maze FROM draft WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(on, 1);
        // New rows default to on as well; off is stored as 0.
        conn.execute_batch(
            "INSERT INTO draft (id, to_json, cc_json, bcc_json, subject, body,
                                references_json, attachments_json, created, updated)
             VALUES (2, '[]', '[]', '[]', '', '', '[]', '[]', 1, 1);
             UPDATE draft SET support_maze = 0 WHERE id = 1;",
        )
        .unwrap();
        let values: Vec<i64> = conn
            .prepare("SELECT support_maze FROM draft ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(values, [0, 1]);
    }

    #[test]
    fn v10_removes_empty_local_accounts_and_keeps_the_ones_with_mail() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(10);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        migrate_with_backup(&mut conn, &key, tmp.path(), "1.0.0", &MAIN[..9]).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, kind, name) VALUES
               (1, 'local', 'Local Folders'), (2, 'local', 'Empty'), (3, 'imap', 'Work'),
               (4, 'imap', 'Empty IMAP');
             INSERT INTO folder (id, account_id, remote_name, display_name)
             VALUES (1, 1, NULL, 'Inbox'), (2, 2, NULL, 'Inbox'), (3, 3, 'INBOX', 'Inbox');
             INSERT INTO identity (id, account_id, email, sent_folder_id)
             VALUES (1, 3, 'me@work.example', 2);
             INSERT INTO thread (id, account_id, last_date) VALUES (1, 1, 1);
             INSERT INTO message (id, account_id, thread_id, date) VALUES (1, 1, 1, 1);
             INSERT INTO message_location (message_id, folder_id, flags, date)
             VALUES (1, 1, 0, 1);",
        )
        .unwrap();
        let report = migrate_main(&mut conn, &key, tmp.path(), "1.1.0").unwrap();
        assert_eq!((report.from, report.to), (9, SCHEMA_VERSION));
        let accounts: Vec<(i64, String)> = conn
            .prepare("SELECT id, kind FROM account ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // The local account with mail stays (still a legacy local account), the empty
        // one goes with its folder; IMAP accounts are untouched, empty or not.
        assert_eq!(
            accounts,
            [
                (1, "local".to_owned()),
                (3, "imap".to_owned()),
                (4, "imap".to_owned())
            ]
        );
        let folders: Vec<i64> = conn
            .prepare("SELECT id FROM folder ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(folders, [1, 3]);
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(messages, 1);
        // An identity that filed into the removed folder just loses that setting.
        let sent: Option<i64> = conn
            .query_row(
                "SELECT sent_folder_id FROM identity WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sent, None);

        // Pending ops count as blob references.
        conn.execute_batch(
            "INSERT INTO blob (id, size, created) VALUES (x'01', 1, 1);
             INSERT INTO pending_op (id, account_id, kind, payload_json, origin, created,
                                     blob_id)
             VALUES (1, 3, 'append', '{}', 'import', 1, x'01');",
        )
        .unwrap();
        let refs = |conn: &Connection| -> i64 {
            conn.query_row("SELECT refs FROM blob WHERE id = x'01'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(refs(&conn), 1);
        conn.execute_batch("DELETE FROM pending_op WHERE id = 1;")
            .unwrap();
        assert_eq!(refs(&conn), 0);
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        for (i, name) in ["mazemail-1-a.db", "mazemail-2-b.db", "mazemail-3-c.db"]
            .iter()
            .enumerate()
        {
            let path = tmp.path().join(name);
            fs::write(&path, b"x").unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 * (i as u64 + 1)),
            )
            .unwrap();
        }
        fs::write(tmp.path().join("unrelated.txt"), b"x").unwrap();
        prune_backups(tmp.path(), BACKUPS_KEPT).unwrap();
        let mut left: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["mazemail-2-b.db", "mazemail-3-c.db", "unrelated.txt"]
        );
    }

    #[test]
    fn newer_schema_is_refused_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("mazemail.db");
        let key = key(3);
        let mut conn = conn::open(&db, &key, Access::ReadWrite, 1024).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        let err = migrate_main(&mut conn, &key, tmp.path(), "1.0.0").unwrap_err();
        assert!(
            matches!(err, Error::SchemaTooNew { found, supported } if found == SCHEMA_VERSION + 5 && supported == SCHEMA_VERSION),
            "{err:?}"
        );
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION + 5);
    }

    #[test]
    fn version_strings_become_file_names() {
        assert_eq!(sanitize("0.4.1-preview+3"), "0.4.1-preview+3");
        assert_eq!(sanitize("../x"), ".._x");
        assert_eq!(sanitize(""), "unknown");
    }
}
