//! Databases of the development builds before the first release.
//!
//! Before 0.1.0 the main store had 14 migrations; they were squashed into one v1 schema
//! (`migrations/main/0001_schema_v1.sql`). A database those builds left at their last
//! schema (`user_version` 14, recognisable by its `legacy_secret_account` table, which
//! v1 never has) is converted in place, after the usual backup, in one transaction:
//!
//! 1. Local Folders (`account.kind = 'local'`, ADR-019) are removed with their mail.
//!    The backup keeps them, and the build that wrote the database can export them.
//! 2. What v1 dropped goes ([`DROPPED`]): the tables `legacy_secret_account` (accounts
//!    whose secrets were still to be moved out of the OS keychain), `domain_backfill`
//!    (indexing the domains of mail stored before the domain index) and
//!    `identity_rule`, and the columns `identity.color`, `identity.match_patterns_json`
//!    and `folder.sync_policy`, which nothing used.
//! 3. `account.kind` gets v1's narrower CHECK (no `local`, no `jmap`). Only the
//!    constraint's text changes, which SQLite allows through `writable_schema`.
//! 4. The result must match a new v1 database object for object (columns, keys,
//!    foreign keys, indexes, triggers and CHECK constraints, ignoring comments and
//!    whitespace); otherwise nothing is changed and opening fails. Then `user_version`
//!    becomes 1.
//!
//! Databases of older development schemas (2 to 13) are not converted.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use super::{SCHEMA_V1, user_version};
use crate::{Error, Result};

/// What v1 no longer has, dropped from a development database.
const DROPPED: &str = "DROP TABLE legacy_secret_account;
     DROP TABLE domain_backfill;
     DROP TABLE identity_rule;
     ALTER TABLE identity DROP COLUMN color;
     ALTER TABLE identity DROP COLUMN match_patterns_json;
     ALTER TABLE folder DROP COLUMN sync_policy;";

/// The last schema version of the development builds.
pub(super) const DEV_VERSION: u32 = 14;

/// The schema version the conversion ends at; later migrations run after it.
const V1: u32 = 1;

/// Whether `conn` is a database of a development build at its last schema.
pub(super) fn is_dev_v14(conn: &Connection) -> Result<bool> {
    Ok(user_version(conn)? == DEV_VERSION && table_exists(conn, "legacy_secret_account")?)
}

/// Converts a development database at schema 14 to v1 (see the module docs).
pub(super) fn convert(conn: &mut Connection) -> Result<()> {
    let expected = fingerprint(&fresh_v1()?)?;
    let account_sql: String = fresh_v1()?.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'account'",
        [],
        |row| row.get(0),
    )?;

    let tx = conn.transaction()?;
    let local = tx.execute("DELETE FROM account WHERE kind = 'local'", [])?;
    if local > 0 {
        tracing::warn!(
            accounts = local,
            "removed the Local Folders of a development build; the database backup keeps them"
        );
    }
    let unmoved: i64 = tx.query_row("SELECT count(*) FROM legacy_secret_account", [], |row| {
        row.get(0)
    })?;
    if unmoved > 0 {
        tracing::warn!(
            accounts = unmoved,
            "account secrets of a development build were never moved out of the keychain; \
             these accounts ask for their password or sign-in again"
        );
    }
    let backfill_pending: bool = tx
        .query_row(
            "SELECT next_message <= last_message FROM domain_backfill WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if backfill_pending {
        tracing::warn!(
            "the domain index of a development build was not complete; older mail stays \
             out of the domain counts"
        );
    }
    tx.execute_batch(DROPPED)?;

    // A narrower CHECK on `account.kind` (the rows already satisfy it): the documented
    // way to change a constraint without rebuilding the table.
    let schema_version: i64 = tx.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    tx.pragma_update(None, "writable_schema", true)?;
    let rewritten = tx.execute(
        "UPDATE sqlite_master SET sql = ?1 WHERE type = 'table' AND name = 'account'",
        [&account_sql],
    );
    tx.pragma_update(None, "schema_version", schema_version + 1)?;
    tx.pragma_update(None, "writable_schema", false)?;
    if rewritten? != 1 {
        return Err(Error::DevSchema("account".to_owned()));
    }

    let found = fingerprint(&tx)?;
    if found != expected {
        let differing: Vec<&str> = expected
            .keys()
            .chain(found.keys())
            .filter(|name| expected.get(*name) != found.get(*name))
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        return Err(Error::DevSchema(differing.join(", ")));
    }
    let violations: Option<String> = tx
        .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
        .optional()?;
    if let Some(table) = violations {
        return Err(Error::DevSchema(format!("foreign keys of {table}")));
    }
    tx.pragma_update(None, "user_version", V1)?;
    tx.commit()?;
    tracing::info!(
        from = DEV_VERSION,
        to = V1,
        "converted a development database to the v1 schema"
    );
    Ok(())
}

/// A new, empty v1 database in memory.
fn fresh_v1() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA_V1)?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// One schema object as far as a comparison needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shape {
    /// Tables: columns, foreign keys, indexes and whether it is WITHOUT ROWID. Indexes,
    /// triggers and views: their table and their SQL without comments and with
    /// whitespace collapsed.
    pub(super) shape: String,
    /// The table's CHECK constraints (normalized SQL).
    pub(super) checks: Vec<String>,
}

/// The schema objects of `conn` by `"<type> <name>"`, without SQLite's own
/// (`sqlite_sequence`, automatic indexes, which show up in their table's shape).
pub(super) fn fingerprint(conn: &Connection) -> Result<BTreeMap<String, Shape>> {
    let mut objects = BTreeMap::new();
    let mut statement = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY type, name",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (kind, name, table, sql) in rows {
        let sql = normalize(sql.as_deref().unwrap_or(""));
        let shape = if kind == "table" {
            let columns = pragma_rows(conn, "table_xinfo", &name)?;
            let foreign_keys = pragma_rows(conn, "foreign_key_list", &name)?;
            let mut indexes = pragma_rows(conn, "index_list", &name)?;
            // The index list's first column is a sequence number, not part of the shape.
            for index in &mut indexes {
                index.remove(0);
            }
            indexes.sort();
            let without_rowid = sql.to_ascii_uppercase().ends_with("WITHOUT ROWID");
            Shape {
                shape: format!(
                    "columns {columns:?}; foreign keys {foreign_keys:?}; indexes {indexes:?}; \
                     without rowid {without_rowid}"
                ),
                checks: checks(&sql),
            }
        } else {
            Shape {
                shape: format!("on {table}: {sql}"),
                checks: Vec::new(),
            }
        };
        objects.insert(format!("{kind} {name}"), shape);
    }
    Ok(objects)
}

/// The rows of `PRAGMA <pragma>(<table>)`, every value as text.
fn pragma_rows(conn: &Connection, pragma: &str, table: &str) -> Result<Vec<Vec<String>>> {
    let mut statement = conn.prepare(&format!("SELECT * FROM pragma_{pragma}(?1)"))?;
    let width = statement.column_count();
    let rows = statement
        .query_map([table], |row| {
            (0..width)
                .map(|i| {
                    Ok(match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => "NULL".to_owned(),
                        rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                        rusqlite::types::ValueRef::Real(v) => v.to_string(),
                        rusqlite::types::ValueRef::Text(v) | rusqlite::types::ValueRef::Blob(v) => {
                            String::from_utf8_lossy(v).into_owned()
                        }
                    })
                })
                .collect::<rusqlite::Result<Vec<String>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// SQL without `--` comments, with runs of whitespace as one space.
fn normalize(sql: &str) -> String {
    let without_comments: Vec<&str> = sql
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect();
    without_comments
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `CHECK (…)` constraints in normalized table SQL, sorted.
fn checks(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = upper[from..].find("CHECK") {
        let start = from + at;
        let Some(open) = sql[start..].find('(').map(|i| start + i) else {
            break;
        };
        let mut depth = 0usize;
        let mut end = sql.len();
        for (i, c) in sql[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        found.push(sql[open..end].to_owned());
        from = end;
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use std::fs;

    use vault::SecretString;

    use super::*;
    use crate::conn::{self, Access};
    use crate::migrate::{MAIN, MigrationReport, SCHEMA_VERSION, migrate_main};

    /// What the 14 migrations of the development builds produced, as SQLite stored it.
    const DEV_V14_SCHEMA: &str = include_str!("../../tests/fixtures/schema_v14.sql");

    fn key(seed: u8) -> SecretString {
        vault::MasterKey::from_bytes(vault::KeyId::FIRST, [seed; vault::KEY_LEN])
            .sqlcipher_pragma_key()
    }

    fn dev_v14(conn: &Connection) {
        conn.execute_batch(DEV_V14_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO domain_backfill (id, next_message, last_message) VALUES (1, 1, 0);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", DEV_VERSION)
            .unwrap();
    }

    #[test]
    fn v1_is_the_dev_schema_without_the_removed_objects() {
        let dev = Connection::open_in_memory().unwrap();
        dev_v14(&dev);
        for table in ["legacy_secret_account", "domain_backfill", "identity_rule"] {
            assert!(table_exists(&dev, table).unwrap());
        }
        dev.execute_batch(DROPPED).unwrap();
        let mut dev = fingerprint(&dev).unwrap();
        let mut fresh = fingerprint(&fresh_v1().unwrap()).unwrap();
        assert!(!fresh["table identity"].shape.contains("color"));
        assert!(!fresh["table folder"].shape.contains("sync_policy"));

        // Only `account.kind` lost values.
        let dev_account = dev.get_mut("table account").unwrap();
        let fresh_account = fresh.get_mut("table account").unwrap();
        assert_eq!(
            dev_account.checks,
            ["(kind IN ('imap', 'gmail', 'm365', 'local', 'jmap'))"]
        );
        assert_eq!(
            fresh_account.checks,
            ["(kind IN ('imap', 'gmail', 'm365'))"]
        );
        dev_account.checks.clear();
        fresh_account.checks.clear();

        pretty_assertions::assert_eq!(dev, fresh);
        // The comparison sees the things that matter.
        assert_eq!(fresh["table domain"].checks.len(), 4);
        assert!(fresh["table message_location"].shape.contains("attachment"));
        assert!(
            fresh["index location_list_unread"]
                .shape
                .contains("WHERE flags & 1 = 0")
        );
    }

    #[test]
    fn a_dev_database_is_backed_up_and_converted_without_losing_mail() {
        let tmp = tempfile::tempdir().unwrap();
        let backups = tmp.path().join("backups");
        fs::create_dir(&backups).unwrap();
        let db = tmp.path().join("mazemail.db");
        let key = key(21);
        let mut conn = conn::open(&db, &key, Access::ReadWrite, 1024).unwrap();
        dev_v14(&conn);
        conn.execute_batch(
            "INSERT INTO account (id, kind, name) VALUES
               (1, 'imap', 'Work'), (2, 'gmail', 'Gmail'), (3, 'local', 'Local Folders');
             INSERT INTO legacy_secret_account (account_uuid)
               SELECT uuid FROM account WHERE id = 1;
             INSERT INTO account_secret (account_uuid, kind, secret, updated)
               SELECT uuid, 'password', 's3cret', 1 FROM account WHERE id = 1;
             INSERT INTO folder (id, account_id, remote_name, display_name)
               VALUES (1, 1, 'INBOX', 'Inbox'), (2, 3, NULL, 'Inbox');
             INSERT INTO blob (id, size, created) VALUES (x'01', 1, 1), (x'02', 1, 1);
             INSERT INTO thread (id, account_id, last_date) VALUES (1, 1, 5), (2, 3, 6);
             INSERT INTO message (id, account_id, thread_id, date, blob_id)
               VALUES (1, 1, 1, 5, x'01'), (2, 3, 2, 6, x'02');
             INSERT INTO message_location (message_id, folder_id, uid, date)
               VALUES (1, 1, 10, 5), (2, 2, NULL, 6);
             INSERT INTO draft (id, created, updated, name) VALUES (1, 1, 1, 'brave-otter');
             UPDATE domain_backfill SET next_message = 3, last_message = 2;",
        )
        .unwrap();
        let uuid: String = conn
            .query_row("SELECT uuid FROM account WHERE id = 1", [], |r| r.get(0))
            .unwrap();

        let report = migrate_main(&mut conn, &key, &backups, "0.1.0-alpha.1").unwrap();
        let backup = backups.join("mazemail-14-0.1.0-alpha.1.db");
        assert_eq!(
            report,
            MigrationReport {
                from: DEV_VERSION,
                to: SCHEMA_VERSION,
                backup: Some(backup.clone()),
            }
        );
        // Converted to v1, then migrated like any v1 database.
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let mut latest = Connection::open_in_memory().unwrap();
        rusqlite_migration::Migrations::from_slice(MAIN)
            .to_latest(&mut latest)
            .unwrap();
        pretty_assertions::assert_eq!(fingerprint(&conn).unwrap(), fingerprint(&latest).unwrap());

        let count = |conn: &Connection, sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0)).unwrap()
        };
        // The IMAP mail, the secret and the draft stay; Local Folders go.
        let accounts: Vec<(i64, String)> = conn
            .prepare("SELECT id, kind FROM account ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(accounts, [(1, "imap".to_owned()), (2, "gmail".to_owned())]);
        assert_eq!(count(&conn, "SELECT id FROM message"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_location"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM folder"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM draft"), 1);
        let secret: String = conn
            .query_row(
                "SELECT secret FROM account_secret WHERE account_uuid = ?1",
                [&uuid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(secret, "s3cret");
        // The removed message's blob is unreferenced now (garbage collection takes it).
        assert_eq!(count(&conn, "SELECT refs FROM blob WHERE id = x'02'"), 0);
        assert_eq!(count(&conn, "SELECT refs FROM blob WHERE id = x'01'"), 1);

        // The constraints and triggers work as in a new database.
        assert!(
            conn.execute("INSERT INTO account (kind, name) VALUES ('local', 'x')", [])
                .is_err()
        );
        conn.execute(
            "INSERT INTO account (kind, name) VALUES ('m365', 'New')",
            [],
        )
        .unwrap();
        let new_uuid: String = conn
            .query_row("SELECT uuid FROM account WHERE name = 'New'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_uuid.len(), 36);
        assert!(
            conn.execute("UPDATE account SET uuid = 'x' WHERE id = 1", [])
                .is_err()
        );

        // Opening it again changes nothing.
        let again = migrate_main(&mut conn, &key, &backups, "0.1.0-alpha.1").unwrap();
        assert_eq!(
            (again.from, again.to, again.backup),
            (SCHEMA_VERSION, SCHEMA_VERSION, None)
        );
        drop(conn);

        // The backup still has everything, Local Folders included.
        let copy = conn::open(&backup, &key, Access::ReadOnly, 1024).unwrap();
        assert_eq!(user_version(&copy).unwrap(), DEV_VERSION);
        assert_eq!(count(&copy, "SELECT COUNT(*) FROM message"), 2);
        assert_eq!(
            count(&copy, "SELECT COUNT(*) FROM account WHERE kind = 'local'"),
            1
        );
    }

    #[test]
    fn a_dev_database_with_another_schema_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(22);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        dev_v14(&conn);
        conn.execute_batch(
            "CREATE TABLE stray (id INTEGER PRIMARY KEY);
             INSERT INTO account (id, kind, name) VALUES (1, 'local', 'Local Folders');",
        )
        .unwrap();
        let err = migrate_main(&mut conn, &key, tmp.path(), "0.1.0").unwrap_err();
        assert!(
            matches!(&err, Error::DevSchema(what) if what == "table stray"),
            "{err:?}"
        );
        // Rolled back: still schema 14, Local Folders and all.
        assert_eq!(user_version(&conn).unwrap(), DEV_VERSION);
        assert!(table_exists(&conn, "legacy_secret_account").unwrap());
        let local: i64 = conn
            .query_row("SELECT COUNT(*) FROM account", [], |r| r.get(0))
            .unwrap();
        assert_eq!(local, 1);
    }

    #[test]
    fn older_dev_schemas_are_not_converted() {
        let tmp = tempfile::tempdir().unwrap();
        let key = key(23);
        let mut conn = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        conn.pragma_update(None, "user_version", 13).unwrap();
        assert!(!is_dev_v14(&conn).unwrap());
        assert!(matches!(
            migrate_main(&mut conn, &key, tmp.path(), "0.1.0"),
            Err(Error::SchemaTooNew { found: 13, .. })
        ));
    }

    #[test]
    fn checks_are_found_in_table_sql() {
        assert_eq!(
            checks("CREATE TABLE t (a INTEGER CHECK (a IN (0, 1)), b TEXT check(b <> ''))"),
            ["(a IN (0, 1))", "(b <> '')"]
        );
        assert_eq!(normalize("a  -- x\n  b"), "a b");
    }
}
