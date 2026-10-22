//! Exporting stored mail as plaintext mbox or Maildir (PLAN §4.14 item 6, the "no
//! lock-in" promise of CONCEPT §1).
//!
//! The layout is Thunderbird's, so the output opens in Thunderbird and imports back
//! into Maze: a folder is an mbox file `<name>` (or a Maildir directory `<name>/`),
//! its subfolders live in `<name>.sbd/`. mbox files use mboxrd escaping and carry the
//! flags and tags in `X-Mozilla-Status`/`X-Mozilla-Keys`; Maildir uses `:2,` flags and
//! Dovecot keywords. Importing an export gives back byte-identical messages.
//!
//! The output is **not encrypted** (one of the documented plaintext exceptions,
//! PLAN §4.17); every report carries [`PLAINTEXT_WARNING`].

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use mazemail_types::{AccountId, FolderId};
use store::{Folder, FolderNode, MessageDetail, MessageScope, Queries, Store};

use crate::maildir::{MaildirWriter, create_dir_private, create_file_private};
use crate::mbox::{Envelope, MboxWriter};
use crate::mozilla::MozillaState;
use crate::progress::{Phase, Progress, Unit, report};
use crate::{Error, Result};

/// Shown with every export: the files are plaintext.
pub const PLAINTEXT_WARNING: &str = "The exported files are not encrypted. Anyone who can read \
them can read this mail; keep them somewhere safe and delete them when you no longer need them.";

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    Mbox,
    Maildir,
}

/// What to export. Without account and folder, everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportScope {
    /// Every folder of this account, under `<dir>/<account name>/`.
    pub account: Option<AccountId>,
    /// This folder and its subfolders, directly under `<dir>/`.
    pub folder: Option<FolderId>,
}

/// What an export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub folders: u64,
    pub messages: u64,
    /// Messages skipped because only their headers are stored.
    pub without_body: u64,
    /// Message bytes written.
    pub bytes: u64,
    /// The mbox files or Maildir directories written, one per folder.
    pub paths: Vec<PathBuf>,
    /// Always [`PLAINTEXT_WARNING`].
    pub warning: &'static str,
}

/// Exports mail from the store into `dir` (created if missing). Existing files are
/// never overwritten: a folder whose target already exists fails the export.
pub fn export(
    store: &Store,
    scope: &ExportScope,
    format: ExportFormat,
    dir: &Path,
    progress: &dyn Progress,
) -> Result<ExportReport> {
    create_dir_private(dir)?;
    let reader = store.reader()?;
    let mut roots: Vec<(PathBuf, FolderNode)> = Vec::new();
    if let Some(folder) = scope.folder {
        let folder_row = reader
            .folder(folder)?
            .ok_or_else(|| Error::InvalidTarget(format!("folder {folder} doesn't exist")))?;
        let tree = reader.folder_tree(Some(folder_row.account))?;
        let node = find_node(tree, folder)
            .ok_or_else(|| Error::InvalidTarget(format!("folder {folder} isn't in its account")))?;
        roots.push((dir.to_owned(), node));
    } else {
        let accounts = match scope.account {
            Some(id) => vec![
                reader
                    .account(id)?
                    .ok_or_else(|| Error::InvalidTarget(format!("account {id} doesn't exist")))?,
            ],
            None => reader.accounts()?,
        };
        for account in accounts {
            let base = dir.join(safe_name(&account.name));
            for node in reader.folder_tree(Some(account.id))? {
                roots.push((base.clone(), node));
            }
        }
    }
    drop(reader);

    let mut report_out = ExportReport {
        folders: 0,
        messages: 0,
        without_body: 0,
        bytes: 0,
        paths: Vec::new(),
        warning: PLAINTEXT_WARNING,
    };
    for (base, node) in roots {
        export_node(store, &node, &base, format, &mut report_out, progress)?;
    }
    Ok(report_out)
}

fn find_node(nodes: Vec<FolderNode>, id: FolderId) -> Option<FolderNode> {
    for node in nodes {
        if node.folder.id == id {
            return Some(node);
        }
        if let Some(found) = find_node(node.children, id) {
            return Some(found);
        }
    }
    None
}

fn export_node(
    store: &Store,
    node: &FolderNode,
    base: &Path,
    format: ExportFormat,
    out: &mut ExportReport,
    progress: &dyn Progress,
) -> Result<()> {
    create_dir_private(base)?;
    let name = safe_name(&node.folder.display_name);
    let target = base.join(&name);
    if !node.folder.is_label {
        export_folder(store, &node.folder, &target, format, out)?;
        out.paths.push(target);
        out.folders += 1;
        report(progress, Phase::Export, out.messages, 0, Unit::Messages);
    }
    if !node.children.is_empty() {
        let sub = base.join(format!("{name}.sbd"));
        for child in &node.children {
            export_node(store, child, &sub, format, out, progress)?;
        }
    }
    Ok(())
}

/// The messages of a folder, oldest first, with the location in that folder.
fn folder_messages(store: &Store, folder: FolderId) -> Result<Vec<MessageDetail>> {
    let reader = store.reader()?;
    let mut messages = Vec::new();
    for id in reader.message_ids_in_scope(&MessageScope::Folder(folder))? {
        if let Some(m) = reader.message(id)? {
            messages.push(m);
        }
    }
    messages.sort_by_key(|m| (m.date, m.id));
    Ok(messages)
}

fn export_folder(
    store: &Store,
    folder: &Folder,
    target: &Path,
    format: ExportFormat,
    out: &mut ExportReport,
) -> Result<()> {
    if target.exists() {
        return Err(Error::InvalidTarget(format!(
            "{} already exists; export into an empty directory",
            target.display()
        )));
    }
    let messages = folder_messages(store, folder.id)?;
    match format {
        ExportFormat::Mbox => {
            let file = create_file_private(target)?;
            let mut writer = MboxWriter::new(BufWriter::with_capacity(1 << 20, file));
            for m in &messages {
                let Some(raw) = body(store, m, out)? else {
                    continue;
                };
                let location = m.locations.iter().find(|l| l.folder == folder.id);
                let state = location.map(|l| MozillaState::from_flags(l.flags, &l.keywords));
                let envelope = Envelope {
                    sender: m.from.as_ref().map(|a| a.email.clone()),
                    date: Some(m.received.unwrap_or(m.date)),
                };
                writer
                    .write_message(&envelope, state.as_ref(), &raw)
                    .map_err(Error::io("writing", target))?;
                out.messages += 1;
                out.bytes += raw.len() as u64;
            }
            let file: BufWriter<File> = writer.finish().map_err(Error::io("writing", target))?;
            let file = file
                .into_inner()
                .map_err(|err| Error::io("writing", target)(err.into_error()))?;
            file.sync_all().map_err(Error::io("syncing", target))?;
        }
        ExportFormat::Maildir => {
            let mut writer = MaildirWriter::create(target)?;
            for m in &messages {
                let Some(raw) = body(store, m, out)? else {
                    continue;
                };
                let location = m.locations.iter().find(|l| l.folder == folder.id);
                let (flags, keywords) = location
                    .map(|l| (l.flags, l.keywords.clone()))
                    .unwrap_or_default();
                writer.write(&raw, flags, &keywords)?;
                out.messages += 1;
                out.bytes += raw.len() as u64;
            }
            writer.finish()?;
        }
    }
    Ok(())
}

/// The stored body, or `None` (counted) if only headers are stored.
fn body(store: &Store, m: &MessageDetail, out: &mut ExportReport) -> Result<Option<Vec<u8>>> {
    match m.blob {
        Some(blob) => match store.blobs().read(&blob) {
            Ok(raw) => Ok(Some(raw)),
            Err(store::Error::BlobMissing(_)) => {
                out.without_body += 1;
                Ok(None)
            }
            Err(err) => Err(err.into()),
        },
        None => {
            out.without_body += 1;
            Ok(None)
        }
    }
}

/// A file name for a folder or account name: no path separators, control characters
/// or leading dots.
pub(crate) fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim_end_matches('.');
    if cleaned.is_empty() {
        "_".to_owned()
    } else {
        cleaned.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("Inbox"), "Inbox");
        assert_eq!(safe_name("a/b\\c:d"), "a_b_c_d");
        assert_eq!(safe_name(".."), "_");
        assert_eq!(safe_name(" .hidden. "), "hidden");
        assert_eq!(safe_name("x\ny"), "x_y");
    }
}
