//! State shared by the account actor and its jobs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mazemail_types::AccountId;
use store::{Reader, Store, WriteTx};

use crate::activity::ActivityTracker;
use crate::config::AccountSyncConfig;
use crate::error::{Result, SyncError};
use crate::events::EventSink;
use crate::ingest::MessageIngest;

/// Everything a job needs besides its session.
pub(crate) struct Shared {
    pub(crate) config: AccountSyncConfig,
    pub(crate) store: Store,
    pub(crate) ingest: Arc<dyn MessageIngest>,
    pub(crate) events: EventSink,
    /// What the engine is doing, for [`crate::AccountHandle::activity`].
    pub(crate) activity: Arc<ActivityTracker>,
}

impl Shared {
    pub(crate) fn account(&self) -> AccountId {
        self.config.account
    }

    /// The configured wall clock, Unix seconds.
    pub(crate) fn now(&self) -> i64 {
        self.config.clock.now()
    }

    /// Runs a read on a pooled reader, off the async worker threads.
    pub(crate) async fn read<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Reader) -> store::Result<R> + Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.read(f))
            .await
            .map_err(|e| SyncError::Internal(format!("store read task: {e}")))?
            .map_err(SyncError::from)
    }

    /// Runs a write job of about `weight` statements and waits for its commit.
    pub(crate) async fn write<R, F>(&self, weight: usize, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> store::Result<R> + Send + 'static,
    {
        Ok(self.store.write_weighted(weight, f).await?)
    }
}

/// Set by the actor when a running job should stop at its next safe point (after the
/// current header batch) so that a more urgent job can use the session.
#[derive(Debug, Clone, Default)]
pub(crate) struct YieldFlag(Arc<AtomicBool>);

impl YieldFlag {
    pub(crate) fn requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, value: bool) {
        self.0.store(value, Ordering::Relaxed);
    }
}

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
