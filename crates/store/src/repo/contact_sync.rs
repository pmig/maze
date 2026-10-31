//! Contacts synced from an account's server (schema v13, ADR-024).
//!
//! A synced person is a row of `contact_person` (its server id, etag, vCard and
//! photo URL) plus one `contact` row per email address in the person's book, which
//! is what completion and the Address Book read. `contact` allows an address only
//! once per book: when two people of a book share one, the row belongs to one of
//! them and moves to the other when it leaves the first ([`WriteTx::upsert_server_contact`]).
//! `contact_source` keeps each book's sync token; `contact_photo` remembers which
//! photo URLs are in the encrypted blob cache and which have no photo.

use std::collections::BTreeSet;

use mazemail_types::AccountId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::UnixTime;
use crate::{Error, Result, WriteTx};

/// A person from a server, to store ([`WriteTx::upsert_server_contact`]).
#[derive(Clone, PartialEq, Eq)]
pub struct ServerContact {
    /// The server's id (`people/c…`).
    pub resource_name: String,
    /// The server's version.
    pub etag: Option<String>,
    /// The name to show.
    pub display_name: Option<String>,
    /// The person's addresses, in the server's order.
    pub emails: Vec<String>,
    /// vCard 4.0 of the person.
    pub vcard: String,
    /// The person's photo, if any.
    pub photo_url: Option<String>,
}

impl std::fmt::Debug for ServerContact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerContact")
            .field("emails", &self.emails.len())
            .field("photo", &self.photo_url.is_some())
            .finish_non_exhaustive()
    }
}

/// A stored synced person.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredServerContact {
    /// The book (`google:<account uuid>`).
    pub book: String,
    /// The server's id.
    pub resource_name: String,
    /// The version Maze has.
    pub etag: Option<String>,
    /// The name to show.
    pub display_name: Option<String>,
    /// The person's addresses, lowercase.
    pub emails: Vec<String>,
    /// vCard 4.0 of the person.
    pub vcard: String,
    /// The person's photo URL, if any.
    pub photo_url: Option<String>,
    /// When it was last stored.
    pub updated_at: UnixTime,
}

impl std::fmt::Debug for StoredServerContact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredServerContact")
            .field("book", &self.book)
            .field("emails", &self.emails.len())
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

/// Where a synced book stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSourceState {
    /// The book.
    pub book: String,
    /// Its account.
    pub account: AccountId,
    /// `google` or `google-other`.
    pub kind: String,
    /// The token of the next incremental sync.
    pub sync_token: Option<String>,
    /// When the last full listing finished.
    pub last_full_sync: Option<UnixTime>,
    /// When the last sync finished.
    pub last_sync: Option<UnixTime>,
    /// `ok`, `missing_scope` or `error`.
    pub state: String,
    /// The last error, for people.
    pub error: Option<String>,
}

/// What Maze knows about a photo URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactPhotoState {
    /// When it was fetched.
    pub fetched_at: UnixTime,
    /// Whether there was a photo (in the blob cache).
    pub found: bool,
}

const PERSON_COLUMNS: &str =
    "book, resource_name, etag, display_name, emails, vcard, photo_url, updated_at";

fn person_row(row: &Row<'_>) -> rusqlite::Result<StoredServerContact> {
    let emails: String = row.get(4)?;
    Ok(StoredServerContact {
        book: row.get(0)?,
        resource_name: row.get(1)?,
        etag: row.get(2)?,
        display_name: row.get(3)?,
        emails: emails
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        vcard: row.get(5)?,
        photo_url: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

const SOURCE_COLUMNS: &str =
    "book, account_id, kind, sync_token, last_full_sync, last_sync, state, error";

fn source_row(row: &Row<'_>) -> rusqlite::Result<ContactSourceState> {
    Ok(ContactSourceState {
        book: row.get(0)?,
        account: AccountId(row.get(1)?),
        kind: row.get(2)?,
        sync_token: row.get(3)?,
        last_full_sync: row.get(4)?,
        last_sync: row.get(5)?,
        state: row.get(6)?,
        error: row.get(7)?,
    })
}

pub(crate) fn contact_source(conn: &Connection, book: &str) -> Result<Option<ContactSourceState>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {SOURCE_COLUMNS} FROM contact_source WHERE book = ?1"
        ))?
        .query_row([book], source_row)
        .optional()?)
}

pub(crate) fn contact_sources(
    conn: &Connection,
    account: AccountId,
) -> Result<Vec<ContactSourceState>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SOURCE_COLUMNS} FROM contact_source WHERE account_id = ?1 ORDER BY book"
    ))?;
    Ok(stmt
        .query_map([account.0], source_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn server_contact(
    conn: &Connection,
    book: &str,
    resource_name: &str,
) -> Result<Option<StoredServerContact>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {PERSON_COLUMNS} FROM contact_person WHERE book = ?1 AND resource_name = ?2"
        ))?
        .query_row(params![book, resource_name], person_row)
        .optional()?)
}

pub(crate) fn server_contact_by_email(
    conn: &Connection,
    book: &str,
    email: &str,
) -> Result<Option<StoredServerContact>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {} FROM contact c
             JOIN contact_person p ON p.book = c.book AND p.resource_name = c.resource_name
             WHERE c.book = ?1 AND c.email = ?2",
            prefixed("p.", PERSON_COLUMNS)
        ))?
        .query_row(params![book, email.trim()], person_row)
        .optional()?)
}

pub(crate) fn server_contact_of_entry(
    conn: &Connection,
    entry: crate::model::ContactId,
) -> Result<Option<StoredServerContact>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {} FROM contact c
             JOIN contact_person p ON p.book = c.book AND p.resource_name = c.resource_name
             WHERE c.id = ?1",
            prefixed("p.", PERSON_COLUMNS)
        ))?
        .query_row([entry.0], person_row)
        .optional()?)
}

pub(crate) fn server_contacts(conn: &Connection, book: &str) -> Result<Vec<StoredServerContact>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PERSON_COLUMNS} FROM contact_person WHERE book = ?1
         ORDER BY display_name COLLATE NOCASE, resource_name"
    ))?;
    Ok(stmt
        .query_map([book], person_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn contact_photo(conn: &Connection, url: &str) -> Result<Option<ContactPhotoState>> {
    Ok(conn
        .prepare_cached("SELECT fetched_at, found FROM contact_photo WHERE url = ?1")?
        .query_row([url], |row| {
            Ok(ContactPhotoState {
                fetched_at: row.get(0)?,
                found: row.get::<_, i64>(1)? != 0,
            })
        })
        .optional()?)
}

pub(crate) fn contact_photo_url(
    conn: &Connection,
    email: &str,
    book: Option<&str>,
) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT p.photo_url FROM contact c
             JOIN contact_person p ON p.book = c.book AND p.resource_name = c.resource_name
             WHERE c.email = ?1 AND (?2 IS NULL OR c.book = ?2) AND p.photo_url IS NOT NULL
             ORDER BY c.book NOT LIKE 'google:%', c.book LIMIT 1",
        )?
        .query_row(params![email.trim(), book], |row| row.get(0))
        .optional()?)
}

pub(crate) fn contact_photos_due(
    conn: &Connection,
    fetched_before: UnixTime,
    limit: usize,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT p.photo_url FROM contact_person p
         LEFT JOIN contact_photo ph ON ph.url = p.photo_url
         WHERE p.photo_url IS NOT NULL AND (ph.url IS NULL OR ph.fetched_at < ?1)
         ORDER BY p.book NOT LIKE 'google:%', p.photo_url LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![fetched_before, limit as i64], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn prefixed(prefix: &str, columns: &str) -> String {
    columns
        .split(", ")
        .map(|c| format!("{prefix}{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Addresses trimmed, without empty ones and duplicates (case-insensitive), in
/// order: `(as given, lowercase)`.
fn normalize_emails(emails: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for email in emails {
        let email = email.trim();
        let key = email.to_lowercase();
        if email.is_empty() || out.iter().any(|(_, k)| *k == key) {
            continue;
        }
        out.push((email.to_owned(), key));
    }
    out
}

fn emails_of(conn: &Connection, book: &str, resource_name: &str) -> Result<Option<Vec<String>>> {
    let emails: Option<String> = conn
        .prepare_cached("SELECT emails FROM contact_person WHERE book = ?1 AND resource_name = ?2")?
        .query_row(params![book, resource_name], |row| row.get(0))
        .optional()?;
    Ok(emails.map(|e| e.lines().map(str::to_owned).collect()))
}

impl WriteTx<'_> {
    /// Registers a synced book of `account` (idempotent). `kind` is the book name's
    /// prefix (`google`, `google-other`).
    pub fn ensure_contact_source(
        &mut self,
        book: &str,
        account: AccountId,
        kind: &str,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO contact_source (book, account_id, kind) VALUES (?1, ?2, ?3)
                 ON CONFLICT (book) DO NOTHING",
            )?
            .execute(params![book, account.0, kind])?;
        Ok(())
    }

    /// Stores a person of `book` as the server sent it (the book must be registered,
    /// [`Self::ensure_contact_source`]): its `contact_person` row and one `contact`
    /// row per address. An address another person of the book already has stays
    /// theirs; addresses the person no longer has go (to another person of the book
    /// who has them, if any). `seen` is the sync run (see
    /// [`Self::finish_full_contact_sync`]).
    pub fn upsert_server_contact(
        &mut self,
        book: &str,
        person: &ServerContact,
        seen: i64,
    ) -> Result<()> {
        if person.resource_name.trim().is_empty() {
            return Err(Error::Invalid("a synced contact without server id"));
        }
        let emails = normalize_emails(&person.emails);
        let old = emails_of(self.conn, book, &person.resource_name)?.unwrap_or_default();
        let joined = emails
            .iter()
            .map(|(_, key)| key.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.conn
            .prepare_cached(
                "INSERT INTO contact_person
                   (book, resource_name, etag, display_name, emails, vcard, photo_url, seen,
                    updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (book, resource_name) DO UPDATE SET
                   etag = excluded.etag, display_name = excluded.display_name,
                   emails = excluded.emails, vcard = excluded.vcard,
                   photo_url = excluded.photo_url, seen = excluded.seen,
                   updated_at = excluded.updated_at",
            )?
            .execute(params![
                book,
                person.resource_name,
                person.etag,
                person.display_name,
                joined,
                person.vcard,
                person.photo_url,
                seen,
                self.now,
            ])?;
        let dropped: Vec<String> = old
            .into_iter()
            .filter(|key| !emails.iter().any(|(_, k)| k == key))
            .collect();
        for key in &dropped {
            self.conn
                .prepare_cached(
                    "DELETE FROM contact WHERE book = ?1 AND email = ?2 AND resource_name = ?3",
                )?
                .execute(params![book, key, person.resource_name])?;
        }
        for (email, _) in &emails {
            self.conn
                .prepare_cached(
                    "INSERT INTO contact (book, display_name, email, vcard, resource_name)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (book, email) DO UPDATE SET
                       display_name = excluded.display_name, email = excluded.email,
                       vcard = excluded.vcard, resource_name = excluded.resource_name
                     WHERE contact.resource_name IS NULL
                        OR contact.resource_name = excluded.resource_name
                        OR NOT EXISTS (SELECT 1 FROM contact_person p
                                       WHERE p.book = contact.book
                                         AND p.resource_name = contact.resource_name)",
                )?
                .execute(params![
                    book,
                    person.display_name,
                    email,
                    person.vcard,
                    person.resource_name
                ])?;
        }
        self.rehome_emails(book, &dropped)?;
        self.changes.contacts = true;
        Ok(())
    }

    /// Removes a person of `book` and its `contact` rows (their addresses go to
    /// another person of the book who has them). Returns whether it existed.
    pub fn delete_server_contact(&mut self, book: &str, resource_name: &str) -> Result<bool> {
        let Some(emails) = emails_of(self.conn, book, resource_name)? else {
            return Ok(false);
        };
        // The trigger deletes the person's `contact` rows.
        self.conn
            .prepare_cached("DELETE FROM contact_person WHERE book = ?1 AND resource_name = ?2")?
            .execute(params![book, resource_name])?;
        self.rehome_emails(book, &emails)?;
        self.changes.contacts = true;
        Ok(true)
    }

    /// Ends a full listing of `book`: people the run `seen` didn't store are gone
    /// on the server and are removed. Returns how many.
    pub fn finish_full_contact_sync(&mut self, book: &str, seen: i64) -> Result<usize> {
        let gone: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT resource_name, emails FROM contact_person WHERE book = ?1 AND seen < ?2",
            )?;
            stmt.query_map(params![book, seen], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        if gone.is_empty() {
            return Ok(0);
        }
        self.conn
            .prepare_cached("DELETE FROM contact_person WHERE book = ?1 AND seen < ?2")?
            .execute(params![book, seen])?;
        let emails: BTreeSet<String> = gone
            .iter()
            .flat_map(|(_, emails)| emails.lines().map(str::to_owned))
            .collect();
        self.rehome_emails(book, &emails.into_iter().collect::<Vec<_>>())?;
        self.changes.contacts = true;
        Ok(gone.len())
    }

    /// Gives each of `emails` (lowercase) that has no row in `book` any more to a
    /// person of the book who has it.
    fn rehome_emails(&mut self, book: &str, emails: &[String]) -> Result<()> {
        for email in emails {
            let taken = self
                .conn
                .prepare_cached("SELECT 1 FROM contact WHERE book = ?1 AND email = ?2")?
                .query_row(params![book, email], |_| Ok(()))
                .optional()?
                .is_some();
            if taken {
                continue;
            }
            let heir: Option<(String, Option<String>, String)> = self
                .conn
                .prepare_cached(
                    "SELECT resource_name, display_name, vcard FROM contact_person
                     WHERE book = ?1
                       AND instr(char(10) || emails || char(10), char(10) || ?2 || char(10)) > 0
                     ORDER BY resource_name LIMIT 1",
                )?
                .query_row(params![book, email], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()?;
            if let Some((resource_name, display_name, vcard)) = heir {
                self.conn
                    .prepare_cached(
                        "INSERT INTO contact (book, display_name, email, vcard, resource_name)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?
                    .execute(params![book, display_name, email, vcard, resource_name])?;
            }
        }
        Ok(())
    }

    /// Records the end of a sync of `book`: the token for the next incremental sync
    /// (`None`: the next sync lists everything), and that it went well.
    pub fn set_contact_sync_token(
        &mut self,
        book: &str,
        sync_token: Option<&str>,
        full: bool,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE contact_source SET sync_token = ?2, last_sync = ?3,
                   last_full_sync = CASE WHEN ?4 THEN ?3 ELSE last_full_sync END,
                   state = 'ok', error = NULL
                 WHERE book = ?1",
            )?
            .execute(params![book, sync_token, self.now, full])?;
        if changed == 0 {
            return Err(Error::Invalid("unknown contact book"));
        }
        Ok(())
    }

    /// Records that syncing `book` failed: `state` `missing_scope` or `error`, with
    /// a message for people. A `missing_scope` book also forgets its sync token.
    pub fn set_contact_source_error(&mut self, book: &str, state: &str, error: &str) -> Result<()> {
        if !matches!(state, "missing_scope" | "error") {
            return Err(Error::Invalid("unknown contact source state"));
        }
        self.conn
            .prepare_cached(
                "UPDATE contact_source SET state = ?2, error = ?3,
                   sync_token = CASE WHEN ?2 = 'missing_scope' THEN NULL ELSE sync_token END
                 WHERE book = ?1",
            )?
            .execute(params![book, state, error])?;
        self.changes.contacts = true;
        Ok(())
    }

    /// Removes every synced book of `account` with its people and their `contact`
    /// rows (when contacts are disconnected; removing the account does the same).
    /// Returns the photo URLs that no longer belong to anyone, whose cache entries
    /// the caller removes.
    pub fn delete_contact_books(&mut self, account: AccountId) -> Result<Vec<String>> {
        let removed = self
            .conn
            .prepare_cached("DELETE FROM contact_source WHERE account_id = ?1")?
            .execute([account.0])?;
        if removed > 0 {
            self.changes.contacts = true;
        }
        self.prune_contact_photos()
    }

    /// Forgets photo URLs no person has any more; returns them.
    pub fn prune_contact_photos(&mut self) -> Result<Vec<String>> {
        let urls: Vec<String> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT url FROM contact_photo WHERE url NOT IN
                   (SELECT photo_url FROM contact_person WHERE photo_url IS NOT NULL)",
            )?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for url in &urls {
            self.conn
                .prepare_cached("DELETE FROM contact_photo WHERE url = ?1")?
                .execute([url])?;
        }
        Ok(urls)
    }

    /// Records that photo `url` was fetched now; `found` false remembers that there
    /// is no photo.
    pub fn record_contact_photo(&mut self, url: &str, found: bool) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO contact_photo (url, fetched_at, found) VALUES (?1, ?2, ?3)
                 ON CONFLICT (url) DO UPDATE SET
                   fetched_at = excluded.fetched_at, found = excluded.found",
            )?
            .execute(params![url, self.now, found])?;
        Ok(())
    }
}
