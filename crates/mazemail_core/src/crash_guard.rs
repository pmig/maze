//! The startup crash guard (PLAN §4.15).
//!
//! [`CoreHandle::open`](crate::CoreHandle::open) writes a sentinel file,
//! `<data_dir>/startup.sentinel`, before anything else can crash, and
//! [`CoreHandle::mark_healthy`](crate::CoreHandle::mark_healthy) removes it once the
//! first window has rendered and the core works. Finding the sentinel at the next launch
//! means the previous one never got that far. The sentinel counts such launches in a
//! row for the same app version; from [`ROLLBACK_THRESHOLD`] on, Maze offers "Roll back
//! to the previous version" (the rollback itself belongs to `auto_update`).
//!
//! The file holds only a counter and a version string, no mail data.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// File name of the sentinel inside the data directory.
pub const SENTINEL_FILE: &str = "startup.sentinel";

/// Unhealthy launches in a row after which a rollback is offered.
pub const ROLLBACK_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Sentinel {
    /// Launches of `version` in a row that wrote a sentinel, including the current one.
    launches: u32,
    version: String,
}

/// What the crash guard found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashGuardStatus {
    /// Launches of this version in a row that never became healthy, not counting the
    /// current one.
    pub unhealthy_launches: u32,
    /// The app version the count refers to.
    pub app_version: String,
}

impl CrashGuardStatus {
    /// Whether to offer "Roll back to the previous version": the last
    /// [`ROLLBACK_THRESHOLD`] launches of this version all failed to become healthy.
    pub fn should_offer_rollback(&self) -> bool {
        self.unhealthy_launches >= ROLLBACK_THRESHOLD
    }
}

/// The sentinel of the running launch.
#[derive(Debug)]
pub(crate) struct CrashGuard {
    path: PathBuf,
    status: CrashGuardStatus,
}

impl CrashGuard {
    /// Reads the previous sentinel (if any) and writes the one for this launch.
    ///
    /// A sentinel from another app version starts a new count: the crashes that matter
    /// for a rollback are those since the last update.
    pub(crate) fn arm(data_dir: &Path, app_version: &str) -> Result<Self> {
        let path = data_dir.join(SENTINEL_FILE);
        let previous = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Sentinel>(&bytes) {
                Ok(sentinel) => Some(sentinel),
                Err(err) => {
                    // An unreadable sentinel (for example a crash mid-write) still means
                    // the last launch didn't finish.
                    tracing::warn!(%err, "unreadable startup sentinel");
                    Some(Sentinel {
                        launches: 1,
                        version: app_version.to_owned(),
                    })
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(Error::io("reading the startup sentinel")(err)),
        };
        let unhealthy_launches = match previous {
            Some(sentinel) if sentinel.version == app_version => sentinel.launches,
            _ => 0,
        };
        let sentinel = Sentinel {
            launches: unhealthy_launches.saturating_add(1),
            version: app_version.to_owned(),
        };
        let bytes = serde_json::to_vec(&sentinel)
            .map_err(|err| Error::io("encoding the startup sentinel")(err.into()))?;
        let tmp = path.with_extension("sentinel.tmp");
        fs::write(&tmp, bytes)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(Error::io("writing the startup sentinel"))?;
        if unhealthy_launches > 0 {
            tracing::warn!(
                unhealthy_launches,
                "previous launches did not become healthy"
            );
        }
        Ok(Self {
            path,
            status: CrashGuardStatus {
                unhealthy_launches,
                app_version: app_version.to_owned(),
            },
        })
    }

    pub(crate) fn status(&self) -> &CrashGuardStatus {
        &self.status
    }

    /// Removes the sentinel: this launch is healthy. Idempotent.
    pub(crate) fn disarm(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::io("removing the startup sentinel")(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_unhealthy_launches_of_one_version() {
        let dir = tempfile::tempdir().unwrap();
        for expected in 0..4 {
            let guard = CrashGuard::arm(dir.path(), "1.0.0").unwrap();
            assert_eq!(guard.status().unhealthy_launches, expected);
            assert_eq!(
                guard.status().should_offer_rollback(),
                expected >= ROLLBACK_THRESHOLD
            );
        }
        // A healthy launch resets the count.
        let guard = CrashGuard::arm(dir.path(), "1.0.0").unwrap();
        guard.disarm().unwrap();
        guard.disarm().unwrap();
        let guard = CrashGuard::arm(dir.path(), "1.0.0").unwrap();
        assert_eq!(guard.status().unhealthy_launches, 0);
    }

    #[test]
    fn a_new_version_starts_a_new_count() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..3 {
            CrashGuard::arm(dir.path(), "1.0.0").unwrap();
        }
        let guard = CrashGuard::arm(dir.path(), "1.1.0").unwrap();
        assert_eq!(guard.status().unhealthy_launches, 0);
        assert_eq!(guard.status().app_version, "1.1.0");
    }

    #[test]
    fn a_garbled_sentinel_counts_as_one_failure() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(SENTINEL_FILE), b"{not json").unwrap();
        let guard = CrashGuard::arm(dir.path(), "1.0.0").unwrap();
        assert_eq!(guard.status().unhealthy_launches, 1);
    }
}
