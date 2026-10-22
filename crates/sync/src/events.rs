//! Events out of the sync engine: [`SyncEvent`]s coalesced into [`SyncEventBatch`]es.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mazemail_types::{AccountId, FolderId, MessageId};
use store::OpId;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

/// What an account's sync engine is doing, for the status bar and activity log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncStatus {
    /// Opening the first connection.
    Connecting,
    /// Folder passes are running: `done` of `total` in this round.
    Syncing {
        /// Folders finished in this round.
        done: usize,
        /// Folders in this round (grows when more are queued).
        total: usize,
    },
    /// Everything is up to date; waiting for changes.
    Idle,
    /// The server can't be reached; retrying.
    Offline {
        /// Why (network error text, no mail content).
        reason: String,
        /// When the next attempt happens, if scheduled.
        retry_in: Option<Duration>,
    },
    /// Something failed that retrying didn't fix.
    Error {
        /// What failed.
        message: String,
    },
    /// The server rejected the credentials, or there are none: sign in again.
    AuthRequired {
        /// The server's or the credential provider's reason.
        message: String,
    },
    /// The engine shut down.
    Stopped,
}

impl SyncStatus {
    /// The status as a sentence for people (the activity log, the status bar).
    pub fn text(&self) -> String {
        match self {
            Self::Connecting => "Connecting…".to_owned(),
            Self::Syncing { done, total } => format!("Syncing folders ({done} of {total})…"),
            Self::Idle => "Up to date".to_owned(),
            Self::Offline {
                reason,
                retry_in: Some(delay),
            } => format!(
                "Offline: {reason}. Maze will try again in {}.",
                human_duration(*delay)
            ),
            Self::Offline {
                reason,
                retry_in: None,
            } => format!("Offline: {reason}."),
            Self::Error { message } => format!("Sync failed: {message}"),
            Self::AuthRequired { message } => {
                format!("Sign-in required: {message}. Maze stopped syncing this account.")
            }
            Self::Stopped => "Sync stopped".to_owned(),
        }
    }

    /// Whether the server can't be used right now (offline or signed out).
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Offline { .. } | Self::AuthRequired { .. } | Self::Stopped
        )
    }
}

impl fmt::Display for SyncStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs_f64().ceil() as u64;
    match secs {
        0 | 1 => "1 second".to_owned(),
        2..=89 => format!("{secs} seconds"),
        _ => format!("{} minutes", secs.div_ceil(60)),
    }
}

/// What happened to a pending op during replay.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpEvent {
    /// Applied on the server and removed from the queue.
    Done(OpId),
    /// Failed; retried after a backoff.
    Failed {
        /// The op.
        op: OpId,
        /// The error text.
        error: String,
        /// Failed attempts so far.
        attempts: u32,
    },
    /// Failed too often (PLAN §4.5: 5 attempts); waits for Retry or Discard in the
    /// activity log.
    Stuck {
        /// The op.
        op: OpId,
        /// The last error text.
        error: String,
    },
}

/// Message and folder counts of a folder after a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderCounts {
    /// Locations without `\Seen`.
    pub unread: u64,
    /// Locations.
    pub total: u64,
}

/// One thing that happened. The engine emits these; subscribers get them merged into
/// [`SyncEventBatch`]es.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncEvent {
    /// Folders were created or their name, parent or role changed.
    FoldersChanged(Vec<FolderId>),
    /// Folders were deleted.
    FoldersRemoved(Vec<FolderId>),
    /// A folder's UIDVALIDITY changed: all of its messages were dropped and it is
    /// synced again from scratch.
    FolderReset(FolderId),
    /// Messages appeared in a folder (or Gmail label).
    MessagesAdded {
        /// The folder.
        folder: FolderId,
        /// The messages.
        messages: Vec<MessageId>,
    },
    /// Messages in a folder changed (flags, keywords).
    MessagesChanged {
        /// The folder.
        folder: FolderId,
        /// The messages.
        messages: Vec<MessageId>,
    },
    /// Messages left a folder (expunged, moved, label removed).
    MessagesRemoved {
        /// The folder.
        folder: FolderId,
        /// The messages.
        messages: Vec<MessageId>,
    },
    /// A folder's counts after a pass.
    Counts {
        /// The folder.
        folder: FolderId,
        /// The counts.
        counts: FolderCounts,
    },
    /// A message body (or its text parts) is now stored.
    BodyFetched(MessageId),
    /// The engine's status changed.
    Status(SyncStatus),
    /// Pending-op replay progress.
    Op(OpEvent),
    /// An `[ALERT]` from the server that the user must see (RFC 3501 §7.1).
    Alert(String),
}

/// Everything that happened in one coalescing window (about 16 ms).
///
/// Message sets are merged per folder: a message added and then removed within the
/// window shows up as removed only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncEventBatch {
    /// The account.
    pub account: AccountId,
    /// Folders created or changed.
    pub folders_changed: BTreeSet<FolderId>,
    /// Folders deleted.
    pub folders_removed: BTreeSet<FolderId>,
    /// Folders whose contents were dropped after a UIDVALIDITY change.
    pub folders_reset: BTreeSet<FolderId>,
    /// Messages added, per folder.
    pub added: BTreeMap<FolderId, BTreeSet<MessageId>>,
    /// Messages changed, per folder.
    pub changed: BTreeMap<FolderId, BTreeSet<MessageId>>,
    /// Messages removed, per folder.
    pub removed: BTreeMap<FolderId, BTreeSet<MessageId>>,
    /// Latest counts per folder.
    pub counts: BTreeMap<FolderId, FolderCounts>,
    /// Messages whose body arrived.
    pub bodies: BTreeSet<MessageId>,
    /// The latest status, if it changed.
    pub status: Option<SyncStatus>,
    /// Pending-op events, in order.
    pub ops: Vec<OpEvent>,
    /// Server alerts, in order.
    pub alerts: Vec<String>,
}

impl SyncEventBatch {
    /// An empty batch for `account`.
    pub fn new(account: AccountId) -> Self {
        Self {
            account,
            folders_changed: BTreeSet::new(),
            folders_removed: BTreeSet::new(),
            folders_reset: BTreeSet::new(),
            added: BTreeMap::new(),
            changed: BTreeMap::new(),
            removed: BTreeMap::new(),
            counts: BTreeMap::new(),
            bodies: BTreeSet::new(),
            status: None,
            ops: Vec::new(),
            alerts: Vec::new(),
        }
    }

    /// Whether nothing happened.
    pub fn is_empty(&self) -> bool {
        *self == Self::new(self.account)
    }

    /// Merges one event into the batch.
    pub fn push(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::FoldersChanged(ids) => {
                for id in ids {
                    self.folders_removed.remove(&id);
                    self.folders_changed.insert(id);
                }
            }
            SyncEvent::FoldersRemoved(ids) => {
                for id in ids {
                    self.folders_changed.remove(&id);
                    self.added.remove(&id);
                    self.changed.remove(&id);
                    self.removed.remove(&id);
                    self.counts.remove(&id);
                    self.folders_removed.insert(id);
                }
            }
            SyncEvent::FolderReset(id) => {
                self.added.remove(&id);
                self.changed.remove(&id);
                self.removed.remove(&id);
                self.folders_reset.insert(id);
            }
            SyncEvent::MessagesAdded { folder, messages } => {
                for m in messages {
                    remove_from(&mut self.removed, folder, m);
                    self.added.entry(folder).or_default().insert(m);
                }
            }
            SyncEvent::MessagesChanged { folder, messages } => {
                for m in messages {
                    let fresh = self.added.get(&folder).is_some_and(|s| s.contains(&m));
                    if !fresh {
                        self.changed.entry(folder).or_default().insert(m);
                    }
                }
            }
            SyncEvent::MessagesRemoved { folder, messages } => {
                for m in messages {
                    remove_from(&mut self.added, folder, m);
                    remove_from(&mut self.changed, folder, m);
                    self.removed.entry(folder).or_default().insert(m);
                }
            }
            SyncEvent::Counts { folder, counts } => {
                self.counts.insert(folder, counts);
            }
            SyncEvent::BodyFetched(m) => {
                self.bodies.insert(m);
            }
            SyncEvent::Status(status) => self.status = Some(status),
            SyncEvent::Op(op) => self.ops.push(op),
            SyncEvent::Alert(text) => self.alerts.push(text),
        }
    }
}

fn remove_from(
    map: &mut BTreeMap<FolderId, BTreeSet<MessageId>>,
    folder: FolderId,
    message: MessageId,
) {
    if let Some(set) = map.get_mut(&folder) {
        set.remove(&message);
        if set.is_empty() {
            map.remove(&folder);
        }
    }
}

/// Where engine parts send events; cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct EventSink {
    tx: mpsc::UnboundedSender<SyncEvent>,
}

impl EventSink {
    pub(crate) fn emit(&self, event: SyncEvent) {
        // The coalescer only stops when the engine does; dropping late events is fine.
        let _ = self.tx.send(event);
    }

    /// `MessagesAdded`/`Changed`/`Removed` for a non-empty list.
    pub(crate) fn messages(
        &self,
        kind: MessageEventKind,
        folder: FolderId,
        messages: Vec<MessageId>,
    ) {
        if messages.is_empty() {
            return;
        }
        self.emit(match kind {
            MessageEventKind::Added => SyncEvent::MessagesAdded { folder, messages },
            MessageEventKind::Changed => SyncEvent::MessagesChanged { folder, messages },
            MessageEventKind::Removed => SyncEvent::MessagesRemoved { folder, messages },
        });
    }
}

/// Which message event to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageEventKind {
    Added,
    Changed,
    Removed,
}

/// Per-folder message changes collected during a job, emitted at once.
#[derive(Debug, Default)]
pub(crate) struct Delta {
    pub(crate) added: BTreeMap<FolderId, Vec<MessageId>>,
    pub(crate) changed: BTreeMap<FolderId, Vec<MessageId>>,
    pub(crate) removed: BTreeMap<FolderId, Vec<MessageId>>,
}

impl Delta {
    pub(crate) fn added(&mut self, folder: FolderId, message: MessageId) {
        self.added.entry(folder).or_default().push(message);
    }

    pub(crate) fn changed(&mut self, folder: FolderId, message: MessageId) {
        self.changed.entry(folder).or_default().push(message);
    }

    pub(crate) fn removed(&mut self, folder: FolderId, message: MessageId) {
        self.removed.entry(folder).or_default().push(message);
    }

    pub(crate) fn merge(&mut self, other: Delta) {
        for (f, m) in other.added {
            self.added.entry(f).or_default().extend(m);
        }
        for (f, m) in other.changed {
            self.changed.entry(f).or_default().extend(m);
        }
        for (f, m) in other.removed {
            self.removed.entry(f).or_default().extend(m);
        }
    }

    pub(crate) fn emit(self, sink: &EventSink) {
        for (f, m) in self.removed {
            sink.messages(MessageEventKind::Removed, f, m);
        }
        for (f, m) in self.added {
            sink.messages(MessageEventKind::Added, f, m);
        }
        for (f, m) in self.changed {
            sink.messages(MessageEventKind::Changed, f, m);
        }
    }
}

/// Starts the coalescer: events from the returned sink go out on `out` at most once
/// per `window`.
pub(crate) fn spawn_coalescer(
    account: AccountId,
    out: broadcast::Sender<Arc<SyncEventBatch>>,
    window: Duration,
) -> (EventSink, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<SyncEvent>();
    let task = tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = SyncEventBatch::new(account);
            batch.push(first);
            let deadline = Instant::now() + window;
            loop {
                tokio::select! {
                    biased;
                    event = rx.recv() => match event {
                        Some(event) => batch.push(event),
                        None => break,
                    },
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            if !batch.is_empty() {
                // No subscribers is fine: nobody is looking.
                let _ = out.send(Arc::new(batch));
            }
        }
    });
    (EventSink { tx }, task)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const F: FolderId = FolderId(1);

    #[test]
    fn batch_merges_add_change_remove() {
        let mut batch = SyncEventBatch::new(AccountId(1));
        assert!(batch.is_empty());
        batch.push(SyncEvent::MessagesAdded {
            folder: F,
            messages: vec![MessageId(1), MessageId(2)],
        });
        batch.push(SyncEvent::MessagesChanged {
            folder: F,
            messages: vec![MessageId(1), MessageId(3)],
        });
        batch.push(SyncEvent::MessagesRemoved {
            folder: F,
            messages: vec![MessageId(2)],
        });
        assert_eq!(batch.added[&F], BTreeSet::from([MessageId(1)]));
        assert_eq!(batch.changed[&F], BTreeSet::from([MessageId(3)]));
        assert_eq!(batch.removed[&F], BTreeSet::from([MessageId(2)]));
        batch.push(SyncEvent::FoldersRemoved(vec![F]));
        assert!(batch.added.is_empty() && batch.removed.is_empty());
        assert!(batch.folders_removed.contains(&F));
        assert!(!batch.is_empty());
    }

    #[test]
    fn status_text_is_human() {
        assert_eq!(SyncStatus::Idle.text(), "Up to date");
        assert_eq!(
            SyncStatus::Syncing { done: 2, total: 5 }.text(),
            "Syncing folders (2 of 5)…"
        );
        let offline = SyncStatus::Offline {
            reason: "connection refused".into(),
            retry_in: Some(Duration::from_secs(30)),
        };
        assert_eq!(
            offline.text(),
            "Offline: connection refused. Maze will try again in 30 seconds."
        );
        assert!(offline.is_unavailable());
        assert!(
            SyncStatus::AuthRequired {
                message: "password rejected".into()
            }
            .text()
            .starts_with("Sign-in required")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn coalescer_batches_per_window() {
        let (out, mut rx) = broadcast::channel(16);
        let (sink, _task) = spawn_coalescer(AccountId(7), out, Duration::from_millis(16));
        sink.emit(SyncEvent::BodyFetched(MessageId(1)));
        sink.emit(SyncEvent::BodyFetched(MessageId(2)));
        sink.emit(SyncEvent::Status(SyncStatus::Idle));
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.account, AccountId(7));
        assert_eq!(batch.bodies.len(), 2);
        assert_eq!(batch.status, Some(SyncStatus::Idle));
        tokio::time::sleep(Duration::from_millis(50)).await;
        sink.emit(SyncEvent::BodyFetched(MessageId(3)));
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.bodies, BTreeSet::from([MessageId(3)]));
    }
}
