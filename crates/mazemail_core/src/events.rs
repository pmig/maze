//! Core events, batched per frame (PLAN §4.1, §4.2).
//!
//! Everything that changes what the UI shows ends up as an internal event on one
//! channel: store change sets (after every commit), sync status, indexer progress,
//! settings reloads and lazily computed message data. A batcher task collects them for
//! [`CoreConfig::event_window`](crate::CoreConfig::event_window) (16 ms by default,
//! one frame at 60 Hz) after the first one arrives, merges them into one
//! [`CoreEventBatch`] and broadcasts it. A burst of sync writes therefore costs the UI
//! one re-render per frame, and an idle core sends nothing (no timers run while there
//! are no events, except the [disk usage meter](crate::storage), which speaks only
//! when the usage changed).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use mazemail_types::{AccountId, FolderId, MessageId, ThreadId};
use store::{ChangeSet, Queries, Store};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Instant, sleep_until};

use crate::indexing::IndexProgress;
use crate::sync_bridge::SyncStatus;

/// Default batching window: one frame at 60 Hz.
pub const DEFAULT_EVENT_WINDOW: Duration = Duration::from_millis(16);

/// Capacity of the broadcast channel; a receiver further behind lags and should reload.
const BROADCAST_CAPACITY: usize = 256;

/// One input to the batcher.
#[derive(Debug, Clone)]
pub(crate) enum CoreEvent {
    /// A store commit.
    Store(Arc<ChangeSet>),
    /// Store change sets were lost (slow forwarder); everything may have changed.
    StoreLagged,
    /// Sync reported a state change.
    Sync(SyncStatus),
    /// The indexer committed or made rebuild progress.
    Index(IndexProgress),
    /// The settings file was reloaded.
    Settings,
    /// Lazily computed data of a message changed (for example its sender verification),
    /// without a store change.
    MessageUpdated(MessageId),
    /// Maze's disk usage was measured and changed (see [`crate::storage`]).
    Storage,
}

/// Unread and total counts of a folder, sent along with folder changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FolderCounts {
    /// The folder.
    pub folder: FolderId,
    /// Locations without `\Seen`.
    pub unread: u64,
    /// All locations.
    pub total: u64,
}

/// Everything that changed within one batching window.
///
/// The UI bridge merges batches that arrive within one frame and re-renders once.
/// Lists are sorted and free of duplicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoreEventBatch {
    /// Folders created, renamed, deleted, or whose counts changed: reload the folder
    /// pane entries.
    pub folders_changed: Vec<FolderId>,
    /// Thread list rows added, changed or removed, per folder: re-query the keys of an
    /// open list showing that folder (or a unified view containing it).
    pub threads_changed: Vec<(FolderId, Vec<ThreadId>)>,
    /// Threads whose row data changed (the row cache already dropped them).
    pub threads_updated: Vec<ThreadId>,
    /// Threads that no longer exist.
    pub removed: Vec<ThreadId>,
    /// Messages added or changed (flags, locations, body, tags, verification).
    pub messages_changed: Vec<MessageId>,
    /// Messages deleted.
    pub messages_removed: Vec<MessageId>,
    /// Current counts of the folders in `folders_changed` that still exist.
    pub counts: Vec<FolderCounts>,
    /// Accounts whose settings, identities, folder list or pending ops changed.
    pub accounts_changed: Vec<AccountId>,
    /// The latest sync state of each account that reported one.
    pub sync_status: Vec<SyncStatus>,
    /// The latest indexer progress, if it changed.
    pub index: Option<IndexProgress>,
    /// The outbox changed.
    pub outbox: bool,
    /// Agent proposals changed.
    pub proposals: bool,
    /// Settings changed: the settings file, or identities, tags, rules and saved
    /// searches in the store.
    pub settings: bool,
    /// Address book entries changed (collected, imported or synced): reload the
    /// Address Book.
    pub contacts: bool,
    /// Maze's disk usage changed ([`CoreHandle::storage_usage`](crate::CoreHandle::storage_usage)).
    pub storage: bool,
    /// The domain list may have changed (new domains, counts, contacts, a favicon
    /// arrived): read [`CoreHandle::domains`](crate::CoreHandle::domains) again.
    pub domains: bool,
    /// Events were lost; reload everything that is on screen.
    pub reload: bool,
}

impl CoreEventBatch {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Default)]
struct Accumulator {
    folders: BTreeSet<FolderId>,
    folder_threads: BTreeMap<FolderId, BTreeSet<ThreadId>>,
    threads: BTreeSet<ThreadId>,
    removed: BTreeSet<ThreadId>,
    messages: BTreeSet<MessageId>,
    messages_removed: BTreeSet<MessageId>,
    accounts: BTreeSet<AccountId>,
    sync: BTreeMap<AccountId, SyncStatus>,
    index: Option<IndexProgress>,
    outbox: bool,
    proposals: bool,
    contacts: bool,
    settings: bool,
    storage: bool,
    domains: bool,
    reload: bool,
}

impl Accumulator {
    fn add(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::Store(changes) => {
                self.folders.extend(changes.folders.iter().copied());
                for (folder, threads) in &changes.folder_threads {
                    self.folder_threads
                        .entry(*folder)
                        .or_default()
                        .extend(threads.iter().copied());
                }
                self.threads.extend(changes.threads.iter().copied());
                self.removed.extend(changes.threads_removed.iter().copied());
                self.messages.extend(changes.messages.iter().copied());
                self.messages_removed
                    .extend(changes.messages_removed.iter().copied());
                self.accounts.extend(changes.accounts.iter().copied());
                self.settings |= changes.settings;
                self.outbox |= changes.outbox;
                self.proposals |= changes.proposals;
                self.contacts |= changes.contacts;
                self.domains |=
                    changes.domains || changes.contacts || !changes.messages_removed.is_empty();
            }
            CoreEvent::StoreLagged => self.reload = true,
            CoreEvent::Sync(status) => {
                self.sync.insert(status.account, status);
            }
            CoreEvent::Index(progress) => self.index = Some(progress),
            CoreEvent::Settings => {
                self.settings = true;
                // The `favicons` setting decides which favicons are shown.
                self.domains = true;
            }
            CoreEvent::MessageUpdated(message) => {
                self.messages.insert(message);
            }
            CoreEvent::Storage => self.storage = true,
        }
    }

    fn finish(mut self, counts: Vec<FolderCounts>) -> CoreEventBatch {
        // Removal wins over a change in the same window.
        for thread in &self.removed {
            self.threads.remove(thread);
        }
        for message in &self.messages_removed {
            self.messages.remove(message);
        }
        CoreEventBatch {
            folders_changed: self.folders.into_iter().collect(),
            threads_changed: self
                .folder_threads
                .into_iter()
                .map(|(folder, threads)| (folder, threads.into_iter().collect()))
                .collect(),
            threads_updated: self.threads.into_iter().collect(),
            removed: self.removed.into_iter().collect(),
            messages_changed: self.messages.into_iter().collect(),
            messages_removed: self.messages_removed.into_iter().collect(),
            counts,
            accounts_changed: self.accounts.into_iter().collect(),
            sync_status: self.sync.into_values().collect(),
            index: self.index,
            outbox: self.outbox,
            proposals: self.proposals,
            contacts: self.contacts,
            settings: self.settings,
            storage: self.storage,
            domains: self.domains,
            reload: self.reload,
        }
    }
}

/// The broadcast side of the event stream.
pub(crate) fn channel() -> broadcast::Sender<CoreEventBatch> {
    broadcast::channel(BROADCAST_CAPACITY).0
}

/// Collects events into batches until the input channel closes.
pub(crate) async fn run_batcher(
    mut rx: mpsc::UnboundedReceiver<CoreEvent>,
    out: broadcast::Sender<CoreEventBatch>,
    store: Store,
    window: Duration,
) {
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let mut acc = Accumulator::default();
        acc.add(first);
        let deadline = Instant::now() + window;
        let mut closed = false;
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => acc.add(event),
                    None => {
                        closed = true;
                        break;
                    }
                },
                () = sleep_until(deadline) => break,
            }
        }
        let folders: Vec<FolderId> = acc.folders.iter().copied().collect();
        let counts = if folders.is_empty() {
            Vec::new()
        } else {
            let store = store.clone();
            tokio::task::spawn_blocking(move || folder_counts(&store, &folders))
                .await
                .unwrap_or_default()
        };
        let batch = acc.finish(counts);
        if !batch.is_empty() {
            // No receivers is fine: nobody is looking.
            let _ = out.send(batch);
        }
        if closed {
            return;
        }
    }
}

fn folder_counts(store: &Store, folders: &[FolderId]) -> Vec<FolderCounts> {
    let Ok(reader) = store.reader() else {
        return Vec::new();
    };
    folders
        .iter()
        .filter_map(|id| match reader.folder(*id) {
            Ok(Some(folder)) => Some(FolderCounts {
                folder: folder.id,
                unread: folder.unread,
                total: folder.total,
            }),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%err, "reading folder counts for an event batch");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_bridge::SyncState;

    #[test]
    fn later_sync_status_of_an_account_wins_and_removals_win() {
        let mut acc = Accumulator::default();
        acc.add(CoreEvent::Sync(SyncStatus {
            detail: None,
            account: AccountId(1),
            state: SyncState::Offline,
        }));
        acc.add(CoreEvent::Sync(SyncStatus {
            detail: None,
            account: AccountId(1),
            state: SyncState::Idle,
        }));
        acc.add(CoreEvent::MessageUpdated(MessageId(7)));
        acc.messages_removed.insert(MessageId(7));
        acc.add(CoreEvent::Settings);
        let batch = acc.finish(Vec::new());
        assert_eq!(batch.sync_status.len(), 1);
        assert_eq!(batch.sync_status[0].state, SyncState::Idle);
        assert!(batch.messages_changed.is_empty());
        assert_eq!(batch.messages_removed, vec![MessageId(7)]);
        assert!(batch.settings);
        assert!(!batch.is_empty());
    }
}
