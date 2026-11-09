//! Thunderbird address books (`abook*.sqlite`, `history.sqlite`;
//! `mailnews/addrbook/modules/SQLiteDirectory.sys.mjs`).
//!
//! Tables: `properties(card, name, value)`, `lists(uid, name, nickName, description)`,
//! `list_cards(list, card)`. Thunderbird may hold the file open, so it (and its WAL)
//! is copied into a private temp directory first and read from there; the copy is
//! deleted when reading is done.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use super::prefs::Prefs;
use crate::{Error, Result};

/// One address book.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct AddressBook {
    pub file: PathBuf,
    /// The name shown in Thunderbird (`ldap_2.servers.*.description`).
    pub name: String,
    pub cards: Vec<Card>,
    pub lists: Vec<MailingList>,
}

impl std::fmt::Debug for AddressBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressBook")
            .field("file", &self.file)
            .field("cards", &self.cards.len())
            .field("lists", &self.lists.len())
            .finish()
    }
}

/// A contact card.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Card {
    pub uid: String,
    pub display_name: Option<String>,
    /// Primary address first.
    pub emails: Vec<String>,
    /// The card as vCard (`_vCard`), if Thunderbird stored one.
    pub vcard: Option<String>,
}

impl std::fmt::Debug for Card {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Card")
            .field("emails", &self.emails.len())
            .finish_non_exhaustive()
    }
}

/// A mailing list (Maze has no contact lists yet; members are imported as cards).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MailingList {
    pub uid: String,
    pub name: String,
    pub members: usize,
}

/// Address book files of a profile with their names: those registered in prefs
/// (`ldap_2.servers.*.filename`), then any other `abook*.sqlite`/`history.sqlite`.
pub fn find(profile: &Path, prefs: &Prefs) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for key in prefs.branch_keys("ldap_2.servers.") {
        let p = |name: &str| format!("ldap_2.servers.{key}.{name}");
        let Some(file) = prefs.string(&p("filename")) else {
            continue;
        };
        if !file.ends_with(".sqlite") {
            continue;
        }
        let path = profile.join(file);
        let name = prefs
            .string(&p("description"))
            .filter(|d| !d.starts_with("chrome://"))
            .map(str::to_owned)
            .unwrap_or_else(|| default_name(file));
        if path.is_file() && !out.iter().any(|(p, _)| *p == path) {
            out.push((path, name));
        }
    }
    if let Ok(entries) = fs::read_dir(profile) {
        let mut extra: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                (name.starts_with("abook") || name == "history.sqlite") && name.ends_with(".sqlite")
            })
            .collect();
        extra.sort();
        for path in extra {
            if !out.iter().any(|(p, _)| *p == path) {
                let file = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let name = default_name(file);
                out.push((path, name));
            }
        }
    }
    out
}

fn default_name(file: &str) -> String {
    match file {
        "abook.sqlite" => "Personal Address Book".to_owned(),
        "history.sqlite" => "Collected Addresses".to_owned(),
        other => other.trim_end_matches(".sqlite").to_owned(),
    }
}

/// Reads one address book through a temp copy.
pub fn read(path: &Path, name: &str) -> Result<AddressBook> {
    let temp = tempfile::Builder::new()
        .prefix("mazemail-abook-")
        .tempdir()
        .map_err(Error::io("creating a temp directory for", path))?;
    let copy = temp.path().join("abook.sqlite");
    fs::copy(path, &copy).map_err(Error::io("copying", path))?;
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        let side = PathBuf::from(side);
        if side.is_file() {
            let mut target = copy.as_os_str().to_owned();
            target.push(suffix);
            fs::copy(&side, PathBuf::from(target)).map_err(Error::io("copying", &side))?;
        }
    }
    check_wal(&copy).map_err(Error::io("checking the write-ahead log of", path))?;
    let conn = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(Error::sqlite(path))?;
    let book = read_tables(&conn, path, name).map_err(Error::sqlite(path))?;
    drop(conn);
    drop(temp);
    Ok(book)
}

/// Refuses a database whose WAL has another page size than the database itself.
///
/// SQLite before 3.54 can return uninitialized memory for such a pair
/// (RUSTSEC-2026-0459, CVE-2026-119375), and the SQLCipher Maze bundles is still on
/// 3.53. Thunderbird never writes one (SQLite can't change the page size in WAL
/// mode), so a mismatch means a damaged or crafted file. The check runs on the
/// private copy, which nothing else can change before it is opened.
fn check_wal(db: &Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Read as _};
    let mut wal_path = db.as_os_str().to_owned();
    wal_path.push("-wal");
    let wal = match fs::File::open(PathBuf::from(wal_path)) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut header = Vec::with_capacity(12);
    wal.take(12).read_to_end(&mut header)?;
    // SQLite ignores (and resets) a WAL without a complete, valid header, so only a
    // valid one can be replayed into the database.
    let Some(header) = header.first_chunk::<12>() else {
        return Ok(());
    };
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if magic != 0x377f_0682 && magic != 0x377f_0683 {
        return Ok(());
    }
    let wal_page = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let mut db_header = [0u8; 18];
    fs::File::open(db)?.read_exact(&mut db_header)?;
    let db_page = match u16::from_be_bytes([db_header[16], db_header[17]]) {
        1 => 65_536,
        size => u32::from(size),
    };
    if wal_page == db_page {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidData,
            "its page size differs from the database's",
        ))
    }
}

fn read_tables(conn: &Connection, path: &Path, name: &str) -> rusqlite::Result<AddressBook> {
    let mut props: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT card, name, value FROM properties")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (card, key, value) = row?;
            if let Some(value) = value {
                props.entry(card).or_default().insert(key, value);
            }
        }
    }
    let cards = props
        .into_iter()
        .filter_map(|(uid, p)| {
            let vcard = p.get("_vCard").cloned().filter(|v| !v.is_empty());
            let mut emails: Vec<String> = ["PrimaryEmail", "SecondEmail"]
                .iter()
                .filter_map(|k| p.get(*k))
                .map(|e| e.trim().to_owned())
                .filter(|e| e.contains('@'))
                .collect();
            if let Some(vcard) = &vcard {
                for email in vcard_values(vcard, "EMAIL") {
                    if email.contains('@') && !emails.iter().any(|e| e.eq_ignore_ascii_case(&email))
                    {
                        emails.push(email);
                    }
                }
            }
            if emails.is_empty() {
                return None;
            }
            let display_name = p
                .get("DisplayName")
                .cloned()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| {
                    let first = p.get("FirstName").map(String::as_str).unwrap_or_default();
                    let last = p.get("LastName").map(String::as_str).unwrap_or_default();
                    let joined = format!("{first} {last}").trim().to_owned();
                    (!joined.is_empty()).then_some(joined)
                })
                .or_else(|| {
                    vcard
                        .as_deref()
                        .and_then(|v| vcard_values(v, "FN").into_iter().next())
                });
            Some(Card {
                uid,
                display_name,
                emails,
                vcard,
            })
        })
        .collect();

    let mut lists = Vec::new();
    let has_lists: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'lists'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if has_lists {
        let mut stmt = conn.prepare(
            "SELECT l.uid, l.name, (SELECT count(*) FROM list_cards c WHERE c.list = l.uid)
             FROM lists l ORDER BY l.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MailingList {
                uid: r.get(0)?,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                members: r.get::<_, i64>(2)? as usize,
            })
        })?;
        for row in rows {
            lists.push(row?);
        }
    }
    Ok(AddressBook {
        file: path.to_owned(),
        name: name.to_owned(),
        cards,
        lists,
    })
}

/// Values of a vCard property (`EMAIL;PREF=1:a@b` → `a@b`), unfolded.
fn vcard_values(vcard: &str, property: &str) -> Vec<String> {
    let unfolded = vcard.replace("\r\n ", "").replace("\n ", "");
    unfolded
        .lines()
        .filter_map(|line| {
            let (head, value) = line.split_once(':')?;
            let name = head.split(';').next()?.rsplit('.').next()?;
            name.eq_ignore_ascii_case(property)
                .then(|| value.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database in WAL mode with an open connection, so the WAL keeps its frames.
    fn wal_database(dir: &Path, page_size: u32) -> (PathBuf, Connection) {
        let path = dir.join("abook.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA page_size = {page_size}; PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE properties (card TEXT, name TEXT, value TEXT);
             CREATE TABLE lists (uid TEXT, name TEXT, nickName TEXT, description TEXT);
             CREATE TABLE list_cards (list TEXT, card TEXT);
             INSERT INTO properties VALUES ('1', 'PrimaryEmail', 'anna@example.com');"
        ))
        .unwrap();
        (path, conn)
    }

    #[test]
    fn a_wal_with_the_databases_page_size_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _conn) = wal_database(dir.path(), 4096);
        assert!(
            dir.path()
                .join("abook.sqlite-wal")
                .metadata()
                .unwrap()
                .len()
                > 0
        );
        check_wal(&path).unwrap();
        let book = read(&path, "Personal Address Book").unwrap();
        assert_eq!(book.cards.len(), 1, "the card only in the WAL is read");
    }

    #[test]
    fn a_wal_with_another_page_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _conn) = wal_database(dir.path(), 4096);
        // Rewrite the WAL header's page size (bytes 8..12) to 8192.
        let wal = dir.path().join("abook.sqlite-wal");
        let mut bytes = fs::read(&wal).unwrap();
        bytes[8..12].copy_from_slice(&8192u32.to_be_bytes());
        fs::write(&wal, bytes).unwrap();
        let err = read(&path, "Personal Address Book").unwrap_err();
        assert!(
            err.to_string().contains("page size differs"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_or_an_empty_wal_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abook.sqlite");
        fs::write(&path, b"").unwrap();
        check_wal(&path).unwrap();
        fs::write(dir.path().join("abook.sqlite-wal"), b"").unwrap();
        check_wal(&path).unwrap();
    }

    #[test]
    fn reads_vcard_values() {
        let v = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Anna Example\r\nEMAIL;PREF=1:anna@\r\n example.com\r\nitem1.EMAIL:a2@example.com\r\nEND:VCARD\r\n";
        assert_eq!(
            vcard_values(v, "EMAIL"),
            vec!["anna@example.com", "a2@example.com"]
        );
        assert_eq!(vcard_values(v, "FN"), vec!["Anna Example"]);
    }
}
