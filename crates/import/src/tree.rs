//! Thunderbird-style mail directories (`ImapMail/<host>`, exports): a folder is an mbox
//! file `<name>` or a Maildir directory `<name>/` (with `cur/`), its subfolders are in
//! `<name>.sbd/`.

use std::fs;
use std::io::Read;
use std::path::Path;

/// How a folder stores its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreFormat {
    /// One mbox file per folder (Thunderbird's default, "berkeleystore").
    Mbox,
    /// One file per message ("maildirstore").
    Maildir,
}

/// Files Thunderbird keeps next to folders that aren't folders.
const NOT_FOLDERS: &[&str] = &[
    "msf",
    "dat",
    "json",
    "html",
    "htm",
    "sqlite",
    "sqlite-wal",
    "sqlite-shm",
    "log",
    "bak",
    "tmp",
    "mab",
    "js",
    "txt",
    "lock",
    "mozmsgs",
    "sbd",
];

pub(crate) fn is_folder_file_name(name: &str) -> bool {
    if name.starts_with('.') || name.is_empty() {
        return false;
    }
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(ext) => !NOT_FOLDERS.contains(&ext.to_ascii_lowercase().as_str()),
        None => true,
    }
}

/// Whether the file is empty or starts with an mbox envelope line.
pub fn looks_like_mbox(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 5];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return false,
        }
    }
    read == 0 || &head[..read] == b"From " || (read < 5 && b"From "[..read] == head[..read])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_folders_from_thunderbirds_other_files() {
        assert!(is_folder_file_name("Inbox"));
        assert!(is_folder_file_name("Archive 2024"));
        assert!(!is_folder_file_name("Inbox.msf"));
        assert!(!is_folder_file_name("msgFilterRules.dat"));
        assert!(!is_folder_file_name(".hidden"));
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, content: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, content).unwrap();
            path
        };
        assert!(looks_like_mbox(&file("Inbox", b"From - x\n")));
        assert!(looks_like_mbox(&file("Trash", b"")));
        assert!(!looks_like_mbox(&file("notes", b"not an mbox")));
    }
}
