//! What a committed write batch changed, for `mazemail_core`'s `CoreEventBatch`.

use std::collections::{BTreeMap, BTreeSet};

use mazemail_types::{AccountId, FolderId, MessageId, ThreadId};

/// Everything one committed writer transaction changed.
///
/// Sent on the channel from [`Store::subscribe`](crate::Store::subscribe) after the
/// commit, so a reader that looks at these ids sees the new state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangeSet {
    /// Folders created, edited, deleted, or whose counts changed.
    pub folders: BTreeSet<FolderId>,
    /// Thread list rows added, changed or removed, per folder.
    pub folder_threads: BTreeMap<FolderId, BTreeSet<ThreadId>>,
    /// Threads whose row data changed.
    pub threads: BTreeSet<ThreadId>,
    /// Threads that no longer exist (merged or emptied).
    pub threads_removed: BTreeSet<ThreadId>,
    /// Messages added or changed (flags, locations, body, tags).
    pub messages: BTreeSet<MessageId>,
    /// Messages deleted.
    pub messages_removed: BTreeSet<MessageId>,
    /// Accounts whose settings, identities or folders list changed, or whose pending
    /// ops changed.
    pub accounts: BTreeSet<AccountId>,
    /// Identities, SMTP servers, signatures, tags, rules or saved searches changed.
    pub settings: bool,
    /// The outbox changed.
    pub outbox: bool,
    /// Local drafts changed.
    pub drafts: bool,
    /// Proposals changed.
    pub proposals: bool,
}

impl ChangeSet {
    /// True if nothing changed.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.folders.extend(other.folders);
        for (folder, threads) in other.folder_threads {
            self.folder_threads
                .entry(folder)
                .or_default()
                .extend(threads);
        }
        self.threads.extend(other.threads);
        self.threads_removed.extend(other.threads_removed);
        self.messages.extend(other.messages);
        self.messages_removed.extend(other.messages_removed);
        self.accounts.extend(other.accounts);
        self.settings |= other.settings;
        self.outbox |= other.outbox;
        self.drafts |= other.drafts;
        self.proposals |= other.proposals;
        // A thread removed in a later job isn't "changed" any more, and vice versa.
        for thread in &self.threads_removed {
            self.threads.remove(thread);
        }
        for message in &self.messages_removed {
            self.messages.remove(message);
        }
    }

    pub(crate) fn folder_thread(&mut self, folder: FolderId, thread: ThreadId) {
        self.folder_threads
            .entry(folder)
            .or_default()
            .insert(thread);
    }
}
