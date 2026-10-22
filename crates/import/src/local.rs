//! Importing mbox files and Maildir folders into a local account (PLAN §4.14 items 5
//! and 6, `mazemail import mbox|maildir`).
//!
//! Messages go through [`ingest`] like synced mail. Re-importing is idempotent: a
//! message is skipped if the target folder already holds one with the same Message-ID,
//! size and [`HeaderFingerprint`], or with exactly the same bytes (which also covers
//! messages without a Message-ID).

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ingest::{DEFAULT_BATCH, HeaderFingerprint, Ingestor, MessageMeta};
use mazemail_mime::thread::normalize_message_id;
use mazemail_types::{AccountId, BlobId, Flags, FolderId, FolderRole};
use rayon::prelude::*;
use store::{
    AccountKind, MessageScope, NewAccount, NewFolder, NewLocation, Queries, Store, UnixTime,
};

use crate::mbox::{Escaping, MboxReader};
use crate::progress::{Phase, Progress, Unit, report};
use crate::{Error, Result, maildir, reconstruct};

/// The default name of the local account imports go to.
pub const LOCAL_ACCOUNT_NAME: &str = "Local Folders";

/// Read buffer for mbox files.
const READ_BUFFER: usize = 1 << 20;

/// Where imported messages go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportTarget {
    /// A `kind = local` account; `None` uses (or creates) the account named
    /// [`LOCAL_ACCOUNT_NAME`].
    pub account: Option<AccountId>,
    /// Folder path below the account (`["Archive", "2019"]`), created as needed;
    /// `None` uses the file or directory name.
    pub folder: Option<Vec<String>>,
}

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Messages read from the source.
    pub messages: u64,
    /// Messages stored.
    pub imported: u64,
    /// Messages already present in the target folder (skipped).
    pub duplicates: u64,
    /// Messages Thunderbird had deleted but not yet compacted away (skipped).
    pub deleted: u64,
    /// Messages that couldn't be stored (unparseable, store errors).
    pub failed: u64,
    /// Source bytes read.
    pub bytes: u64,
    /// Bytes before the first `From ` line of mbox files (skipped).
    pub skipped_bytes: u64,
    /// Folders created.
    pub folders_created: u64,
}

impl ImportStats {
    /// Adds another import's numbers.
    pub fn add(&mut self, other: &Self) {
        self.messages += other.messages;
        self.imported += other.imported;
        self.duplicates += other.duplicates;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.bytes += other.bytes;
        self.skipped_bytes += other.skipped_bytes;
        self.folders_created += other.folders_created;
    }
}

/// Imports an mbox file (mboxrd unescaping, Thunderbird status headers read and
/// removed) into a local folder.
pub fn import_mbox(
    ingestor: &Ingestor,
    path: &Path,
    target: &ImportTarget,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    let account = resolve_account(ingestor.store(), target.account)?;
    let folder_path = target
        .folder
        .clone()
        .unwrap_or_else(|| vec![file_label(path)]);
    let (folder, created) = ensure_folder_path(ingestor.store(), account, &folder_path, None)?;
    let mut stats = import_mbox_into(ingestor, path, account, folder, progress)?;
    stats.folders_created += created;
    Ok(stats)
}

/// Imports a Maildir folder, and its Maildir++ subfolders (`.Sent`, `.Archive.2024`)
/// as child folders, into a local folder.
pub fn import_maildir(
    ingestor: &Ingestor,
    dir: &Path,
    target: &ImportTarget,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    if !maildir::is_maildir(dir) {
        return Err(Error::InvalidTarget(format!(
            "{} is not a Maildir folder (no cur/ directory)",
            dir.display()
        )));
    }
    let account = resolve_account(ingestor.store(), target.account)?;
    let root_path = target
        .folder
        .clone()
        .unwrap_or_else(|| vec![file_label(dir)]);
    let (root, created) = ensure_folder_path(ingestor.store(), account, &root_path, None)?;
    let mut stats = import_maildir_into(ingestor, dir, account, root, progress)?;
    stats.folders_created += created;
    for (segments, sub) in maildir::maildirpp_subfolders(dir)? {
        let mut path = root_path.clone();
        path.extend(segments);
        let (folder, created) = ensure_folder_path(ingestor.store(), account, &path, None)?;
        stats.folders_created += created;
        stats.add(&import_maildir_into(
            ingestor, &sub, account, folder, progress,
        )?);
    }
    Ok(stats)
}

/// A folder name from a file or directory name.
fn file_label(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Imported".to_owned())
}

/// The given local account, or the default one (created if missing).
pub fn resolve_account(store: &Store, account: Option<AccountId>) -> Result<AccountId> {
    match account {
        Some(id) => {
            let found = store.reader()?.account(id)?;
            match found {
                Some(a) if a.kind == AccountKind::Local => Ok(id),
                Some(_) => Err(Error::InvalidTarget(format!(
                    "account {id} is not a local account"
                ))),
                None => Err(Error::InvalidTarget(format!("account {id} doesn't exist"))),
            }
        }
        None => local_account(store, LOCAL_ACCOUNT_NAME),
    }
}

/// The local account with this name, created if there is none.
pub fn local_account(store: &Store, name: &str) -> Result<AccountId> {
    let name = name.to_owned();
    Ok(store.write_blocking(move |w| {
        let accounts = w.accounts()?;
        if let Some(a) = accounts
            .iter()
            .find(|a| a.kind == AccountKind::Local && a.name == name)
        {
            return Ok(a.id);
        }
        let ordinal = accounts.iter().map(|a| a.ordinal + 1).max().unwrap_or(0);
        w.create_account(&NewAccount {
            kind: AccountKind::Local,
            name,
            config_json: "{}".to_owned(),
            ordinal,
        })
    })?)
}

/// Finds or creates the folder at `path` below `account` (local folders: no remote
/// name). `leaf_role` is set on a newly created leaf if no folder of the account has
/// that role yet. Returns the folder and how many folders were created.
pub fn ensure_folder_path(
    store: &Store,
    account: AccountId,
    path: &[String],
    leaf_role: Option<FolderRole>,
) -> Result<(FolderId, u64)> {
    if path.is_empty() || path.iter().any(|s| s.trim().is_empty()) {
        return Err(Error::InvalidTarget("empty folder name".to_owned()));
    }
    let path = path.to_vec();
    Ok(store.write_blocking(move |w| {
        let mut folders = w.folders(Some(account))?;
        let mut parent: Option<FolderId> = None;
        let mut created = 0;
        for (i, segment) in path.iter().enumerate() {
            if let Some(existing) = folders
                .iter()
                .find(|f| f.parent == parent && f.display_name == *segment)
            {
                parent = Some(existing.id);
                continue;
            }
            let is_leaf = i + 1 == path.len();
            let role = leaf_role
                .filter(|_| is_leaf)
                .filter(|role| !folders.iter().any(|f| f.role == Some(*role)));
            let ordinal = folders
                .iter()
                .filter(|f| f.parent == parent)
                .map(|f| f.ordinal + 1)
                .max()
                .unwrap_or(0);
            let id = w.create_folder(&NewFolder {
                account,
                parent,
                remote_name: None,
                display_name: segment.clone(),
                role,
                is_label: false,
                sync_policy: None,
                ordinal,
            })?;
            created += 1;
            if let Some(folder) = w.folder(id)? {
                folders.push(folder);
            }
            parent = Some(id);
        }
        // `path` is non-empty, so `parent` is set.
        parent
            .map(|p| (p, created))
            .ok_or(store::Error::Invalid("empty folder path"))
    })?)
}

/// Imports an mbox file into an existing folder.
pub(crate) fn import_mbox_into(
    ingestor: &Ingestor,
    path: &Path,
    account: AccountId,
    folder: FolderId,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    let file = File::open(path).map_err(Error::io("opening", path))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = MboxReader::new(BufReader::with_capacity(READ_BUFFER, file))
        .with_escaping(Escaping::Mboxrd);
    let mut batch = FolderBatch::new(ingestor, account, folder)?;
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
            internal_date: record.envelope.date,
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

/// Imports one Maildir folder (no subfolders) into an existing folder.
pub(crate) fn import_maildir_into(
    ingestor: &Ingestor,
    dir: &Path,
    account: AccountId,
    folder: FolderId,
    progress: &dyn Progress,
) -> Result<ImportStats> {
    let entries = maildir::list(dir)?;
    let total = entries.len() as u64;
    let mut batch = FolderBatch::new(ingestor, account, folder)?;
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
            internal_date: entry.mtime,
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

/// A message read from a source, ready to ingest.
struct Candidate {
    raw: Vec<u8>,
    flags: Flags,
    keywords: Vec<String>,
    internal_date: Option<UnixTime>,
}

/// The dedupe key of a message: Message-ID, size and fingerprint (with the date the
/// store records: the `Date` header, else the envelope date).
type DedupeKey = (String, u64, HeaderFingerprint);

/// What the target folder already holds.
#[derive(Default)]
struct Dedupe {
    keys: HashSet<DedupeKey>,
    blobs: HashSet<BlobId>,
}

impl Dedupe {
    fn load(store: &Store, folder: FolderId) -> Result<Self> {
        let reader = store.reader()?;
        let mut out = Self::default();
        for id in reader.message_ids_in_scope(&MessageScope::Folder(folder))? {
            let Some(m) = reader.message(id)? else {
                continue;
            };
            if let (Some(rfc), Some(size)) = (m.rfc_message_id.as_deref(), m.size) {
                out.keys.insert((
                    rfc.to_owned(),
                    size,
                    HeaderFingerprint::new(
                        Some(m.date),
                        m.from.as_ref().map(|a| a.email.as_str()),
                        m.subject.as_deref(),
                    ),
                ));
            }
            if let Some(blob) = m.blob {
                out.blobs.insert(blob);
            }
        }
        Ok(out)
    }
}

fn dedupe_key(raw: &[u8], internal_date: Option<UnixTime>) -> Option<DedupeKey> {
    let headers = mazemail_mime::parse_headers_only(raw).ok()?;
    let rfc = headers
        .message_id
        .as_deref()
        .and_then(normalize_message_id)?
        .to_owned();
    let date = headers.date.map(|d| d.unix_timestamp()).or(internal_date)?;
    Some((
        rfc,
        raw.len() as u64,
        HeaderFingerprint::new(
            Some(date),
            headers
                .from
                .first()
                .or(headers.sender.as_ref())
                .map(|a| a.email.as_str()),
            headers.subject.as_deref(),
        ),
    ))
}

/// Collects messages for one folder and ingests them in batches.
struct FolderBatch<'a> {
    ingestor: &'a Ingestor,
    account: AccountId,
    folder: FolderId,
    dedupe: Dedupe,
    pending: Vec<Candidate>,
    stats: ImportStats,
}

impl<'a> FolderBatch<'a> {
    fn new(ingestor: &'a Ingestor, account: AccountId, folder: FolderId) -> Result<Self> {
        Ok(Self {
            ingestor,
            account,
            folder,
            dedupe: Dedupe::load(ingestor.store(), folder)?,
            pending: Vec::with_capacity(DEFAULT_BATCH),
            stats: ImportStats::default(),
        })
    }

    fn push(&mut self, candidate: Candidate) {
        self.stats.messages += 1;
        self.pending.push(candidate);
    }

    /// Drops duplicates, prepares the rest in parallel (parse, blob), commits them in
    /// one write job.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let blobs = self.ingestor.store().blobs();
        let keyed: Vec<(Candidate, Option<DedupeKey>, BlobId)> = pending
            .into_par_iter()
            .map(|c| {
                let key = dedupe_key(&c.raw, c.internal_date);
                let blob = blobs.id_for(&c.raw);
                (c, key, blob)
            })
            .collect();
        let mut fresh = Vec::with_capacity(keyed.len());
        for (candidate, key, blob) in keyed {
            let seen_key = key.as_ref().is_some_and(|k| self.dedupe.keys.contains(k));
            if seen_key || self.dedupe.blobs.contains(&blob) {
                self.stats.duplicates += 1;
                continue;
            }
            if let Some(key) = key {
                self.dedupe.keys.insert(key);
            }
            self.dedupe.blobs.insert(blob);
            fresh.push(candidate);
        }
        let (account, folder, ingestor) = (self.account, self.folder, self.ingestor);
        let prepared: Vec<_> = fresh
            .into_par_iter()
            .map(|c| {
                let location = NewLocation {
                    folder,
                    uid: None,
                    modseq: None,
                    flags: c.flags,
                    keywords: c.keywords,
                };
                ingestor.prepare_raw(
                    MessageMeta::new(account, location).with_internal_date(c.internal_date),
                    &c.raw,
                )
            })
            .collect();
        let mut ready = Vec::with_capacity(prepared.len());
        for result in prepared {
            match result {
                Ok(p) => ready.push(p),
                Err(err) => {
                    tracing::debug!(error = %err, "message not imported");
                    self.stats.failed += 1;
                }
            }
        }
        for result in self.ingestor.commit_blocking(ready) {
            match result {
                Ok(_) => self.stats.imported += 1,
                Err(err) => {
                    tracing::warn!(error = %err, "message not imported");
                    self.stats.failed += 1;
                }
            }
        }
    }
}
