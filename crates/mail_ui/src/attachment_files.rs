//! Attachments as files: opened in another app, or saved where the user picks.
//!
//! Mail stays encrypted on disk (PLAN §4.17); an attachment you open is the documented
//! exception. [`write_for_opening`] puts it into a private folder ([`open_dir`]): on
//! Linux `$XDG_RUNTIME_DIR/mazemail/attachments` (a per-user tmpfs, gone at logout),
//! elsewhere `<temp dir>/mazemail-attachments`, which only the user can read (mode
//! 0700, and Maze refuses a folder that someone else owns or can read). Each file gets
//! a folder of its own, so it keeps its name, and is read-only: edits must be saved
//! elsewhere. [`remove_old`] deletes what is older than [`KEEP_FOR`] (at start) and
//! [`remove_all`] everything (on quit).
//!
//! File names come from the message: [`safe_file_name`] keeps only the last path
//! component, without control characters or a leading dot. [`is_risky`] spots programs
//! and scripts, which the UI only opens after asking.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use mazemail_types::TECHNICAL_NAME;

/// How long opened attachments are kept when Maze isn't quit cleanly.
pub const KEEP_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Extensions of programs and scripts: opening one could run it.
const RISKY_EXTENSIONS: &[&str] = &[
    "app",
    "applescript",
    "bat",
    "bin",
    "cmd",
    "com",
    "command",
    "cpl",
    "desktop",
    "dmg",
    "exe",
    "hta",
    "jar",
    "js",
    "jse",
    "lnk",
    "msi",
    "pif",
    "pkg",
    "ps1",
    "py",
    "rb",
    "reg",
    "run",
    "scr",
    "sh",
    "terminal",
    "tool",
    "url",
    "vb",
    "vbe",
    "vbs",
    "workflow",
    "ws",
    "wsf",
];

/// The private folder opened attachments are written to.
pub fn open_dir() -> PathBuf {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(runtime)
            .join(TECHNICAL_NAME)
            .join("attachments");
    }
    std::env::temp_dir().join(format!("{TECHNICAL_NAME}-attachments"))
}

/// `name` as a file name that stays inside its folder: the last path component, without
/// control characters, a leading dot or characters Windows rejects. "attachment" if
/// nothing is left.
pub fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            c if c.is_control() => '_',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    if trimmed.is_empty() {
        "attachment".to_owned()
    } else {
        trimmed.chars().take(200).collect()
    }
}

/// Whether opening `name` could run a program or script.
pub fn is_risky(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            RISKY_EXTENSIONS
                .iter()
                .any(|risky| risky.eq_ignore_ascii_case(ext))
        })
}

/// Writes `bytes` as `name` (made safe) into a new folder in [`open_dir`], read-only,
/// and returns the file's path.
pub fn write_for_opening(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    write_in(&open_dir(), name, bytes)
}

fn write_in(root: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    ensure_private_dir(root)?;
    let folder = unique_folder(root)?;
    let file = folder.join(safe_file_name(name));
    fs::write(&file, bytes)?;
    let mut permissions = fs::metadata(&file)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&file, permissions)?;
    Ok(file)
}

/// Writes `bytes` to `path` (a place the user picked in the save dialog).
pub fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

/// Creates `dir` for the user only, or checks that an existing one is.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let meta = fs::symlink_metadata(dir)?;
        let mine = meta.uid() == rustix::process::getuid().as_raw();
        if !meta.is_dir() || !mine || meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the attachments folder isn't private",
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// A new, empty folder in `root` with a random name.
fn unique_folder(root: &Path) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let name = format!(
            "{:016x}{:x}",
            u64::from_le_bytes(random),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let folder = root.join(name);
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("no free folder name"))
}

/// Deletes opened attachments older than `max_age`.
pub fn remove_old(max_age: Duration) {
    remove_in(&open_dir(), Some(max_age));
}

/// Deletes every opened attachment.
pub fn remove_all() {
    remove_in(&open_dir(), None);
}

fn remove_in(root: &Path, max_age: Option<Duration>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let old_enough = max_age.is_none_or(|max_age| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age)
        });
        if old_enough && let Err(error) = remove_folder(&path) {
            tracing::debug!(%error, "cannot remove an opened attachment");
        }
    }
}

/// Removes a folder with its read-only files.
fn remove_folder(path: &Path) -> io::Result<()> {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                let mut permissions = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(entry.path(), permissions);
            }
        }
    }
    fs::remove_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside_their_folder() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Users\\x\\a.doc"), "a.doc");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("a\u{0}b\nc.txt"), "a_b_c.txt");
        assert_eq!(safe_file_name("   "), "attachment");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name("what?.txt"), "what_.txt");
    }

    #[test]
    fn programs_and_scripts_are_risky() {
        assert!(is_risky("setup.EXE"));
        assert!(is_risky("run.sh"));
        assert!(is_risky("Invoice.pdf.app"));
        assert!(!is_risky("invoice.pdf"));
        assert!(!is_risky("noextension"));
    }

    #[test]
    fn opened_files_are_private_read_only_and_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("attachments");
        let first = write_in(&root, "../r.pdf", b"%PDF-").unwrap();
        let second = write_in(&root, "r.pdf", b"other").unwrap();
        assert_eq!(first.file_name().unwrap(), "r.pdf");
        assert!(first.starts_with(&root));
        assert_ne!(first, second, "each file gets a folder of its own");
        assert_eq!(fs::read(&first).unwrap(), b"%PDF-");
        assert!(fs::metadata(&first).unwrap().permissions().readonly());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&root).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        // Nothing is old yet; then everything goes.
        remove_in(&root, Some(KEEP_FOR));
        assert!(first.exists());
        remove_in(&root, None);
        assert!(!first.exists() && !second.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("attachments");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write_in(&root, "r.pdf", b"x").is_err());
    }
}
