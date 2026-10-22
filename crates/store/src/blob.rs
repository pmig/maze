//! The encrypted, content-addressed blob store and its garbage collector (PLAN §4.3).
//!
//! # Write path
//!
//! [`BlobStore::stage`] computes the keyed blob id, compresses with zstd (level 3),
//! seals with the vault's `MZB1` format (the blob id is the associated data), writes the
//! result to `mail/tmp/` and hands it to the device with a plain `fsync` (cheap; see
//! the private `fsync` module). It returns a [`StagedBlob`] that is not durable yet.
//! [`BlobStore::persist`] then makes any number of staged blobs durable at once:
//!
//! 1. **Order**: one full flush (`F_FULLFSYNC` on Apple platforms), so the contents of
//!    every staged file are on stable storage before any of them gets its final name.
//! 2. **Install**: rename each file to `mail/blobs/<2 hex>/<2 hex>/<id>.mzb` (creating
//!    the shard directories), then `fsync` every directory that changed.
//! 3. **Flush**: one more full flush, after which the renames are durable too.
//!
//! So a batch costs two device flushes plus a cheap `fsync` per file and directory,
//! instead of two device flushes per blob. The `blob` row is inserted (or its reference
//! count bumped by a trigger) in the writer transaction that references the blob, for
//! example [`WriteTx::set_message_body`](crate::WriteTx::set_message_body).
//!
//! # Crash safety
//!
//! The invariant: **a committed row never points at a blob that isn't durably on
//! disk.** The writer enforces it itself: before it commits a transaction it calls
//! [`BlobStore::persist`] on every blob the transaction referenced, and if that fails,
//! it rolls the whole transaction back ([`Error::BlobsNotDurable`]). Callers can persist
//! earlier (the ingest pipeline does, on its own thread, so the writer finds nothing
//! left to do), but never have to.
//!
//! A crash at any point therefore leaves at most files that no row references:
//!
//! - before step 1: complete or torn files in `tmp/`;
//! - between steps 1 and 3: complete files in `tmp/` and complete files at their final
//!   names whose renames may or may not survive;
//! - after step 3 but before the commit: complete orphan files.
//!
//! A file only ever gets its final name after its contents are on stable storage, so
//! whatever is at a final name after a crash is complete. Garbage collection removes
//! the leftovers ([`GcOptions::full`]). Staging a blob whose final file already exists
//! skips the write but still syncs the file's directory and flushes in step 3, because
//! the rename that created it (by another thread, or before a crash of Maze) may not be
//! durable yet.
//!
//! Plain [`BlobStore::put`] stages and persists one blob; [`BlobStore::put_many`] does
//! a batch.
//!
//! # Why garbage collection can't delete a blob that is being referenced
//!
//! A [`StagedBlob`] *pins* its id in memory from before the file is checked or written
//! until the writer transaction that references it has committed (or the staged blob is
//! dropped unused). The collector runs on the writer thread, so it never interleaves
//! with a write transaction, and it checks pins under the same lock it holds while
//! deleting rows and unlinking files. So for every id either the collector sees the pin
//! and skips it, or [`BlobStore::stage`] runs after the file is gone and writes it
//! again. Staging files are named after the full blob id, so the collector skips those
//! of pinned blobs too.
//!
//! - Rows with `refs = 0` are deleted, then their files.
//! - Files without a row are deleted only if older than 24 hours (written, but the
//!   referencing transaction never committed, for example after a crash).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use mazemail_types::BlobId;
use rusqlite::{Connection, OptionalExtension, params};
use vault::{BlobCipher, BlobIdHasher, OpenReader};

use crate::fsync;
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

/// Where a staged blob is on its way to being durable (shared by its clones).
#[derive(Debug)]
enum Stage {
    /// Written to this file in `tmp/` and pushed to the device, not flushed.
    Tmp(PathBuf),
    /// At its final path; the directory entry may not be durable yet.
    Installed,
    /// On stable storage under its final path.
    Durable,
}

/// A blob written by [`BlobStore::stage`] (or one of the `put` methods) that no
/// committed row references yet.
///
/// Hand it to a writer method that references it; the store keeps it pinned until that
/// transaction has committed, and makes it durable before committing if
/// [`BlobStore::persist`] hasn't already. Dropping it unused leaves the file for the
/// orphan rule. Clones share the durability state.
#[derive(Debug, Clone)]
pub struct StagedBlob {
    id: BlobId,
    size: u64,
    _pin: BlobPin,
    stage: Arc<Mutex<Stage>>,
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

    /// Whether the blob is on stable storage under its final path (after
    /// [`BlobStore::persist`]). Only then can [`BlobStore::open`] read it.
    pub fn is_durable(&self) -> bool {
        matches!(*self.stage(), Stage::Durable)
    }

    fn stage(&self) -> MutexGuard<'_, Stage> {
        self.stage.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A step of [`BlobStore::persist`], for crash tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Staged files are flushed; nothing is renamed yet.
    Ordered,
    /// One more file got its final name.
    Renamed,
    /// Every directory that changed is synced; the final flush is next.
    DirsSynced,
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
    /// Serializes [`BlobStore::persist`] calls (they share the device flushes anyway),
    /// so a staged blob's state only ever changes under this lock.
    persist_lock: Mutex<()>,
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
                persist_lock: Mutex::new(()),
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

    /// Writes `content` (compressed, encrypted) to `mail/tmp/` without waiting for the
    /// device, unless the blob's file already exists, and pins it until it is
    /// referenced. Cheap enough to call per message from many threads; the expensive
    /// part is shared by a whole batch in [`BlobStore::persist`] (see the module docs).
    ///
    /// The blob isn't readable through [`BlobStore::open`] until it is persisted.
    pub fn stage(&self, content: &[u8]) -> Result<StagedBlob> {
        let id = self.id_for(content);
        // Pin before looking at the file, so the collector can't remove it in between.
        let pin = BlobPin::new(&self.inner.pins, id);
        let stage = if self.path(&id).exists() {
            Stage::Installed
        } else {
            let (tmp, file) = self.create_tmp(&id)?;
            if let Err(err) = self.write_sealed(file, &id, content, true) {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
            Stage::Tmp(tmp)
        };
        Ok(StagedBlob {
            id,
            size: content.len() as u64,
            _pin: pin,
            stage: Arc::new(Mutex::new(stage)),
        })
    }

    /// Stores `content` durably ([`BlobStore::stage`], then [`BlobStore::persist`]) and
    /// pins it until it is referenced. For many blobs, [`BlobStore::put_many`] is much
    /// cheaper.
    pub fn put(&self, content: &[u8]) -> Result<StagedBlob> {
        let staged = self.stage(content)?;
        self.persist([&staged])?;
        Ok(staged)
    }

    /// Stores several blobs durably, sharing one round of device flushes.
    pub fn put_many<'a>(
        &self,
        contents: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Vec<StagedBlob>> {
        let staged = contents
            .into_iter()
            .map(|content| self.stage(content))
            .collect::<Result<Vec<_>>>()?;
        self.persist(&staged)?;
        Ok(staged)
    }

    /// Makes staged blobs durable under their final paths, with two device flushes for
    /// the whole batch (see the module docs). Blobs that already are durable cost
    /// nothing. The writer calls this before every commit that references blobs; call it
    /// yourself to keep that work off the writer thread.
    ///
    /// On error some blobs may be durable and others not; calling it again retries the
    /// rest.
    pub fn persist<'a>(&self, blobs: impl IntoIterator<Item = &'a StagedBlob>) -> Result<()> {
        self.persist_with(blobs, |_| Ok(()))
    }

    /// [`BlobStore::persist`], calling `checkpoint` after each step (tests stop there to
    /// simulate a crash).
    fn persist_with<'a>(
        &self,
        blobs: impl IntoIterator<Item = &'a StagedBlob>,
        mut checkpoint: impl FnMut(Step) -> Result<()>,
    ) -> Result<()> {
        let _serial = self
            .inner
            .persist_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Every state that isn't durable yet, once (clones share their state).
        let mut seen = HashSet::new();
        let mut to_install: Vec<(&StagedBlob, PathBuf)> = Vec::new();
        let mut installed: Vec<&StagedBlob> = Vec::new();
        for blob in blobs {
            if !seen.insert(Arc::as_ptr(&blob.stage)) {
                continue;
            }
            match &*blob.stage() {
                Stage::Tmp(tmp) => to_install.push((blob, tmp.clone())),
                Stage::Installed => installed.push(blob),
                Stage::Durable => {}
            }
        }
        if to_install.is_empty() && installed.is_empty() {
            return Ok(());
        }

        let blobs_dir = self.inner.layout.blobs_dir();
        let mut dirs = BTreeSet::new();
        if !to_install.is_empty() {
            // 1. The staged contents reach stable storage before any final name does.
            let tmp_dir = open_dir(&self.inner.layout.tmp_dir())?;
            fsync::order(&tmp_dir).map_err(Error::io("flushing staged blobs"))?;
            checkpoint(Step::Ordered)?;

            // 2. Final names.
            for (blob, tmp) in &to_install {
                let dest = self.path(&blob.id);
                let shard = dest
                    .parent()
                    .ok_or(Error::Invalid("blob path without a parent"))?;
                create_shard_dirs(&blobs_dir, shard, &mut dirs)?;
                fs::rename(tmp, &dest).map_err(Error::io("installing a blob"))?;
                *blob.stage() = Stage::Installed;
                dirs.insert(shard.to_owned());
                checkpoint(Step::Renamed)?;
            }
        }
        for blob in &installed {
            if let Some(shard) = self.path(&blob.id).parent() {
                dirs.insert(shard.to_owned());
            }
        }
        for dir in &dirs {
            fsync::push(&open_dir(dir)?).map_err(Error::io("syncing a blob directory"))?;
        }
        checkpoint(Step::DirsSynced)?;

        // 3. The renames are durable.
        fsync::flush(&open_dir(&blobs_dir)?).map_err(Error::io("flushing blobs"))?;
        for (blob, _) in &to_install {
            *blob.stage() = Stage::Durable;
        }
        for blob in &installed {
            *blob.stage() = Stage::Durable;
        }
        Ok(())
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

    /// A new staging file `tmp/<id hex>-<pid>-<n>.tmp`. A name can be taken by a
    /// leftover of an earlier process with the same pid (after a crash); then the next
    /// number is tried.
    fn create_tmp(&self, id: &BlobId) -> Result<(PathBuf, File)> {
        let hex = id.to_hex();
        let pid = std::process::id();
        loop {
            let n = self.inner.tmp_counter.fetch_add(1, Ordering::Relaxed);
            let path = self
                .inner
                .layout
                .tmp_dir()
                .join(format!("{hex}-{pid}-{n}.tmp"));
            match create_private_file(&path) {
                Ok(file) => return Ok((path, file)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(Error::io("creating a blob file")(err)),
            }
        }
    }

    /// zstd → MZB1 seal → `file`, optionally pushed to the device.
    fn write_sealed(&self, file: File, ad: &BlobId, content: &[u8], sync: bool) -> Result<()> {
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
            fsync::push(&file).map_err(Error::io("syncing a blob"))?;
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
        let (tmp, file) = self.create_tmp(&cache_id)?;
        let result = self
            .write_sealed(file, &cache_id, content, false)
            .and_then(|()| install(&tmp, &self.cache_path(&cache_id)));
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
            if !older_than(&path, tmp_cutoff) {
                continue;
            }
            // A staged blob waiting for its commit keeps its file, however old.
            let pins = lock(&self.inner.pins);
            let pinned = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.get(..64))
                .and_then(BlobId::from_hex)
                .is_some_and(|id| pins.contains_key(&id));
            if !pinned && remove_file_if_exists(&path)? {
                report.tmp_deleted += 1;
            }
            drop(pins);
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

/// Renames `tmp` to `dest` (a cache entry), creating the shard directories.
fn install(tmp: &Path, dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or(Error::Invalid("cache path without a parent"))?;
    fs::create_dir_all(dir).map_err(Error::io("creating a cache directory"))?;
    fs::rename(tmp, dest).map_err(Error::io("installing a cache entry"))?;
    Ok(())
}

/// Creates the missing directories between `root` (which exists) and `shard`, and
/// records the parent of each one created: its new entry needs syncing.
fn create_shard_dirs(root: &Path, shard: &Path, changed: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut dir = shard;
    while dir != root && !dir.is_dir() {
        missing.push(dir);
        dir = dir
            .parent()
            .ok_or(Error::Invalid("blob directory outside the blob store"))?;
    }
    for dir in missing.into_iter().rev() {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Error::io("creating a blob directory")(err)),
        }
        if let Some(parent) = dir.parent() {
            changed.insert(parent.to_owned());
        }
    }
    Ok(())
}

fn open_dir(dir: &Path) -> Result<File> {
    File::open(dir).map_err(Error::io("opening a blob directory"))
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
    fn staged_blobs_become_readable_when_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = store(tmp.path());
        let contents: Vec<Vec<u8>> = (0..5).map(|i| format!("message {i}").into()).collect();
        let staged: Vec<_> = contents.iter().map(|c| blobs.stage(c).unwrap()).collect();
        for blob in &staged {
            assert!(!blob.is_durable());
            assert!(blobs.pinned(&blob.id()));
            assert!(matches!(blobs.open(&blob.id()), Err(Error::BlobMissing(_))));
        }
        // Clones share their state; a blob listed twice is installed once.
        let twice: Vec<_> = staged.iter().chain(staged.iter()).cloned().collect();
        blobs.persist(&twice).unwrap();
        for (blob, content) in staged.iter().zip(&contents) {
            assert!(blob.is_durable());
            assert_eq!(&blobs.read(&blob.id()).unwrap(), content);
        }
        assert_eq!(
            fs::read_dir(tmp.path().join("mail/tmp")).unwrap().count(),
            0
        );
        // Persisting again is free; staging existing content doesn't rewrite it.
        blobs.persist(&staged).unwrap();
        let again = blobs.stage(&contents[0]).unwrap();
        assert!(!again.is_durable());
        assert_eq!(
            fs::read_dir(tmp.path().join("mail/tmp")).unwrap().count(),
            0
        );
        blobs.persist([&again]).unwrap();
        assert!(again.is_durable());

        let many = blobs.put_many([b"a".as_slice(), b"b".as_slice()]).unwrap();
        assert!(many.iter().all(StagedBlob::is_durable));
        assert_eq!(blobs.read(&many[1].id()).unwrap(), b"b");
    }

    /// A minimal `blob` table for running the collector without a store.
    fn blob_table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE blob (id BLOB PRIMARY KEY, size INTEGER NOT NULL,
               created INTEGER NOT NULL, refs INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;",
        )
        .unwrap();
        conn
    }

    /// Simulates a crash after each step of `persist` (the process dies: nothing in
    /// memory survives, files stay as they are), then a restart. Whatever is at a final
    /// path must be complete, staging the same content again must work, and garbage
    /// collection must remove the leftovers.
    #[test]
    fn a_crash_between_persist_steps_leaves_only_complete_orphans() {
        let crash_points = [
            None,
            Some((Step::Ordered, 1)),
            Some((Step::Renamed, 1)),
            Some((Step::Renamed, 3)),
            Some((Step::DirsSynced, 1)),
        ];
        let contents: Vec<Vec<u8>> = (0..3)
            .map(|i| format!("Subject: {i}\r\n\r\n{}", "body ".repeat(500)).into())
            .collect();
        for crash in crash_points {
            let tmp = tempfile::tempdir().unwrap();
            {
                let blobs = store(tmp.path());
                let staged: Vec<_> = contents.iter().map(|c| blobs.stage(c).unwrap()).collect();
                if let Some((step, nth)) = crash {
                    let mut seen = 0;
                    let result = blobs.persist_with(&staged, |at| {
                        if at == step {
                            seen += 1;
                            if seen == nth {
                                return Err(Error::Invalid("simulated crash"));
                            }
                        }
                        Ok(())
                    });
                    assert!(result.is_err(), "{crash:?}");
                }
                assert!(staged.iter().all(|b| !b.is_durable()), "{crash:?}");
                // The process dies here: no commit, no cleanup.
            }

            let blobs = store(tmp.path());
            // Every file at a final name is complete.
            for content in &contents {
                let id = blobs.id_for(content);
                if blobs.exists(&id) {
                    assert_eq!(&blobs.read(&id).unwrap(), content, "{crash:?}");
                }
            }
            // A new attempt makes everything durable and readable.
            let staged = blobs.put_many(contents.iter().map(Vec::as_slice)).unwrap();
            for (blob, content) in staged.iter().zip(&contents) {
                assert_eq!(&blobs.read(&blob.id()).unwrap(), content, "{crash:?}");
            }
            drop(staged);
            // Nothing references them: the collector removes the files and the staging
            // leftovers once they are old enough.
            let conn = blob_table();
            let mut conn = conn;
            let report = blobs
                .collect(
                    &mut conn,
                    &GcOptions {
                        orphans: true,
                        orphan_min_age: Duration::ZERO,
                        now: SystemTime::now() + Duration::from_secs(2 * 60 * 60),
                    },
                )
                .unwrap();
            assert_eq!(report.orphans_deleted, contents.len(), "{crash:?}");
            let mut left = Vec::new();
            walk_files(tmp.path(), &mut left).unwrap();
            assert_eq!(left, Vec::<PathBuf>::new(), "{crash:?}");
        }
    }

    #[test]
    fn staging_files_of_pinned_blobs_survive_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = store(tmp.path());
        let staged = blobs.stage(b"waiting for a long commit").unwrap();
        let mut conn = blob_table();
        let options = GcOptions {
            orphans: true,
            orphan_min_age: Duration::ZERO,
            now: SystemTime::now() + Duration::from_secs(2 * 60 * 60),
        };
        assert_eq!(blobs.collect(&mut conn, &options).unwrap().tmp_deleted, 0);
        blobs.persist([&staged]).unwrap();
        assert!(staged.is_durable());
        drop(staged);
        assert_eq!(
            blobs.collect(&mut conn, &options).unwrap().orphans_deleted,
            1
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
