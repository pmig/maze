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
