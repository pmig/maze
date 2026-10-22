//! Message upsert (with Gmail dedup), locations, flags, keywords and tags, bodies.

use std::collections::BTreeSet;

use mazemail_types::{
    AccountId, Address, BlobId, Flags, FolderId, IdentityId, MessageId, TagId, ThreadId,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::blob::{self, StagedBlob};
use crate::model::{BodyState, Location, MessageDetail, NewLocation, NewMessage, UpsertOutcome};
use crate::repo::threads::MAX_REFERENCES;
use crate::{Error, Result, WriteTx};

/// Lowercases, deduplicates and sorts IMAP keywords (they are case-insensitive).
pub(crate) fn normalize_keywords<'a>(
    keywords: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let set: BTreeSet<String> = keywords
        .into_iter()
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    set.into_iter().collect()
}

fn keywords_to_sql(keywords: &[String]) -> Result<Option<String>> {
    if keywords.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(keywords)?))
    }
}

fn keywords_from_sql(json: Option<&str>) -> Result<Vec<String>> {
    Ok(json
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

fn addresses_to_sql(addresses: &[Address]) -> Result<Option<String>> {
    if addresses.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(addresses)?))
    }
}

fn addresses_from_sql(json: Option<&str>) -> Result<Vec<Address>> {
    Ok(json
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

fn flags_from_sql(value: i64) -> Flags {
    Flags::from_bits_retain(value as u32)
}

pub(crate) fn locations(conn: &Connection, message: MessageId) -> Result<Vec<Location>> {
    let mut stmt = conn.prepare_cached(
        "SELECT folder_id, uid, modseq, flags, keywords_json FROM message_location
         WHERE message_id = ?1 ORDER BY folder_id",
    )?;
    let mut rows = stmt.query([message.0])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(Location {
            folder: FolderId(row.get(0)?),
            uid: row.get(1)?,
            modseq: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            flags: flags_from_sql(row.get(3)?),
            keywords: keywords_from_sql(row.get::<_, Option<String>>(4)?.as_deref())?,
        });
    }
    Ok(out)
}

const DETAIL_COLUMNS: &str = "id, account_id, thread_id, blob_id, body_state, rfc_message_id, \
     in_reply_to, references_json, date, received, from_addr, from_name, to_json, cc_json, \
     delivered_to, subject, snippet, size, has_attachments, list_id, bodystructure_json, \
     gm_msgid, gm_thrid, auth_json, identity_hint, detached_at";

fn detail_row(row: &Row<'_>) -> Result<MessageDetail> {
    let from_addr: Option<String> = row.get(10)?;
    let from_name: Option<String> = row.get(11)?;
    let references: Option<String> = row.get(7)?;
    Ok(MessageDetail {
        id: MessageId(row.get(0)?),
        account: AccountId(row.get(1)?),
        thread: ThreadId(row.get(2)?),
        blob: row.get::<_, Option<[u8; 32]>>(3)?.map(BlobId),
        body_state: BodyState::from_i64(row.get(4)?),
        rfc_message_id: row.get(5)?,
        in_reply_to: row.get(6)?,
        references: references
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default(),
        date: row.get(8)?,
        received: row.get(9)?,
        from: from_addr.map(|email| Address {
            name: from_name,
            email,
        }),
        to: addresses_from_sql(row.get::<_, Option<String>>(12)?.as_deref())?,
        cc: addresses_from_sql(row.get::<_, Option<String>>(13)?.as_deref())?,
        delivered_to: row.get(14)?,
        subject: row.get(15)?,
        snippet: row.get(16)?,
        size: row.get::<_, Option<i64>>(17)?.map(|v| v.max(0) as u64),
        has_attachments: row.get(18)?,
        list_id: row.get(19)?,
        bodystructure_json: row.get(20)?,
        gm_msgid: row.get::<_, Option<i64>>(21)?.map(|v| v as u64),
        gm_thrid: row.get::<_, Option<i64>>(22)?.map(|v| v as u64),
        auth_json: row.get(23)?,
        identity_hint: row.get::<_, Option<i64>>(24)?.map(IdentityId),
        locations: Vec::new(),
        tags: Vec::new(),
        detached_at: row.get(25)?,
    })
}

pub(crate) fn message(conn: &Connection, id: MessageId) -> Result<Option<MessageDetail>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {DETAIL_COLUMNS} FROM message WHERE id = ?1"
    ))?;
    let mut rows = stmt.query([id.0])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut detail = detail_row(row)?;
    drop(rows);
    detail.locations = locations(conn, id)?;
    detail.tags = message_tags(conn, id)?;
    Ok(Some(detail))
}

pub(crate) fn message_tags(conn: &Connection, id: MessageId) -> Result<Vec<TagId>> {
    let mut stmt = conn
        .prepare_cached("SELECT tag_id FROM message_tag WHERE message_id = ?1 ORDER BY tag_id")?;
    Ok(stmt
        .query_map([id.0], |row| row.get(0).map(TagId))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn message_by_uid(
    conn: &Connection,
    folder: FolderId,
    uid: u32,
) -> Result<Option<MessageId>> {
    Ok(conn
        .prepare_cached(
            "SELECT message_id FROM message_location WHERE folder_id = ?1 AND uid = ?2",
        )?
        .query_row(params![folder.0, uid], |row| row.get(0).map(MessageId))
        .optional()?)
}

pub(crate) fn messages_by_rfc_id(
    conn: &Connection,
    account: AccountId,
    rfc_message_id: &str,
) -> Result<Vec<MessageId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM message WHERE rfc_message_id = ?1 AND account_id = ?2 ORDER BY id",
    )?;
    Ok(stmt
        .query_map(params![rfc_message_id, account.0], |row| {
            row.get(0).map(MessageId)
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn message_by_gm_msgid(
    conn: &Connection,
    account: AccountId,
    gm_msgid: u64,
) -> Result<Option<MessageId>> {
    Ok(conn
        .prepare_cached("SELECT id FROM message WHERE account_id = ?1 AND gm_msgid = ?2")?
        .query_row(params![account.0, gm_msgid as i64], |row| {
            row.get(0).map(MessageId)
        })
        .optional()?)
}

/// UIDs and flags of every location in `folder` with a UID, ascending by UID (for the
/// sync engine's flag and expunge diffs).
pub(crate) fn folder_uids(conn: &Connection, folder: FolderId) -> Result<Vec<(u32, Flags)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT uid, flags FROM message_location WHERE folder_id = ?1 AND uid IS NOT NULL
         ORDER BY uid",
    )?;
    Ok(stmt
        .query_map([folder.0], |row| {
            Ok((row.get(0)?, flags_from_sql(row.get(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

impl WriteTx<'_> {
    /// Inserts a message or updates the one it already is.
    ///
    /// An existing message is found, in this order, by Gmail's `(account, X-GM-MSGID)`,
    /// by any of the given `(folder, uid)` locations, or as a detached message (no
    /// locations left after an expunge or UIDVALIDITY reset) with the same Message-ID in
    /// the account, which keeps its body. An existing message gets its locations added or
    /// updated (flags, keywords, modseq) and empty summary fields filled in; headers are
    /// not rewritten. A new message gets a thread according to `message.thread`.
    ///
    /// All locations must be folders of `message.account`.
    pub fn upsert_message(&mut self, message: &NewMessage) -> Result<UpsertOutcome> {
        for location in &message.locations {
            self.check_location_folder(location.folder, message.account)?;
        }

        let existing = self.find_existing(message)?;
        let (id, thread, created) = match existing {
            Some((id, thread)) => {
                self.conn
                    .prepare_cached(
                        "UPDATE message SET
                           snippet = COALESCE(snippet, ?2),
                           bodystructure_json = COALESCE(bodystructure_json, ?3),
                           auth_json = COALESCE(auth_json, ?4),
                           size = COALESCE(size, ?5),
                           gm_thrid = COALESCE(gm_thrid, ?6),
                           gm_msgid = COALESCE(gm_msgid, ?7),
                           received = COALESCE(received, ?8),
                           identity_hint = COALESCE(identity_hint, ?9)
                         WHERE id = ?1",
                    )?
                    .execute(params![
                        id.0,
                        message.snippet,
                        message.bodystructure_json,
                        message.auth_json,
                        message.size.map(|s| s as i64),
                        message.gm_thrid.map(|v| v as i64),
                        message.gm_msgid.map(|v| v as i64),
                        message.received,
                        message.identity_hint.map(|i| i.0),
                    ])?;
                (id, thread, false)
            }
            None => {
                let thread = self.resolve_thread(message)?;
                let id = self.insert_message(message, thread)?;
                (id, thread, true)
            }
        };

        for location in &message.locations {
            // A UID the server now reports for this message belonged to another one:
            // that location is stale.
            if let Some(uid) = location.uid
                && let Some(owner) = message_by_uid(self.conn, location.folder, uid)?
                && owner != id
            {
                self.delete_location(owner, location.folder)?;
                let thread = self.thread_of(owner)?;
                self.after_location_change(owner, thread)?;
            }
            self.put_location(id, location)?;
        }
        if message.replace_labels {
            let keep: Vec<i64> = message.locations.iter().map(|l| l.folder.0).collect();
            let stale: Vec<i64> = self
                .conn
                .prepare_cached(
                    "SELECT ml.folder_id FROM message_location ml JOIN folder f ON f.id = ml.folder_id
                     WHERE ml.message_id = ?1 AND f.is_label = 1",
                )?
                .query_map([id.0], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
                .into_iter()
                .filter(|f| !keep.contains(f))
                .collect();
            for folder in stale {
                self.delete_location(id, FolderId(folder))?;
            }
        }
        self.after_location_change(id, thread)?;
        Ok(UpsertOutcome {
            message: id,
            thread,
            created,
        })
    }

    fn check_location_folder(&self, folder: FolderId, account: AccountId) -> Result<()> {
        let owner: Option<i64> = self
            .conn
            .prepare_cached("SELECT account_id FROM folder WHERE id = ?1")?
            .query_row([folder.0], |row| row.get(0))
            .optional()?;
        match owner {
            None => Err(Error::not_found("folder", folder)),
            Some(owner) if owner != account.0 => {
                Err(Error::Invalid("location folder belongs to another account"))
            }
            Some(_) => Ok(()),
        }
    }

    fn find_existing(&self, message: &NewMessage) -> Result<Option<(MessageId, ThreadId)>> {
        let by_id = |conn: &Connection, id: i64| -> Result<Option<(MessageId, ThreadId)>> {
            Ok(conn
                .prepare_cached("SELECT id, thread_id FROM message WHERE id = ?1")?
                .query_row([id], |row| {
                    Ok((MessageId(row.get(0)?), ThreadId(row.get(1)?)))
                })
                .optional()?)
        };
        if let Some(gm_msgid) = message.gm_msgid {
            let found: Option<i64> = self
                .conn
                .prepare_cached("SELECT id FROM message WHERE account_id = ?1 AND gm_msgid = ?2")?
                .query_row(params![message.account.0, gm_msgid as i64], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(id) = found {
                return by_id(self.conn, id);
            }
        }
        let mut found = None;
        for location in &message.locations {
            let Some(uid) = location.uid else { continue };
            let Some(owner) = message_by_uid(self.conn, location.folder, uid)? else {
                continue;
            };
            if found.is_none() {
                found = Some(owner);
            }
        }
        if let Some(id) = found {
            return by_id(self.conn, id.0);
        }
        if let Some(rfc_id) = message.rfc_message_id.as_deref() {
            let detached: Option<i64> = self
                .conn
                .prepare_cached(
                    "SELECT id FROM message WHERE rfc_message_id = ?1 AND account_id = ?2
                       AND detached_at IS NOT NULL ORDER BY detached_at DESC, id DESC LIMIT 1",
                )?
                .query_row(params![rfc_id, message.account.0], |row| row.get(0))
                .optional()?;
            if let Some(id) = detached {
                return by_id(self.conn, id);
            }
        }
        Ok(None)
    }

    fn insert_message(&mut self, m: &NewMessage, thread: ThreadId) -> Result<MessageId> {
        let references: Vec<&String> = m.references.iter().take(MAX_REFERENCES).collect();
        self.conn
            .prepare_cached(
                "INSERT INTO message (account_id, rfc_message_id, thread_id, in_reply_to,
                   references_json, date, received, from_addr, from_name, to_json, cc_json,
                   delivered_to, subject, snippet, size, has_attachments, list_id,
                   bodystructure_json, gm_msgid, gm_thrid, auth_json, identity_hint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21, ?22)",
            )?
            .execute(params![
                m.account.0,
                m.rfc_message_id,
                thread.0,
                m.in_reply_to,
                if references.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&references)?)
                },
                m.date,
                m.received,
                m.from.as_ref().map(|a| a.email.as_str()),
                m.from.as_ref().and_then(|a| a.name.as_deref()),
                addresses_to_sql(&m.to)?,
                addresses_to_sql(&m.cc)?,
                m.delivered_to,
                m.subject,
                m.snippet,
                m.size.map(|s| s as i64),
                m.has_attachments,
                m.list_id,
                m.bodystructure_json,
                m.gm_msgid.map(|v| v as i64),
                m.gm_thrid.map(|v| v as i64),
                m.auth_json,
                m.identity_hint.map(|i| i.0),
            ])?;
        let id = MessageId(self.conn.last_insert_rowid());
        let mut insert_ref = self.conn.prepare_cached(
            "INSERT INTO message_ref (rfc_id, message_id) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
        )?;
        for reference in m.in_reply_to.iter().chain(references.iter().copied()) {
            if !reference.is_empty() {
                insert_ref.execute(params![reference, id.0])?;
            }
        }
        Ok(id)
    }

    /// Inserts or updates one location (flags and keywords are replaced; uid and modseq
    /// only when given).
    fn put_location(&mut self, message: MessageId, location: &NewLocation) -> Result<()> {
        let keywords = normalize_keywords(&location.keywords);
        self.conn
            .prepare_cached(
                "INSERT INTO message_location (message_id, folder_id, uid, modseq, flags, keywords_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (folder_id, message_id) DO UPDATE SET
                   uid = COALESCE(excluded.uid, uid),
                   modseq = COALESCE(excluded.modseq, modseq),
                   flags = excluded.flags,
                   keywords_json = excluded.keywords_json",
            )?
            .execute(params![
                message.0,
                location.folder.0,
                location.uid,
                location.modseq.map(|v| v as i64),
                i64::from(location.flags.bits()),
                keywords_to_sql(&keywords)?,
            ])?;
        Ok(())
    }

    fn delete_location(&mut self, message: MessageId, folder: FolderId) -> Result<bool> {
        let deleted = self
            .conn
            .prepare_cached(
                "DELETE FROM message_location WHERE message_id = ?1 AND folder_id = ?2",
            )?
            .execute(params![message.0, folder.0])?;
        Ok(deleted > 0)
    }

    fn thread_of(&self, message: MessageId) -> Result<ThreadId> {
        self.conn
            .prepare_cached("SELECT thread_id FROM message WHERE id = ?1")?
            .query_row([message.0], |row| row.get(0).map(ThreadId))
            .optional()?
            .ok_or_else(|| Error::not_found("message", message))
    }

    /// Bookkeeping after a message's locations changed: detached flag, tags, thread.
    fn after_location_change(&mut self, message: MessageId, thread: ThreadId) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE message SET detached_at = CASE
                   WHEN EXISTS (SELECT 1 FROM message_location WHERE message_id = ?1) THEN NULL
                   ELSE COALESCE(detached_at, ?2) END
                 WHERE id = ?1",
            )?
            .execute(params![message.0, self.now])?;
        self.sync_message_tags(message)?;
        self.touch_thread(thread);
        self.changes.messages.insert(message);
        Ok(())
    }

    /// Rebuilds `message_tag` for a message from the keywords of all its locations.
    pub(crate) fn sync_message_tags(&mut self, message: MessageId) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM message_tag WHERE message_id = ?1")?
            .execute([message.0])?;
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_tag (message_id, tag_id)
                 SELECT DISTINCT ?1, t.id
                 FROM message_location ml, json_each(ml.keywords_json) k
                 JOIN tag t ON t.keyword = k.value
                 WHERE ml.message_id = ?1 AND ml.keywords_json IS NOT NULL",
            )?
            .execute([message.0])?;
        Ok(())
    }

    // ---- locations -----------------------------------------------------------------

    /// Adds (or updates) one location of an existing message, for example a copy, or a
    /// move applied locally before the server confirmed it (`uid: None`).
    pub fn add_location(&mut self, message: MessageId, location: &NewLocation) -> Result<()> {
        let account: i64 = self
            .conn
            .prepare_cached("SELECT account_id FROM message WHERE id = ?1")?
            .query_row([message.0], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::not_found("message", message))?;
        self.check_location_folder(location.folder, AccountId(account))?;
        if let Some(uid) = location.uid
            && let Some(owner) = message_by_uid(self.conn, location.folder, uid)?
            && owner != message
        {
            self.delete_location(owner, location.folder)?;
            let thread = self.thread_of(owner)?;
            self.after_location_change(owner, thread)?;
        }
        self.put_location(message, location)?;
        let thread = self.thread_of(message)?;
        self.after_location_change(message, thread)
    }

    /// Removes a message's location in `folder`. A message left without locations is
    /// detached (kept with its body until [`WriteTx::purge_detached`]).
    pub fn remove_location(&mut self, message: MessageId, folder: FolderId) -> Result<bool> {
        let thread = self.thread_of(message)?;
        let removed = self.delete_location(message, folder)?;
        if removed {
            self.after_location_change(message, thread)?;
        }
        Ok(removed)
    }

    /// Moves a location from one folder to another, keeping flags and keywords (the
    /// optimistic local apply of a MOVE; `new_uid` from COPYUID if known).
    pub fn move_location(
        &mut self,
        message: MessageId,
        from: FolderId,
        to: FolderId,
        new_uid: Option<u32>,
    ) -> Result<()> {
        let current = locations(self.conn, message)?
            .into_iter()
            .find(|l| l.folder == from)
            .ok_or_else(|| Error::not_found("location", format!("{message} in {from}")))?;
        self.delete_location(message, from)?;
        self.add_location(
            message,
            &NewLocation {
                folder: to,
                uid: new_uid,
                modseq: None,
                flags: current.flags,
                keywords: current.keywords,
            },
        )
    }

    /// Sets the UID of a location once the server reported it (COPYUID, APPENDUID).
    pub fn set_location_uid(
        &mut self,
        message: MessageId,
        folder: FolderId,
        uid: u32,
    ) -> Result<()> {
        if let Some(owner) = message_by_uid(self.conn, folder, uid)? {
            if owner == message {
                return Ok(());
            }
            self.delete_location(owner, folder)?;
            let thread = self.thread_of(owner)?;
            self.after_location_change(owner, thread)?;
        }
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE message_location SET uid = ?3 WHERE message_id = ?1 AND folder_id = ?2",
            )?
            .execute(params![message.0, folder.0, uid])?;
        if changed == 0 {
            return Err(Error::not_found(
                "location",
                format!("{message} in {folder}"),
            ));
        }
        self.changes.messages.insert(message);
        Ok(())
    }

    /// The server expunged these UIDs from `folder`. Returns the affected messages;
    /// those left without a location are detached.
    pub fn expunge(&mut self, folder: FolderId, uids: &[u32]) -> Result<Vec<MessageId>> {
        let mut affected = Vec::new();
        for &uid in uids {
            let Some(message) = message_by_uid(self.conn, folder, uid)? else {
                continue;
            };
            self.remove_location(message, folder)?;
            affected.push(message);
        }
        Ok(affected)
    }

    // ---- flags and keywords --------------------------------------------------------

    /// Flags (and optionally keywords) of one location from the server, by UID.
    /// Returns the message, or `None` if the UID is unknown.
    pub fn update_location_flags(
        &mut self,
        folder: FolderId,
        uid: u32,
        flags: Flags,
        keywords: Option<&[String]>,
        modseq: Option<u64>,
    ) -> Result<Option<MessageId>> {
        let Some(message) = message_by_uid(self.conn, folder, uid)? else {
            return Ok(None);
        };
        self.conn
            .prepare_cached(
                "UPDATE message_location SET flags = ?3, modseq = COALESCE(?4, modseq)
                 WHERE folder_id = ?1 AND uid = ?2",
            )?
            .execute(params![
                folder.0,
                uid,
                i64::from(flags.bits()),
                modseq.map(|v| v as i64)
            ])?;
        if let Some(keywords) = keywords {
            let keywords = normalize_keywords(keywords);
            self.conn
                .prepare_cached(
                    "UPDATE message_location SET keywords_json = ?3 WHERE folder_id = ?1 AND uid = ?2",
                )?
                .execute(params![folder.0, uid, keywords_to_sql(&keywords)?])?;
            self.sync_message_tags(message)?;
        }
        let thread = self.thread_of(message)?;
        self.touch_thread(thread);
        self.changes.messages.insert(message);
        Ok(Some(message))
    }

    /// Adds and removes flags on every location of these messages (marking read writes
    /// all locations, PLAN §4.3).
    pub fn set_flags(&mut self, messages: &[MessageId], add: Flags, remove: Flags) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "UPDATE message_location SET flags = (flags | ?2) & ~?3 WHERE message_id = ?1",
        )?;
        let mut touched = Vec::with_capacity(messages.len());
        for message in messages {
            stmt.execute(params![
                message.0,
                i64::from(add.bits()),
                i64::from(remove.bits())
            ])?;
            touched.push(*message);
        }
        drop(stmt);
        for message in touched {
            let thread = self.thread_of(message)?;
            self.touch_thread(thread);
            self.changes.messages.insert(message);
        }
        Ok(())
    }

    /// Sets the flags of one location.
    pub fn set_location_flags(
        &mut self,
        message: MessageId,
        folder: FolderId,
        flags: Flags,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE message_location SET flags = ?3 WHERE message_id = ?1 AND folder_id = ?2",
            )?
            .execute(params![message.0, folder.0, i64::from(flags.bits())])?;
        if changed == 0 {
            return Err(Error::not_found(
                "location",
                format!("{message} in {folder}"),
            ));
        }
        let thread = self.thread_of(message)?;
        self.touch_thread(thread);
        self.changes.messages.insert(message);
        Ok(())
    }

    /// Adds and removes IMAP keywords on every location of these messages, and updates
    /// their tags.
    pub fn set_keywords(
        &mut self,
        messages: &[MessageId],
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        let add = normalize_keywords(add);
        let remove = normalize_keywords(remove);
        for &message in messages {
            let rows: Vec<(i64, Option<String>)> = self
                .conn
                .prepare_cached(
                    "SELECT folder_id, keywords_json FROM message_location WHERE message_id = ?1",
                )?
                .query_map([message.0], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (folder, json) in rows {
                let current = keywords_from_sql(json.as_deref())?;
                let updated: Vec<String> = normalize_keywords(
                    current
                        .iter()
                        .chain(add.iter())
                        .filter(|k| !remove.contains(k)),
                );
                if updated != current {
                    self.conn
                        .prepare_cached(
                            "UPDATE message_location SET keywords_json = ?3
                             WHERE message_id = ?1 AND folder_id = ?2",
                        )?
                        .execute(params![message.0, folder, keywords_to_sql(&updated)?])?;
                }
            }
            self.sync_message_tags(message)?;
            let thread = self.thread_of(message)?;
            self.touch_thread(thread);
            self.changes.messages.insert(message);
        }
        Ok(())
    }

    /// Adds and removes tags (as their keywords) on these messages.
    pub fn set_tags(
        &mut self,
        messages: &[MessageId],
        add: &[TagId],
        remove: &[TagId],
    ) -> Result<()> {
        let keyword_of = |conn: &Connection, tag: TagId| -> Result<String> {
            conn.prepare_cached("SELECT keyword FROM tag WHERE id = ?1")?
                .query_row([tag.0], |row| row.get(0))
                .optional()?
                .ok_or_else(|| Error::not_found("tag", tag))
        };
        let add = add
            .iter()
            .map(|t| keyword_of(self.conn, *t))
            .collect::<Result<Vec<_>>>()?;
        let remove = remove
            .iter()
            .map(|t| keyword_of(self.conn, *t))
            .collect::<Result<Vec<_>>>()?;
        self.set_keywords(messages, &add, &remove)
    }

    // ---- bodies --------------------------------------------------------------------

    /// Points a message at its stored body. The blob row is created if needed and its
    /// reference count bumped; the previous body (if any) loses a reference.
    pub fn set_message_body(
        &mut self,
        message: MessageId,
        blob: &StagedBlob,
        state: BodyState,
    ) -> Result<()> {
        self.register_blob(blob)?;
        let changed = self
            .conn
            .prepare_cached("UPDATE message SET blob_id = ?2, body_state = ?3 WHERE id = ?1")?
            .execute(params![message.0, &blob.id().0, state as i64])?;
        if changed == 0 {
            return Err(Error::not_found("message", message));
        }
        self.changes.messages.insert(message);
        Ok(())
    }

    /// Records a staged blob in this transaction (inserting its `blob` row) and keeps
    /// it pinned until the commit. Methods that take a [`StagedBlob`] call this.
    pub fn register_blob(&mut self, blob: &StagedBlob) -> Result<()> {
        blob::ensure_row(self.conn, blob, self.now)?;
        self.pins.push(blob.pin());
        Ok(())
    }

    /// Forgets a message's body (its blob loses a reference).
    pub fn clear_message_body(&mut self, message: MessageId) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE message SET blob_id = NULL, body_state = 0 WHERE id = ?1")?
            .execute([message.0])?;
        self.changes.messages.insert(message);
        Ok(())
    }

    /// A blob file turned out to be missing (for example after a rollback): every
    /// message using it goes back to "no body", so the body is fetched again.
    pub fn blob_missing(&mut self, blob: BlobId) -> Result<usize> {
        let messages: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM message WHERE blob_id = ?1")?
            .query_map([&blob.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.conn
            .prepare_cached("UPDATE message SET blob_id = NULL, body_state = 0 WHERE blob_id = ?1")?
            .execute([&blob.0])?;
        self.changes
            .messages
            .extend(messages.iter().map(|m| MessageId(*m)));
        Ok(messages.len())
    }

    // ---- deleting --------------------------------------------------------------------

    /// Deletes a message for good (locations, tags and the body reference with it).
    pub fn delete_message(&mut self, message: MessageId) -> Result<()> {
        let thread = self.thread_of(message)?;
        self.conn
            .prepare_cached("DELETE FROM message WHERE id = ?1")?
            .execute([message.0])?;
        self.touch_thread(thread);
        self.changes.messages_removed.insert(message);
        Ok(())
    }

    /// Deletes messages of `account` that have been detached since before
    /// `detached_before` (run after a full sync pass). Returns how many were deleted.
    pub fn purge_detached(&mut self, account: AccountId, detached_before: i64) -> Result<usize> {
        let doomed: Vec<(i64, i64)> = self
            .conn
            .prepare_cached(
                "SELECT id, thread_id FROM message
                 WHERE account_id = ?1 AND detached_at IS NOT NULL AND detached_at < ?2",
            )?
            .query_map(params![account.0, detached_before], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut delete = self
            .conn
            .prepare_cached("DELETE FROM message WHERE id = ?1")?;
        for (message, _) in &doomed {
            delete.execute([message])?;
        }
        drop(delete);
        for (message, thread) in &doomed {
            self.touch_thread(ThreadId(*thread));
            self.changes.messages_removed.insert(MessageId(*message));
        }
        Ok(doomed.len())
    }
}
