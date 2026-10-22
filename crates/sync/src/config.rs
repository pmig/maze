//! Configuration of an account's sync engine.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mazemail_types::AccountId;
pub use settings::OfflinePolicy;

/// How one account is synced. Build with [`AccountSyncConfig::new`] and adjust fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSyncConfig {
    /// The account in the store.
    pub account: AccountId,
    /// Which bodies are downloaded ahead of time (CONCEPT §5.3).
    pub offline: OfflinePolicy,
    /// Days of bodies kept with [`OfflinePolicy::Recent`].
    pub offline_recent_days: u32,
    /// Worker sessions (besides the IDLE session): 1 or 2.
    pub workers: usize,
    /// Background body downloads running at the same time (PLAN §4.5: 2).
    pub background_body_fetches: usize,
    /// Headers fetched per `UID FETCH` and ingested per store transaction.
    pub header_batch: usize,
    /// UIDs per `UID FETCH … FLAGS` in the flag diff of servers without CONDSTORE.
    pub flag_chunk: usize,
    /// Messages larger than this (`RFC822.SIZE`) are opened with their text parts
    /// only; the full message follows in the background or on demand.
    pub large_message_bytes: u64,
    /// Timers.
    pub timing: SyncTiming,
    /// Wall clock for pending-op retry times and the offline-policy cutoff.
    pub clock: Clock,
}

impl AccountSyncConfig {
    /// The defaults of PLAN §4.5 for `account`.
    pub fn new(account: AccountId) -> Self {
        Self {
            account,
            offline: OfflinePolicy::default(),
            offline_recent_days: 90,
            workers: 2,
            background_body_fetches: 2,
            header_batch: 200,
            flag_chunk: 1000,
            large_message_bytes: 256 * 1024,
            timing: SyncTiming::default(),
            clock: Clock::system(),
        }
    }

    /// Sets the offline policy (and the days for [`OfflinePolicy::Recent`]).
    #[must_use]
    pub fn with_offline(mut self, policy: OfflinePolicy, recent_days: u32) -> Self {
        self.offline = policy;
        self.offline_recent_days = recent_days;
        self
    }

    /// Whether bodies are downloaded in the background at all.
    pub fn prefetches_bodies(&self) -> bool {
        matches!(self.offline, OfflinePolicy::Recent | OfflinePolicy::All)
    }

    /// The oldest message date (Unix seconds) whose body is downloaded in the
    /// background, or `None` if none are (see [`prefetches_bodies`](Self::prefetches_bodies)).
    /// `i64::MIN` means every body.
    pub(crate) fn body_cutoff(&self, now: i64) -> Option<i64> {
        match self.offline {
            OfflinePolicy::All => Some(i64::MIN),
            OfflinePolicy::Recent => Some(now - i64::from(self.offline_recent_days) * 86_400),
            OfflinePolicy::HeadersOnly | OfflinePolicy::OnDemand => None,
        }
    }

    pub(crate) fn workers(&self) -> usize {
        self.workers.clamp(1, 2)
    }
}

/// The wall clock as Unix seconds. [`Clock::system`] in production; tests use
/// [`Clock::from_fn`] to move time forward (for example past an op's retry time).
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> i64 + Send + Sync>);

impl Clock {
    /// The system clock.
    pub fn system() -> Self {
        Self(Arc::new(crate::shared::unix_now))
    }

    /// A custom clock.
    pub fn from_fn(now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    /// The current time, Unix seconds.
    pub fn now(&self) -> i64 {
        (self.0)()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Clock").field(&self.now()).finish()
    }
}

impl PartialEq for Clock {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Clock {}

/// The engine's timers (PLAN §4.5). Tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTiming {
    /// IDLE is re-issued after this long (RFC 2177: below 29 minutes).
    pub idle_reissue: Duration,
    /// `STATUS` poll of the other subscribed folders.
    pub poll_interval: Duration,
    /// `STATUS` poll of the folder open in the UI.
    pub open_folder_poll: Duration,
    /// A CONDSTORE folder (no QRESYNC) gets a `UID SEARCH ALL` expunge diff at
    /// least this often, besides the diff triggered by a message count mismatch.
    pub uid_diff_interval: Duration,
    /// Events within this window go out as one batch.
    pub coalesce_window: Duration,
    /// First reconnect delay; doubles per failure, with jitter.
    pub reconnect_initial: Duration,
    /// Longest reconnect delay.
    pub reconnect_max: Duration,
}

impl Default for SyncTiming {
    fn default() -> Self {
        Self {
            idle_reissue: Duration::from_secs(25 * 60),
            poll_interval: Duration::from_secs(2 * 60),
            open_folder_poll: Duration::from_secs(30),
            uid_diff_interval: Duration::from_secs(30 * 60),
            coalesce_window: Duration::from_millis(16),
            reconnect_initial: Duration::from_secs(2),
            reconnect_max: Duration::from_secs(5 * 60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_cutoff_follows_the_policy() {
        let config = AccountSyncConfig::new(AccountId(1));
        assert_eq!(config.offline, OfflinePolicy::Recent);
        assert_eq!(config.body_cutoff(100 * 86_400), Some(10 * 86_400));
        let all = config.clone().with_offline(OfflinePolicy::All, 0);
        assert_eq!(all.body_cutoff(0), Some(i64::MIN));
        for policy in [OfflinePolicy::HeadersOnly, OfflinePolicy::OnDemand] {
            let c = config.clone().with_offline(policy, 90);
            assert_eq!(c.body_cutoff(0), None);
            assert!(!c.prefetches_bodies());
        }
    }
}
