//! Tags, contacts, rules and saved searches.

use mazemail_types::{AccountId, FolderId, TagId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    Contact, ContactId, Rule, RuleId, RuleInput, SavedSearch, SavedSearchId, Tag, UnixTime,
};
use crate::repo::accounts::ensure_changed;
use crate::{Error, Result, WriteTx};

fn tag_row(row: &Row<'_>) -> rusqlite::Result<Tag> {
    Ok(Tag {
        id: TagId(row.get(0)?),
        keyword: row.get(1)?,
        name: row.get(2)?,
        color: row.get(3)?,
        ordinal: row.get(4)?,
    })
}

pub(crate) fn tags(conn: &Connection) -> Result<Vec<Tag>> {
    let mut stmt = conn
        .prepare_cached("SELECT id, keyword, name, color, ordinal FROM tag ORDER BY ordinal, id")?;
    Ok(stmt
        .query_map([], tag_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn tag_by_keyword(conn: &Connection, keyword: &str) -> Result<Option<Tag>> {
    Ok(conn
        .prepare_cached("SELECT id, keyword, name, color, ordinal FROM tag WHERE keyword = ?1")?
        .query_row([keyword.trim().to_lowercase()], tag_row)
        .optional()?)
}

const CONTACT_COLUMNS: &str = "id, book, display_name, email, frequency, last_used, vcard";

fn contact_row(row: &Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: ContactId(row.get(0)?),
        book: row.get(1)?,
        display_name: row.get(2)?,
        email: row.get(3)?,
        frequency: row.get(4)?,
        last_used: row.get(5)?,
        vcard: row.get(6)?,
    })
}

/// Contacts whose address or name starts with `prefix` (case-insensitive), most used
/// first.
pub(crate) fn search_contacts(
    conn: &Connection,
    prefix: &str,
    limit: usize,
) -> Result<Vec<Contact>> {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("{escaped}%");
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contact
         WHERE email LIKE ?1 ESCAPE '\\' OR display_name LIKE ?1 ESCAPE '\\'
            OR display_name LIKE '% ' || ?1 ESCAPE '\\'
         ORDER BY frequency DESC, last_used DESC, id LIMIT ?2"
    ))?;
    Ok(stmt
        .query_map(params![pattern, limit as i64], contact_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn contact_by_email(conn: &Connection, email: &str) -> Result<Option<Contact>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {CONTACT_COLUMNS} FROM contact WHERE email = ?1
             ORDER BY book != 'collected', frequency DESC LIMIT 1"
        ))?
        .query_row([email], contact_row)
        .optional()?)
}

/// Every address book entry without its vCard (`vcard` is `None`), for the Address
/// Book's lists: name, address, book, use. A few hundred bytes per entry, so 10,000
/// contacts read in a few milliseconds.
pub(crate) fn contact_list(conn: &Connection) -> Result<Vec<Contact>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, book, display_name, email, frequency, last_used, NULL FROM contact
         ORDER BY id",
    )?;
    Ok(stmt
        .query_map([], contact_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Every entry for an address (case-insensitive), in every book, with its vCard.
pub(crate) fn contacts_by_email(conn: &Connection, email: &str) -> Result<Vec<Contact>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contact WHERE email = ?1 ORDER BY id"
    ))?;
    Ok(stmt
        .query_map([email.trim()], contact_row)?
        .collect::<rusqlite::Result<_>>()?)
}

const RULE_COLUMNS: &str = "id, account_id, name, ordinal, enabled, trigger_kind, condition_json, actions_json, created, updated";

fn rule_row(row: &Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: RuleId(row.get(0)?),
        input: RuleInput {
            account: row.get::<_, Option<i64>>(1)?.map(AccountId),
            name: row.get(2)?,
            ordinal: row.get(3)?,
            enabled: row.get(4)?,
            trigger: row.get(5)?,
            condition_json: row.get(6)?,
            actions_json: row.get(7)?,
        },
        created: row.get(8)?,
        updated: row.get(9)?,
    })
}

/// Rules in execution order: global rules and the account's rules, by ordinal.
pub(crate) fn rules(conn: &Connection, account: Option<AccountId>) -> Result<Vec<Rule>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {RULE_COLUMNS} FROM rule
         WHERE ?1 IS NULL OR account_id IS NULL OR account_id = ?1
         ORDER BY ordinal, id"
    ))?;
    Ok(stmt
        .query_map([account.map(|a| a.0)], rule_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn saved_search_row(row: &Row<'_>) -> Result<SavedSearch> {
    let folders: Option<String> = row.get(3)?;
    Ok(SavedSearch {
        id: SavedSearchId(row.get(0)?),
        name: row.get(1)?,
        query: row.get(2)?,
        folders: folders
            .as_deref()
            .map(serde_json::from_str::<Vec<FolderId>>)
            .transpose()?,
        ordinal: row.get(4)?,
        created: row.get(5)?,
    })
}

pub(crate) fn saved_searches(conn: &Connection) -> Result<Vec<SavedSearch>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, query, folders_json, ordinal, created FROM saved_search
         ORDER BY ordinal, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(saved_search_row(row)?);
    }
    Ok(out)
}

impl WriteTx<'_> {
    // ---- tags --------------------------------------------------------------------

    /// Creates a tag for an IMAP keyword (stored lowercase). Messages that already carry
    /// the keyword get the tag.
    pub fn create_tag(
        &mut self,
        keyword: &str,
        name: &str,
        color: Option<&str>,
        ordinal: i64,
    ) -> Result<TagId> {
        let keyword = keyword.trim().to_lowercase();
        if keyword.is_empty() || keyword.contains(char::is_whitespace) {
            return Err(Error::Invalid(
                "an IMAP keyword can't be empty or contain spaces",
            ));
        }
        self.conn
            .prepare_cached(
                "INSERT INTO tag (keyword, name, color, ordinal) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![keyword, name, color, ordinal])?;
        let id = TagId(self.conn.last_insert_rowid());
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_tag (message_id, tag_id)
                 SELECT DISTINCT ml.message_id, ?1
                 FROM message_location ml, json_each(ml.keywords_json) k
                 WHERE ml.keywords_json IS NOT NULL AND k.value = ?2",
            )?
            .execute(params![id.0, keyword])?;
        self.changes.settings = true;
        Ok(id)
    }

    /// Renames, recolors or reorders a tag (the keyword stays).
    pub fn update_tag(
        &mut self,
        id: TagId,
        name: &str,
        color: Option<&str>,
        ordinal: i64,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE tag SET name = ?2, color = ?3, ordinal = ?4 WHERE id = ?1")?
            .execute(params![id.0, name, color, ordinal])?;
        ensure_changed(changed, "tag", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes a tag definition. The keyword stays on the messages (and on the server).
    pub fn delete_tag(&mut self, id: TagId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM tag WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "tag", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    // ---- contacts ----------------------------------------------------------------

    /// Records that you wrote to (or heard from) `email`: creates the collected contact
    /// or bumps its frequency and last use. Fills in a missing display name.
    pub fn bump_contact(
        &mut self,
        email: &str,
        display_name: Option<&str>,
        at: UnixTime,
    ) -> Result<ContactId> {
        let email = email.trim();
        if email.is_empty() {
            return Err(Error::Invalid("empty email address"));
        }
        self.conn
            .prepare_cached(
                "INSERT INTO contact (book, display_name, email, frequency, last_used)
                 VALUES ('collected', ?1, ?2, 1, ?3)
                 ON CONFLICT (book, email) DO UPDATE SET
                   frequency = frequency + 1,
                   last_used = max(COALESCE(last_used, 0), excluded.last_used),
                   display_name = COALESCE(display_name, excluded.display_name)",
            )?
            .execute(params![display_name, email, at])?;
        self.changes.contacts = true;
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM contact WHERE book = 'collected' AND email = ?1")?
            .query_row([email], |row| row.get(0).map(ContactId))?)
    }

    /// Creates or replaces an address book entry by `(book, email)`.
    pub fn upsert_contact(
        &mut self,
        book: &str,
        email: &str,
        display_name: Option<&str>,
        vcard: Option<&str>,
    ) -> Result<ContactId> {
        self.conn
            .prepare_cached(
                "INSERT INTO contact (book, display_name, email, vcard) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (book, email) DO UPDATE SET
                   display_name = excluded.display_name, vcard = excluded.vcard",
            )?
            .execute(params![book, display_name, email.trim(), vcard])?;
        self.changes.contacts = true;
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM contact WHERE book = ?1 AND email = ?2")?
            .query_row(params![book, email.trim()], |row| row.get(0).map(ContactId))?)
    }

    /// Deletes a contact.
    pub fn delete_contact(&mut self, id: ContactId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM contact WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "contact", id.0)?;
        self.changes.contacts = true;
        Ok(())
    }

    // ---- rules -------------------------------------------------------------------

    /// Creates a rule.
    pub fn create_rule(&mut self, input: &RuleInput) -> Result<RuleId> {
        self.conn
            .prepare_cached(
                "INSERT INTO rule (account_id, name, ordinal, enabled, trigger_kind, condition_json,
                   actions_json, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            )?
            .execute(params![
                input.account.map(|a| a.0),
                input.name,
                input.ordinal,
                input.enabled,
                input.trigger,
                input.condition_json,
                input.actions_json,
                self.now
            ])?;
        self.changes.settings = true;
        Ok(RuleId(self.conn.last_insert_rowid()))
    }

    /// Replaces a rule.
    pub fn update_rule(&mut self, id: RuleId, input: &RuleInput) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE rule SET account_id = ?2, name = ?3, ordinal = ?4, enabled = ?5,
                   trigger_kind = ?6, condition_json = ?7, actions_json = ?8, updated = ?9
                 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                input.account.map(|a| a.0),
                input.name,
                input.ordinal,
                input.enabled,
                input.trigger,
                input.condition_json,
                input.actions_json,
                self.now
            ])?;
        ensure_changed(changed, "rule", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes a rule.
    pub fn delete_rule(&mut self, id: RuleId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM rule WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "rule", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    // ---- saved searches ----------------------------------------------------------

    /// Creates a saved search; `folders: None` searches everything.
    pub fn create_saved_search(
        &mut self,
        name: &str,
        query: &str,
        folders: Option<&[FolderId]>,
        ordinal: i64,
    ) -> Result<SavedSearchId> {
        let folders = folders.map(serde_json::to_string).transpose()?;
        self.conn
            .prepare_cached(
                "INSERT INTO saved_search (name, query, folders_json, ordinal, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![name, query, folders, ordinal, self.now])?;
        self.changes.settings = true;
        Ok(SavedSearchId(self.conn.last_insert_rowid()))
    }

    /// Replaces a saved search.
    pub fn update_saved_search(
        &mut self,
        id: SavedSearchId,
        name: &str,
        query: &str,
        folders: Option<&[FolderId]>,
        ordinal: i64,
    ) -> Result<()> {
        let folders = folders.map(serde_json::to_string).transpose()?;
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE saved_search SET name = ?2, query = ?3, folders_json = ?4, ordinal = ?5
                 WHERE id = ?1",
            )?
            .execute(params![id.0, name, query, folders, ordinal])?;
        ensure_changed(changed, "saved search", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes a saved search.
    pub fn delete_saved_search(&mut self, id: SavedSearchId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM saved_search WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "saved search", id.0)?;
        self.changes.settings = true;
        Ok(())
    }
}
