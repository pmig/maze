//! [`EncryptedDirectory`]: the Tantivy `Directory` behind `mail/index/` (PLAN §4.17).

mod cache;
mod file;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tantivy::Directory;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    DirectoryLock, FileHandle, Lock, WatchCallback, WatchCallbackList, WatchHandle, WritePtr,
};
use vault::BlockCipher;
use vault::block::DEFAULT_BLOCK_SIZE;

pub use cache::{BlockCache, BlockCacheStats, DEFAULT_CACHE_BYTES};
use file::{EncryptedFile, EncryptedWriter, NAME_TAG_LEN, invalid_data, name_tag};

/// The file Tantivy watches for new commits.
const META_FILE: &str = "meta.json";

/// Tuning for an [`EncryptedDirectory`].
#[derive(Clone, Debug)]
pub struct DirectoryOptions {
    /// Plaintext bytes per sealed block for new files (existing files keep theirs).
    /// Defaults to 16 KiB; must lie within the vault's accepted range.
    pub block_size: u32,
    /// The decrypted block cache. Share one cache across directories to cap the total.
    pub cache: Arc<BlockCache>,
}

impl Default for DirectoryOptions {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            cache: Arc::new(BlockCache::new(DEFAULT_CACHE_BYTES)),
        }
    }
}

/// A Tantivy [`Directory`] whose files are all encrypted with the vault's `MZI1` block
/// format and the `index` subkey.
///
/// - **Segment files** are written through a buffered writer that seals every full
///   block and the final partial block (`last = true`) on `terminate`. Reads decrypt
///   only the blocks a byte range touches, through the shared [`BlockCache`]; there is
///   no mmap.
/// - **`meta.json` and `managed.json`** go through `atomic_write`: sealed whole with
///   `seal_file`, written to a temporary file and renamed into place.
/// - **Every file is bound to its name** by a tag inside the authenticated plaintext
///   (see the `file` module), so swapping two index files fails to open.
/// - **Locks** are OS advisory locks on empty lock files, like `MmapDirectory`, so a
///   crashed process never leaves a stale lock behind. Lock files hold no data.
/// - **`watch`** fires when this process writes `meta.json`. Maze has a single
///   writer per index, in-process, so changes by other processes are not observed.
///
/// Rolling back `meta.json` to an older copy (from a backup) is not detected; it only
/// makes the index older, and Maze rebuilds stale entries from the store.
#[derive(Clone)]
pub struct EncryptedDirectory {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    cipher: BlockCipher,
    block_size: u32,
    cache: Arc<BlockCache>,
    watchers: WatchCallbackList,
}

impl EncryptedDirectory {
    /// Opens (creating it if needed) the index directory at `root`.
    pub fn open(
        root: impl AsRef<Path>,
        cipher: BlockCipher,
        options: DirectoryOptions,
    ) -> io::Result<Self> {
        // Reject a bad block size now rather than on the first write.
        cipher
            .new_file_header(options.block_size)
            .map_err(io::Error::from)?;
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                cipher,
                block_size: options.block_size,
                cache: options.cache,
                watchers: WatchCallbackList::default(),
            }),
        })
    }

    /// The directory on disk.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The block cache this directory reads through.
    pub fn cache(&self) -> &Arc<BlockCache> {
        &self.inner.cache
    }

    /// The block size used for new files.
    pub fn block_size(&self) -> u32 {
        self.inner.block_size
    }

    /// Maps a Tantivy file name to a path under the root. Tantivy only uses plain
    /// relative names; anything that could escape the directory is refused.
    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        let mut components = path.components().peekable();
        if components.peek().is_none()
            || !components.all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "index file names must be plain relative paths",
            ));
        }
        Ok(self.inner.root.join(path))
    }

    fn read_atomic(&self, path: &Path) -> io::Result<Vec<u8>> {
        let full_path = self.resolve(path)?;
        let sealed = fs::read(full_path)?;
        let mut plaintext = self.inner.cipher.open_file(&sealed).map_err(invalid_data)?;
        if plaintext.get(..NAME_TAG_LEN) != Some(&name_tag(path)[..]) {
            return Err(invalid_data(
                "index file does not belong under this name (swapped or renamed file)",
            ));
        }
        plaintext.drain(..NAME_TAG_LEN);
        Ok(plaintext)
    }
}

impl fmt::Debug for EncryptedDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedDirectory")
            .field("root", &self.inner.root)
            .field("block_size", &self.inner.block_size)
            .finish_non_exhaustive()
    }
}

fn open_read_error(err: io::Error, path: &Path) -> OpenReadError {
    if err.kind() == io::ErrorKind::NotFound {
        OpenReadError::FileDoesNotExist(path.to_owned())
    } else {
        OpenReadError::wrap_io_error(err, path.to_owned())
    }
}

/// Holds an OS lock until dropped.
struct LockGuard {
    _file: File,
}

fn new_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl Directory for EncryptedDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let full_path = self
            .resolve(path)
            .map_err(|err| open_read_error(err, path))?;
        let file = File::open(full_path).map_err(|err| open_read_error(err, path))?;
        let handle = EncryptedFile::open(
            path,
            file,
            self.inner.cipher.clone(),
            Arc::clone(&self.inner.cache),
        )
        .map_err(|err| open_read_error(err, path))?;
        Ok(Arc::new(handle))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        let io_error = |err: io::Error| {
            if err.kind() == io::ErrorKind::NotFound {
                DeleteError::FileDoesNotExist(path.to_owned())
            } else {
                DeleteError::IoError {
                    io_error: Arc::new(err),
                    filepath: path.to_owned(),
                }
            }
        };
        let full_path = self.resolve(path).map_err(io_error)?;
        fs::remove_file(full_path).map_err(io_error)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        let full_path = self
            .resolve(path)
            .map_err(|err| open_read_error(err, path))?;
        full_path
            .try_exists()
            .map_err(|err| OpenReadError::wrap_io_error(err, path.to_owned()))
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let wrap = |err: io::Error| {
            if err.kind() == io::ErrorKind::AlreadyExists {
                OpenWriteError::FileAlreadyExists(path.to_owned())
            } else {
                OpenWriteError::wrap_io_error(err, path.to_owned())
            }
        };
        let full_path = self.resolve(path).map_err(wrap)?;
        let file = new_file_options().open(full_path).map_err(wrap)?;
        let writer =
            EncryptedWriter::new(path, file, self.inner.cipher.clone(), self.inner.block_size)
                .map_err(wrap)?;
        // As with MmapDirectory, the directory entry is durable only after
        // `sync_directory`.
        Ok(BufWriter::new(Box::new(writer)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.read_atomic(path)
            .map_err(|err| open_read_error(err, path))
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let full_path = self.resolve(path)?;
        let mut plaintext = Vec::with_capacity(NAME_TAG_LEN + data.len());
        plaintext.extend_from_slice(&name_tag(path));
        plaintext.extend_from_slice(data);
        let sealed = self
            .inner
            .cipher
            .seal_file(&plaintext, self.inner.block_size)
            .map_err(io::Error::from)?;
        let parent = full_path.parent().unwrap_or(&self.inner.root);
        let mut temp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(parent)?;
        temp.write_all(&sealed)?;
        temp.flush()?;
        temp.as_file().sync_data()?;
        temp.persist(&full_path).map_err(|err| err.error)?;
        if path == Path::new(META_FILE) {
            // The callbacks run on their own thread; nothing to wait for here.
            drop(self.inner.watchers.broadcast());
        }
        Ok(())
    }

    fn sync_directory(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            File::open(&self.inner.root)?.sync_all()?;
        }
        Ok(())
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        let full_path = self
            .resolve(&lock.filepath)
            .map_err(LockError::wrap_io_error)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(full_path)
            .map_err(LockError::wrap_io_error)?;
        if lock.is_blocking {
            file.lock().map_err(LockError::wrap_io_error)?;
        } else {
            file.try_lock().map_err(|err| match err {
                fs::TryLockError::WouldBlock => LockError::LockBusy,
                fs::TryLockError::Error(err) => LockError::wrap_io_error(err),
            })?;
        }
        Ok(DirectoryLock::from(Box::new(LockGuard { _file: file })))
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(self.inner.watchers.subscribe(watch_callback))
    }
}

#[cfg(test)]
mod tests;
