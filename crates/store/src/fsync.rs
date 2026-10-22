//! The three kinds of "sync" the blob store's write protocol needs (see [`crate::blob`]).
//!
//! - [`push`]: hand a file's dirty data and metadata to the storage device.
//! - [`order`]: make everything pushed so far reach stable storage before anything
//!   written afterwards.
//! - [`flush`]: make everything pushed so far durable before returning.
//!
//! On Apple platforms these are different system calls with very different costs
//! (measured on an Apple Silicon Mac, APFS, internal SSD: `fsync` 20–60 µs,
//! `F_BARRIERFSYNC` about 0.8 ms, `F_FULLFSYNC` 1.5–8 ms depending on queued writes):
//!
//! - `fsync(2)` "will flush all data from the host to the drive", but "the drive itself
//!   may not physically write the data [...] and it may be written in an out-of-order
//!   sequence" (`man 2 fsync`). So [`push`] is plain `fsync`.
//! - `F_FULLFSYNC` "does the same thing as fsync(2) then asks the drive to flush all
//!   buffered data to the permanent storage device. As this drains the entire queue of
//!   the device and acts as a barrier, data that had been fsync'd on the same device
//!   before is guaranteed to be persisted when this call returns" (`man 2 fcntl`). One
//!   call therefore covers every file and directory pushed before it, which is what
//!   lets a batch of blobs share it. [`flush`] and [`order`] both use it.
//! - `F_BARRIERFSYNC` would be enough for [`order`] ("a barrier can be used to order two
//!   phases of operations on a set of file descriptors [...] execute operations of
//!   phase one, then fsync(2) each FD and issue a single barrier"), but it "requires
//!   hardware support, which Apple SSDs are guaranteed to provide": on other drives
//!   (an external disk holding the data dir) the man page promises nothing. Saving
//!   about a millisecond per batch isn't worth a weaker guarantee there, so [`order`]
//!   is a full flush as well.
//!
//! Rust's `File::sync_all`/`sync_data` are `F_FULLFSYNC` on Apple platforms, which is
//! why the blob store doesn't use them per file. Like SQLite (`full_fsync` in
//! `os_unix.c`), a file system that rejects `F_FULLFSYNC` (network volumes) falls back
//! to `fsync`.
//!
//! Elsewhere `fsync(2)` already waits for stable storage (Linux issues a cache flush),
//! so [`push`] is `fsync`/`fdatasync` and [`order`] and [`flush`] have nothing left to
//! do.

use std::fs::File;
use std::io;

/// Hands `file`'s data and metadata to the device; a directory's entries included.
pub(crate) fn push(file: &File) -> io::Result<()> {
    imp::push(file)
}

/// Everything [`push`]ed before reaches stable storage before anything written after.
pub(crate) fn order(file: &File) -> io::Result<()> {
    imp::full(file)
}

/// Everything [`push`]ed before is on stable storage when this returns.
pub(crate) fn flush(file: &File) -> io::Result<()> {
    imp::full(file)
}

#[cfg(target_vendor = "apple")]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    pub(super) fn push(file: &File) -> io::Result<()> {
        // SAFETY: the descriptor is owned by `file` and open for the whole call; `fsync`
        // has no other preconditions.
        let result = unsafe { libc::fsync(file.as_raw_fd()) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn full(file: &File) -> io::Result<()> {
        // SAFETY: as above; `F_FULLFSYNC` takes no argument and only reads the
        // descriptor.
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if result != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        tracing::debug!(error = %err, "F_FULLFSYNC unsupported here; falling back to fsync");
        push(file)
    }
}

#[cfg(not(target_vendor = "apple"))]
mod imp {
    use std::fs::File;
    use std::io;

    pub(super) fn push(file: &File) -> io::Result<()> {
        file.sync_all()
    }

    pub(super) fn full(_file: &File) -> io::Result<()> {
        Ok(())
    }
}
