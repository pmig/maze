//! Gmail label folders (ADR-007): mapping `X-GM-LABELS` to label folders and back,
//! and keeping a message's label locations in line with its labels.

use std::collections::{BTreeSet, HashMap, HashSet};

use imap::GmailLabel;
use mazemail_types::{AccountId, Flags, FolderId, FolderRole, MessageId};
use store::{Folder, NewFolder, NewLocation, Queries, WriteTx};

use crate::error::Result;
use crate::events::Delta;
use crate::shared::Shared;

/// The Gmail folders of an account as the engine needs them.
#[derive(Debug, Clone, Default)]
pub(crate) struct GmailFolders {
    pub(crate) all_mail: Option<FolderId>,
    pub(crate) trash: Option<FolderId>,
    pub(crate) spam: Option<FolderId>,
    /// Every label folder.
    pub(crate) labels: HashSet<FolderId>,
    by_role: HashMap<FolderRole, FolderId>,
    by_name: HashMap<String, FolderId>,
    folders: HashMap<FolderId, Folder>,
}

impl GmailFolders {
    pub(crate) fn from_folders(folders: Vec<Folder>) -> Self {
        let mut out = Self::default();
        for f in folders {
            if f.is_label {
                out.labels.insert(f.id);
                if let Some(role) = f.role {
                    out.by_role.entry(role).or_insert(f.id);
                }
                if let Some(name) = &f.remote_name {
                    out.by_name.insert(name.clone(), f.id);
                }
            } else if f.remote_name.is_some() {
                match f.role {
                    Some(FolderRole::All) => out.all_mail = Some(f.id),
                    Some(FolderRole::Trash) => out.trash = Some(f.id),
                    Some(FolderRole::Junk) => out.spam = Some(f.id),
                    _ => {}
                }
            }
            out.folders.insert(f.id, f);
        }
        out
    }

    pub(crate) async fn load(shared: &Shared) -> Result<Self> {
        let account = shared.account();
        let folders = shared.read(move |r| r.folders(Some(account))).await?;
        Ok(Self::from_folders(folders))
    }

    pub(crate) fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.get(&id)
    }

    pub(crate) fn is_label(&self, id: FolderId) -> bool {
        self.labels.contains(&id)
    }

    /// The folder a label maps to, if it exists locally. `Err(())` for labels that
    /// are not shown as folders (Trash and Spam are mailboxes; unknown system labels).
    fn lookup(&self, label: &GmailLabel) -> Result<Option<FolderId>, ()> {
        match label {
            GmailLabel::Trash | GmailLabel::Spam | GmailLabel::System(_) => Err(()),
            GmailLabel::User(name) => Ok(self.by_name.get(name).copied()),
            system => {
                let role = system.role().ok_or(())?;
                Ok(self
                    .by_role
                    .get(&role)
                    .or_else(|| self.by_name.get(&system.to_string()))
                    .copied())
            }
        }
    }

    /// Maps labels to label folders, creating missing ones. Returns, per input list,
    /// the folder ids (deduplicated, sorted).
    pub(crate) async fn resolve(
        &mut self,
        shared: &Shared,
        lists: &[&[GmailLabel]],
    ) -> Result<Vec<Vec<FolderId>>> {
        let mut missing: BTreeSet<GmailLabel> = BTreeSet::new();
        for list in lists {
            for label in *list {
                if let Ok(None) = self.lookup(label) {
                    missing.insert(label.clone());
                }
            }
        }
        if !missing.is_empty() {
            let account = shared.account();
            let to_create: Vec<GmailLabel> = missing.into_iter().collect();
            let created = shared
                .write(to_create.len(), move |w| {
                    create_labels(w, account, &to_create)
                })
                .await?;
            for folder in created {
                self.labels.insert(folder.id);
                if let Some(role) = folder.role {
                    self.by_role.entry(role).or_insert(folder.id);
                }
                if let Some(name) = &folder.remote_name {
                    self.by_name.insert(name.clone(), folder.id);
                }
                shared
                    .events
                    .emit(crate::SyncEvent::FoldersChanged(vec![folder.id]));
                self.folders.insert(folder.id, folder);
            }
        }
        Ok(lists
            .iter()
            .map(|list| {
                let set: BTreeSet<FolderId> = list
                    .iter()
                    .filter_map(|l| self.lookup(l).ok().flatten())
                    .collect();
                set.into_iter().collect()
            })
            .collect())
    }

    /// The Gmail label a label folder stands for.
    pub(crate) fn label_of(&self, folder: FolderId) -> Option<GmailLabel> {
        let f = self.folders.get(&folder).filter(|f| f.is_label)?;
        if let Some(label) = f.role.and_then(GmailLabel::for_role) {
            return Some(label);
        }
        let name = f.remote_name.as_deref()?;
        Some(if name.starts_with('\\') {
            GmailLabel::parse(name)
        } else {
            GmailLabel::User(name.to_owned())
        })
    }
}

/// Creates label folders for labels that aren't listed as folders (hidden from IMAP,
/// or created since the last LIST).
fn create_labels(
    w: &mut WriteTx<'_>,
    account: AccountId,
    labels: &[GmailLabel],
) -> store::Result<Vec<Folder>> {
    let mut out = Vec::new();
    for label in labels {
        let (remote, display, role) = match label {
            GmailLabel::User(name) => {
                let leaf = name.rsplit('/').next().unwrap_or(name);
                (name.clone(), leaf.to_owned(), None)
            }
            system => {
                let wire = system.to_string();
                let display = wire.trim_start_matches('\\').to_owned();
                (wire, display, system.role())
            }
        };
        // Another job may have created it in the meantime.
        if let Some(existing) = w.folder_by_remote_name(account, &remote)? {
            out.push(existing);
            continue;
        }
        let mut folder = NewFolder::new(account, &remote, &display).label();
        folder.role = role;
        let id = w.create_folder(&folder)?;
        if let Some(created) = w.folder(id)? {
            out.push(created);
        }
    }
    Ok(out)
}

fn normalized(keywords: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = keywords
        .iter()
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    set.into_iter().collect()
}

/// Makes `message`'s label locations exactly `desired`, all with `flags` and
/// `keywords` (Gmail keeps one flag state per message). Records the changes.
pub(crate) fn apply_labels(
    w: &mut WriteTx<'_>,
    message: MessageId,
    desired: &[FolderId],
    flags: Flags,
    keywords: &[String],
    label_folders: &HashSet<FolderId>,
    delta: &mut Delta,
) -> store::Result<()> {
    let Some(detail) = w.message(message)? else {
        return Ok(());
    };
    let wanted_keywords = normalized(keywords);
    for loc in &detail.locations {
        if label_folders.contains(&loc.folder) && !desired.contains(&loc.folder) {
            w.remove_location(message, loc.folder)?;
            delta.removed(loc.folder, message);
        }
    }
    for folder in desired {
        let current = detail.locations.iter().find(|l| l.folder == *folder);
        if let Some(loc) = current
            && loc.flags == flags
            && loc.keywords == wanted_keywords
        {
            continue;
        }
        w.add_location(
            message,
            &NewLocation {
                folder: *folder,
                uid: None,
                modseq: None,
                flags,
                keywords: keywords.to_vec(),
            },
        )?;
        if current.is_some() {
            delta.changed(*folder, message);
        } else {
            delta.added(*folder, message);
        }
    }
    Ok(())
}

/// Removes every label location of `message` (it left All Mail).
pub(crate) fn drop_labels(
    w: &mut WriteTx<'_>,
    message: MessageId,
    label_folders: &HashSet<FolderId>,
    delta: &mut Delta,
) -> store::Result<()> {
    apply_labels(w, message, &[], Flags::empty(), &[], label_folders, delta)
}

#[cfg(test)]
mod tests {
    use store::FolderSyncState;

    use super::*;

    fn folder(id: i64, name: &str, role: Option<FolderRole>, is_label: bool) -> Folder {
        Folder {
            id: FolderId(id),
            account: AccountId(1),
            parent: None,
            remote_name: Some(name.to_owned()),
            display_name: name.to_owned(),
            role,
            is_label,
            sync: FolderSyncState::default(),
            sync_policy: None,
            unread: 0,
            total: 0,
            ordinal: 0,
            favorite: false,
        }
    }

    #[test]
    fn maps_labels_to_folders_and_back() {
        let g = GmailFolders::from_folders(vec![
            folder(1, "[Gmail]/All Mail", Some(FolderRole::All), false),
            folder(2, "[Gmail]/Trash", Some(FolderRole::Trash), false),
            folder(3, "[Gmail]/Spam", Some(FolderRole::Junk), false),
            folder(4, "INBOX", Some(FolderRole::Inbox), true),
            folder(5, "Work", None, true),
            folder(6, "\\Important", Some(FolderRole::Important), true),
        ]);
        assert_eq!(g.all_mail, Some(FolderId(1)));
        assert_eq!(g.trash, Some(FolderId(2)));
        assert_eq!(g.spam, Some(FolderId(3)));
        assert_eq!(g.lookup(&GmailLabel::Inbox), Ok(Some(FolderId(4))));
        assert_eq!(
            g.lookup(&GmailLabel::User("Work".into())),
            Ok(Some(FolderId(5)))
        );
        assert_eq!(g.lookup(&GmailLabel::User("Other".into())), Ok(None));
        assert_eq!(g.lookup(&GmailLabel::Sent), Ok(None));
        assert_eq!(g.lookup(&GmailLabel::Trash), Err(()));
        assert_eq!(g.lookup(&GmailLabel::System("Muted".into())), Err(()));
        assert_eq!(g.label_of(FolderId(4)), Some(GmailLabel::Inbox));
        assert_eq!(
            g.label_of(FolderId(5)),
            Some(GmailLabel::User("Work".into()))
        );
        assert_eq!(g.label_of(FolderId(6)), Some(GmailLabel::Important));
        assert_eq!(g.label_of(FolderId(1)), None);
    }
}
