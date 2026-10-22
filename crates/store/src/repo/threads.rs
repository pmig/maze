//! Thread assignment, merging, and the `thread` / `folder_thread` aggregates.
//!
//! Pure threading logic (JWZ) lives in `mazemail_mime`; the store offers the lookups it
//! needs ([`Queries::threads_for_rfc_ids`](crate::Queries::threads_for_rfc_ids)) and a
//! simple built-in resolver ([`ThreadAssignment::Resolve`]).
//!
//! # Aggregates
//!
//! Every write that touches a message's thread, date, flags or locations marks the
//! thread dirty. At the end of each write job the writer recomputes each dirty thread
//! from its messages (one indexed query per thread) and diffs the per-folder result
//! against the stored `folder_thread` rows, applying the differences to the rows and to
//! the folders' `unread`/`total` counts. Cost is proportional to the size of the
//! touched threads, never to the size of a folder.
//!
//! Semantics:
//! - `folder_thread` counts the copies in that folder, with that copy's flags (IMAP
//!   flags are per mailbox): `unread_count` = locations without `\Seen`.
//! - `thread` counts messages, with effective flags: a message is unread unless every
//!   location is seen, flagged if any location is.
//! - Messages without any location (detached) don't count anywhere.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use mazemail_types::{AccountId, Flags, FolderId, MessageId, TagId, ThreadId};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{NewMessage, Participant, ThreadAssignment, ThreadRow};
use crate::{Error, Result, WriteTx};

/// How far back the subject fallback looks for a thread to join.
const SUBJECT_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;
/// At most this many references are used for threading.
pub(crate) const MAX_REFERENCES: usize = 64;
/// Participants kept per thread row.
const MAX_PARTICIPANTS: usize = 8;

// Lookups by Message-ID. `+m.account_id` keeps SQLite from using `message_account`
// for the account filter: without `ANALYZE` statistics it rates that index as good as
// `message_rfcid` and then scans every message of the account for each id, which made
// ingest quadratic in the size of the account. The unary `+` makes the term
// unindexable, so the planner looks the id up in `message_rfcid` and filters the few
// matches by account. `query_plans_use_the_selective_index` pins the plans.

/// Threads of messages with one of the ids in `rarray(?1)`, in account `?2`.
const THREADS_BY_RFC_ID: &str = "SELECT DISTINCT m.thread_id FROM rarray(?1) x
     CROSS JOIN message m ON m.rfc_message_id = x.value WHERE +m.account_id = ?2";

/// Threads of messages that refer to one of the ids in `rarray(?1)`, in account `?2`.
const THREADS_REFERRING_TO: &str = "SELECT DISTINCT m.thread_id FROM rarray(?1) x
     CROSS JOIN message_ref r ON r.rfc_id = x.value
     CROSS JOIN message m ON m.id = r.message_id WHERE m.account_id = ?2";

const THREAD_BY_GM_THRID: &str =
    "SELECT thread_id FROM message WHERE account_id = ?1 AND gm_thrid = ?2 LIMIT 1";

const THREAD_BY_SUBJECT: &str = "SELECT id FROM thread WHERE account_id = ?1 AND subject_key = ?2
       AND last_date >= ?3 ORDER BY last_date DESC LIMIT 1";

pub(crate) fn text_array<'a>(values: impl IntoIterator<Item = &'a str>) -> Rc<Vec<Value>> {
    Rc::new(
        values
            .into_iter()
            .map(|v| Value::Text(v.to_owned()))
            .collect(),
    )
}

pub(crate) fn int_array(values: impl IntoIterator<Item = i64>) -> Rc<Vec<Value>> {
    Rc::new(values.into_iter().map(Value::Integer).collect())
}

/// Threads in `account` that contain a message with one of these Message-IDs, or a
/// message that refers to one of them.
pub(crate) fn threads_for_rfc_ids(
    conn: &Connection,
    account: AccountId,
    rfc_ids: &[&str],
) -> Result<BTreeSet<ThreadId>> {
    let mut threads = BTreeSet::new();
    if rfc_ids.is_empty() {
        return Ok(threads);
    }
    let ids = text_array(rfc_ids.iter().copied());
    let mut direct = conn.prepare_cached(THREADS_BY_RFC_ID)?;
    for thread in direct.query_map(params![ids, account.0], |row| row.get(0))? {
        threads.insert(ThreadId(thread?));
    }
    let mut referring = conn.prepare_cached(THREADS_REFERRING_TO)?;
    for thread in referring.query_map(params![ids, account.0], |row| row.get(0))? {
        threads.insert(ThreadId(thread?));
    }
    Ok(threads)
}

pub(crate) fn thread_for_gm_thrid(
    conn: &Connection,
    account: AccountId,
    gm_thrid: u64,
) -> Result<Option<ThreadId>> {
    Ok(conn
        .prepare_cached(THREAD_BY_GM_THRID)?
        .query_row(params![account.0, gm_thrid as i64], |row| row.get(0))
        .optional()?
        .map(ThreadId))
}

pub(crate) fn thread_message_ids(conn: &Connection, thread: ThreadId) -> Result<Vec<MessageId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM message WHERE thread_id = ?1 AND detached_at IS NULL ORDER BY date, id",
    )?;
    Ok(stmt
        .query_map([thread.0], |row| row.get(0).map(MessageId))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Row data for `ids`, in the order given; unknown ids are skipped. With `context`, the
/// date and counts are those of the thread in that folder.
pub(crate) fn thread_rows(
    conn: &Connection,
    ids: &[ThreadId],
    context: Option<FolderId>,
) -> Result<Vec<ThreadRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let array = int_array(ids.iter().map(|t| t.0));
    let mut rows: HashMap<ThreadId, ThreadRow> = HashMap::with_capacity(ids.len());
    {
        let mut stmt = conn.prepare_cached(
            "SELECT t.id, t.account_id, t.subject, t.participants_json, t.last_date,
                    t.message_count, t.unread_count, t.flagged, t.attachment_count,
                    t.last_message_id, m.snippet,
                    ft.last_date, ft.message_count, ft.unread_count, ft.flagged,
                    ft.attachment_count
             FROM rarray(?1) x
             CROSS JOIN thread t ON t.id = x.value
             LEFT JOIN message m ON m.id = t.last_message_id
             LEFT JOIN folder_thread ft ON ft.folder_id = ?2 AND ft.thread_id = t.id",
        )?;
        let mut query = stmt.query(params![array, context.map(|f| f.0)])?;
        while let Some(row) = query.next()? {
            let id = ThreadId(row.get(0)?);
            let participants: Option<String> = row.get(3)?;
            let participants = participants
                .as_deref()
                .map(serde_json::from_str::<Vec<Participant>>)
                .transpose()?
                .unwrap_or_default();
            let in_folder = row.get::<_, Option<i64>>(11)?.is_some();
            let (date, count, unread, flagged, attachments) = if in_folder {
                (
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                )
            } else {
                (
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                )
            };
            let count: i64 = count;
            let unread: i64 = unread;
            let flagged: i64 = flagged;
            let attachments: i64 = attachments;
            rows.insert(
                id,
                ThreadRow {
                    id,
                    account: AccountId(row.get(1)?),
                    subject: row.get(2)?,
                    participants,
                    date,
                    message_count: count.max(0) as u32,
                    unread_count: unread.max(0) as u32,
                    flagged: flagged > 0,
                    has_attachments: attachments > 0,
                    tags: Vec::new(),
                    snippet: row.get(10)?,
                    last_message: row.get::<_, Option<i64>>(9)?.map(MessageId),
                },
            );
        }
    }
    {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT m.thread_id, mt.tag_id FROM rarray(?1) x
             CROSS JOIN message m ON m.thread_id = x.value
             CROSS JOIN message_tag mt ON mt.message_id = m.id
             ORDER BY m.thread_id, mt.tag_id",
        )?;
        let mut query = stmt.query([array])?;
        while let Some(row) = query.next()? {
            if let Some(entry) = rows.get_mut(&ThreadId(row.get(0)?)) {
                entry.tags.push(TagId(row.get(1)?));
            }
        }
    }
    Ok(ids.iter().filter_map(|id| rows.remove(id)).collect())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FolderAgg {
    last_date: i64,
    messages: i64,
    unread: i64,
    flagged: i64,
    attachments: i64,
}

struct MessageAgg {
    id: i64,
    date: i64,
    has_attachments: bool,
    from_name: Option<String>,
    from_addr: Option<String>,
    subject: Option<String>,
    any_unseen: bool,
    flagged: bool,
}

impl WriteTx<'_> {
    /// Marks a thread for aggregate recomputation at the end of this job.
    pub(crate) fn touch_thread(&mut self, thread: ThreadId) {
        self.dirty_threads.insert(thread);
    }

    /// Picks the thread for a new message according to `message.thread`.
    pub(crate) fn resolve_thread(&mut self, message: &NewMessage) -> Result<ThreadId> {
        match &message.thread {
            ThreadAssignment::Existing(thread) => {
                let account: Option<i64> = self
                    .conn
                    .prepare_cached("SELECT account_id FROM thread WHERE id = ?1")?
                    .query_row([thread.0], |row| row.get(0))
                    .optional()?;
                match account {
                    None => Err(Error::not_found("thread", thread)),
                    Some(a) if a != message.account.0 => {
                        Err(Error::Invalid("thread belongs to another account"))
                    }
                    Some(_) => Ok(*thread),
                }
            }
            ThreadAssignment::New => self.create_thread(message),
            ThreadAssignment::Resolve { by_subject } => {
                if let Some(gm_thrid) = message.gm_thrid {
                    // Gmail decides; never merge across X-GM-THRID.
                    return match thread_for_gm_thrid(self.conn, message.account, gm_thrid)? {
                        Some(thread) => Ok(thread),
                        None => self.create_thread(message),
                    };
                }
                let mut keys: Vec<&str> = Vec::new();
                // Own id first (copies and late parents), then the direct parent, then
                // the rest of the chain.
                for key in message
                    .rfc_message_id
                    .iter()
                    .chain(message.in_reply_to.iter())
                    .chain(message.references.iter().rev())
                {
                    if !key.is_empty() && !keys.contains(&key.as_str()) {
                        keys.push(key);
                    }
                    if keys.len() >= MAX_REFERENCES {
                        break;
                    }
                }
                let candidates = threads_for_rfc_ids(self.conn, message.account, &keys)?;
                let mut candidates = candidates.into_iter();
                if let Some(target) = candidates.next() {
                    for other in candidates {
                        self.merge_threads(target, other)?;
                    }
                    return Ok(target);
                }
                if *by_subject
                    && let Some(key) = message.subject_key.as_deref().filter(|k| !k.is_empty())
                    && let Some(thread) = self
                        .conn
                        .prepare_cached(THREAD_BY_SUBJECT)?
                        .query_row(
                            params![message.account.0, key, message.date - SUBJECT_WINDOW_SECS],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()?
                {
                    return Ok(ThreadId(thread));
                }
                self.create_thread(message)
            }
        }
    }

    fn create_thread(&mut self, message: &NewMessage) -> Result<ThreadId> {
        self.conn
            .prepare_cached(
                "INSERT INTO thread (account_id, subject, subject_key, last_date)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                message.account.0,
                message.subject,
                message.subject_key,
                message.date
            ])?;
        let thread = ThreadId(self.conn.last_insert_rowid());
        self.touch_thread(thread);
        Ok(thread)
    }

    /// Moves every message of `from` into `into` and removes `from`. Both threads must
    /// belong to the same account.
    pub fn merge_threads(&mut self, into: ThreadId, from: ThreadId) -> Result<()> {
        if into == from {
            return Ok(());
        }
        let accounts: Vec<i64> = self
            .conn
            .prepare_cached("SELECT account_id FROM thread WHERE id IN (?1, ?2)")?
            .query_map(params![into.0, from.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        match accounts.as_slice() {
            [a, b] if a == b => {}
            [_, _] => return Err(Error::Invalid("threads belong to different accounts")),
            _ => {
                return Err(Error::not_found(
                    "thread",
                    if accounts.is_empty() { into } else { from },
                ));
            }
        }
        let moved: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM message WHERE thread_id = ?1")?
            .query_map([from.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.conn
            .prepare_cached("UPDATE message SET thread_id = ?1 WHERE thread_id = ?2")?
            .execute(params![into.0, from.0])?;
        self.conn
            .prepare_cached(
                "UPDATE agent_thread SET mail_thread_id = ?1 WHERE mail_thread_id = ?2",
            )?
            .execute(params![into.0, from.0])?;
        self.changes
            .messages
            .extend(moved.into_iter().map(MessageId));
        self.touch_thread(into);
        self.touch_thread(from);
        Ok(())
    }

    /// Moves one message to another thread of the same account (for example after the
    /// JWZ threader regrouped a conversation).
    pub fn set_message_thread(&mut self, message: MessageId, thread: ThreadId) -> Result<()> {
        let (old, account): (i64, i64) = self
            .conn
            .prepare_cached("SELECT thread_id, account_id FROM message WHERE id = ?1")?
            .query_row([message.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
            .ok_or_else(|| Error::not_found("message", message))?;
        let target_account: i64 = self
            .conn
            .prepare_cached("SELECT account_id FROM thread WHERE id = ?1")?
            .query_row([thread.0], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::not_found("thread", thread))?;
        if target_account != account {
            return Err(Error::Invalid("thread belongs to another account"));
        }
        self.conn
            .prepare_cached("UPDATE message SET thread_id = ?2 WHERE id = ?1")?
            .execute(params![message.0, thread.0])?;
        self.touch_thread(ThreadId(old));
        self.touch_thread(thread);
        self.changes.messages.insert(message);
        Ok(())
    }

    /// Starts a new, empty thread for `message` (splitting it out of its thread).
    pub fn split_message_into_new_thread(&mut self, message: MessageId) -> Result<ThreadId> {
        let (account, subject): (i64, Option<String>) = self
            .conn
            .prepare_cached("SELECT account_id, subject FROM message WHERE id = ?1")?
            .query_row([message.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
            .ok_or_else(|| Error::not_found("message", message))?;
        self.conn
            .prepare_cached("INSERT INTO thread (account_id, subject) VALUES (?1, ?2)")?
            .execute(params![account, subject])?;
        let thread = ThreadId(self.conn.last_insert_rowid());
        self.set_message_thread(message, thread)?;
        Ok(thread)
    }

    /// Recomputes every dirty thread (see the module docs).
    pub(crate) fn flush_threads(&mut self) -> Result<()> {
        while let Some(thread) = self.dirty_threads.pop_first() {
            self.recompute_thread(thread)?;
        }
        Ok(())
    }

    fn recompute_thread(&mut self, thread: ThreadId) -> Result<()> {
        let mut messages: Vec<MessageAgg> = Vec::new();
        let mut folders: BTreeMap<i64, FolderAgg> = BTreeMap::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT m.id, m.date, m.has_attachments, m.from_name, m.from_addr, m.subject,
                        ml.folder_id, ml.flags
                 FROM message m CROSS JOIN message_location ml ON ml.message_id = m.id
                 WHERE m.thread_id = ?1
                 ORDER BY m.date, m.id",
            )?;
            let mut rows = stmt.query([thread.0])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let date: i64 = row.get(1)?;
                let has_attachments: bool = row.get(2)?;
                let folder: i64 = row.get(6)?;
                let flags = Flags::from_bits_retain(row.get::<_, i64>(7)? as u32);
                let unseen = !flags.contains(Flags::SEEN);
                let flagged = flags.contains(Flags::FLAGGED);

                let agg = folders.entry(folder).or_default();
                agg.last_date = agg.last_date.max(date);
                agg.messages += 1;
                agg.unread += i64::from(unseen);
                agg.flagged += i64::from(flagged);
                agg.attachments += i64::from(has_attachments);

                match messages.last_mut() {
                    Some(last) if last.id == id => {
                        last.any_unseen |= unseen;
                        last.flagged |= flagged;
                    }
                    _ => messages.push(MessageAgg {
                        id,
                        date,
                        has_attachments,
                        from_name: row.get(3)?,
                        from_addr: row.get(4)?,
                        subject: row.get(5)?,
                        any_unseen: unseen,
                        flagged,
                    }),
                }
            }
        }

        // Diff the per-folder rows.
        let old: BTreeMap<i64, FolderAgg> = self
            .conn
            .prepare_cached(
                "SELECT folder_id, last_date, message_count, unread_count, flagged,
                        attachment_count
                 FROM folder_thread WHERE thread_id = ?1",
            )?
            .query_map([thread.0], |row| {
                Ok((
                    row.get(0)?,
                    FolderAgg {
                        last_date: row.get(1)?,
                        messages: row.get(2)?,
                        unread: row.get(3)?,
                        flagged: row.get(4)?,
                        attachments: row.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let folder_ids: BTreeSet<i64> = old.keys().chain(folders.keys()).copied().collect();
        for folder in folder_ids {
            let before = old.get(&folder).copied();
            let after = folders.get(&folder).copied();
            if before == after {
                continue;
            }
            match after {
                Some(agg) => {
                    self.conn
                        .prepare_cached(
                            "INSERT INTO folder_thread (folder_id, thread_id, last_date,
                               message_count, unread_count, flagged, attachment_count)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                             ON CONFLICT (folder_id, thread_id) DO UPDATE SET
                               last_date = excluded.last_date,
                               message_count = excluded.message_count,
                               unread_count = excluded.unread_count,
                               flagged = excluded.flagged,
                               attachment_count = excluded.attachment_count",
                        )?
                        .execute(params![
                            folder,
                            thread.0,
                            agg.last_date,
                            agg.messages,
                            agg.unread,
                            agg.flagged,
                            agg.attachments
                        ])?;
                }
                None => {
                    self.conn
                        .prepare_cached(
                            "DELETE FROM folder_thread WHERE folder_id = ?1 AND thread_id = ?2",
                        )?
                        .execute(params![folder, thread.0])?;
                }
            }
            let before = before.unwrap_or_default();
            let after = after.unwrap_or_default();
            let total_delta = after.messages - before.messages;
            let unread_delta = after.unread - before.unread;
            if total_delta != 0 || unread_delta != 0 {
                self.conn
                    .prepare_cached(
                        "UPDATE folder SET total = total + ?2, unread = unread + ?3 WHERE id = ?1",
                    )?
                    .execute(params![folder, total_delta, unread_delta])?;
                self.changes.folders.insert(FolderId(folder));
            }
            self.changes.folder_thread(FolderId(folder), thread);
        }

        // The thread row.
        if messages.is_empty() {
            let any_left: bool = self
                .conn
                .prepare_cached("SELECT EXISTS (SELECT 1 FROM message WHERE thread_id = ?1)")?
                .query_row([thread.0], |row| row.get(0))?;
            if any_left {
                self.conn
                    .prepare_cached(
                        "UPDATE thread SET message_count = 0, unread_count = 0, flagged = 0,
                           attachment_count = 0 WHERE id = ?1",
                    )?
                    .execute([thread.0])?;
                self.changes.threads.insert(thread);
            } else {
                let deleted = self
                    .conn
                    .prepare_cached("DELETE FROM thread WHERE id = ?1")?
                    .execute([thread.0])?;
                if deleted > 0 {
                    self.changes.threads_removed.insert(thread);
                    self.changes.threads.remove(&thread);
                }
            }
            return Ok(());
        }

        let mut participants: Vec<Participant> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for message in &messages {
            if participants.len() >= MAX_PARTICIPANTS {
                break;
            }
            if let Some(email) = &message.from_addr
                && seen.insert(email.to_ascii_lowercase())
            {
                participants.push(Participant {
                    name: message.from_name.clone(),
                    email: email.clone(),
                });
            }
        }
        let last = messages
            .iter()
            .max_by_key(|m| (m.date, m.id))
            .map(|m| (m.date, m.id))
            .unwrap_or_default();
        let subject = messages.iter().find_map(|m| m.subject.clone());
        self.conn
            .prepare_cached(
                "UPDATE thread SET subject = ?2, last_date = ?3, last_message_id = ?4,
                   message_count = ?5, unread_count = ?6, flagged = ?7, attachment_count = ?8,
                   participants_json = ?9
                 WHERE id = ?1",
            )?
            .execute(params![
                thread.0,
                subject,
                last.0,
                last.1,
                messages.len() as i64,
                messages.iter().filter(|m| m.any_unseen).count() as i64,
                messages.iter().filter(|m| m.flagged).count() as i64,
                messages.iter().filter(|m| m.has_attachments).count() as i64,
                serde_json::to_string(&participants)?,
            ])?;
        self.changes.threads.insert(thread);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{self, Access};
    use crate::repo::messages::{DETACHED_BY_RFC_ID, MESSAGES_BY_RFC_ID};

    /// The plan's detail lines, with every parameter bound to NULL.
    fn plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        for i in 1..=stmt.parameter_count() {
            stmt.raw_bind_parameter(i, rusqlite::types::Null).unwrap();
        }
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(row.get::<_, String>(3).unwrap());
        }
        out.join(" | ")
    }

    /// Threading looks messages up by Message-ID once or more per ingested message; a
    /// lookup that scans the account makes ingest quadratic.
    #[test]
    fn query_plans_use_the_selective_index() {
        let tmp = tempfile::tempdir().unwrap();
        let key = vault::MasterKey::from_bytes(vault::KeyId::FIRST, [1; vault::KEY_LEN])
            .sqlcipher_pragma_key();
        let mut c = conn::open(&tmp.path().join("db"), &key, Access::ReadWrite, 1024).unwrap();
        crate::migrate::migrate_main(&mut c, &key, tmp.path(), "test").unwrap();
        for (sql, index) in [
            (THREADS_BY_RFC_ID, "message_rfcid"),
            (THREADS_REFERRING_TO, "PRIMARY KEY (rfc_id=?)"),
            (MESSAGES_BY_RFC_ID, "message_rfcid"),
            (DETACHED_BY_RFC_ID, "message_rfcid"),
            (THREAD_BY_GM_THRID, "message_gm_thread"),
            (THREAD_BY_SUBJECT, "thread_subject"),
        ] {
            let plan = plan(&c, sql);
            assert!(plan.contains(index), "{sql}\n=> {plan}");
            assert!(!plan.contains("message_account"), "{sql}\n=> {plan}");
        }
    }
}
