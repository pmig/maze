//! Folder trees of Thunderbird-style mail directories (`Mail/Local Folders`,
//! `ImapMail/<host>`, exports): a folder is an mbox file `<name>` or a Maildir
//! directory `<name>/` (with `cur/`), its subfolders are in `<name>.sbd/`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::maildir::is_maildir;
use crate::{Error, Result};

/// How a folder stores its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreFormat {
    /// One mbox file per folder (Thunderbird's default, "berkeleystore").
    Mbox,
    /// One file per message ("maildirstore").
    Maildir,
}

/// A folder found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    /// The folder name (the file or directory name).
    pub name: String,
    /// The mbox file or Maildir directory; for a folder that only holds subfolders,
    /// the `.sbd` directory.
    pub path: PathBuf,
    /// `None` if the folder has no message store of its own.
    pub format: Option<StoreFormat>,
    /// Size of the store in bytes (mbox file length, or the sum of Maildir files).
    pub bytes: u64,
    pub children: Vec<MailFolder>,
}

impl MailFolder {
    /// This folder and all below it, depth first, with their path of names.
    pub fn flatten(&self) -> Vec<(Vec<String>, &MailFolder)> {
        let mut out = Vec::new();
        self.flatten_into(&mut Vec::new(), &mut out);
        out
    }

    fn flatten_into<'a>(
        &'a self,
        prefix: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, &'a MailFolder)>,
    ) {
        prefix.push(self.name.clone());
        out.push((prefix.clone(), self));
        for child in &self.children {
            child.flatten_into(prefix, out);
        }
        prefix.pop();
    }
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

/// Scans a mail directory into its top-level folders, sorted by name.
pub fn scan(dir: &Path) -> Result<Vec<MailFolder>> {
    let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(Error::io("listing", dir))? {
        let entry = entry.map_err(Error::io("listing", dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry
            .file_type()
            .map_err(Error::io("reading", entry.path()))?
            .is_dir();
        entries.push((name, entry.path(), is_dir));
    }
    entries.sort();

    let mut folders: Vec<MailFolder> = Vec::new();
    for (name, path, is_dir) in &entries {
        if *is_dir {
            if name.ends_with(".sbd") || name.starts_with('.') {
                continue;
            }
            if is_maildir(path) {
                folders.push(MailFolder {
                    name: name.clone(),
                    bytes: maildir_bytes(path),
                    path: path.clone(),
                    format: Some(StoreFormat::Maildir),
                    children: Vec::new(),
                });
            }
        } else if is_folder_file_name(name) && looks_like_mbox(path) {
            folders.push(MailFolder {
                name: name.clone(),
                bytes: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                path: path.clone(),
                format: Some(StoreFormat::Mbox),
                children: Vec::new(),
            });
        }
    }
    // Subfolders: `<name>.sbd/`, also for folders whose own store is missing.
    for (name, path, is_dir) in &entries {
        let Some(parent) = name.strip_suffix(".sbd").filter(|_| *is_dir) else {
            continue;
        };
        let children = scan(path)?;
        match folders.iter_mut().find(|f| f.name == parent) {
            Some(folder) => folder.children = children,
            None => folders.push(MailFolder {
                name: parent.to_owned(),
                path: path.clone(),
                format: None,
                bytes: 0,
                children,
            }),
        }
    }
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(folders)
}

fn maildir_bytes(dir: &Path) -> u64 {
    ["cur", "new"]
        .iter()
        .filter_map(|sub| fs::read_dir(dir.join(sub)).ok())
        .flatten()
        .filter_map(|e| e.ok()?.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_mbox_and_maildir_trees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Inbox"), b"From - x\n").unwrap();
        fs::write(root.join("Inbox.msf"), b"// mork").unwrap();
        fs::write(root.join("Trash"), b"").unwrap();
        fs::write(root.join("msgFilterRules.dat"), b"version=\"9\"").unwrap();
        fs::write(root.join("notes"), b"not an mbox").unwrap();
        fs::create_dir_all(root.join("Inbox.sbd")).unwrap();
        fs::write(root.join("Inbox.sbd").join("Projects"), b"From - y\n").unwrap();
        fs::create_dir_all(root.join("Archive.sbd").join("2024").join("cur")).unwrap();
        fs::write(
            root.join("Archive.sbd")
                .join("2024")
                .join("cur")
                .join("1:2,S"),
            b"Subject: x\n\n",
        )
        .unwrap();

        let folders = scan(root).unwrap();
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Archive", "Inbox", "Trash"]);
        assert_eq!(folders[0].format, None);
        assert_eq!(folders[0].children[0].format, Some(StoreFormat::Maildir));
        assert_eq!(folders[0].children[0].bytes, 12);
        assert_eq!(folders[1].format, Some(StoreFormat::Mbox));
        assert_eq!(folders[1].children[0].name, "Projects");
        let flat: Vec<Vec<String>> = folders[1].flatten().into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            flat,
            vec![
                vec!["Inbox".to_owned()],
                vec!["Inbox".to_owned(), "Projects".to_owned()]
            ]
        );
    }
}
