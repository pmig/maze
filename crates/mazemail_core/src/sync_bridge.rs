//! The seam between core and the `sync` crate (PLAN §4.5, CONCEPT §7).
//!
//! Core never talks IMAP. It needs three things from sync, all behind [`SyncBridge`]:
//!
//! - **Priority body fetches.** [`CoreHandle::message`](crate::CoreHandle::message) on a
//!   message whose body isn't stored returns a view with
//!   [`Body::Pending`](crate::Body::Pending) and calls [`SyncBridge::fetch_body`] with
//!   [`FetchPriority::Interactive`], so the account actor jumps its queue. When the body
//!   lands in the store, the store's change set reaches the UI as an event and it opens
//!   the message again.
//! - **Refresh.** [`MailCommand::Refresh`](crate::MailCommand::Refresh) calls
//!   [`SyncBridge::refresh`].
//! - **Pending-op wake-up.** After a command enqueued ops (in the same transaction as
//!   its optimistic local apply), core calls [`SyncBridge::ops_enqueued`] so the account
//!   actors replay them without waiting for their next poll. The payload format is
//!   documented in [`crate::ops`].
//!
//! In the other direction, sync reports its state through a [`SyncStatusSink`] (from
//! [`CoreHandle::sync_status_sink`](crate::CoreHandle::sync_status_sink)); core merges it
//! into [`CoreEventBatch::sync_status`](crate::CoreEventBatch::sync_status). New
//! messages and bodies need no extra channel: sync writes them to the store (through
//! `ingest`), and the store's change sets become event batches.
//!
//! # The engine
//!
//! With [`CoreConfig::sync_engine`](crate::CoreConfig::sync_engine) set (the app and
//! `mazemail --headless` do), core runs the real engine (see [`crate::sync_engine`]):
//! one `sync` account actor per IMAP, Gmail and Microsoft 365 account, whose
//! supervisor implements [`SyncBridge`] and reports status into the event batches.
//! Without it, core uses [`CoreConfig::sync`](crate::CoreConfig::sync): [`NoSync`] by
//! default, which only logs, or a test double.

use std::fmt;

use mazemail_types::{AccountId, FolderId, MessageId};
use serde::{Deserialize, Serialize};
use store::OpId;
use tokio::sync::mpsc;

use crate::events::CoreEvent;

/// How urgently a body is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchPriority {
    /// Background download according to the offline policy.
    Background,
    /// Prefetch of a neighbor of the open message.
    Prefetch,
    /// The user opened the message: jump the queue, fetch text parts first.
    Interactive,
    /// The user needs the whole message now (an attachment): jump the queue and fetch
    /// the full raw message.
    Full,
}

/// What to refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum RefreshScope {
    /// Every account.
    All,
    /// One account.
    Account(AccountId),
    /// One folder.
    Folder(FolderId),
}

/// The sync engine as core sees it. Implementations must not block: queue the request
/// and return.
pub trait SyncBridge: Send + Sync + fmt::Debug {
    /// Fetch the body of `message` (text parts first for large messages when
    /// `priority` is [`FetchPriority::Interactive`]).
    fn fetch_body(&self, message: MessageId, priority: FetchPriority);

    /// Run a sync pass over `scope` now.
    fn refresh(&self, scope: RefreshScope);

    /// Pending ops were queued for these accounts; replay them soon.
    fn ops_enqueued(&self, accounts: &[AccountId]);

    /// Pending ops were queued: `(account, op)` in queue order. The default calls
    /// [`SyncBridge::ops_enqueued`] with the accounts.
    fn ops_queued(&self, ops: &[(AccountId, OpId)]) {
        let mut accounts: Vec<AccountId> = ops.iter().map(|(account, _)| *account).collect();
        accounts.sort();
        accounts.dedup();
        self.ops_enqueued(&accounts);
    }
}

/// A [`SyncBridge`] that does nothing but log, for headless use without accounts,
/// tests, and until the `sync` crate is wired in.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSync;

impl SyncBridge for NoSync {
    fn fetch_body(&self, message: MessageId, priority: FetchPriority) {
        tracing::debug!(%message, ?priority, "body fetch requested without a sync engine");
    }

    fn refresh(&self, scope: RefreshScope) {
        tracing::debug!(?scope, "refresh requested without a sync engine");
    }

    fn ops_enqueued(&self, accounts: &[AccountId]) {
        tracing::debug!(
            accounts = accounts.len(),
            "ops queued without a sync engine"
        );
    }
}

/// The state of one account's synchronization, as shown in the status bar and the
/// activity log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncState {
    /// Opening the first connection.
    Connecting,
    /// Connected and up to date (IDLE or polling).
    Idle,
    /// A sync pass is running.
    Syncing {
        /// The folder being synchronized, if any.
        folder: Option<FolderId>,
        /// Work done so far (messages or folders, as the engine counts).
        done: u64,
        /// Total work, if known.
        total: Option<u64>,
    },
    /// No network, or the server can't be reached; retrying with backoff.
    Offline,
    /// Authentication failed; the account needs attention.
    AuthRequired,
    /// Something else failed. `message` is for the activity log and must not contain
    /// message content.
    Error {
        /// What went wrong.
        message: String,
    },
}

impl SyncState {
    /// A short sentence for the status bar.
    pub fn text(&self) -> String {
        match self {
            Self::Connecting => "Connecting…".to_owned(),
            Self::Idle => "Up to date".to_owned(),
            Self::Syncing {
                done,
                total: Some(total),
                ..
            } => format!("Syncing ({done} of {total})…"),
            Self::Syncing { .. } => "Syncing…".to_owned(),
            Self::Offline => "Offline".to_owned(),
            Self::AuthRequired => "Sign-in required".to_owned(),
            Self::Error { message } => format!("Sync failed: {message}"),
        }
    }
}

/// One account's sync state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    /// The account.
    pub account: AccountId,
    /// Its state.
    pub state: SyncState,
    /// The engine's own sentence for people (with the reason and the next retry, say),
    /// if it gave one. Never contains message content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SyncStatus {
    /// A status without detail text.
    pub fn new(account: AccountId, state: SyncState) -> Self {
        Self {
            account,
            state,
            detail: None,
        }
    }

    /// The sentence for the status bar and the activity log: the engine's detail, or
    /// the state's default text.
    pub fn text(&self) -> String {
        self.detail.clone().unwrap_or_else(|| self.state.text())
    }
}

/// Where sync reports [`SyncStatus`] changes. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct SyncStatusSink {
    pub(crate) tx: mpsc::UnboundedSender<CoreEvent>,
}

impl fmt::Debug for SyncStatusSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncStatusSink")
            .field("closed", &self.tx.is_closed())
            .finish()
    }
}

impl SyncStatusSink {
    /// Reports a state change. Returns `false` once the core has shut down.
    pub fn report(&self, status: SyncStatus) -> bool {
        self.tx.send(CoreEvent::Sync(status)).is_ok()
    }
}
