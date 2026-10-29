//! The public face of an account's engine: [`spawn_account`], [`AccountHandle`] and
//! the [`SyncCommand`]s it sends.

use std::sync::{Arc, Mutex, PoisonError};

use mazemail_types::{AccountId, FolderId, MessageId};
use store::{BodyState, OpId, Store};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::activity::{ActivityTracker, SyncActivity};
use crate::actor::Actor;
use crate::config::AccountSyncConfig;
use crate::connect::SessionFactory;
use crate::error::{Result, SyncError};
use crate::events::{SyncEventBatch, SyncStatus, spawn_coalescer};
use crate::ingest::MessageIngest;
use crate::shared::Shared;

/// What a [`SyncCommand::SyncNow`] covers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncScope {
    /// Folder list, pending ops and every folder, with an expunge diff.
    Account,
    /// One folder (a Gmail label syncs All Mail).
    Folder(FolderId),
    /// Some folders.
    Folders(Vec<FolderId>),
}

/// How urgent a body download is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyPriority {
    /// The user opened the message: jumps the queue; large messages get their text
    /// parts first.
    Interactive,
    /// Prefetch at low priority (throttled with the offline-policy downloads).
    Background,
    /// The user needs the whole message now (to open an attachment): jumps the queue
    /// like [`BodyPriority::Interactive`] but always fetches the full raw message.
    Full,
}

/// Commands to an account's engine. [`AccountHandle`] has a method for each.
#[derive(Debug)]
#[non_exhaustive]
pub enum SyncCommand {
    /// Sync now (the Get Messages button, `mazemail sync`). Also retries at once when
    /// offline or after a sign-in error.
    SyncNow {
        /// What to sync.
        scope: SyncScope,
    },
    /// The folder shown in the UI changed: it is synced first and polled every 30 s.
    OpenFolder(Option<FolderId>),
    /// Folders to check as often as the open folder (every 30 s) instead of with the
    /// other folders: the user's favorites. Replaces the previous list.
    WatchFolders(Vec<FolderId>),
    /// Download a message body.
    FetchBody {
        /// The message.
        message: MessageId,
        /// How urgent.
        priority: BodyPriority,
        /// Gets the result (what is stored now) when done.
        reply: Option<oneshot::Sender<Result<BodyState>>>,
    },
    /// A pending op was enqueued: replay the queue now.
    ApplyOp(OpId),
    /// The network changed: drop every connection, reconnect, run one pass.
    NetworkChanged,
    /// The computer woke from sleep: like [`SyncCommand::NetworkChanged`].
    Wake,
    /// New credentials are available (after a sign-in): retry at once.
    CredentialsChanged,
    /// Stop: finish the current batch, log out, end the task.
    Shutdown,
}

/// What the engine needs from the rest of Maze.
#[derive(Clone)]
pub struct SyncDeps {
    /// The store.
    pub store: Store,
    /// The ingest pipeline new headers and bodies go through.
    pub ingest: Arc<dyn MessageIngest>,
    /// Opens IMAP sessions for this account.
    pub sessions: Arc<dyn SessionFactory>,
}

impl std::fmt::Debug for SyncDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncDeps").finish_non_exhaustive()
    }
}

/// Starts the engine of one account on the current Tokio runtime.
///
/// It connects at once, replays pending ops, syncs the folder list and every folder,
/// then waits for changes (IDLE, polling) and commands.
///
/// # Panics
///
/// Outside a Tokio runtime.
pub fn spawn_account(config: AccountSyncConfig, deps: SyncDeps) -> AccountHandle {
    let account = config.account;
    let (events_tx, _) = broadcast::channel(1024);
    let (sink, coalescer) =
        spawn_coalescer(account, events_tx.clone(), config.timing.coalesce_window);
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let (status_tx, status_rx) = watch::channel(SyncStatus::Connecting);
    let activity = Arc::new(ActivityTracker::default());
    let shared = Arc::new(Shared {
        config,
        store: deps.store,
        ingest: deps.ingest,
        events: sink,
        activity: Arc::clone(&activity),
    });
    let actor = Actor::new(shared, deps.sessions, commands_rx, status_tx);
    let task = tokio::spawn(async move {
        Box::pin(actor.run()).await;
        // Let the coalescer flush the last events.
        let _ = coalescer.await;
    });
    AccountHandle {
        account,
        commands: commands_tx,
        events: events_tx,
        status: status_rx,
        activity,
        task: Arc::new(Mutex::new(Some(task))),
    }
}

/// A handle to a running account engine. Cheap to clone; the engine stops on
/// [`shutdown`](AccountHandle::shutdown) or when every handle is dropped.
#[derive(Debug, Clone)]
pub struct AccountHandle {
    account: AccountId,
    commands: mpsc::UnboundedSender<SyncCommand>,
    events: broadcast::Sender<Arc<SyncEventBatch>>,
    status: watch::Receiver<SyncStatus>,
    activity: Arc<ActivityTracker>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AccountHandle {
    /// The account.
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// Event batches (at most one per coalescing window). A slow receiver lags and
    /// should reload what it shows.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SyncEventBatch>> {
        self.events.subscribe()
    }

    /// The current status.
    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    /// What the engine is doing right now: folder passes with their progress, op
    /// replay, the body queue, the last finished round. A cheap snapshot; take one
    /// whenever it is shown.
    pub fn activity(&self) -> SyncActivity {
        self.activity.snapshot()
    }

    /// A receiver that sees every status change.
    pub fn status_watch(&self) -> watch::Receiver<SyncStatus> {
        self.status.clone()
    }

    /// Sends a raw command.
    ///
    /// # Errors
    ///
    /// [`SyncError::Stopped`] if the engine has stopped.
    pub fn send(&self, command: SyncCommand) -> Result<()> {
        self.commands.send(command).map_err(|_| SyncError::Stopped)
    }

    /// [`SyncCommand::SyncNow`].
    pub fn sync_now(&self, scope: SyncScope) -> Result<()> {
        self.send(SyncCommand::SyncNow { scope })
    }

    /// [`SyncCommand::OpenFolder`].
    pub fn open_folder(&self, folder: Option<FolderId>) -> Result<()> {
        self.send(SyncCommand::OpenFolder(folder))
    }

    /// [`SyncCommand::WatchFolders`].
    pub fn watch_folders(&self, folders: Vec<FolderId>) -> Result<()> {
        self.send(SyncCommand::WatchFolders(folders))
    }

    /// Downloads a body and waits for it ([`SyncCommand::FetchBody`]).
    ///
    /// # Errors
    ///
    /// [`SyncError::Offline`] when the account can't reach the server,
    /// [`SyncError::NoServerCopy`] when the message is gone from the server, or the
    /// download's error.
    pub async fn fetch_body(
        &self,
        message: MessageId,
        priority: BodyPriority,
    ) -> Result<BodyState> {
        let (reply, rx) = oneshot::channel();
        self.send(SyncCommand::FetchBody {
            message,
            priority,
            reply: Some(reply),
        })?;
        rx.await.map_err(|_| SyncError::Stopped)?
    }

    /// [`SyncCommand::ApplyOp`].
    pub fn apply_op(&self, op: OpId) -> Result<()> {
        self.send(SyncCommand::ApplyOp(op))
    }

    /// [`SyncCommand::NetworkChanged`].
    pub fn network_changed(&self) -> Result<()> {
        self.send(SyncCommand::NetworkChanged)
    }

    /// [`SyncCommand::Wake`].
    pub fn wake(&self) -> Result<()> {
        self.send(SyncCommand::Wake)
    }

    /// [`SyncCommand::CredentialsChanged`].
    pub fn credentials_changed(&self) -> Result<()> {
        self.send(SyncCommand::CredentialsChanged)
    }

    /// Stops the engine and waits until it has logged out.
    pub async fn shutdown(&self) {
        let _ = self.commands.send(SyncCommand::Shutdown);
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}
