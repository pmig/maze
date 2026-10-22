//! Exponential backoff with jitter for reconnects, and a small jitter source.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

/// A random `u64` from the standard library's per-process random hasher keys. Good
/// enough for jitter; never used for anything security-related.
pub(crate) fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0x6d61_7a65);
    hasher.finish()
}

/// A random duration in `[0, max]`.
pub(crate) fn jitter(max: Duration) -> Duration {
    let nanos = u64::try_from(max.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(random_u64() % (nanos + 1))
}

/// Exponential backoff: `initial · 2^n`, capped at `max`, with "equal jitter" (the
/// delay is uniformly random in the upper half), so many clients reconnecting after
/// an outage spread out.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    initial: Duration,
    max: Duration,
    failures: u32,
}

impl Backoff {
    pub(crate) fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            failures: 0,
        }
    }

    /// The delay before the next attempt; counts a failure.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let exponent = self.failures.min(20);
        self.failures = self.failures.saturating_add(1);
        let base = self
            .initial
            .checked_mul(1u32 << exponent)
            .unwrap_or(self.max)
            .min(self.max);
        let half = base / 2;
        half + jitter(base - half)
    }

    /// Failures since the last success.
    #[cfg(test)]
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    /// A success: start over.
    pub(crate) fn reset(&mut self) {
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps_with_jitter() {
        let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        let first = backoff.next_delay();
        assert!(first >= Duration::from_secs(1) && first <= Duration::from_secs(2));
        let second = backoff.next_delay();
        assert!(second >= Duration::from_secs(2) && second <= Duration::from_secs(4));
        for _ in 0..4 {
            assert!(backoff.next_delay() <= Duration::from_secs(60));
        }
        for _ in 0..26 {
            let d = backoff.next_delay();
            assert!(d <= Duration::from_secs(60) && d >= Duration::from_secs(30));
        }
        assert_eq!(backoff.failures(), 32);
        backoff.reset();
        assert!(backoff.next_delay() <= Duration::from_secs(2));
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            assert!(jitter(Duration::from_millis(10)) <= Duration::from_millis(10));
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
