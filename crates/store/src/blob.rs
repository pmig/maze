//! The encrypted, content-addressed blob store and its garbage collector (PLAN §4.3).
//!
//! # Write path
//!
//! [`BlobStore::put`] computes the keyed blob id, compresses with zstd (level 3), seals
//! with the vault's `MZB1` format (the blob id is the associated data), writes to
//! `mail/tmp/`, fsyncs, and renames into `mail/blobs/<2 hex>/<2 hex>/<id>.mzb`. It
//! returns a [`StagedBlob`]. The `blob` row is inserted (or its reference count bumped
//! by a trigger) in the writer transaction that references it, for example
//! [`WriteTx::set_message_body`](crate::WriteTx::set_message_body).
//!
//! # Why garbage collection can't delete a blob that is being referenced
//!
//! A [`StagedBlob`] *pins* its id in memory from before the file is checked or written
//! until the writer transaction that references it has committed (or the staged blob is
//! dropped unused). The collector runs on the writer thread, so it never interleaves
//! with a write transaction, and it checks pins under the same lock it holds while
//! deleting rows and unlinking files. So for every id either the collector sees the pin
//! and skips it, or [`BlobStore::put`] runs after the file is gone and writes it again.
//!
//! - Rows with `refs = 0` are deleted, then their files.
//! - Files without a row are deleted only if older than 24 hours (written, but the
//!   referencing transaction never committed, for example after a crash).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use mazemail_types::BlobId;
use rusqlite::{Connection, OptionalExtension, params};
use vault::{BlobCipher, BlobIdHasher, OpenReader};

use crate::layout::{DataLayout, create_private_dir, sharded};
use crate::{Error, Result};

/// zstd level for blobs and caches (PLAN §4.3).
pub const ZSTD_LEVEL: i32 = 3;

/// Files without a `blob` row younger than this are never collected.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Staging files in `tmp/` younger than this are never collected.
const TMP_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Rows deleted per collection pass (the pin lock is held for one pass).
const GC_BATCH: usize = 1000;

type Pins = Arc<Mutex<HashMap<BlobId, usize>>>;

fn lock(pins: &Pins) -> MutexGuard<'_, HashMap<BlobId, usize>> {
    pins.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps a blob id safe from garbage collection while it is alive.
#[derive(Debug)]
pub(crate) struct BlobPin {
    pins: Pins,
    id: BlobId,
}

impl BlobPin {
    fn new(pins: &Pins, id: BlobId) -> Self {
        *lock(pins).entry(id).or_insert(0) += 1;
        Self {
            pins: Arc::clone(pins),
            id,
        }
    }
}

impl Clone for BlobPin {
    fn clone(&self) -> Self {
        Self::new(&self.pins, self.id)
    }
}

impl Drop for BlobPin {
    fn drop(&mut self) {
        let mut pins = lock(&self.pins);
        if let Some(count) = pins.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&self.id);
            }
        }
    }
}

/// A blob whose file is on disk but that no committed row references yet.
///
/// Hand it to a writer method that references it; the store keeps it pinned until that
/// transaction has committed. Dropping it unused leaves the file for the orphan rule.
#[derive(Debug, Clone)]
pub struct StagedBlob {
    id: BlobId,
    size: u64,
    pin: BlobPin,
}

impl StagedBlob {
    /// The content address.
    pub fn id(&self) -> BlobId {
        self.id
    }

    /// The uncompressed size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn pin(&self) -> BlobPin {
        self.pin.clone()
    }
}

/// A streaming reader over a stored blob (decrypt, then decompress).
///
/// The plaintext of each 64 KiB chunk is released once its tag verified; if a read
/// fails part-way, discard everything read so far.
pub struct BlobReader {
    inner: zstd::stream::read::Decoder<'static, BufReader<OpenReader<BufReader<File>>>>,
}

impl Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl std::fmt::Debug for BlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobReader").finish_non_exhaustive()
    }
}

/// The encrypted blob store under `mail/blobs/` and the droppable cache under
/// `mail/cache/`. Cheap to clone; all methods are synchronous and do file IO and
/// crypto, so call them off the UI thread (`spawn_blocking`).
#[derive(Clone)]
pub struct BlobStore {
    inner: Arc<Inner>,
}

struct Inner {
    layout: DataLayout,
    cipher: BlobCipher,
    hasher: BlobIdHasher,
    pins: Pins,
    tmp_counter: AtomicU64,
}

impl std::fmt::Debug for BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStore")
            .field("blobs", &self.inner.layout.blobs_dir())
            .finish_non_exhaustive()
    }
}

impl BlobStore {
    pub(crate) fn new(layout: DataLayout, cipher: BlobCipher, hasher: BlobIdHasher) -> Self {
        Self {
            inner: Arc::new(Inner {
                layout,
                cipher,
                hasher,
                pins: Arc::default(),
                tmp_counter: AtomicU64::new(0),
            }),
        }
    }

    /// The blob id `content` would get (keyed BLAKE3 over the raw bytes).
    pub fn id_for(&self, content: &[u8]) -> BlobId {
        self.inner.hasher.hash(content)
    }

    /// The file of blob `id`.
    pub fn path(&self, id: &BlobId) -> PathBuf {
        self.inner.layout.blob_path(id)
    }

    /// Whether the file of blob `id` exists.
    pub fn exists(&self, id: &BlobId) -> bool {
        self.path(id).exists()
    }

    /// Stores `content` (compress, encrypt, tmp, fsync, rename) unless a file with the
    /// same id already exists, and pins it until it is referenced.
    pub fn put(&self, content: &[u8]) -> Result<StagedBlob> {
        let id = self.id_for(content);
        // Pin before looking at the file, so the collector can't remove it in between.
        let pin = BlobPin::new(&self.inner.pins, id);
        let path = self.path(&id);
        if !path.exists() {
            let tmp = self.tmp_path(&id);
            let result = self
                .write_sealed(&tmp, &id, content, true)
                .and_then(|()| install(&tmp, &path, true));
            if let Err(err) = result {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        }
        Ok(StagedBlob {
            id,
            size: content.len() as u64,
            pin,
        })
    }

    /// Opens blob `id` for streaming reads. [`Error::BlobMissing`] if there's no file.
    pub fn open(&self, id: &BlobId) -> Result<BlobReader> {
        open_sealed(&self.inner.cipher, &self.path(id), id)?.ok_or(Error::BlobMissing(*id))
    }

    /// Reads all of blob `id`.
    pub fn read(&self, id: &BlobId) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.open(id)?
            .read_to_end(&mut out)
            .map_err(read_error("reading a blob"))?;
        Ok(out)
    }

    fn tmp_path(&self, id: &BlobId) -> PathBuf {
        let n = self.inner.tmp_counter.fetch_add(1, Ordering::Relaxed);
        self.inner.layout.tmp_dir().join(format!(
            "{}-{}-{n}.tmp",
            &id.to_hex()[..16],
            std::process::id()
        ))
    }

    /// zstd → MZB1 seal → `path` (mode 0600), optionally fsynced.
    fn write_sealed(&self, path: &Path, ad: &BlobId, content: &[u8], sync: bool) -> Result<()> {
        let file = create_private_file(path).map_err(Error::io("creating a blob file"))?;
        let sealer = self.inner.cipher.seal_writer(ad, BufWriter::new(file))?;
        let mut encoder = zstd::stream::write::Encoder::new(sealer, ZSTD_LEVEL)
            .map_err(Error::io("starting zstd"))?;
        encoder
            .write_all(content)
            .map_err(Error::io("compressing a blob"))?;
        let sealer = encoder.finish().map_err(Error::io("compressing a blob"))?;
        let writer = sealer.finish()?;
        let file = writer
            .into_inner()
            .map_err(|err| Error::io("writing a blob")(err.into_error()))?;
        if sync {
            file.sync_all().map_err(Error::io("syncing a blob"))?;
        }
        Ok(())
    }

    // ---- cache ---------------------------------------------------------------------

    /// The id a cache entry is stored under: derived from the blob id, the artifact kind
    /// and the renderer version, so a new renderer never reads stale artifacts.
    pub fn cache_id(&self, blob: &BlobId, kind: &str, renderer_version: u32) -> BlobId {
        let mut hasher = blake3::Hasher::new_derive_key("mazemail 2026 cache entry id v1");
        hasher.update(&blob.0);
        hasher.update(&renderer_version.to_be_bytes());
        hasher.update(kind.as_bytes());
        BlobId(*hasher.finalize().as_bytes())
    }

    fn cache_path(&self, cache_id: &BlobId) -> PathBuf {
        sharded(&self.inner.layout.cache_dir(), &cache_id.to_hex())
    }

    /// Stores a parsed artifact (reader model, sanitized HTML) for `blob`, encrypted like
    /// blobs. Caches are not fsynced: they can be dropped any time.
    pub fn cache_put(
        &self,
        blob: &BlobId,
        kind: &str,
        renderer_version: u32,
        content: &[u8],
    ) -> Result<()> {
        let cache_id = self.cache_id(blob, kind, renderer_version);
        let tmp = self.tmp_path(&cache_id);
        let result = self
            .write_sealed(&tmp, &cache_id, content, false)
            .and_then(|()| install(&tmp, &self.cache_path(&cache_id), false));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Reads a cached artifact; `None` if there is none or it doesn't open (a damaged
    /// cache entry is deleted, since it can always be rebuilt).
    pub fn cache_get(&self, blob: &BlobId, kind: &str, renderer_version: u32) -> Option<Vec<u8>> {
        let cache_id = self.cache_id(blob, kind, renderer_version);
        let path = self.cache_path(&cache_id);
        let read = || -> Result<Option<Vec<u8>>> {
            let Some(mut reader) = open_sealed(&self.inner.cipher, &path, &cache_id)? else {
                return Ok(None);
            };
            let mut out = Vec::new();
            reader
                .read_to_end(&mut out)
                .map_err(read_error("reading a cache entry"))?;
            Ok(Some(out))
        };
        match read() {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(error = %err, "dropping a damaged cache entry");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Deletes the whole cache.
    pub fn cache_clear(&self) -> Result<()> {
        let dir = self.inner.layout.cache_dir();
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::io("clearing the cache")(err)),
        }
        create_private_dir(&dir)
    }

    // ---- garbage collection ----------------------------------------------------------

    /// Collects garbage; runs on the writer thread only (see the module docs).
    pub(crate) fn collect(&self, conn: &mut Connection, options: &GcOptions) -> Result<GcReport> {
        let mut report = GcReport::default();
        loop {
            let deleted = self.collect_unreferenced(conn, &mut report)?;
            if deleted < GC_BATCH {
                break;
            }
        }
        if options.orphans {
            self.collect_orphans(conn, options, &mut report)?;
        }
        if report != GcReport::default() {
            tracing::info!(?report, "blob garbage collection");
        }
        Ok(report)
    }

    /// One batch of `refs = 0` rows: delete the rows, commit, unlink the files, all
    /// under the pin lock. Returns the number of candidate rows seen.
    fn collect_unreferenced(&self, conn: &mut Connection, report: &mut GcReport) -> Result<usize> {
        let pins = lock(&self.inner.pins);
        let tx = conn.transaction()?;
        let candidates: Vec<BlobId> = {
            let mut stmt = tx.prepare_cached("SELECT id FROM blob WHERE refs = 0 LIMIT ?1")?;
            stmt.query_map([GC_BATCH as i64], |row| row.get::<_, [u8; 32]>(0))?
                .map(|r| r.map(BlobId))
                .collect::<rusqlite::Result<_>>()?
        };
        let seen = candidates.len();
        let doomed: Vec<BlobId> = candidates
            .into_iter()
            .filter(|id| !pins.contains_key(id))
            .collect();
        {
            let mut delete = tx.prepare_cached("DELETE FROM blob WHERE id = ?1 AND refs = 0")?;
            for id in &doomed {
                delete.execute([&id.0])?;
            }
        }
        tx.commit()?;
        report.rows_deleted += doomed.len();
        for id in &doomed {
            if remove_file_if_exists(&self.path(id))? {
                report.files_deleted += 1;
            }
        }
        drop(pins);
        // Pinned rows stay; if every candidate was pinned, stop instead of spinning.
        Ok(if doomed.is_empty() { 0 } else { seen })
    }

    /// Files under `blobs/` without a row, older than the minimum age; stale `tmp/`
    /// files.
    fn collect_orphans(
        &self,
        conn: &Connection,
        options: &GcOptions,
        report: &mut GcReport,
    ) -> Result<()> {
        let cutoff = options
            .now
            .checked_sub(options.orphan_min_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut has_row = conn.prepare_cached("SELECT 1 FROM blob WHERE id = ?1")?;
        let mut files = Vec::new();
        walk_files(&self.inner.layout.blobs_dir(), &mut files)?;
        for path in files {
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(BlobId::from_hex)
            else {
                continue;
            };
            if !older_than(&path, cutoff) {
                continue;
            }
            let pins = lock(&self.inner.pins);
            if pins.contains_key(&id) {
                continue;
            }
            let referenced = has_row.query_row([&id.0], |_| Ok(())).optional()?.is_some();
            if !referenced && remove_file_if_exists(&path)? {
                report.orphans_deleted += 1;
            }
            drop(pins);
        }
        // Staging files of writes still in flight are young; never touch those, even
        // with a small orphan age.
        let tmp_cutoff = options
            .now
            .checked_sub(options.orphan_min_age.max(TMP_MIN_AGE))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut tmp = Vec::new();
        walk_files(&self.inner.layout.tmp_dir(), &mut tmp)?;
        for path in tmp {
            if older_than(&path, tmp_cutoff) && remove_file_if_exists(&path)? {
                report.tmp_deleted += 1;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pinned(&self, id: &BlobId) -> bool {
        lock(&self.inner.pins).contains_key(id)
    }
}

/// Options for a garbage collection run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcOptions {
    /// Also scan `blobs/` for files without a row (expensive on big stores).
    pub orphans: bool,
    /// Files without a row younger than this are kept ([`ORPHAN_MIN_AGE`]).
    pub orphan_min_age: Duration,
    /// The current time (overridable for tests).
    pub now: SystemTime,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            orphans: false,
            orphan_min_age: ORPHAN_MIN_AGE,
            now: SystemTime::now(),
        }
    }
}

impl GcOptions {
    /// Unreferenced rows and orphan files.
    pub fn full() -> Self {
        Self {
            orphans: true,
            ..Self::default()
        }
    }
}

/// What a garbage collection run deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    /// `blob` rows with `refs = 0`.
    pub rows_deleted: usize,
    /// Their files.
    pub files_deleted: usize,
    /// Files without a row, older than the minimum age.
    pub orphans_deleted: usize,
    /// Stale staging files in `tmp/` (older than the orphan age, and at least an hour).
    pub tmp_deleted: usize,
}

/// Inserts the `blob` row for a staged blob if it doesn't exist (refs stay as they are;
/// the referencing row's trigger bumps them).
pub(crate) fn ensure_row(conn: &Connection, blob: &StagedBlob, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO blob (id, size, created, refs) VALUES (?1, ?2, ?3, 0)
         ON CONFLICT (id) DO NOTHING",
    )?
    .execute(params![&blob.id.0, blob.size as i64, now])?;
    Ok(())
}

fn open_sealed(cipher: &BlobCipher, path: &Path, ad: &BlobId) -> Result<Option<BlobReader>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io("opening a blob")(err)),
    };
    let opened = cipher.open_reader(ad, BufReader::new(file))?;
    let inner = zstd::stream::read::Decoder::new(opened).map_err(Error::io("starting zstd"))?;
    Ok(Some(BlobReader { inner }))
}

/// Maps a read error: authentication failures from the vault come wrapped in
/// `io::Error`; surface them as vault errors.
fn read_error(context: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |err| {
        if err
            .get_ref()
            .is_some_and(|inner| inner.is::<vault::Error>())
        {
            match err
                .into_inner()
                .map(|inner| inner.downcast::<vault::Error>())
            {
                Some(Ok(vault_err)) => Error::Vault(*vault_err),
                _ => Error::Invalid("blob could not be decoded"),
            }
        } else {
            Error::io(context)(err)
        }
    }
}

/// Renames `tmp` to `dest`, creating the shard directories; with `sync`, also fsyncs the
/// directory so the rename survives a crash.
fn install(tmp: &Path, dest: &Path, sync: bool) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or(Error::Invalid("blob path without a parent"))?;
    fs::create_dir_all(dir).map_err(Error::io("creating a blob directory"))?;
    fs::rename(tmp, dest).map_err(Error::io("installing a blob"))?;
    if sync {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(Error::io("syncing a blob directory"))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::io("deleting a blob")(err)),
    }
}

fn older_than(path: &Path, cutoff: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|modified| modified <= cutoff)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io("scanning blobs")(err)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::io("scanning blobs"))?;
        let file_type = entry.file_type().map_err(Error::io("scanning blobs"))?;
        if file_type.is_dir() {
            walk_files(&entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use vault::{KEY_LEN, KeyId, MasterKey};

    use super::*;

    fn store(dir: &Path) -> BlobStore {
        let layout = DataLayout::new(dir);
        layout.create_dirs().unwrap();
        let master = MasterKey::from_bytes(KeyId::FIRST, [5; KEY_LEN]);
        BlobStore::new(layout, BlobCipher::new(&master), BlobIdHasher::new(&master))
    }

    #[test]
    fn put_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = store(tmp.path());
        let content = b"Subject: hello\r\n\r\nbody body body body body".repeat(1000);
        let staged = blobs.put(&content).unwrap();
        assert_eq!(staged.size(), content.len() as u64);
        assert_eq!(blobs.read(&staged.id()).unwrap(), content);

        // Compressed and encrypted on disk.
        let on_disk = fs::read(blobs.path(&staged.id())).unwrap();
        assert!(on_disk.starts_with(b"MZB1"));
        assert!(on_disk.len() < content.len() / 10);

        // Deduplicated: same id, same file.
        let again = blobs.put(&content).unwrap();
        assert_eq!(again.id(), staged.id());
        assert!(blobs.pinned(&staged.id()));
        drop((staged, again));
        assert!(!blobs.pinned(&blobs.id_for(&content)));

        // Nothing left in tmp/.
        assert_eq!(
            fs::read_dir(tmp.path().join("mail/tmp")).unwrap().count(),
            0
        );
    }

    #[test]
    fn missing_and_tampered_blobs_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = store(tmp.path());
        let missing = BlobId([9; 32]);
        assert!(matches!(blobs.open(&missing), Err(Error::BlobMissing(id)) if id == missing));

        let staged = blobs.put(b"original content").unwrap();
        let path = blobs.path(&staged.id());
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            blobs.read(&staged.id()),
            Err(Error::Vault(vault::Error::Decrypt))
        ));
    }

    #[test]
    fn cache_entries_are_versioned_and_droppable() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = store(tmp.path());
        let blob = BlobId([1; 32]);
        assert_eq!(blobs.cache_get(&blob, "reader", 1), None);
        blobs
            .cache_put(&blob, "reader", 1, b"<p>model</p>")
            .unwrap();
        assert_eq!(
            blobs.cache_get(&blob, "reader", 1).unwrap(),
            b"<p>model</p>"
        );
        assert_eq!(blobs.cache_get(&blob, "reader", 2), None);
        assert_eq!(blobs.cache_get(&blob, "html", 1), None);

        // A damaged entry reads as a miss and is removed.
        let path = blobs.cache_path(&blobs.cache_id(&blob, "reader", 1));
        fs::write(&path, b"garbage").unwrap();
        assert_eq!(blobs.cache_get(&blob, "reader", 1), None);
        assert!(!path.exists());

        blobs.cache_put(&blob, "reader", 1, b"x").unwrap();
        blobs.cache_clear().unwrap();
        assert_eq!(blobs.cache_get(&blob, "reader", 1), None);
        assert!(tmp.path().join("mail/cache").is_dir());
    }
}
