//! Atomic exchange of two directory entries.
//!
//! This is the only module of the crate with `unsafe` code: two FFI calls into libc.

use std::io;
use std::path::Path;

/// Exchanges `a` and `b` in one step: afterwards `a` has what was at `b` and the other
/// way round. Both must exist and be on the same file system. There is no moment in
/// which either path is missing or half-written.
///
/// macOS: `renamex_np(RENAME_SWAP)` (APFS). Linux: `renameat2(RENAME_EXCHANGE)`.
pub fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    imp::exchange(a, b)
}

#[cfg(unix)]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn exchange(a: &Path, b: &Path) -> io::Result<()> {
        let a = super::c_path(a)?;
        let b = super::c_path(b)?;
        // SAFETY: `a` and `b` are valid NUL-terminated C strings that outlive the call;
        // `renamex_np` only reads them and has no other preconditions.
        let result = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn exchange(a: &Path, b: &Path) -> io::Result<()> {
        let a = super::c_path(a)?;
        let b = super::c_path(b)?;
        // SAFETY: `a` and `b` are valid NUL-terminated C strings that outlive the call;
        // `renameat2` (called through `syscall`, so glibc older than 2.28 and musl work
        // too) only reads them. `AT_FDCWD` resolves relative paths against the working
        // directory, and the arguments match the syscall's signature.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn exchange(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic exchange is not supported on this platform",
        ))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn exchanges_directories() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Maze.app");
        let b = dir.path().join(".mazemail-update-1.0.0.app");
        fs::create_dir_all(a.join("Contents")).unwrap();
        fs::write(a.join("Contents/version"), "old").unwrap();
        fs::create_dir_all(b.join("Contents")).unwrap();
        fs::write(b.join("Contents/version"), "new").unwrap();

        exchange(&b, &a).unwrap();
        assert_eq!(
            fs::read_to_string(a.join("Contents/version")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(b.join("Contents/version")).unwrap(),
            "old"
        );
    }

    #[test]
    fn fails_if_one_side_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir(&a).unwrap();
        let err = exchange(&a, &dir.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(a.exists());
    }
}
