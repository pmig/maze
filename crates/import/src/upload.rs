//! Uploading mbox files and Maildir folders into a folder of an IMAP account
//! (ADR-019; `mazemail import <file> --account … --folder …`).
//!
//! Maze keeps every message on a server, so an import doesn't store messages locally:
//! each message becomes a queued [`OpPayload::Append`] that the account's sync engine
//! replays (`APPEND` with the message's flags and tags), and the folder's next sync
//! brings it in like any other mail. The queued op holds the message's blob, so the
//! upload survives restarts and works offline.
//!
//! Re-running an import doesn't duplicate anything: a message is skipped if the target
//! folder already holds one with the same Message-ID (messages without one: the same
//! date, sender and subject), or if an upload of exactly the same bytes is still
//! queued for the folder.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ingest::{DEFAULT_BATCH, HeaderFingerprint};
use mazemail_mime::thread::normalize_message_id;
use mazemail_types::{AccountId, BlobId, Flags, FolderId};
use rayon::prelude::*;
use store::{AccountKind, MessageScope, Queries, StagedBlob, Store};
use sync::ops::OpPayload;

use crate::mbox::{Escaping, MboxReader};
use crate::progress::{Phase, Progress, Unit, report};
use crate::{Error, Result, maildir, reconstruct};

/// Read buffer for mbox files.
const READ_BUFFER: usize = 1 << 20;

/// Where an import uploads to: a folder of an IMAP, Gmail or Microsoft 365 account
/// that exists on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadTarget {
    pub account: AccountId,
    pub folder: FolderId,
}

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Messages read from the source.
    pub messages: u64,
    /// Messages queued for upload.
    pub queued: u64,
    /// Messages the folder already has, or that are queued already (skipped).
    pub duplicates: u64,
    /// Messages Thunderbird had deleted but not yet compacted away (skipped).
    pub deleted: u64,
    /// Messages that couldn't be queued (store errors).
    pub failed: u64,
    /// Source bytes read.
    pub bytes: u64,
    /// Bytes before the first `From ` line of mbox files (skipped).
    pub skipped_bytes: u64,
}

impl ImportStats {
    /// Adds another import's numbers.
    pub fn add(&mut self, other: &Self) {
        self.messages += other.messages;
        self.queued += other.queued;
        self.duplicates += other.duplicates;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.bytes += other.bytes;
        self.skipped_bytes += other.skipped_bytes;
    }
}

/// Uploads an mbox file (mboxrd unescaping, Thunderbird status headers turned into
/// flags and tags and removed) into `target`. `origin` is recorded with the queued
/// ops (`agent:<token id>`, `cli`, `user`).
pub fn upload_mbox(
    store: &Store,
    path: &Path,
    target: UploadTarget,
    origin: &str,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    check_target(store, target)?;
    let file = File::open(path).map_err(Error::io("opening", path))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = MboxReader::new(BufReader::with_capacity(READ_BUFFER, file))
        .with_escaping(Escaping::Mboxrd);
    let mut batch = Batch::new(store, target, origin)?;
    for record in reader.by_ref() {
        let record = record.map_err(Error::io("reading", path))?;
        batch.stats.bytes += record.len;
        let r = reconstruct(&record.message);
        if r.mozilla.is_expunged() {
            batch.stats.messages += 1;
            batch.stats.deleted += 1;
            continue;
        }
        batch.push(Candidate {
            flags: r.mozilla.flags(),
            keywords: r.mozilla.keywords(),
            date: record.envelope.date,
            raw: r.bytes,
        });
        if batch.pending.len() >= DEFAULT_BATCH {
            batch.flush();
            report(
                progress,
                Phase::Messages,
                batch.stats.bytes,
                total,
                Unit::Bytes,
            );
        }
    }
    batch.flush();
    batch.stats.skipped_bytes = reader.skipped_bytes();
    batch.stats.bytes += reader.skipped_bytes();
    report(progress, Phase::Messages, total, total, Unit::Bytes);
    Ok(batch.stats)
}

/// Uploads one Maildir folder (not its Maildir++ subfolders; see
/// [`maildir::maildirpp_subfolders`]) into `target`.
pub fn upload_maildir(
    store: &Store,
    dir: &Path,
    target: UploadTarget,
    origin: &str,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    if !maildir::is_maildir(dir) {
        return Err(Error::InvalidTarget(format!(
            "{} is not a Maildir folder (no cur/ directory)",
            dir.display()
        )));
    }
    check_target(store, target)?;
    let entries = maildir::list(dir)?;
    let total = entries.len() as u64;
    let mut batch = Batch::new(store, target, origin)?;
    for (i, entry) in entries.iter().enumerate() {
        let bytes = std::fs::read(&entry.path).map_err(Error::io("reading", &entry.path))?;
        batch.stats.bytes += bytes.len() as u64;
        let r = reconstruct(&bytes);
        if r.mozilla.is_expunged() {
            batch.stats.messages += 1;
            batch.stats.deleted += 1;
            continue;
        }
        let mut keywords = r.mozilla.keywords();
        keywords.extend(entry.keywords.iter().cloned());
        keywords.sort();
        keywords.dedup();
        batch.push(Candidate {
            flags: entry.flags | r.mozilla.flags(),
            keywords,
            date: entry.mtime,
            raw: r.bytes,
        });
        if batch.pending.len() >= DEFAULT_BATCH {
            batch.flush();
            report(
                progress,
                Phase::Messages,
                i as u64 + 1,
                total,
                Unit::Messages,
            );
        }
    }
    batch.flush();
    report(progress, Phase::Messages, total, total, Unit::Messages);
    Ok(batch.stats)
}

/// The target must be a server folder of a synced account.
fn check_target(store: &Store, target: UploadTarget) -> Result<()> {
    let reader = store.reader()?;
    let account = reader
        .account(target.account)?
        .ok_or_else(|| Error::InvalidTarget(format!("account {} doesn't exist", target.account)))?;
    if !matches!(
        account.kind,
        AccountKind::Imap | AccountKind::Gmail | AccountKind::M365
    ) {
        return Err(Error::InvalidTarget(format!(
            "account {} isn't an IMAP account; Maze imports into IMAP, Gmail and Microsoft 365 \
             accounts",
            target.account
        )));
    }
    let folder = reader
        .folder(target.folder)?
        .filter(|f| f.account == target.account)
        .ok_or_else(|| {
            Error::InvalidTarget(format!(
                "folder {} isn't a folder of account {}",
                target.folder, target.account
            ))
        })?;
    if folder.remote_name.is_none() {
        return Err(Error::InvalidTarget(format!(
            "folder {} isn't on the server",
            target.folder
        )));
    }
    Ok(())
}

/// A message read from a source, ready to queue.
struct Candidate {
    raw: Vec<u8>,
    flags: Flags,
    keywords: Vec<String>,
    /// The envelope or file date, for messages without a `Date` header.
    date: Option<i64>,
}

/// What identifies a message for the duplicate check: its Message-ID, or for messages
/// without one, date, sender and subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Id(String),
    Fingerprint(HeaderFingerprint),
}

fn key_of(raw: &[u8], fallback_date: Option<i64>) -> Option<Key> {
    let headers = mazemail_mime::parse_headers_only(raw).ok()?;
    if let Some(id) = headers.message_id.as_deref().and_then(normalize_message_id) {
        return Some(Key::Id(id.to_owned()));
    }
    let date = headers.date.map(|d| d.unix_timestamp()).or(fallback_date);
    Some(Key::Fingerprint(HeaderFingerprint::new(
        date,
        headers
            .from
            .first()
            .or(headers.sender.as_ref())
            .map(|a| a.email.as_str()),
        headers.subject.as_deref(),
    )))
}

/// What the target folder already has or will get.
#[derive(Default)]
struct Seen {
    keys: HashSet<Key>,
    /// Blobs of uploads still queued for the folder.
    queued: HashSet<BlobId>,
}

impl Seen {
    fn load(store: &Store, target: UploadTarget) -> Result<Self> {
        let reader = store.reader()?;
        let mut out = Self::default();
        for id in reader.message_ids_in_scope(&MessageScope::Folder(target.folder))? {
            let Some(m) = reader.message(id)? else {
                continue;
            };
            if let Some(rfc) = m.rfc_message_id.as_deref().and_then(normalize_message_id) {
                out.keys.insert(Key::Id(rfc.to_owned()));
            }
            // Also for messages with a Message-ID: some servers add one on APPEND.
            out.keys.insert(Key::Fingerprint(HeaderFingerprint::new(
                Some(m.date),
                m.from.as_ref().map(|a| a.email.as_str()),
                m.subject.as_deref(),
            )));
        }
        for op in reader.pending_ops(target.account)? {
            if let Ok(OpPayload::Append { folder, blob, .. }) = OpPayload::from_pending(&op)
                && folder == target.folder
            {
                out.queued.insert(blob);
            }
        }
        Ok(out)
    }
}

/// Collects messages and queues their uploads in batches.
struct Batch<'a> {
    store: &'a Store,
    target: UploadTarget,
    origin: &'a str,
    seen: Seen,
    pending: Vec<Candidate>,
    stats: ImportStats,
}

impl<'a> Batch<'a> {
    fn new(store: &'a Store, target: UploadTarget, origin: &'a str) -> Result<Self> {
        Ok(Self {
            store,
            target,
            origin,
            seen: Seen::load(store, target)?,
            pending: Vec::with_capacity(DEFAULT_BATCH),
            stats: ImportStats::default(),
        })
    }

    fn push(&mut self, candidate: Candidate) {
        self.stats.messages += 1;
        self.pending.push(candidate);
    }

    /// Drops duplicates, writes the rest's blobs in parallel and queues their uploads
    /// in one write job.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let blobs = self.store.blobs();
        let keyed: Vec<(Candidate, Option<Key>, BlobId)> = pending
            .into_par_iter()
            .map(|c| {
                let key = key_of(&c.raw, c.date);
                let blob = blobs.id_for(&c.raw);
                (c, key, blob)
            })
            .collect();
        let mut fresh = Vec::with_capacity(keyed.len());
        for (candidate, key, blob) in keyed {
            let seen_key = key.as_ref().is_some_and(|k| self.seen.keys.contains(k));
            if seen_key || self.seen.queued.contains(&blob) {
                self.stats.duplicates += 1;
                continue;
            }
            if let Some(key) = key {
                self.seen.keys.insert(key);
            }
            self.seen.queued.insert(blob);
            fresh.push(candidate);
        }
        let staged: Vec<(Candidate, std::result::Result<StagedBlob, store::Error>)> = fresh
            .into_par_iter()
            .map(|c| {
                let blob = blobs.stage(&c.raw);
                (c, blob)
            })
            .collect();
        let mut ready = Vec::with_capacity(staged.len());
        for (candidate, blob) in staged {
            match blob {
                Ok(blob) => ready.push((candidate, blob)),
                Err(err) => {
                    tracing::warn!(error = %err, "message not queued for upload");
                    self.stats.failed += 1;
                }
            }
        }
        if ready.is_empty() {
            return;
        }
        // Durable on this thread, so the writer has nothing left to do; it retries what
        // failed here before it commits.
        if let Err(err) = blobs.persist(ready.iter().map(|(_, b)| b)) {
            tracing::warn!(error = %err, "making import blobs durable failed; the writer retries");
        }
        let count = ready.len() as u64;
        let (account, folder, origin) = (self.target.account, self.target.folder, self.origin);
        let origin = origin.to_owned();
        let queued = self.store.write_blocking(move |w| {
            for (candidate, blob) in &ready {
                OpPayload::Append {
                    folder,
                    blob: blob.id(),
                    flags: candidate.flags,
                    keywords: candidate.keywords.clone(),
                }
                .enqueue_with_blob(w, account, &origin, blob)?;
            }
            Ok(())
        });
        match queued {
            Ok(()) => self.stats.queued += count,
            Err(err) => {
                tracing::warn!(error = %err, "messages not queued for upload");
                self.stats.failed += count;
            }
        }
    }
}
