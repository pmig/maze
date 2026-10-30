//! The per-account actor: owns the sessions, schedules jobs by priority, runs IDLE,
//! polling, reconnects and the background body queue.
//!
//! # Scheduling
//!
//! All decisions happen on the actor task; work runs in spawned tasks that borrow a
//! worker session and give it back when done ("session lending"). Jobs:
//!
//! | Job | Lane | Priority |
//! | --- | --- | --- |
//! | body for an opened message | body | 100 |
//! | pass of the open folder | sync | 90 |
//! | pending-op replay | sync | 80 |
//! | folder list | sync | 70 |
//! | pass requested by the user | sync | 65 |
//! | pass after IDLE/STATUS saw a change | sync | 60 |
//! | round passes (Inbox 56, Sent/Drafts 52, others 50, Trash/Junk 45) | sync | 45–56 |
//! | STATUS poll | sync | 40 |
//! | background body (offline policy) | body | lowest, ≤ 2 in flight |
//!
//! The sync lane runs one job at a time (ops replay never races a pass, so UIDs learned
//! from COPYUID are in place before the destination is synced). A running pass yields
//! after its current header batch when a more urgent job waits for the lane or for a
//! session; it is requeued and resumes without refetching (stored UIDs are skipped).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use imap::{CancellationToken, IdleOutcome, SessionEvent};
use mazemail_types::{FolderId, FolderRole, MessageId};
use store::{BodyState, ListFilter, ListKey, ListScope, Page, Queries, SortOrder};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id as TaskId, JoinError, JoinSet};
use tokio::time::Instant;

use crate::backoff::Backoff;
use crate::bodies;
use crate::connect::{Conn, SessionFactory};
use crate::error::{Result, SyncError};
use crate::events::{SyncEvent, SyncStatus};
use crate::folders::{self, FolderKind, FolderList, FolderMeta};
use crate::handle::{BodyPriority, SyncCommand, SyncScope};
use crate::pass::{self, PassOptions, PassReport};
use crate::replay::{self, ReplayReport};
use crate::shared::{Shared, YieldFlag};

const P_BODY_INTERACTIVE: u8 = 100;
const P_OPEN_FOLDER: u8 = 90;
const P_OPS: u8 = 80;
const P_FOLDER_LIST: u8 = 70;
const P_USER_SYNC: u8 = 65;
const P_CHANGED: u8 = 60;
const P_POLL: u8 = 40;
const P_BACKGROUND_BODY: u8 = 0;

/// How long shutdown waits for running jobs before aborting them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

fn round_priority(role: Option<FolderRole>) -> u8 {
    match role {
        Some(FolderRole::Inbox | FolderRole::All) => 56,
        Some(FolderRole::Sent | FolderRole::Drafts) => 52,
        Some(FolderRole::Trash | FolderRole::Junk) => 45,
        _ => 50,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Job {
    FolderList,
    ReplayOps,
    Pass {
        folder: FolderId,
        opts: PassOptions,
    },
    Poll {
        folders: Vec<FolderId>,
    },
    Body {
        message: MessageId,
        interactive: bool,
    },
}

impl Job {
    fn sync_lane(&self) -> bool {
        !matches!(self, Self::Body { .. })
    }
}

#[derive(Debug)]
struct Queued {
    job: Job,
    priority: u8,
    seq: u64,
}

#[derive(Debug)]
struct Running {
    job: Job,
    priority: u8,
    flag: YieldFlag,
}

#[derive(Debug)]
enum JobOutput {
    FolderList(Result<FolderList>),
    Ops(Result<ReplayReport>),
    Pass(Result<PassReport>),
    Poll(Result<Vec<FolderId>>),
    Body(Result<BodyState>),
}

impl JobOutput {
    fn error(&self) -> Option<&SyncError> {
        match self {
            Self::FolderList(r) => r.as_ref().err(),
            Self::Ops(r) => r.as_ref().err(),
            Self::Pass(r) => r.as_ref().err(),
            Self::Poll(r) => r.as_ref().err(),
            Self::Body(r) => r.as_ref().err(),
        }
    }
}

enum TaskDone {
    Connected(Result<Conn>),
    Job {
        job: Job,
        conn: Option<Conn>,
        output: JobOutput,
    },
    Idle(Result<()>),
    Backfill(Result<Vec<MessageId>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Connect,
    Job,
    Idle,
    Backfill,
}

#[derive(Debug)]
enum IdleState {
    Off,
    Running {
        folder: FolderId,
        cancel: CancellationToken,
        task: TaskId,
    },
    Waiting {
        until: Instant,
    },
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
enum IdleSignal {
    Changed(FolderId),
}

type Waiter = oneshot::Sender<Result<BodyState>>;

pub(crate) struct Actor {
    shared: Arc<Shared>,
    sessions: Arc<dyn SessionFactory>,
    commands: mpsc::UnboundedReceiver<SyncCommand>,
    status_tx: watch::Sender<SyncStatus>,
    idle_tx: mpsc::UnboundedSender<IdleSignal>,
    idle_rx: mpsc::UnboundedReceiver<IdleSignal>,

    // Sessions.
    free: Vec<Conn>,
    live: usize,
    connecting: usize,
    backoff: Backoff,
    retry_at: Option<Instant>,
    offline: Option<(String, Duration)>,
    auth_error: Option<String>,
    caps_idle: bool,

    // Jobs.
    queue: Vec<Queued>,
    seq: u64,
    running: HashMap<TaskId, Running>,
    kinds: HashMap<TaskId, TaskKind>,
    tasks: JoinSet<TaskDone>,

    // Folders.
    folders: HashMap<FolderId, FolderMeta>,
    folders_known: bool,
    gmail: bool,
    open_folder: Option<FolderId>,
    /// Folders polled with the open folder ([`SyncCommand::WatchFolders`]).
    watched: Vec<FolderId>,
    unresolved: HashSet<FolderId>,
    last_uid_diff: HashMap<FolderId, Instant>,
    round_pending: Option<bool>,
    last_folder_list: Option<Instant>,

    // Timers.
    next_poll: Option<Instant>,
    next_open_poll: Option<Instant>,
    ops_due: Option<Instant>,

    // IDLE.
    idle: IdleState,
    idle_backoff: Backoff,

    // Bodies.
    background: VecDeque<MessageId>,
    background_set: HashSet<MessageId>,
    background_running: usize,
    backfill_started: bool,
    waiters: HashMap<MessageId, Vec<Waiter>>,

    // Status.
    round_done: usize,
    job_error: Option<String>,
    reconnect_round: bool,
    published: SyncStatus,
    stopping: bool,
}

impl Actor {
    pub(crate) fn new(
        shared: Arc<Shared>,
        sessions: Arc<dyn SessionFactory>,
        commands: mpsc::UnboundedReceiver<SyncCommand>,
        status_tx: watch::Sender<SyncStatus>,
    ) -> Self {
        let timing = shared.config.timing;
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        Self {
            shared,
            sessions,
            commands,
            status_tx,
            idle_tx,
            idle_rx,
            free: Vec::new(),
            live: 0,
            connecting: 0,
            backoff: Backoff::new(timing.reconnect_initial, timing.reconnect_max),
            retry_at: None,
            offline: None,
            auth_error: None,
            caps_idle: false,
            queue: Vec::new(),
            seq: 0,
            running: HashMap::new(),
            kinds: HashMap::new(),
            tasks: JoinSet::new(),
            folders: HashMap::new(),
            folders_known: false,
            gmail: false,
            open_folder: None,
            watched: Vec::new(),
            unresolved: HashSet::new(),
            last_uid_diff: HashMap::new(),
            round_pending: None,
            last_folder_list: None,
            next_poll: None,
            next_open_poll: None,
            ops_due: None,
            idle: IdleState::Off,
            idle_backoff: Backoff::new(timing.reconnect_initial, timing.reconnect_max),
            background: VecDeque::new(),
            background_set: HashSet::new(),
            background_running: 0,
            backfill_started: false,
            waiters: HashMap::new(),
            round_done: 0,
            job_error: None,
            reconnect_round: false,
            published: SyncStatus::Connecting,
            stopping: false,
        }
    }

    pub(crate) async fn run(mut self) {
        tracing::info!(account = %self.shared.account(), "sync engine started");
        self.start_round(false);
        loop {
            self.dispatch();
            self.publish_status();
            self.publish_activity();
            if self.stopping {
                break;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command),
                    None => self.stopping = true,
                },
                Some(done) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    self.on_task(done);
                }
                Some(signal) = self.idle_rx.recv() => self.on_idle_signal(signal),
                () = sleep_until(deadline) => self.on_timers(),
            }
        }
        self.shutdown().await;
    }

    // ---- commands ---------------------------------------------------------------------

    fn on_command(&mut self, command: SyncCommand) {
        match command {
            SyncCommand::SyncNow { scope } => {
                self.retry_now();
                match scope {
                    SyncScope::Account => self.start_round(true),
                    SyncScope::Folder(folder) => self.sync_folders(&[folder]),
                    SyncScope::Folders(folders) => self.sync_folders(&folders),
                }
            }
            SyncCommand::OpenFolder(folder) => {
                self.open_folder = folder;
                self.schedule_open_poll(true);
                if let Some(target) = folder.and_then(|f| self.mailbox_for(f)) {
                    self.enqueue_pass(target, P_OPEN_FOLDER, PassOptions::default());
                }
            }
            SyncCommand::WatchFolders(folders) => {
                self.watched = folders;
                self.schedule_open_poll(false);
            }
            SyncCommand::FetchBody {
                message,
                priority,
                reply,
            } => self.request_body(message, priority, reply),
            SyncCommand::ApplyOp(op) => {
                tracing::debug!(%op, "op enqueued; replaying");
                self.ops_due = None;
                self.enqueue(Job::ReplayOps, P_OPS);
            }
            SyncCommand::NetworkChanged | SyncCommand::Wake => self.reset_connections(),
            SyncCommand::CredentialsChanged => {
                self.auth_error = None;
                self.retry_now();
                if matches!(self.idle, IdleState::Waiting { .. }) {
                    self.idle = IdleState::Off;
                }
            }
            SyncCommand::Shutdown => self.stopping = true,
        }
    }

    fn sync_folders(&mut self, folders: &[FolderId]) {
        for folder in folders {
            if let Some(target) = self.mailbox_for(*folder) {
                let opts = PassOptions {
                    force_uid_diff: true,
                    ..PassOptions::default()
                };
                self.enqueue_pass(target, P_USER_SYNC, opts);
            }
        }
    }

    fn retry_now(&mut self) {
        self.auth_error = None;
        self.retry_at = None;
        self.backoff.reset();
    }

    fn request_body(&mut self, message: MessageId, priority: BodyPriority, reply: Option<Waiter>) {
        match priority {
            BodyPriority::Interactive | BodyPriority::Full => {
                if self.unavailable() {
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(SyncError::Offline));
                    }
                    return;
                }
                if let Some(reply) = reply {
                    self.waiters.entry(message).or_default().push(reply);
                }
                self.background.retain(|m| *m != message);
                self.background_set.remove(&message);
                self.enqueue(
                    Job::Body {
                        message,
                        interactive: priority == BodyPriority::Interactive,
                    },
                    P_BODY_INTERACTIVE,
                );
            }
            BodyPriority::Background => {
                if let Some(reply) = reply {
                    self.waiters.entry(message).or_default().push(reply);
                }
                if self.background_set.insert(message) {
                    self.background.push_front(message);
                }
            }
        }
    }

    /// Network change or wake: drop everything, reconnect, run one round.
    fn reset_connections(&mut self) {
        tracing::info!("network changed or woke up; reconnecting");
        self.free.clear();
        self.live = self.running_job_count();
        self.cancel_idle();
        // Running jobs and connects are aborted; their results requeue the jobs.
        self.tasks.abort_all();
        self.retry_now();
        self.offline = None;
        self.start_round(false);
    }

    fn running_job_count(&self) -> usize {
        self.running.len()
    }

    /// Ops, folder list, then every folder.
    fn start_round(&mut self, force_uid_diff: bool) {
        self.enqueue(Job::ReplayOps, P_OPS);
        self.enqueue(Job::FolderList, P_FOLDER_LIST);
        self.round_pending = Some(self.round_pending.unwrap_or(false) || force_uid_diff);
    }

    fn schedule_round(&mut self, force_uid_diff: bool) {
        let metas: Vec<FolderMeta> = self
            .folders
            .values()
            .filter(|m| m.kind == FolderKind::Mailbox)
            .cloned()
            .collect();
        let open = self.open_folder.and_then(|f| self.mailbox_for(f));
        for meta in metas {
            let priority = if Some(meta.id) == open {
                P_OPEN_FOLDER
            } else {
                round_priority(meta.role)
            };
            let opts = PassOptions {
                force_uid_diff,
                ..PassOptions::default()
            };
            self.enqueue_pass(meta.id, priority, opts);
        }
    }

    /// The mailboxes polled every 30 s: the open folder and the watched ones, without
    /// the one IDLE watches while it runs.
    fn fast_polled(&self) -> Vec<FolderId> {
        let idle_folder = self
            .idle_running()
            .then(|| self.idle_folder().map(|m| m.id))
            .flatten();
        let mut folders = Vec::new();
        for folder in self.open_folder.iter().chain(&self.watched) {
            if let Some(mailbox) = self.mailbox_for(*folder)
                && Some(mailbox) != idle_folder
                && !folders.contains(&mailbox)
            {
                folders.push(mailbox);
            }
        }
        folders
    }

    /// Starts or stops the 30 s poll after the open or watched folders changed.
    /// `restart` starts a full interval even if a poll is due sooner.
    fn schedule_open_poll(&mut self, restart: bool) {
        let wanted = self.open_folder.is_some() || !self.watched.is_empty();
        if !wanted {
            self.next_open_poll = None;
        } else if restart || self.next_open_poll.is_none() {
            self.next_open_poll = Some(Instant::now() + self.shared.config.timing.open_folder_poll);
        }
    }

    /// The mailbox whose pass refreshes `folder` (a Gmail label lives in All Mail).
    fn mailbox_for(&self, folder: FolderId) -> Option<FolderId> {
        match self.folders.get(&folder).map(|m| m.kind) {
            Some(FolderKind::Mailbox) => Some(folder),
            Some(FolderKind::Label) => self.all_mail(),
            Some(FolderKind::Placeholder) => None,
            // Not known (yet): let the pass find out.
            None => (!self.folders_known).then_some(folder),
        }
    }

    fn all_mail(&self) -> Option<FolderId> {
        self.folders
            .values()
            .find(|m| m.kind == FolderKind::Mailbox && m.role == Some(FolderRole::All))
            .map(|m| m.id)
    }

    /// The folder IDLE watches: All Mail on Gmail, else the Inbox.
    fn idle_folder(&self) -> Option<&FolderMeta> {
        let role = if self.gmail {
            FolderRole::All
        } else {
            FolderRole::Inbox
        };
        self.folders
            .values()
            .find(|m| m.kind == FolderKind::Mailbox && m.role == Some(role))
    }

    // ---- queue ------------------------------------------------------------------------

    fn enqueue_pass(&mut self, folder: FolderId, priority: u8, opts: PassOptions) {
        self.enqueue(Job::Pass { folder, opts }, priority);
    }

    fn enqueue(&mut self, job: Job, priority: u8) {
        if self.stopping {
            return;
        }
        for queued in &mut self.queue {
            let merged = match (&mut queued.job, &job) {
                (Job::FolderList, Job::FolderList) | (Job::ReplayOps, Job::ReplayOps) => true,
                (
                    Job::Pass { folder: a, opts },
                    Job::Pass {
                        folder: b,
                        opts: new,
                    },
                ) if a == b => {
                    opts.force_uid_diff |= new.force_uid_diff;
                    opts.claim_unassigned |= new.claim_unassigned;
                    true
                }
                (Job::Poll { folders }, Job::Poll { folders: new }) => {
                    for f in new {
                        if !folders.contains(f) {
                            folders.push(*f);
                        }
                    }
                    true
                }
                (
                    Job::Body {
                        message: a,
                        interactive,
                    },
                    Job::Body {
                        message: b,
                        interactive: new,
                    },
                ) if a == b => {
                    *interactive |= *new;
                    true
                }
                _ => false,
            };
            if merged {
                queued.priority = queued.priority.max(priority);
                return;
            }
        }
        self.seq += 1;
        self.queue.push(Queued {
            job,
            priority,
            seq: self.seq,
        });
    }

    fn sync_lane_busy(&self) -> bool {
        self.running.values().any(|r| r.job.sync_lane())
    }

    /// The index of the next queued job that may start now.
    fn next_queued(&self) -> Option<usize> {
        let lane_busy = self.sync_lane_busy();
        self.queue
            .iter()
            .enumerate()
            .filter(|(_, q)| !(q.job.sync_lane() && lane_busy))
            .max_by(|(_, a), (_, b)| a.priority.cmp(&b.priority).then(b.seq.cmp(&a.seq)))
            .map(|(i, _)| i)
    }

    fn dispatch(&mut self) {
        if self.stopping {
            return;
        }
        loop {
            let next = self.next_queued();
            let background = next.is_none()
                && !self.background.is_empty()
                && self.background_running < self.shared.config.background_body_fetches.max(1);
            if next.is_none() && !background {
                break;
            }
            let Some(conn) = self.free.pop() else {
                self.ensure_connecting();
                break;
            };
            let (job, priority) = match next {
                Some(index) => {
                    let q = self.queue.remove(index);
                    (q.job, q.priority)
                }
                None => {
                    let Some(message) = self.background.pop_front() else {
                        self.free.push(conn);
                        break;
                    };
                    self.background_set.remove(&message);
                    self.background_running += 1;
                    (
                        Job::Body {
                            message,
                            interactive: false,
                        },
                        P_BACKGROUND_BODY,
                    )
                }
            };
            self.start_job(job, priority, conn);
        }
        self.update_yield_flags();
        self.ensure_idle();
    }

    fn start_job(&mut self, mut job: Job, priority: u8, conn: Conn) {
        if let Job::Pass { folder, opts } = &mut job {
            let interval = self.shared.config.timing.uid_diff_interval;
            let due = self
                .last_uid_diff
                .get(folder)
                .is_none_or(|t| t.elapsed() >= interval);
            if due && conn.caps.condstore() && !conn.caps.qresync() {
                opts.force_uid_diff = true;
            }
            opts.claim_unassigned |= self.unresolved.contains(folder);
        }
        let flag = YieldFlag::default();
        let shared = Arc::clone(&self.shared);
        let task_job = job.clone();
        let task_flag = flag.clone();
        let handle = self
            .tasks
            .spawn(async move { run_job(shared, conn, task_job, task_flag).await });
        self.kinds.insert(handle.id(), TaskKind::Job);
        self.running.insert(
            handle.id(),
            Running {
                job,
                priority,
                flag,
            },
        );
    }

    /// Asks running sync-lane jobs to stop after their batch when something more
    /// urgent waits.
    fn update_yield_flags(&self) {
        let no_session = self.free.is_empty();
        for running in self.running.values() {
            let wanted = self.stopping
                || (running.job.sync_lane()
                    && self.queue.iter().any(|q| {
                        q.priority > running.priority && (q.job.sync_lane() || no_session)
                    }));
            running.flag.set(wanted);
        }
    }

    fn ensure_connecting(&mut self) {
        if self.stopping || self.auth_error.is_some() {
            return;
        }
        if self.retry_at.is_some_and(|t| t > Instant::now()) {
            return;
        }
        let workers = self.shared.config.workers();
        let offline_probe = self.live == 0 && self.connecting > 0;
        if self.live + self.connecting >= workers || offline_probe {
            return;
        }
        self.connecting += 1;
        let factory = Arc::clone(&self.sessions);
        let handle = self.tasks.spawn(async move {
            let result = match factory.connect().await {
                Ok(session) => Conn::new(session).await,
                Err(err) => Err(err),
            };
            TaskDone::Connected(result)
        });
        self.kinds.insert(handle.id(), TaskKind::Connect);
    }

    // ---- IDLE ---------------------------------------------------------------------------

    fn ensure_idle(&mut self) {
        if self.stopping || self.auth_error.is_some() || !self.folders_known || self.live == 0 {
            return;
        }
        let desired = self.idle_folder().map(|m| (m.id, m.remote.clone()));
        match &self.idle {
            IdleState::Unsupported => return,
            IdleState::Waiting { until } if *until > Instant::now() => return,
            IdleState::Running { folder, .. } => {
                if desired.as_ref().map(|(id, _)| *id) != Some(*folder) {
                    self.cancel_idle();
                }
                return;
            }
            _ => {}
        }
        if !self.caps_idle {
            self.idle = IdleState::Unsupported;
            return;
        }
        let Some((folder, remote)) = desired else {
            return;
        };
        let cancel = CancellationToken::new();
        let factory = Arc::clone(&self.sessions);
        let shared = Arc::clone(&self.shared);
        let tx = self.idle_tx.clone();
        let reissue = self.shared.config.timing.idle_reissue;
        let token = cancel.clone();
        let handle = self.tasks.spawn(async move {
            TaskDone::Idle(run_idle(shared, factory, folder, remote, token, reissue, tx).await)
        });
        self.kinds.insert(handle.id(), TaskKind::Idle);
        self.idle = IdleState::Running {
            folder,
            cancel,
            task: handle.id(),
        };
    }

    fn cancel_idle(&mut self) {
        if let IdleState::Running { cancel, .. } = &self.idle {
            cancel.cancel();
        }
        if !matches!(self.idle, IdleState::Unsupported) {
            self.idle = IdleState::Off;
        }
    }

    fn idle_running(&self) -> bool {
        matches!(self.idle, IdleState::Running { .. })
    }

    fn on_idle_signal(&mut self, signal: IdleSignal) {
        match signal {
            IdleSignal::Changed(folder) => {
                self.idle_backoff.reset();
                self.enqueue_pass(folder, P_CHANGED, PassOptions::default());
            }
        }
    }

    // ---- timers -------------------------------------------------------------------------

    fn next_deadline(&self) -> Option<Instant> {
        let idle_wait = match self.idle {
            IdleState::Waiting { until } => Some(until),
            _ => None,
        };
        let retry = self
            .retry_at
            .filter(|_| self.auth_error.is_none() && !self.queue.is_empty());
        [
            retry,
            self.next_poll,
            self.next_open_poll,
            self.ops_due,
            idle_wait,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn on_timers(&mut self) {
        let now = Instant::now();
        let timing = self.shared.config.timing;
        if self.retry_at.is_some_and(|t| t <= now) {
            self.retry_at = None;
        }
        if self.ops_due.is_some_and(|t| t <= now) {
            self.ops_due = None;
            self.enqueue(Job::ReplayOps, P_OPS);
        }
        if let IdleState::Waiting { until } = self.idle
            && until <= now
        {
            self.idle = IdleState::Off;
        }
        if self.next_poll.is_some_and(|t| t <= now) {
            self.next_poll = Some(now + timing.poll_interval);
            if !self.unavailable() {
                let skip_idle = self
                    .idle_running()
                    .then(|| self.idle_folder().map(|m| m.id));
                let fast = self.fast_polled();
                let folders: Vec<FolderId> = self
                    .folders
                    .values()
                    .filter(|m| m.kind == FolderKind::Mailbox)
                    .map(|m| m.id)
                    .filter(|id| skip_idle.flatten() != Some(*id) && !fast.contains(id))
                    .collect();
                if !folders.is_empty() {
                    self.enqueue(Job::Poll { folders }, P_POLL);
                }
            }
        }
        if self.next_open_poll.is_some_and(|t| t <= now) {
            self.next_open_poll = Some(now + timing.open_folder_poll);
            if !self.unavailable() {
                let mut folders = self.fast_polled();
                let idle_folder = self.idle_folder().map(|m| m.id);
                // Without IDLE, the Inbox is watched like the open folder.
                if !self.idle_running()
                    && let Some(inbox) = idle_folder
                    && !folders.contains(&inbox)
                {
                    folders.push(inbox);
                }
                if !folders.is_empty() {
                    self.enqueue(Job::Poll { folders }, P_CHANGED);
                }
            }
        }
    }

    // ---- task results -------------------------------------------------------------------

    fn on_task(&mut self, done: std::result::Result<(TaskId, TaskDone), JoinError>) {
        match done {
            Ok((id, done)) => {
                self.kinds.remove(&id);
                let running = self.running.remove(&id);
                match done {
                    TaskDone::Connected(result) => self.on_connected(result),
                    TaskDone::Job { job, conn, output } => {
                        let priority = running.map_or(P_CHANGED, |r| r.priority);
                        self.on_job_done(job, priority, conn, output);
                    }
                    TaskDone::Idle(result) => {
                        // A task cancelled earlier (network change) is not the current one.
                        if self.is_current_idle(id) {
                            self.on_idle_done(result);
                        }
                    }
                    TaskDone::Backfill(result) => self.on_backfill(result),
                }
            }
            Err(err) => {
                let id = err.id();
                let kind = self.kinds.remove(&id);
                if err.is_panic() {
                    tracing::error!(?kind, "a sync task panicked");
                }
                match kind {
                    Some(TaskKind::Connect) => self.connecting = self.connecting.saturating_sub(1),
                    Some(TaskKind::Job) => {
                        self.live = self.live.saturating_sub(1);
                        if let Some(running) = self.running.remove(&id) {
                            if matches!(
                                running.job,
                                Job::Body {
                                    interactive: false,
                                    ..
                                }
                            ) {
                                self.background_running = self.background_running.saturating_sub(1);
                            }
                            self.enqueue(running.job, running.priority);
                        }
                    }
                    Some(TaskKind::Idle) => {
                        if self.is_current_idle(id) {
                            self.idle = IdleState::Off;
                        }
                    }
                    Some(TaskKind::Backfill) | None => {}
                }
            }
        }
    }

    fn on_connected(&mut self, result: Result<Conn>) {
        self.connecting = self.connecting.saturating_sub(1);
        match result {
            Ok(conn) => {
                tracing::debug!("IMAP session connected");
                let was_offline = self.offline.take().is_some();
                self.live += 1;
                self.backoff.reset();
                self.retry_at = None;
                self.auth_error = None;
                self.gmail = conn.caps.gmail();
                self.caps_idle = conn.caps.idle();
                if matches!(self.idle, IdleState::Unsupported) && self.caps_idle {
                    self.idle = IdleState::Off;
                }
                self.free.push(conn);
                if was_offline || self.reconnect_round {
                    self.reconnect_round = false;
                    self.start_round(false);
                }
            }
            Err(err) if err.is_auth() => {
                tracing::warn!(error = %err, "sign-in failed; sync paused until credentials change");
                self.auth_error = Some(auth_text(&err));
                self.fail_interactive(|| SyncError::Offline);
            }
            Err(err) => {
                let delay = self.backoff.next_delay();
                self.retry_at = Some(Instant::now() + delay);
                if self.live == 0 {
                    tracing::info!(error = %err, ?delay, "server unreachable; retrying");
                    self.offline = Some((err.to_string(), delay));
                    self.fail_interactive(|| SyncError::Offline);
                }
            }
        }
    }

    fn on_job_done(&mut self, job: Job, priority: u8, conn: Option<Conn>, output: JobOutput) {
        if matches!(
            job,
            Job::Body {
                interactive: false,
                ..
            }
        ) {
            self.background_running = self.background_running.saturating_sub(1);
        }
        match conn {
            Some(conn) => self.free.push(conn),
            None => {
                self.live = self.live.saturating_sub(1);
                if self.live == 0 {
                    self.reconnect_round = true;
                }
            }
        }
        if let Some(err) = output.error()
            && (err.is_transient() || matches!(err, SyncError::Store(store::Error::Closed)))
        {
            tracing::info!(error = %err, "connection lost during a job; requeued");
            if let Job::Body {
                message,
                interactive: false,
            } = job
            {
                if self.background_set.insert(message) {
                    self.background.push_front(message);
                }
            } else {
                self.enqueue(job, priority);
            }
            return;
        }
        match (job, output) {
            (_, JobOutput::FolderList(result)) => match result {
                Ok(list) => self.on_folder_list(list),
                Err(err) => self.note_error("folder list", &err),
            },
            (_, JobOutput::Ops(result)) => match result {
                Ok(report) => self.on_ops(report),
                Err(err) => self.note_error("pending ops", &err),
            },
            (Job::Pass { folder, opts }, JobOutput::Pass(result)) => match result {
                Ok(report) => self.on_pass(folder, opts, priority, report),
                Err(SyncError::FolderNotSynced(_)) => {}
                Err(err) => {
                    self.note_error("folder sync", &err);
                    if matches!(err, SyncError::Imap(imap::ImapError::No { .. })) {
                        self.refresh_folder_list();
                    }
                }
            },
            (_, JobOutput::Poll(result)) => match result {
                Ok(changed) => {
                    for folder in changed {
                        self.enqueue_pass(folder, P_CHANGED, PassOptions::default());
                    }
                }
                Err(err) => self.note_error("status poll", &err),
            },
            (Job::Body { message, .. }, JobOutput::Body(result)) => {
                if let Err(SyncError::FolderNotSynced(folder)) = &result {
                    self.enqueue_pass(*folder, P_CHANGED, PassOptions::default());
                }
                if let Err(err) = &result {
                    tracing::debug!(%message, error = %err, "body download failed");
                }
                self.answer(message, result);
            }
            (job, output) => {
                tracing::error!(?job, ?output, "job output does not match its job");
            }
        }
    }

    fn refresh_folder_list(&mut self) {
        let recent = self
            .last_folder_list
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
        if !recent {
            self.enqueue(Job::FolderList, P_FOLDER_LIST);
        }
    }

    fn note_error(&mut self, what: &str, err: &SyncError) {
        tracing::warn!(error = %err, "{what} failed");
        self.job_error = Some(format!("{what}: {err}"));
    }

    fn on_folder_list(&mut self, list: FolderList) {
        self.last_folder_list = Some(Instant::now());
        self.job_error = None;
        self.gmail = list.gmail;
        self.folders = list.folders.into_iter().map(|m| (m.id, m)).collect();
        self.folders_known = true;
        if let Some(force) = self.round_pending.take() {
            self.schedule_round(force);
        }
    }

    fn on_ops(&mut self, report: ReplayReport) {
        self.ops_due = report.next_due.map(|due| {
            let wait = u64::try_from(due - self.shared.now()).unwrap_or(0);
            Instant::now() + Duration::from_secs(wait)
        });
        self.unresolved.extend(report.unresolved.iter().copied());
        for folder in report.touched {
            if let Some(target) = self.mailbox_for(folder) {
                self.enqueue_pass(target, P_CHANGED, PassOptions::default());
            }
        }
        if report.folders_changed {
            self.enqueue(Job::FolderList, P_FOLDER_LIST);
        }
    }

    fn on_pass(&mut self, folder: FolderId, opts: PassOptions, priority: u8, report: PassReport) {
        if report.did_uid_diff {
            self.last_uid_diff.insert(folder, Instant::now());
        }
        if report.complete {
            self.round_done += 1;
            if opts.claim_unassigned {
                self.unresolved.remove(&folder);
            }
        } else {
            self.enqueue_pass(folder, priority, opts);
        }
        for (message, _) in report.body_candidates {
            if self.background_set.insert(message) {
                self.background.push_back(message);
            }
        }
    }

    fn is_current_idle(&self, id: TaskId) -> bool {
        matches!(self.idle, IdleState::Running { task, .. } if task == id)
    }

    fn on_idle_done(&mut self, result: Result<()>) {
        match result {
            Ok(()) => {
                if !matches!(self.idle, IdleState::Unsupported) {
                    self.idle = IdleState::Off;
                }
            }
            Err(SyncError::Imap(imap::ImapError::Unsupported(_))) => {
                self.idle = IdleState::Unsupported;
            }
            Err(err) => {
                let delay = self.idle_backoff.next_delay();
                tracing::info!(error = %err, ?delay, "IDLE session ended; restarting later");
                self.idle = IdleState::Waiting {
                    until: Instant::now() + delay,
                };
            }
        }
    }

    fn on_backfill(&mut self, result: Result<Vec<MessageId>>) {
        match result {
            Ok(messages) => {
                tracing::debug!(count = messages.len(), "bodies to prefetch");
                for message in messages {
                    if self.background_set.insert(message) {
                        self.background.push_back(message);
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "finding bodies to prefetch failed"),
        }
    }

    /// Replies to everyone waiting for a body.
    fn answer(&mut self, message: MessageId, result: Result<BodyState>) {
        let Some(waiters) = self.waiters.remove(&message) else {
            return;
        };
        let mut result = Some(result);
        let count = waiters.len();
        for (i, waiter) in waiters.into_iter().enumerate() {
            let reply = if i + 1 == count {
                result.take().unwrap_or(Err(SyncError::Stopped))
            } else {
                match &result {
                    Some(Ok(state)) => Ok(*state),
                    Some(Err(err)) => Err(copy_error(err)),
                    None => Err(SyncError::Stopped),
                }
            };
            let _ = waiter.send(reply);
        }
    }

    /// Fails interactive body requests (the server can't be reached).
    fn fail_interactive(&mut self, error: impl Fn() -> SyncError) {
        let interactive: Vec<MessageId> = self
            .queue
            .iter()
            .filter_map(|q| match q.job {
                Job::Body {
                    message,
                    interactive: true,
                } => Some(message),
                _ => None,
            })
            .collect();
        self.queue.retain(|q| {
            !matches!(
                q.job,
                Job::Body {
                    interactive: true,
                    ..
                }
            )
        });
        for message in interactive {
            if let Some(waiters) = self.waiters.remove(&message) {
                for waiter in waiters {
                    let _ = waiter.send(Err(error()));
                }
            }
        }
    }

    // ---- status -------------------------------------------------------------------------

    fn unavailable(&self) -> bool {
        self.live == 0 && (self.offline.is_some() || self.auth_error.is_some())
    }

    fn pending_passes(&self) -> usize {
        let queued = self
            .queue
            .iter()
            .filter(|q| matches!(q.job, Job::Pass { .. }))
            .count();
        let running = self
            .running
            .values()
            .filter(|r| matches!(r.job, Job::Pass { .. } | Job::FolderList))
            .count();
        queued + running
    }

    fn compute_status(&self) -> SyncStatus {
        if self.stopping {
            return SyncStatus::Stopped;
        }
        if let Some(message) = &self.auth_error {
            return SyncStatus::AuthRequired {
                message: message.clone(),
            };
        }
        if self.live == 0
            && let Some((reason, delay)) = &self.offline
        {
            return SyncStatus::Offline {
                reason: reason.clone(),
                retry_in: Some(*delay),
            };
        }
        if self.live == 0 && self.connecting > 0 && !self.folders_known {
            return SyncStatus::Connecting;
        }
        let pending = self.pending_passes();
        if pending > 0 || self.round_pending.is_some() {
            return SyncStatus::Syncing {
                done: self.round_done,
                total: self.round_done + pending,
            };
        }
        match &self.job_error {
            Some(message) => SyncStatus::Error {
                message: message.clone(),
            },
            None => SyncStatus::Idle,
        }
    }

    fn publish_status(&mut self) {
        let status = self.compute_status();
        if status == SyncStatus::Idle || matches!(status, SyncStatus::Error { .. }) {
            self.round_done = 0;
            self.on_round_finished();
        }
        if status != self.published {
            if status == SyncStatus::Idle {
                self.shared.activity.synced(self.shared.now());
            }
            tracing::debug!(status = %status, "sync status");
            self.published = status.clone();
            self.status_tx.send_replace(status.clone());
            self.shared.events.emit(SyncEvent::Status(status));
        }
    }

    /// Tells the activity tracker which passes run and wait.
    fn publish_activity(&self) {
        let running: Vec<FolderId> = self
            .running
            .values()
            .filter_map(|r| match r.job {
                Job::Pass { folder, .. } => Some(folder),
                _ => None,
            })
            .collect();
        let mut queued: Vec<&Queued> = self
            .queue
            .iter()
            .filter(|q| matches!(q.job, Job::Pass { .. }))
            .collect();
        queued.sort_by_key(|q| (std::cmp::Reverse(q.priority), q.seq));
        let waiting = queued
            .into_iter()
            .filter_map(|q| match q.job {
                Job::Pass { folder, .. } => Some(folder),
                _ => None,
            })
            .collect();
        let replaying = self
            .running
            .values()
            .any(|r| matches!(r.job, Job::ReplayOps));
        self.shared.activity.jobs(
            running,
            waiting,
            replaying,
            self.background.len() + self.background_running,
        );
    }

    /// After a round: start polling and the body backfill (once).
    fn on_round_finished(&mut self) {
        if !self.folders_known {
            return;
        }
        if self.next_poll.is_none() {
            self.next_poll = Some(Instant::now() + self.shared.config.timing.poll_interval);
        }
        if !self.backfill_started
            && let Some(cutoff) = self.shared.config.body_cutoff(self.shared.now())
        {
            self.backfill_started = true;
            let shared = Arc::clone(&self.shared);
            let handle = self
                .tasks
                .spawn(async move { TaskDone::Backfill(backfill(&shared, cutoff).await) });
            self.kinds.insert(handle.id(), TaskKind::Backfill);
        }
    }

    // ---- shutdown -----------------------------------------------------------------------

    async fn shutdown(mut self) {
        self.stopping = true;
        self.cancel_idle();
        self.update_yield_flags();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !self.tasks.is_empty() {
            match tokio::time::timeout_at(deadline, self.tasks.join_next()).await {
                Ok(Some(Ok(TaskDone::Job {
                    conn: Some(conn), ..
                })))
                | Ok(Some(Ok(TaskDone::Connected(Ok(conn))))) => self.free.push(conn),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    self.tasks.abort_all();
                    while self.tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
        for mut conn in self.free.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.session.logout()).await;
        }
        for (_, waiters) in self.waiters.drain() {
            for waiter in waiters {
                let _ = waiter.send(Err(SyncError::Stopped));
            }
        }
        self.status_tx.send_replace(SyncStatus::Stopped);
        self.shared
            .events
            .emit(SyncEvent::Status(SyncStatus::Stopped));
        tracing::info!(account = %self.shared.account(), "sync engine stopped");
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn auth_text(err: &SyncError) -> String {
    match err {
        SyncError::Imap(imap::ImapError::Auth(text)) => text.clone(),
        SyncError::Credentials(c) => c.message.clone(),
        other => other.to_string(),
    }
}

/// A copy of an error for the second and later waiters of the same body.
fn copy_error(err: &SyncError) -> SyncError {
    match err {
        SyncError::Offline => SyncError::Offline,
        SyncError::Stopped => SyncError::Stopped,
        SyncError::MessageNotFound(m) => SyncError::MessageNotFound(*m),
        SyncError::NoServerCopy(m) => SyncError::NoServerCopy(*m),
        SyncError::FolderNotSynced(f) => SyncError::FolderNotSynced(*f),
        other => SyncError::Internal(other.to_string()),
    }
}

/// Runs one job on a borrowed session and gives the session back (unless broken).
async fn run_job(shared: Arc<Shared>, mut conn: Conn, job: Job, flag: YieldFlag) -> TaskDone {
    for alert in conn.drain_alerts() {
        shared.events.emit(SyncEvent::Alert(alert));
    }
    let output = match &job {
        Job::FolderList => {
            JobOutput::FolderList(folders::sync_folder_list(&shared, &mut conn).await)
        }
        Job::ReplayOps => JobOutput::Ops(replay::replay_ops(&shared, &mut conn, &flag).await),
        Job::Pass { folder, opts } => JobOutput::Pass(
            Box::pin(pass::sync_folder(&shared, &mut conn, *folder, *opts, &flag)).await,
        ),
        Job::Poll { folders } => JobOutput::Poll(poll(&shared, &mut conn, folders).await),
        Job::Body {
            message,
            interactive,
        } => JobOutput::Body(bodies::fetch_body(&shared, &mut conn, *message, *interactive).await),
    };
    for alert in conn.drain_alerts() {
        shared.events.emit(SyncEvent::Alert(alert));
    }
    let broken = conn.broken || output.error().is_some_and(SyncError::breaks_session);
    TaskDone::Job {
        job,
        conn: (!broken).then_some(conn),
        output,
    }
}

/// `STATUS` of each folder; returns those whose UIDNEXT, HIGHESTMODSEQ or (without
/// CONDSTORE) counts differ from the store.
async fn poll(shared: &Shared, conn: &mut Conn, folders: &[FolderId]) -> Result<Vec<FolderId>> {
    let ids = folders.to_vec();
    let rows = shared
        .read(move |r| {
            ids.iter()
                .map(|id| r.folder(*id))
                .collect::<store::Result<Vec<_>>>()
        })
        .await?;
    let mut changed = Vec::new();
    for folder in rows.into_iter().flatten() {
        let Some(remote) = folder.remote_name.clone().filter(|_| !folder.is_label) else {
            continue;
        };
        if conn.selected_folder() == Some(remote.as_str()) {
            // STATUS must not target the selected folder; a pass is cheap when nothing changed.
            changed.push(folder.id);
            continue;
        }
        let status = conn.session.status(&remote).await;
        let status = match conn.track(status) {
            Ok(status) => status,
            Err(err) if conn.broken => return Err(err),
            Err(err) => {
                tracing::debug!(folder = %folder.id, error = %err, "STATUS failed");
                continue;
            }
        };
        let sync = folder.sync;
        let differs = status.uidvalidity != sync.uidvalidity
            || status.uidnext != sync.uidnext
            || match status.highest_modseq {
                Some(modseq) => Some(modseq) != sync.highestmodseq,
                None => {
                    status.messages.map(u64::from) != Some(folder.total)
                        || status.unseen.map(u64::from) != Some(folder.unread)
                }
            };
        if differs {
            changed.push(folder.id);
        }
    }
    Ok(changed)
}

/// The IDLE session: select the folder, IDLE, report changes, re-issue every
/// `reissue`.
async fn run_idle(
    shared: Arc<Shared>,
    factory: Arc<dyn SessionFactory>,
    folder: FolderId,
    remote: String,
    cancel: CancellationToken,
    reissue: Duration,
    tx: mpsc::UnboundedSender<IdleSignal>,
) -> Result<()> {
    let session = tokio::select! {
        session = factory.connect() => session?,
        () = cancel.cancelled() => return Ok(()),
    };
    let mut conn = Conn::new(session).await?;
    if !conn.caps.idle() {
        return Err(SyncError::Imap(imap::ImapError::Unsupported("IDLE")));
    }
    conn.select(&remote, None).await?;
    let _ = conn.session.take_events();
    loop {
        let outcome = conn.session.idle(cancel.clone(), reissue).await;
        match conn.track(outcome)? {
            IdleOutcome::NewData => {
                let mut changed = false;
                for event in conn.session.take_events() {
                    match event {
                        SessionEvent::Alert(text) => shared.events.emit(SyncEvent::Alert(text)),
                        SessionEvent::Recent(_) => {}
                        _ => changed = true,
                    }
                }
                if changed && tx.send(IdleSignal::Changed(folder)).is_err() {
                    return Ok(());
                }
            }
            IdleOutcome::Timeout => {}
            IdleOutcome::Cancelled => {
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.session.logout()).await;
                return Ok(());
            }
        }
    }
}

/// Messages whose bodies the offline policy wants but which have none yet, newest
/// first: walks the account's threads newest-first until the cutoff. A message's age
/// is its stored date ([`message_date`](crate::ingest::message_date)), as in the pass.
async fn backfill(shared: &Shared, cutoff: i64) -> Result<Vec<MessageId>> {
    const PAGE: usize = 1000;
    let account = shared.account();
    shared
        .read(move |r| {
            let mut out = Vec::new();
            let mut after: Option<ListKey> = None;
            loop {
                let page = match after {
                    Some(key) => Page::after(key, PAGE),
                    None => Page::first(PAGE),
                };
                let keys = r.thread_list_keys(
                    ListScope::Account(account),
                    SortOrder::NewestFirst,
                    &ListFilter::default(),
                    page,
                )?;
                for key in &keys {
                    if key.key < cutoff {
                        return Ok(out);
                    }
                    let mut messages = Vec::new();
                    for id in r.thread_message_ids(key.thread)? {
                        if let Some(m) = r.message(id)?
                            && m.body_state == BodyState::None
                            && m.date >= cutoff
                            && m.locations.iter().any(|l| l.uid.is_some())
                        {
                            messages.push((m.date, id));
                        }
                    }
                    messages.sort_by_key(|m| std::cmp::Reverse(m.0));
                    out.extend(messages.into_iter().map(|(_, id)| id));
                }
                match keys.last() {
                    Some(last) if keys.len() == PAGE => after = Some(*last),
                    _ => return Ok(out),
                }
            }
        })
        .await
}
