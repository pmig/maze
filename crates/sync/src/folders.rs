//! Folder list sync: `LIST` → local folders (create, update, rename, delete), roles
//! from SPECIAL-USE, subscriptions, and the Gmail mapping of ADR-007.

use std::collections::{HashMap, HashSet};

use imap::RemoteFolder;
use mazemail_types::{FolderId, FolderRole};
use store::{Folder, FolderPatch, NewFolder, OpState, Queries};

use crate::connect::Conn;
use crate::error::Result;
use crate::events::SyncEvent;
use crate::ops::OpPayload;
use crate::shared::Shared;

/// How the engine treats a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderKind {
    /// A mailbox that is synced (UIDs, flags, bodies).
    Mailbox,
    /// A Gmail label: its contents come from All Mail's `X-GM-LABELS`.
    Label,
    /// A `\Noselect` parent, shown for the hierarchy only.
    Placeholder,
}

/// A folder as the actor schedules it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FolderMeta {
    pub(crate) id: FolderId,
    /// The server name.
    pub(crate) remote: String,
    pub(crate) role: Option<FolderRole>,
    pub(crate) kind: FolderKind,
}

/// The result of a folder list sync.
#[derive(Debug, Clone)]
pub(crate) struct FolderList {
    pub(crate) folders: Vec<FolderMeta>,
    pub(crate) gmail: bool,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    delimiter: Option<char>,
    role: Option<FolderRole>,
    kind: FolderKind,
}

impl Entry {
    fn depth(&self) -> usize {
        self.delimiter
            .map_or(0, |d| self.name.chars().filter(|c| *c == d).count())
    }

    fn parent_name(&self) -> Option<&str> {
        let d = self.delimiter?;
        self.name.rsplit_once(d).map(|(parent, _)| parent)
    }

    fn display_name(&self) -> String {
        if self.name.eq_ignore_ascii_case("INBOX") {
            return "Inbox".to_owned();
        }
        match self.delimiter {
            Some(d) => self.name.rsplit(d).next().unwrap_or(&self.name).to_owned(),
            None => self.name.clone(),
        }
    }
}

/// A role from a well-known folder name, for servers without SPECIAL-USE.
fn guess_role(leaf: &str) -> Option<FolderRole> {
    Some(match leaf.to_lowercase().as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet" => FolderRole::Sent,
        "drafts" | "draft" | "entwürfe" => FolderRole::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" | "papierkorb" => FolderRole::Trash,
        "junk" | "spam" | "junk e-mail" | "junk email" | "bulk mail" => FolderRole::Junk,
        "archive" | "archives" | "archiv" => FolderRole::Archive,
        _ => return None,
    })
}

/// Turns the LIST result into the folders Maze keeps.
fn classify(remote: &[RemoteFolder], gmail: bool) -> Vec<Entry> {
    let is_inbox = |f: &RemoteFolder| f.name.eq_ignore_ascii_case("INBOX");
    let any_subscribed = remote.iter().any(|f| f.subscribed && !is_inbox(f));
    let mut entries: Vec<Entry> = remote
        .iter()
        .map(|f| {
            let role = if is_inbox(f) {
                Some(FolderRole::Inbox)
            } else {
                f.role
            };
            let kind = if !f.selectable {
                FolderKind::Placeholder
            } else if gmail {
                match role {
                    Some(FolderRole::All | FolderRole::Trash | FolderRole::Junk) => {
                        FolderKind::Mailbox
                    }
                    _ => FolderKind::Label,
                }
            } else {
                FolderKind::Mailbox
            };
            Entry {
                name: f.name.clone(),
                delimiter: f.delimiter,
                role,
                kind,
            }
        })
        .filter(|e| {
            e.kind == FolderKind::Placeholder || e.role.is_some() || gmail || {
                // Servers that don't track subscriptions report none: keep everything.
                let f = remote.iter().find(|f| f.name == e.name);
                !any_subscribed || f.is_some_and(|f| f.subscribed)
            }
        })
        .collect();

    // Name-based roles where the server has no SPECIAL-USE for that role.
    if !gmail {
        let taken: HashSet<FolderRole> = entries.iter().filter_map(|e| e.role).collect();
        let mut assigned = HashSet::new();
        for e in entries.iter_mut().filter(|e| e.role.is_none()) {
            if e.kind != FolderKind::Mailbox {
                continue;
            }
            if let Some(role) = guess_role(&e.display_name())
                && !taken.contains(&role)
                && e.depth() <= 1
                && assigned.insert(role)
            {
                e.role = Some(role);
            }
        }
    }

    // Placeholders only as ancestors of kept folders.
    let kept: Vec<(String, Option<char>)> = entries
        .iter()
        .filter(|e| e.kind != FolderKind::Placeholder)
        .map(|e| (e.name.clone(), e.delimiter))
        .collect();
    entries.retain(|e| {
        e.kind != FolderKind::Placeholder
            || kept.iter().any(|(name, delim)| {
                delim.is_some_and(|d| name.starts_with(&format!("{}{d}", e.name)))
            })
    });
    entries.sort_by(|a, b| a.depth().cmp(&b.depth()).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// Local folders that queued ops still need (an optimistic create or rename that
/// hasn't reached the server must not be deleted by a LIST that predates it).
async fn protected(shared: &Shared) -> Result<(HashSet<FolderId>, HashSet<String>)> {
    let account = shared.account();
    let ops = shared.read(move |r| r.pending_ops(account)).await?;
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for op in ops.iter().filter(|op| op.state == OpState::Queued) {
        if let Ok(payload) = OpPayload::from_pending(op) {
            ids.extend(payload.protected_folders());
            names.extend(payload.protected_names());
        }
    }
    Ok((ids, names))
}

struct Plan {
    entries: Vec<Entry>,
    /// Local folder per remote name (existing or renamed).
    matched: HashMap<String, Folder>,
    deletes: Vec<FolderId>,
}

/// Syncs the folder list of the account.
pub(crate) async fn sync_folder_list(shared: &Shared, conn: &mut Conn) -> Result<FolderList> {
    let gmail = conn.caps.gmail();
    let listed = conn.session.list().await;
    let remote = conn.track(listed)?;
    let entries = classify(&remote, gmail);
    let account = shared.account();
    let local = shared.read(move |r| r.folders(Some(account))).await?;
    let (protected_ids, protected_names) = protected(shared).await?;

    let wanted: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut matched: HashMap<String, Folder> = HashMap::new();
    let mut missing: Vec<Folder> = Vec::new();
    for folder in local {
        let Some(name) = folder.remote_name.clone() else {
            continue; // local-only folder
        };
        if wanted.contains(name.as_str()) {
            matched.insert(name, folder);
        } else if !protected_ids.contains(&folder.id)
            && !protected_names.contains(&name)
            // Label folders Maze created for labels hidden from IMAP.
            && !(folder.is_label && name.starts_with('\\'))
        {
            missing.push(folder);
        }
    }

    // Rename detection: a vanished mailbox and a new one with the same UIDVALIDITY.
    let mut by_validity: HashMap<u32, usize> = missing
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.is_label)
        .filter_map(|(i, f)| f.sync.uidvalidity.map(|v| (v, i)))
        .collect();
    let mut renamed: HashSet<usize> = HashSet::new();
    if !by_validity.is_empty() {
        let candidates: Vec<String> = entries
            .iter()
            .filter(|e| e.kind == FolderKind::Mailbox && !matched.contains_key(&e.name))
            .map(|e| e.name.clone())
            .collect();
        for name in candidates {
            if by_validity.is_empty() {
                break;
            }
            if conn.selected_folder() == Some(name.as_str()) {
                continue; // STATUS must not target the selected folder
            }
            let status = conn.session.status(&name).await;
            let Ok(status) = conn.track(status) else {
                if conn.broken {
                    return Err(crate::SyncError::Imap(imap::ImapError::Broken));
                }
                continue;
            };
            if let Some(index) = status.uidvalidity.and_then(|v| by_validity.remove(&v)) {
                renamed.insert(index);
                matched.insert(name.clone(), missing[index].clone());
            }
        }
    }
    let deletes: Vec<FolderId> = missing
        .iter()
        .enumerate()
        .filter(|(i, _)| !renamed.contains(i))
        .map(|(_, f)| f.id)
        .collect();

    let plan = Plan {
        entries,
        matched,
        deletes,
    };
    let (ids, changed, removed) = shared
        .write(plan.entries.len() + plan.deletes.len(), move |w| {
            apply_plan(w, account, plan)
        })
        .await?;
    if !changed.is_empty() {
        shared.events.emit(SyncEvent::FoldersChanged(changed));
    }
    if !removed.is_empty() {
        shared.events.emit(SyncEvent::FoldersRemoved(removed));
    }
    Ok(FolderList {
        folders: ids,
        gmail,
    })
}

type Applied = (Vec<FolderMeta>, Vec<FolderId>, Vec<FolderId>);

fn apply_plan(
    w: &mut store::WriteTx<'_>,
    account: mazemail_types::AccountId,
    plan: Plan,
) -> store::Result<Applied> {
    let mut ids: HashMap<String, FolderId> = HashMap::new();
    let mut metas = Vec::new();
    let mut changed = Vec::new();
    for e in &plan.entries {
        let parent = e.parent_name().and_then(|p| ids.get(p)).copied();
        let display = e.display_name();
        let id = if let Some(local) = plan.matched.get(&e.name) {
            let renamed = local.remote_name.as_deref() != Some(e.name.as_str());
            let patch = FolderPatch {
                parent: (local.parent != parent).then_some(parent),
                remote_name: renamed.then(|| Some(e.name.clone())),
                display_name: (renamed && local.display_name != display).then(|| display.clone()),
                role: e.role.filter(|r| local.role != Some(*r)).map(Some),
                ..FolderPatch::default()
            };
            if patch != FolderPatch::default() {
                w.update_folder(local.id, &patch)?;
                changed.push(local.id);
            }
            local.id
        } else {
            let mut folder = NewFolder::new(account, &e.name, &display);
            folder.parent = parent;
            folder.role = e.role;
            folder.is_label = e.kind == FolderKind::Label;
            let id = w.create_folder(&folder)?;
            changed.push(id);
            id
        };
        ids.insert(e.name.clone(), id);
        metas.push(FolderMeta {
            id,
            remote: e.name.clone(),
            role: e.role,
            kind: e.kind,
        });
    }
    let mut removed = Vec::new();
    for id in plan.deletes {
        match w.delete_folder(id) {
            Ok(()) => removed.push(id),
            // Already gone with a deleted parent.
            Err(store::Error::NotFound { .. }) => removed.push(id),
            Err(err) => return Err(err),
        }
    }
    Ok((metas, changed, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(
        name: &str,
        role: Option<FolderRole>,
        selectable: bool,
        subscribed: bool,
    ) -> RemoteFolder {
        RemoteFolder {
            name: name.to_owned(),
            raw_name: name.to_owned(),
            delimiter: Some('/'),
            attributes: Vec::new(),
            role,
            selectable,
            subscribed,
        }
    }

    #[test]
    fn gmail_keeps_three_mailboxes_and_labels() {
        let listed = vec![
            remote("INBOX", Some(FolderRole::Inbox), true, true),
            remote("[Gmail]", None, false, true),
            remote("[Gmail]/All Mail", Some(FolderRole::All), true, true),
            remote("[Gmail]/Trash", Some(FolderRole::Trash), true, true),
            remote("[Gmail]/Spam", Some(FolderRole::Junk), true, true),
            remote("[Gmail]/Sent Mail", Some(FolderRole::Sent), true, true),
            remote("Work", None, true, true),
        ];
        let entries = classify(&listed, true);
        let kind = |n: &str| entries.iter().find(|e| e.name == n).map(|e| e.kind);
        assert_eq!(kind("[Gmail]/All Mail"), Some(FolderKind::Mailbox));
        assert_eq!(kind("[Gmail]/Trash"), Some(FolderKind::Mailbox));
        assert_eq!(kind("[Gmail]/Spam"), Some(FolderKind::Mailbox));
        assert_eq!(kind("INBOX"), Some(FolderKind::Label));
        assert_eq!(kind("[Gmail]/Sent Mail"), Some(FolderKind::Label));
        assert_eq!(kind("Work"), Some(FolderKind::Label));
        assert_eq!(kind("[Gmail]"), Some(FolderKind::Placeholder));
        // Parents come first.
        assert_eq!(entries[0].depth(), 0);
    }

    #[test]
    fn subscriptions_roles_and_placeholders() {
        let listed = vec![
            remote("INBOX", None, true, false),
            remote("Sent Items", None, true, true),
            remote("Lists", None, false, false),
            remote("Lists/rust", None, true, true),
            remote("Old", None, false, false),
            remote("Hidden", None, true, false),
        ];
        let entries = classify(&listed, false);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["INBOX", "Lists", "Sent Items", "Lists/rust"]);
        assert_eq!(entries[0].role, Some(FolderRole::Inbox));
        assert_eq!(entries[0].display_name(), "Inbox");
        assert_eq!(entries[2].role, Some(FolderRole::Sent));
        assert_eq!(entries[3].parent_name(), Some("Lists"));
        assert_eq!(entries[3].display_name(), "rust");
    }

    #[test]
    fn no_subscriptions_means_everything() {
        let listed = vec![
            remote("INBOX", None, true, false),
            remote("Projects", None, true, false),
        ];
        assert_eq!(classify(&listed, false).len(), 2);
    }
}
