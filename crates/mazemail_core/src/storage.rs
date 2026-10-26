//! How much disk space Maze uses: the status bar's "2.4 GB" and its breakdown.
//!
//! [`StorageUsage::measure`] walks the data directory (symlinks are counted as links,
//! never followed) and sorts every file into the mail store (database and message
//! blobs), the search index, caches, backups and everything else (logs, updates). The
//! core runs it on its blocking pool from a background task ([`run_meter`]): once at
//! startup, again every [`REFRESH_EVERY`], and soon after changes that grow or shrink
//! the data (sync batches, imports, deletes, index commits), but never more often than
//! every [`MIN_GAP`]. The last value is cached in a [`StorageMeter`], so
//! [`CoreHandle::storage_usage`](crate::CoreHandle::storage_usage) never touches the
//! disk; a new value that differs from the last one is announced with
//! [`CoreEventBatch::storage`](crate::CoreEventBatch::storage).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use store::{DATABASE_FILE, SENDLOG_FILE};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Instant, sleep_until};

use crate::events::{CoreEvent, CoreEventBatch};

/// How often the usage is measured when nothing changed (blob collection, logs and
/// updates change the size without events).
pub const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);

/// The shortest time between two measurements, however much changes.
pub const MIN_GAP: Duration = Duration::from_secs(30);

/// The disk space Maze's data directory takes, by area, in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageUsage {
    /// The data directory that was measured.
    pub data_dir: PathBuf,
    /// The mail database and the send log (`mail/mazemail.db`, `mail/sendlog.db`,
    /// with their journal files).
    pub database: u64,
    /// The encrypted message blobs (`mail/blobs/`).
    pub blobs: u64,
    /// The encrypted search index (`mail/index/`).
    pub search_index: u64,
    /// Caches and staging files (`mail/cache/`, `mail/tmp/`).
    pub caches: u64,
    /// Database copies taken before migrations (`mail/backups/`).
    pub backups: u64,
    /// Everything else: logs, downloaded updates, schemas.
    pub other: u64,
}

/// Where a file under the data directory belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Area {
    Database,
    Blobs,
    SearchIndex,
    Caches,
    Backups,
    Other,
}

impl StorageUsage {
    /// All areas together.
    pub fn total(&self) -> u64 {
        self.database + self.blobs + self.search_index + self.caches + self.backups + self.other
    }

    /// The mail store: database and message blobs.
    pub fn mail_store(&self) -> u64 {
        self.database + self.blobs
    }

    /// Measures `data_dir`. Files that vanish or can't be read while walking are
    /// skipped; a missing directory measures as zero.
    pub fn measure(data_dir: &Path) -> Self {
        let mut usage = Self {
            data_dir: data_dir.to_owned(),
            ..Self::default()
        };
        let mut pending = vec![data_dir.to_owned()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // `DirEntry::file_type` doesn't follow symlinks.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let bytes = metadata.len();
                match area(data_dir, &path) {
                    Area::Database => usage.database += bytes,
                    Area::Blobs => usage.blobs += bytes,
                    Area::SearchIndex => usage.search_index += bytes,
                    Area::Caches => usage.caches += bytes,
                    Area::Backups => usage.backups += bytes,
                    Area::Other => usage.other += bytes,
                }
            }
        }
        usage
    }
}

/// The area of `path`, a file below `data_dir` (see [`store::DataLayout`]).
fn area(data_dir: &Path, path: &Path) -> Area {
    let Ok(relative) = path.strip_prefix(data_dir) else {
        return Area::Other;
    };
    let mut parts = relative.components().map(|c| c.as_os_str());
    if parts.next().is_none_or(|first| first != "mail") {
        return Area::Other;
    }
    let Some(second) = parts.next() else {
        return Area::Other;
    };
    let is_last = parts.next().is_none();
    match second.to_str().unwrap_or_default() {
        "blobs" if !is_last => Area::Blobs,
        "index" if !is_last => Area::SearchIndex,
        "cache" | "tmp" if !is_last => Area::Caches,
        "backups" if !is_last => Area::Backups,
        // The database and send log with their `-wal`, `-shm` and `-journal` files.
        name if is_last
            && [DATABASE_FILE, SENDLOG_FILE]
                .iter()
                .any(|db| name.starts_with(db)) =>
        {
            Area::Database
        }
        _ => Area::Other,
    }
}

/// When to measure next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefreshPolicy {
    /// The shortest time between two measurements.
    pub min_gap: Duration,
    /// How often to measure when nothing changed.
    pub every: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            min_gap: MIN_GAP,
            every: REFRESH_EVERY,
        }
    }
}

impl RefreshPolicy {
    /// When the next measurement is due, after one at `last` (`None`: never measured)
    /// and with `dirty` when the data changed since. `None` means now.
    pub(crate) fn due(&self, last: Option<Instant>, dirty: bool) -> Option<Instant> {
        let last = last?;
        Some(last + if dirty { self.min_gap } else { self.every })
    }
}

/// The cached measurement.
#[derive(Debug, Default)]
pub(crate) struct StorageMeter {
    usage: Mutex<Option<StorageUsage>>,
}

impl StorageMeter {
    /// The last measurement, if there was one.
    pub(crate) fn get(&self) -> Option<StorageUsage> {
        self.usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores a measurement; whether it differs from the last one.
    pub(crate) fn set(&self, usage: StorageUsage) -> bool {
        let mut slot = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = slot.as_ref() != Some(&usage);
        *slot = Some(usage);
        changed
    }
}

/// Whether a batch means the data on disk grew or shrank (mail arrived or left, the
/// index committed, events were lost). Batches that only say the usage changed don't
/// count, or the meter would wake itself.
pub(crate) fn touches_disk(batch: &CoreEventBatch) -> bool {
    !batch.messages_changed.is_empty()
        || !batch.messages_removed.is_empty()
        || !batch.removed.is_empty()
        || !batch.folders_changed.is_empty()
        || !batch.accounts_changed.is_empty()
        || batch.index.is_some()
        || batch.reload
}

/// Measures `data_dir` as [`RefreshPolicy`] says until the event stream closes,
/// keeping the value in `meter` and sending [`CoreEvent::Storage`] when it changed.
pub(crate) async fn run_meter(
    meter: std::sync::Arc<StorageMeter>,
    data_dir: PathBuf,
    mut batches: broadcast::Receiver<CoreEventBatch>,
    events: mpsc::UnboundedSender<CoreEvent>,
    policy: RefreshPolicy,
) {
    let mut last: Option<Instant> = None;
    let mut dirty = false;
    loop {
        let due = policy.due(last, dirty);
        let wait = async move {
            if let Some(at) = due {
                sleep_until(at).await;
            }
        };
        tokio::select! {
            () = wait => {
                let dir = data_dir.clone();
                let Ok(usage) =
                    tokio::task::spawn_blocking(move || StorageUsage::measure(&dir)).await
                else {
                    return;
                };
                last = Some(Instant::now());
                dirty = false;
                tracing::debug!(bytes = usage.total(), "measured the data directory");
                if meter.set(usage) && events.send(CoreEvent::Storage).is_err() {
                    return;
                }
            }
            batch = batches.recv() => match batch {
                Ok(batch) => dirty |= touches_disk(&batch),
                Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn sorts_files_into_areas_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        write(&data.join("mail/mazemail.db"), 1_000);
        write(&data.join("mail/mazemail.db-wal"), 200);
        write(&data.join("mail/sendlog.db"), 30);
        write(&data.join("mail/blobs/ab/cd/abcd.mzb"), 4_000);
        write(&data.join("mail/blobs/ef/01/ef01.mzb"), 1_000);
        write(&data.join("mail/index/segment.idx"), 700);
        write(&data.join("mail/cache/x"), 50);
        write(&data.join("mail/tmp/y"), 5);
        write(&data.join("mail/backups/mazemail-v3.db"), 900);
        write(&data.join("logs/mazemail.log"), 60);
        write(&data.join("mail/stray"), 4);
        // A big file outside the data directory, linked from inside: not counted.
        write(&dir.path().join("outside/huge"), 100_000);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("outside"), data.join("mail/linked"))
                .unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("outside/huge"),
                data.join("mail/blobs/huge"),
            )
            .unwrap();
        }

        let usage = StorageUsage::measure(&data);
        assert_eq!(usage.data_dir, data);
        assert_eq!(usage.database, 1_230);
        assert!(usage.blobs >= 5_000 && usage.blobs < 6_000, "{usage:?}");
        assert_eq!(usage.search_index, 700);
        assert_eq!(usage.caches, 55);
        assert_eq!(usage.backups, 900);
        assert!(usage.other >= 64 && usage.other < 1_000, "{usage:?}");
        assert_eq!(usage.mail_store(), usage.database + usage.blobs);
        assert!(usage.total() < 100_000, "the symlinked file isn't followed");
    }

    #[test]
    fn a_missing_directory_measures_zero() {
        let dir = tempfile::tempdir().unwrap();
        let usage = StorageUsage::measure(&dir.path().join("nothing"));
        assert_eq!(usage.total(), 0);
    }

    #[test]
    fn measures_at_once_then_throttles() {
        let policy = RefreshPolicy::default();
        assert_eq!(policy.due(None, false), None, "the first one runs at once");
        assert_eq!(policy.due(None, true), None);
        let last = Instant::now();
        assert_eq!(policy.due(Some(last), false), Some(last + REFRESH_EVERY));
        assert_eq!(
            policy.due(Some(last), true),
            Some(last + MIN_GAP),
            "changes are measured soon, but not more than every 30 s"
        );
        assert!(MIN_GAP >= Duration::from_secs(30));
    }

    #[test]
    fn the_meter_reports_changes_only() {
        let meter = StorageMeter::default();
        assert_eq!(meter.get(), None);
        let usage = StorageUsage {
            blobs: 7,
            ..StorageUsage::default()
        };
        assert!(meter.set(usage.clone()));
        assert!(!meter.set(usage.clone()));
        assert_eq!(meter.get(), Some(usage));
    }

    #[test]
    fn only_data_changes_wake_the_meter() {
        let storage_only = CoreEventBatch {
            storage: true,
            ..CoreEventBatch::default()
        };
        assert!(!touches_disk(&storage_only));
        let mail = CoreEventBatch {
            messages_changed: vec![mazemail_types::MessageId(1)],
            ..CoreEventBatch::default()
        };
        assert!(touches_disk(&mail));
        let deleted = CoreEventBatch {
            messages_removed: vec![mazemail_types::MessageId(1)],
            ..CoreEventBatch::default()
        };
        assert!(touches_disk(&deleted));
    }
}
