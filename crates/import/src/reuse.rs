//! The offline body reuse index (PLAN §4.14 item 4).
//!
//! Thunderbird keeps offline copies of IMAP messages in `ImapMail/**` (mbox or
//! Maildir). [`BodyReuse::scan`] reads every store in parallel (one rayon worker per
//! mbox file or Maildir folder), reconstructs the server's bytes of each message and
//! records `Message-ID → (file, offset, length, reconstructed size, header
//! fingerprint, content hash)`. During the first sync, [`BodyReuse::lookup`] (the
//! [`BodyLookup`] implementation) hands sync the body of a message it is about to
//! download, if it is certainly the same message.
//!
//! # Where the index lives
//!
//! The store's connection is private to the `store` crate, so the index can't be a
//! temp table in `mazemail.db` as PLAN §4.14 sketches. It is a separate SQLCipher
//! database, by default `mail/tmp/thunderbird-body-reuse.db`, keyed with its own
//! subkey derived from the master key (label [`KEY_LABEL`]). It survives restarts
//! until the first sync of every imported account is done, then
//! [`BodyReuse::discard`] deletes it. It holds Message-IDs and paths, never message
//! content; the bodies stay in the Thunderbird profile until reused, and reused
//! bodies are encrypted by the blob store like any other.
//!
//! # Performance
//!
//! The ignored `perf` integration test (release, Apple Silicon, warm page cache): a
//! generated 8 GB tree of 254k messages (2–60 KB) in mbox files of 1–300 MB is indexed
//! in 0.95 s (about 8.4 GB/s, peak RSS 55 MiB); 2 GB in 0.36 s (5.5 GB/s, 36 MiB). The
//! 24 GB target (under 10 minutes, under 500 MB) is met by a wide margin; on a cold
//! cache the SSD's read speed is the limit. Memory stays bounded: one message per
//! worker plus at most 64 queued chunks of 1,000 rows.
//!
//! # Matching rules
//!
//! A lookup returns bytes only if the Message-ID, the size (`RFC822.SIZE`) and the
//! fingerprint all match, and all entries matching those three have the same content
//! (identical copies in several folders, for example INBOX and Gmail's All Mail, count
//! as one). Several different contents, size-only matches and fingerprint-only
//! matches return `None`. The bytes are read again and verified against the recorded
//! size and content hash, so a store Thunderbird compacted since the scan never
//! yields the wrong message.

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ingest::{BodyLookup, HeaderFingerprint};
use mazemail_mime::thread::normalize_message_id;
use rayon::prelude::*;
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use store::DataLayout;
use vault::Vault;
use zeroize::Zeroizing;

use crate::maildir::is_maildir;
use crate::mbox::{MboxReader, read_single_record};
use crate::progress::{Phase, Progress, Unit, report};
use crate::tree::{is_folder_file_name, looks_like_mbox};
use crate::{Error, Result, reconstruct};

/// File name of the index in the store's `tmp/` directory.
pub const REUSE_INDEX_FILE: &str = "thunderbird-body-reuse.db";

/// HKDF label of the index's SQLCipher key (`mazemail/vault/v1/import-body-reuse`).
pub const KEY_LABEL: &str = "import-body-reuse";

/// Rows per insert transaction.
const ROWS_PER_COMMIT: usize = 20_000;

/// Rows per message sent from a worker to the writer.
const ROWS_PER_CHUNK: usize = 1_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS file (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  kind INTEGER NOT NULL            -- 0 mbox, 1 maildir message
);
CREATE TABLE IF NOT EXISTS entry (
  message_id TEXT NOT NULL,
  file_id INTEGER NOT NULL REFERENCES file,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL,
  size INTEGER NOT NULL,
  fingerprint BLOB NOT NULL,
  content BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS entry_message ON entry(message_id);
";

const KIND_MBOX: i64 = 0;
const KIND_MAILDIR: i64 = 1;

/// What a scan found.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScanStats {
    /// mbox files scanned.
    pub mbox_files: u64,
    /// Maildir message files scanned.
    pub maildir_files: u64,
    /// Messages read.
    pub messages: u64,
    /// Messages recorded (those with a Message-ID).
    pub indexed: u64,
    /// Messages without a Message-ID (can't be matched, not recorded).
    pub without_message_id: u64,
    /// Files that couldn't be read (skipped, logged).
    pub unreadable: u64,
    /// Bytes read.
    pub bytes: u64,
    pub elapsed: Duration,
}

impl ScanStats {
    /// Read throughput in MB/s (10^6 bytes).
    pub fn mb_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.bytes as f64 / 1e6 / secs
        } else {
            0.0
        }
    }
}

/// The body reuse index (see the module docs). Thread-safe; lookups serialize on one
/// connection, which is plenty for sync's pace.
pub struct BodyReuse {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for BodyReuse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyReuse")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// One work unit of a scan.
enum Work {
    Mbox { file_id: i64, path: PathBuf },
    Maildir { files: Vec<(i64, PathBuf)> },
}

impl Work {
    fn weight(&self) -> u64 {
        match self {
            Self::Mbox { path, .. } => fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            Self::Maildir { files } => files.len() as u64 * 8192,
        }
    }
}

/// One index row produced by a worker.
struct Row {
    message_id: String,
    file_id: i64,
    offset: u64,
    len: u64,
    size: u64,
    fingerprint: HeaderFingerprint,
    content: [u8; 16],
}

/// Counters shared by the workers.
#[derive(Default)]
struct Counters {
    messages: AtomicU64,
    indexed: AtomicU64,
    without_id: AtomicU64,
    unreadable: AtomicU64,
    bytes: AtomicU64,
}

impl BodyReuse {
    /// The default location: `<data_dir>/mail/tmp/thunderbird-body-reuse.db`.
    pub fn default_path(layout: &DataLayout) -> PathBuf {
        layout.tmp_dir().join(REUSE_INDEX_FILE)
    }

    /// Creates an empty index at `path`, replacing an existing one.
    pub fn create(path: &Path, vault: &Vault) -> Result<Self> {
        remove_db_files(path)?;
        Self::open_inner(path, vault, true)
    }

    /// Opens an existing index (for example after a restart, before the first sync).
    pub fn open(path: &Path, vault: &Vault) -> Result<Self> {
        if !path.exists() {
            return Err(Error::Io {
                context: "opening the body reuse index",
                path: path.to_owned(),
                source: std::io::ErrorKind::NotFound.into(),
            });
        }
        Self::open_inner(path, vault, false)
    }

    fn open_inner(path: &Path, vault: &Vault, create: bool) -> Result<Self> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(path, flags).map_err(Error::sqlite(path))?;
        let subkey = vault.derive(KEY_LABEL);
        let mut statement = Zeroizing::new(String::with_capacity(96));
        statement.push_str("PRAGMA key = \"x'");
        for byte in subkey.expose_secret() {
            statement.push_str(&format!("{byte:02x}"));
        }
        statement.push_str("'\";");
        conn.execute_batch(&statement)
            .map_err(Error::sqlite(path))?;
        match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        }) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(err, _)) if err.code == ErrorCode::NotADatabase => {
                return Err(Error::ReuseIndexKey(path.to_owned()));
            }
            Err(err) => return Err(Error::sqlite(path)(err)),
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY;",
        )
        .map_err(Error::sqlite(path))?;
        conn.execute_batch(SCHEMA).map_err(Error::sqlite(path))?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_owned(),
        })
    }

    /// The index file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Scans every mbox file and Maildir folder below `roots` (for example each
    /// `ImapMail/<host>` directory) and adds their messages to the index.
    pub fn scan(&self, roots: &[PathBuf], progress: &dyn Progress) -> Result<ScanStats> {
        let started = Instant::now();
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);

        let mut mbox_files = Vec::new();
        let mut maildirs = Vec::new();
        for root in roots {
            collect(root, &mut mbox_files, &mut maildirs)?;
        }
        // Register files first (sequentially) so workers can tag rows with ids.
        let mut work = Vec::new();
        let mut stats = ScanStats::default();
        {
            let tx = conn
                .unchecked_transaction()
                .map_err(Error::sqlite(&self.path))?;
            let mut insert = tx
                .prepare("INSERT OR IGNORE INTO file (path, kind) VALUES (?1, ?2)")
                .map_err(Error::sqlite(&self.path))?;
            let mut select = tx
                .prepare("SELECT id FROM file WHERE path = ?1")
                .map_err(Error::sqlite(&self.path))?;
            // A rescan replaces what an earlier scan recorded for the same file.
            let mut clear = tx
                .prepare("DELETE FROM entry WHERE file_id = ?1")
                .map_err(Error::sqlite(&self.path))?;
            let mut file_id = |path: &Path, kind: i64| -> Result<Option<i64>> {
                let Some(text) = path.to_str() else {
                    tracing::debug!("skipping a store file with a non-UTF-8 path");
                    return Ok(None);
                };
                insert
                    .execute(params![text, kind])
                    .map_err(Error::sqlite(&self.path))?;
                let id: i64 = select
                    .query_row([text], |r| r.get(0))
                    .map_err(Error::sqlite(&self.path))?;
                clear.execute([id]).map_err(Error::sqlite(&self.path))?;
                Ok(Some(id))
            };
            for path in mbox_files {
                if let Some(id) = file_id(&path, KIND_MBOX)? {
                    stats.mbox_files += 1;
                    work.push(Work::Mbox { file_id: id, path });
                }
            }
            for dir in maildirs {
                let mut files = Vec::new();
                for entry in crate::maildir::list(&dir)? {
                    if let Some(id) = file_id(&entry.path, KIND_MAILDIR)? {
                        files.push((id, entry.path));
                    }
                }
                stats.maildir_files += files.len() as u64;
                if !files.is_empty() {
                    work.push(Work::Maildir { files });
                }
            }
            drop(insert);
            drop(select);
            drop(clear);
            tx.commit().map_err(Error::sqlite(&self.path))?;
        }
        // Largest first, so one huge INBOX doesn't start last.
        let mut weighted: Vec<(u64, Work)> = work.into_iter().map(|w| (w.weight(), w)).collect();
        weighted.sort_by_key(|(weight, _)| std::cmp::Reverse(*weight));
        let total_bytes: u64 = weighted.iter().map(|(w, _)| *w).sum();

        let counters = Counters::default();
        let (tx, rx) = sync_channel::<Vec<Row>>(64);
        let path = self.path.clone();
        // Workers on the rayon pool (driven from a scoped thread), the writer here: the
        // connection stays on this thread.
        let written = std::thread::scope(|scope| -> Result<u64> {
            let counters = &counters;
            scope.spawn(move || {
                weighted
                    .into_par_iter()
                    .for_each_with(tx, |tx, (_, unit)| scan_unit(unit, tx, counters));
            });
            write_rows(&conn, &path, rx, counters, total_bytes, progress)
        })?;

        stats.messages = counters.messages.load(Ordering::Relaxed);
        stats.indexed = written;
        stats.without_message_id = counters.without_id.load(Ordering::Relaxed);
        stats.unreadable = counters.unreadable.load(Ordering::Relaxed);
        stats.bytes = counters.bytes.load(Ordering::Relaxed);
        stats.elapsed = started.elapsed();
        report(
            progress,
            Phase::BodyIndex,
            total_bytes,
            total_bytes,
            Unit::Bytes,
        );
        tracing::info!(
            messages = stats.messages,
            indexed = stats.indexed,
            mb_per_sec = stats.mb_per_sec(),
            "offline body index built"
        );
        Ok(stats)
    }

    /// How many messages the index holds.
    pub fn len(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("SELECT count(*) FROM entry", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(Error::sqlite(&self.path))
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Like [`BodyReuse::lookup`], with errors reported.
    pub fn try_lookup(
        &self,
        rfc_message_id: &str,
        size: u64,
        fingerprint: &HeaderFingerprint,
    ) -> Result<Option<Vec<u8>>> {
        let Some(id) = normalize_message_id(rfc_message_id) else {
            return Ok(None);
        };
        let candidates: Vec<(u64, u64, [u8; 16], String, i64)> = {
            let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.offset, e.len, e.size, e.fingerprint, e.content, f.path, f.kind
                     FROM entry e JOIN file f ON f.id = e.file_id WHERE e.message_id = ?1",
                )
                .map_err(Error::sqlite(&self.path))?;
            let rows = stmt
                .query_map([id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                })
                .map_err(Error::sqlite(&self.path))?;
            let mut out = Vec::new();
            for row in rows {
                let (offset, len, row_size, fp, content, path, kind) =
                    row.map_err(Error::sqlite(&self.path))?;
                // All three must match; anything else isn't a candidate.
                if row_size as u64 != size
                    || HeaderFingerprint::from_slice(&fp) != Some(*fingerprint)
                {
                    continue;
                }
                let Ok(content) = <[u8; 16]>::try_from(content.as_slice()) else {
                    continue;
                };
                out.push((offset as u64, len as u64, content, path, kind));
            }
            out
        };
        let Some(first) = candidates.first() else {
            return Ok(None);
        };
        if candidates.iter().any(|c| c.2 != first.2) {
            // Different messages share Message-ID, size and fingerprint: ambiguous.
            return Ok(None);
        }
        for (offset, len, content, path, kind) in &candidates {
            if let Some(bytes) = read_verified(Path::new(path), *kind, *offset, *len, size, content)
            {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Closes the index and deletes its files.
    pub fn discard(self) -> Result<()> {
        let path = self.path.clone();
        drop(self.conn);
        remove_db_files(&path)
    }
}

impl BodyLookup for BodyReuse {
    fn lookup(
        &self,
        rfc_message_id: &str,
        size: u64,
        fingerprint: &HeaderFingerprint,
    ) -> Option<Vec<u8>> {
        match self.try_lookup(rfc_message_id, size, fingerprint) {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(error = %err, "body reuse lookup failed; fetching from the server");
                None
            }
        }
    }
}

fn remove_db_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::io("removing", PathBuf::from(p))(err)),
        }
    }
    Ok(())
}

/// Collects mbox files and Maildir folders below `dir`.
fn collect(dir: &Path, mbox: &mut Vec<PathBuf>, maildirs: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(Error::io("listing", dir))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(Error::io("listing", dir))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().map_err(Error::io("reading", &path))?;
        if file_type.is_dir() {
            if matches!(name.as_str(), "cur" | "new" | "tmp") {
                continue;
            }
            if is_maildir(&path) {
                maildirs.push(path.clone());
            }
            collect(&path, mbox, maildirs)?;
        } else if file_type.is_file()
            && is_folder_file_name(&name)
            && entry.metadata().is_ok_and(|m| m.len() > 0)
            && looks_like_mbox(&path)
        {
            mbox.push(path);
        }
    }
    Ok(())
}

/// What one stored message contributes to the index.
fn analyze(stored: &[u8]) -> Option<(String, u64, HeaderFingerprint, [u8; 16])> {
    let r = reconstruct(stored);
    let header_end = r
        .bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(r.bytes.len(), |i| i + 4);
    let headers = mazemail_mime::parse_headers_only(&r.bytes[..header_end]).ok()?;
    let id = headers
        .message_id
        .as_deref()
        .and_then(normalize_message_id)?
        .to_owned();
    Some((
        id,
        r.bytes.len() as u64,
        HeaderFingerprint::from_headers(&headers),
        content_hash(&r.bytes),
    ))
}

fn content_hash(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    out
}

fn scan_unit(unit: Work, tx: &mut SyncSender<Vec<Row>>, counters: &Counters) {
    let mut rows = Vec::with_capacity(ROWS_PER_CHUNK);
    let push = |rows: &mut Vec<Row>, row: Option<Row>| {
        counters.messages.fetch_add(1, Ordering::Relaxed);
        match row {
            Some(row) => {
                rows.push(row);
                if rows.len() >= ROWS_PER_CHUNK {
                    // The writer only stops on error; then the rows don't matter.
                    let _ = tx.send(std::mem::replace(rows, Vec::with_capacity(ROWS_PER_CHUNK)));
                }
            }
            None => {
                counters.without_id.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    match unit {
        Work::Mbox { file_id, path } => {
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(err) => {
                    tracing::warn!(error = %err, path = %path.display(), "can't read an offline store");
                    counters.unreadable.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            for record in MboxReader::new(BufReader::with_capacity(1 << 20, file)) {
                let record = match record {
                    Ok(record) => record,
                    Err(err) => {
                        tracing::warn!(error = %err, path = %path.display(), "offline store read failed part-way");
                        counters.unreadable.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                };
                counters.bytes.fetch_add(record.len, Ordering::Relaxed);
                let row =
                    analyze(&record.message).map(|(message_id, size, fingerprint, content)| Row {
                        message_id,
                        file_id,
                        offset: record.offset,
                        len: record.len,
                        size,
                        fingerprint,
                        content,
                    });
                push(&mut rows, row);
            }
        }
        Work::Maildir { files } => {
            for (file_id, path) in files {
                let bytes = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        tracing::debug!(error = %err, "can't read an offline message file");
                        counters.unreadable.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                counters
                    .bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                let len = bytes.len() as u64;
                let row = analyze(&bytes).map(|(message_id, size, fingerprint, content)| Row {
                    message_id,
                    file_id,
                    offset: 0,
                    len,
                    size,
                    fingerprint,
                    content,
                });
                push(&mut rows, row);
            }
        }
    }
    if !rows.is_empty() {
        let _ = tx.send(rows);
    }
}

fn write_rows(
    conn: &Connection,
    path: &Path,
    rx: std::sync::mpsc::Receiver<Vec<Row>>,
    counters: &Counters,
    total_bytes: u64,
    progress: &dyn Progress,
) -> Result<u64> {
    let mut written = 0u64;
    let mut in_tx = 0usize;
    conn.execute_batch("BEGIN").map_err(Error::sqlite(path))?;
    let result = (|| -> Result<()> {
        let mut insert = conn
            .prepare_cached(
                "INSERT INTO entry (message_id, file_id, offset, len, size, fingerprint, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(Error::sqlite(path))?;
        for chunk in rx {
            for row in chunk {
                insert
                    .execute(params![
                        row.message_id,
                        row.file_id,
                        row.offset as i64,
                        row.len as i64,
                        row.size as i64,
                        &row.fingerprint.as_bytes()[..],
                        &row.content[..],
                    ])
                    .map_err(Error::sqlite(path))?;
                written += 1;
                in_tx += 1;
            }
            if in_tx >= ROWS_PER_COMMIT {
                conn.execute_batch("COMMIT; BEGIN")
                    .map_err(Error::sqlite(path))?;
                in_tx = 0;
                report(
                    progress,
                    Phase::BodyIndex,
                    counters.bytes.load(Ordering::Relaxed),
                    total_bytes,
                    Unit::Bytes,
                );
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(Error::sqlite(path))?;
            counters.indexed.store(written, Ordering::Relaxed);
            Ok(written)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// Reads an entry back and checks it is still the recorded message.
fn read_verified(
    path: &Path,
    kind: i64,
    offset: u64,
    len: u64,
    size: u64,
    content: &[u8; 16],
) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let stored = if kind == KIND_MBOX {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut range = Vec::with_capacity(usize::try_from(len).ok()?);
        file.take(len).read_to_end(&mut range).ok()?;
        if range.len() as u64 != len {
            return None;
        }
        read_single_record(&range)?.message
    } else {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        bytes
    };
    let bytes = reconstruct(&stored).bytes;
    (bytes.len() as u64 == size && content_hash(&bytes) == *content).then_some(bytes)
}
