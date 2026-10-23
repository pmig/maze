//! Noticing that the computer woke from sleep, so the updater can check again right
//! away (PLAN §4.15: "a re-check after the machine wakes from sleep").
//!
//! The same technique as the core's sync watchdog: the monotonic clock
//! ([`Instant`], `CLOCK_UPTIME_RAW` on macOS) stands still while the machine sleeps,
//! the wall clock doesn't. When the wall clock got ahead by more than
//! [`SLEEP_THRESHOLD`] between two looks, the machine slept.

use std::time::{Duration, Instant, SystemTime};

/// How often [`wait_for_wake`] compares the clocks.
pub const WAKE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// A gap this large between wall and monotonic time counts as sleep.
pub const SLEEP_THRESHOLD: Duration = Duration::from_secs(60);

/// Whether the machine slept, given how much wall-clock and monotonic time passed.
pub fn slept(wall_elapsed: Duration, mono_elapsed: Duration, threshold: Duration) -> bool {
    wall_elapsed > mono_elapsed + threshold
}

/// Completes after the machine woke from sleep. Needs a Tokio timer.
pub async fn wait_for_wake() {
    loop {
        let wall = SystemTime::now();
        let mono = Instant::now();
        tokio::time::sleep(WAKE_CHECK_INTERVAL).await;
        let wall_elapsed = wall.elapsed().unwrap_or_default();
        let mono_elapsed = mono.elapsed();
        if slept(wall_elapsed, mono_elapsed, SLEEP_THRESHOLD) {
            tracing::info!(
                slept_secs = (wall_elapsed - mono_elapsed).as_secs(),
                "the computer slept; checking for updates"
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sleep_from_the_clock_gap() {
        let secs = Duration::from_secs;
        assert!(!slept(secs(30), secs(30), SLEEP_THRESHOLD));
        // Timer jitter and a slow wake-up of the task are not sleep.
        assert!(!slept(secs(85), secs(30), SLEEP_THRESHOLD));
        assert!(slept(secs(3600), secs(30), SLEEP_THRESHOLD));
        // The wall clock going backwards (NTP) is not sleep either.
        assert!(!slept(Duration::ZERO, secs(30), SLEEP_THRESHOLD));
    }
}
