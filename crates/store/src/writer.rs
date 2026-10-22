//! The single writer: a dedicated thread that owns the read-write connection and runs
//! write jobs in batched transactions (PLAN §4.3).
//!
//! # Batching
//!
//! The thread takes the first job from the channel, opens a transaction and keeps
//! running jobs that are already queued until the batch holds [`MAX_BATCH_WEIGHT`]
//! statements' worth of work, [`MAX_BATCH_TIME`] has passed, or the queue is empty;
//! then it commits. A lone job therefore commits at once (no added latency), and a busy
//! sync gets few, large transactions.
//!
//! Each job runs inside its own savepoint, so a failing job is rolled back alone and
//! the others in the batch still commit. Results are delivered only after the commit,
//! so an `Ok` from [`Store::write`](crate::Store::write) means the change is durable
//! (with `synchronous = NORMAL` in WAL mode: survives a crash of Maze, may be lost on
//! power loss; PLAN §4.3) and visible to readers. If the commit itself fails, every job
//! of the batch gets [`Error::Commit`].
//!
//! # Runtime
//!
//! The API is runtime-agnostic: futures resolve through `tokio::sync::oneshot`
//! channels, which need no Tokio runtime, and the writer is a plain OS thread.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use mazemail_types::ThreadId;
use rusqlite::Connection;
use tokio::sync::{broadcast, oneshot};

use crate::blob::{BlobStore, GcOptions, GcReport, StagedBlob};
use crate::changes::ChangeSet;
use crate::{Error, Result};

/// Upper bound of statements' worth of work per transaction.
pub const MAX_BATCH_WEIGHT: usize = 500;

/// Upper bound of time spent in one transaction before committing.
pub const MAX_BATCH_TIME: Duration = Duration::from_millis(20);

/// A queued job. It is called with the transaction to run in, or with the error that
/// kept the writer from opening one (then it only reports that error).
type JobFn = Box<dyn FnOnce(Result<&mut WriteTx<'_>, Error>) -> (bool, Box<dyn Deliver>) + Send>;

pub(crate) enum Command {
    Job {
        weight: usize,
        run: JobFn,
    },
    Gc {
        options: GcOptions,
        reply: oneshot::Sender<Result<GcReport>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

/// A job's result, held until the batch commits or fails.
pub(crate) trait Deliver: Send {
    fn committed(self: Box<Self>);
    fn aborted(self: Box<Self>, cause: &Abort);
}

/// Why a batch that ran was rolled back instead of committed.
#[derive(Debug, Clone)]
pub(crate) enum Abort {
    /// Making the referenced blobs durable failed (before `COMMIT`).
    Blobs(Arc<Error>),
    /// `COMMIT` failed.
    Commit(Arc<rusqlite::Error>),
}

impl Abort {
    fn error(&self) -> Error {
        match self {
            Self::Blobs(err) => Error::BlobsNotDurable(Arc::clone(err)),
            Self::Commit(err) => Error::Commit(Arc::clone(err)),
        }
    }
}

struct Reply<R> {
    tx: oneshot::Sender<Result<R>>,
    result: Result<R>,
}

impl<R: Send> Deliver for Reply<R> {
    fn committed(self: Box<Self>) {
        let _ = self.tx.send(self.result);
    }

    fn aborted(self: Box<Self>, cause: &Abort) {
        let result = match self.result {
            // The job itself had failed; report its own error.
            Err(err) => Err(err),
            Ok(_) => Err(cause.error()),
        };
        let _ = self.tx.send(result);
    }
}

/// Wraps a user closure into a job; returns the job and the receiver of its result.
pub(crate) fn job<R, F>(f: F) -> (JobFn, oneshot::Receiver<Result<R>>)
where
    R: Send + 'static,
    F: FnOnce(&mut WriteTx<'_>) -> Result<R> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let run: JobFn = Box::new(move |w: Result<&mut WriteTx<'_>, Error>| {
        let result = match w {
            Err(err) => Err(err),
            Ok(w) => match catch_unwind(AssertUnwindSafe(|| {
                f(w).and_then(|value| w.finish_job().map(|()| value))
            })) {
                Ok(result) => result,
                Err(_) => {
                    tracing::error!("a store write job panicked; it was rolled back");
                    Err(Error::Invalid("write job panicked"))
                }
            },
        };
        let ok = result.is_ok();
        (ok, Box::new(Reply { tx, result }) as Box<dyn Deliver>)
    });
    (run, rx)
}

/// The write side of one job: the connection inside the job's savepoint, plus the
/// bookkeeping the store does for you (aggregates, change set, the blobs it references,
/// which stay pinned and are made durable before the commit).
///
/// Repository methods are defined on this type across the crate's modules; read
/// methods come from [`Queries`](crate::Queries).
pub struct WriteTx<'c> {
    pub(crate) conn: &'c Connection,
    pub(crate) blobs: &'c BlobStore,
    pub(crate) now: i64,
    pub(crate) dirty_threads: BTreeSet<ThreadId>,
    pub(crate) changes: ChangeSet,
    pub(crate) blobs_referenced: Vec<StagedBlob>,
}

impl std::fmt::Debug for WriteTx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTx")
            .field("dirty_threads", &self.dirty_threads.len())
            .finish_non_exhaustive()
    }
}

impl<'c> WriteTx<'c> {
    fn new(conn: &'c Connection, blobs: &'c BlobStore) -> Self {
        Self {
            conn,
            blobs,
            now: unix_now(),
            dirty_threads: BTreeSet::new(),
            changes: ChangeSet::default(),
            blobs_referenced: Vec::new(),
        }
    }

    /// The time this job started (Unix seconds); used for `created`/`updated` columns.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// The blob store, to read blobs inside a job.
    pub fn blobs(&self) -> &BlobStore {
        self.blobs
    }

    /// Brings derived data (thread and folder aggregates) up to date; runs at the end
    /// of every job, inside its savepoint.
    fn finish_job(&mut self) -> Result<()> {
        self.flush_threads()
    }
}

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) struct WriterThread {
    conn: Connection,
    blobs: BlobStore,
    rx: Receiver<Command>,
    events: broadcast::Sender<Arc<ChangeSet>>,
    gc_interval: Option<Duration>,
}

impl WriterThread {
    pub(crate) fn spawn(
        conn: Connection,
        blobs: BlobStore,
        rx: Receiver<Command>,
        events: broadcast::Sender<Arc<ChangeSet>>,
        gc_interval: Option<Duration>,
    ) -> Result<JoinHandle<()>> {
        let thread = Self {
            conn,
            blobs,
            rx,
            events,
            gc_interval,
        };
        std::thread::Builder::new()
            .name("mazemail-store-writer".to_owned())
            .spawn(move || thread.run())
            .map_err(Error::io("starting the store writer thread"))
    }

    fn run(mut self) {
        let mut deferred: Option<Command> = None;
        let mut last_gc = Instant::now();
        loop {
            let command = match deferred.take() {
                Some(command) => command,
                None => match self.next_command(last_gc) {
                    Some(command) => command,
                    None => {
                        if self.gc_interval.is_some_and(|i| last_gc.elapsed() >= i) {
                            last_gc = Instant::now();
                            if let Err(err) =
                                self.blobs.collect(&mut self.conn, &GcOptions::default())
                            {
                                tracing::warn!(error = %err, "periodic blob collection failed");
                            }
                            continue;
                        }
                        match self.rx.try_recv() {
                            Ok(command) => deferred = Some(command),
                            Err(TryRecvError::Disconnected) => {
                                let _ = self.close();
                                return;
                            }
                            Err(TryRecvError::Empty) => {}
                        }
                        continue;
                    }
                },
            };
            match command {
                Command::Job { weight, run } => deferred = self.run_batch(weight, run),
                Command::Gc { options, reply } => {
                    last_gc = Instant::now();
                    let _ = reply.send(self.blobs.collect(&mut self.conn, &options));
                }
                Command::Shutdown { reply } => {
                    let _ = reply.send(self.close());
                    return;
                }
            }
        }
    }

    /// Waits for the next command; `None` on timeout (time for periodic GC) or when
    /// every sender is gone.
    fn next_command(&self, last_gc: Instant) -> Option<Command> {
        match self.gc_interval {
            Some(interval) => {
                let wait = interval.saturating_sub(last_gc.elapsed());
                self.rx
                    .recv_timeout(wait.max(Duration::from_millis(1)))
                    .ok()
            }
            None => self.rx.recv().ok(),
        }
    }

    /// Checkpoints the WAL so the database is one file at rest.
    fn close(&mut self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Runs `first` and whatever is queued behind it in one transaction. Returns a
    /// non-job command that ended the batch, to be handled next.
    fn run_batch(&mut self, first_weight: usize, first: JobFn) -> Option<Command> {
        let started = Instant::now();
        if let Err(err) = self.conn.execute_batch("BEGIN IMMEDIATE") {
            tracing::error!(error = %err, "could not begin a write transaction");
            let (_, reply) = first(Err(Error::Sqlite(err)));
            reply.committed();
            return None;
        }

        let mut batch = Batch::default();
        let mut weight = first_weight.max(1);
        self.run_job(first, &mut batch);
        let mut next = None;
        while weight < MAX_BATCH_WEIGHT && started.elapsed() < MAX_BATCH_TIME {
            match self.rx.try_recv() {
                Ok(Command::Job { weight: w, run }) => {
                    weight += w.max(1);
                    self.run_job(run, &mut batch);
                }
                Ok(other) => {
                    next = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }

        // A committed row must never point at a blob that isn't durable (see `blob`).
        let committed = self
            .blobs
            .persist(&batch.blobs)
            .map_err(|err| {
                tracing::error!(error = %err, "making blobs durable failed; the batch was rolled back");
                Abort::Blobs(Arc::new(err))
            })
            .and_then(|()| {
                self.conn.execute_batch("COMMIT").map_err(|err| {
                    tracing::error!(error = %err, "store commit failed; the batch was rolled back");
                    Abort::Commit(Arc::new(err))
                })
            });
        match committed {
            Ok(()) => {
                for reply in batch.replies {
                    reply.committed();
                }
                // Unpins the blobs: their rows hold references now.
                drop(batch.blobs);
                if !batch.changes.is_empty() {
                    let _ = self.events.send(Arc::new(batch.changes));
                }
            }
            Err(cause) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                for reply in batch.replies {
                    reply.aborted(&cause);
                }
            }
        }
        next
    }

    fn run_job(&self, run: JobFn, batch: &mut Batch) {
        if let Err(err) = self.conn.execute_batch("SAVEPOINT job") {
            // Can't isolate the job; fail it without running it.
            tracing::error!(error = %err, "could not open a savepoint");
            let (_, reply) = run(Err(Error::Sqlite(err)));
            batch.replies.push(reply);
            return;
        }
        let mut w = WriteTx::new(&self.conn, &self.blobs);
        let (ok, reply) = run(Ok(&mut w));
        let WriteTx {
            changes,
            blobs_referenced,
            ..
        } = w;
        if ok {
            match self.conn.execute_batch("RELEASE job") {
                Ok(()) => {
                    batch.changes.merge(changes);
                    batch.blobs.extend(blobs_referenced);
                }
                Err(err) => tracing::error!(error = %err, "could not release a savepoint"),
            }
        } else if let Err(err) = self.conn.execute_batch("ROLLBACK TO job; RELEASE job") {
            tracing::error!(error = %err, "could not roll back a failed job");
        }
        batch.replies.push(reply);
    }
}

#[derive(Default)]
struct Batch {
    replies: Vec<Box<dyn Deliver>>,
    changes: ChangeSet,
    blobs: Vec<StagedBlob>,
}

/// Creates the command channel.
pub(crate) fn channel() -> (mpsc::Sender<Command>, Receiver<Command>) {
    mpsc::channel()
}
