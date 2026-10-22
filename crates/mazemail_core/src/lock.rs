//! The single-instance lock on the data directory.
//!
//! An exclusive OS advisory lock (`flock` on Unix, `LockFileEx` on Windows, through
//! [`std::fs::File::try_lock`]) on `<data_dir>/mazemail.lock`. The kernel releases it
//! when the process exits, so a crash never leaves a stale lock behind, and the file
//! itself is left in place. The lock file holds the owner's process id for the
//! "already running" message; nothing reads it for correctness.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{Error, Result};

/// File name of the lock inside the data directory.
pub const LOCK_FILE: &str = "mazemail.lock";

/// Holds the data directory for this process until dropped.
#[derive(Debug)]
pub(crate) struct DataDirLock {
    file: File,
}

impl DataDirLock {
    /// Takes the lock, failing with [`Error::AlreadyRunning`] if another process (or
    /// another open handle in this process) holds it. Creates the data directory.
    pub(crate) fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir).map_err(Error::io("creating the data directory"))?;
        let path = data_dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(Error::io("opening the data directory lock"))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let pid = file
                    .read_to_string(&mut text)
                    .ok()
                    .and_then(|_| text.trim().parse().ok());
                return Err(Error::AlreadyRunning {
                    data_dir: data_dir.to_path_buf(),
                    pid,
                });
            }
            Err(TryLockError::Error(err)) => {
                return Err(Error::io("locking the data directory")(err));
            }
        }
        // Best effort: the pid is only informational.
        let _ = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| write!(file, "{}", std::process::id()))
            .and_then(|()| file.flush());
        tracing::debug!(path = %path.display(), "data directory locked");
        Ok(Self { file })
    }
}

impl Drop for DataDirLock {
    fn drop(&mut self) {
        // Closing the file releases the lock too; unlocking first makes the order
        // explicit for anyone waiting on it.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        assert!(dir.path().join(LOCK_FILE).exists());
        match DataDirLock::acquire(dir.path()) {
            Err(Error::AlreadyRunning { pid, data_dir }) => {
                assert_eq!(pid, Some(std::process::id()));
                assert_eq!(data_dir, dir.path());
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        drop(first);
        DataDirLock::acquire(dir.path()).unwrap();
    }
}
