//! Actions on a whole folder, from the folder pane's right-click menu: **Mark All as
//! Read** and **Delete All** (or **Empty Trash** on a Trash folder).
//!
//! They are ordinary [`MailCommand`]s over the folder's messages, so they sync like
//! any change, are audited, and `cmd-z` undoes them where the command can be undone
//! (not emptying the Trash).

use mazemail_types::{FolderId, FolderRole, MessageId};
use store::{ListScope, Queries as _};

use crate::commands::{CommandReceipt, MailCommand, Origin};
use crate::global_search::message_scope;
use crate::{CoreHandle, Error, Result};

impl CoreHandle {
    /// Marks every message in `scope` (a folder or a unified folder) read.
    pub async fn mark_all_read(&self, scope: ListScope) -> Result<Option<CommandReceipt>> {
        self.ensure_open()?;
        let ids = self
            .store()
            .reader()?
            .message_ids_in_scope(&message_scope(scope))?;
        if ids.is_empty() {
            return Ok(None);
        }
        let command = MailCommand::SetFlags {
            ids,
            add: mazemail_types::Flags::SEEN,
            remove: mazemail_types::Flags::empty(),
        };
        self.apply(command, Origin::User).await.map(Some)
    }

    /// Deletes the mail of `folder`: moves its copies into the account's Trash (undoable
    /// like Delete). In a Trash folder it empties the Trash: the messages that are only
    /// there are deleted from the server for good.
    pub async fn delete_all_in_folder(&self, folder: FolderId) -> Result<Option<CommandReceipt>> {
        self.ensure_open()?;
        let command = {
            let reader = self.store().reader()?;
            let row = reader
                .folder(folder)?
                .ok_or_else(|| Error::not_found("folder", folder))?;
            let ids = reader.message_ids_in_scope(&message_scope(ListScope::Folder(folder)))?;
            if row.role == Some(FolderRole::Trash) {
                // Only what is nowhere else: a copy in another folder isn't Trash.
                let mut only_here: Vec<MessageId> = Vec::with_capacity(ids.len());
                for id in ids {
                    if reader
                        .message(id)?
                        .is_some_and(|d| d.locations.iter().all(|l| l.folder == folder))
                    {
                        only_here.push(id);
                    }
                }
                MailCommand::Delete {
                    ids: only_here,
                    permanent: true,
                }
            } else {
                let trash = reader
                    .folders_with_role(FolderRole::Trash, Some(row.account))?
                    .into_iter()
                    .next();
                match trash {
                    Some(trash) => MailCommand::Move {
                        ids,
                        to: trash.id,
                        from: Some(folder),
                    },
                    None => MailCommand::Delete {
                        ids,
                        permanent: false,
                    },
                }
            }
        };
        let empty = match &command {
            MailCommand::Move { ids, .. } | MailCommand::Delete { ids, .. } => ids.is_empty(),
            _ => false,
        };
        if empty {
            return Ok(None);
        }
        self.apply(command, Origin::User).await.map(Some)
    }
}
