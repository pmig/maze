//! What an account engine is doing right now, for "Sync activity" in the app.
//!
//! The actor and its folder passes write into an [`ActivityTracker`] (a mutex, no
//! events: header batches update a counter); [`AccountHandle::activity`]
//! (crate::AccountHandle::activity) takes a [`SyncActivity`] snapshot whenever the UI
//! wants one, so nothing per message reaches the UI.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use mazemail_types::FolderId;

/// The step a folder pass is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FolderPhase {
    /// Queued behind other work.
    Waiting,
    /// Selecting the folder and comparing it with what is stored.
    Checking,
    /// Reading flag changes and expunges.
    ReadingFlags,
    /// Re-reading Gmail labels (All Mail).
    ReadingLabels,
    /// Fetching the headers of new messages (`done` of `total`).
    FetchingHeaders,
}

/// One folder the engine is working on or will work on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderActivity {
    /// The folder.
    pub folder: FolderId,
    /// What the pass is doing.
    pub phase: FolderPhase,
    /// Work done in this phase (headers fetched).
    pub done: u64,
    /// Work in this phase, if known (new headers to fetch).
    pub total: Option<u64>,
}

/// A snapshot of an account engine's work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncActivity {
    /// Running folder passes first (in the order they started), then waiting ones.
    pub folders: Vec<FolderActivity>,
    /// Pending ops (moves, flags, appends) are being replayed on the server.
    pub replaying_ops: bool,
    /// Message bodies waiting to be downloaded for offline reading.
    pub bodies_queued: usize,
    /// When the last full round finished, Unix seconds (since the engine started).
    pub last_synced: Option<i64>,
}

#[derive(Debug, Default)]
struct State {
    /// Running passes, in start order.
    running: Vec<FolderId>,
    /// Waiting passes, in queue order.
    waiting: Vec<FolderId>,
    /// What running passes reported.
    progress: HashMap<FolderId, (FolderPhase, u64, Option<u64>)>,
    replaying_ops: bool,
    bodies_queued: usize,
    last_synced: Option<i64>,
}

/// Where the engine notes what it is doing. Cheap to update.
#[derive(Debug, Default)]
pub(crate) struct ActivityTracker {
    state: Mutex<State>,
}

impl ActivityTracker {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A pass of `folder` is at `phase`, with `done` of `total`.
    pub(crate) fn phase(
        &self,
        folder: FolderId,
        phase: FolderPhase,
        done: u64,
        total: Option<u64>,
    ) {
        self.state().progress.insert(folder, (phase, done, total));
    }

    /// The actor's view of its jobs: running and waiting passes, op replay, the body
    /// queue. Progress of passes that no longer run is dropped.
    pub(crate) fn jobs(
        &self,
        running: Vec<FolderId>,
        waiting: Vec<FolderId>,
        replaying_ops: bool,
        bodies_queued: usize,
    ) {
        let mut state = self.state();
        state.progress.retain(|folder, _| running.contains(folder));
        // Keep the start order of passes that were running already.
        let mut ordered: Vec<FolderId> = state
            .running
            .iter()
            .copied()
            .filter(|f| running.contains(f))
            .collect();
        for folder in running {
            if !ordered.contains(&folder) {
                ordered.push(folder);
            }
        }
        state.running = ordered;
        state.waiting = waiting;
        state.replaying_ops = replaying_ops;
        state.bodies_queued = bodies_queued;
    }

    /// A round finished at `now` (Unix seconds).
    pub(crate) fn synced(&self, now: i64) {
        self.state().last_synced = Some(now);
    }

    /// The current activity.
    pub(crate) fn snapshot(&self) -> SyncActivity {
        let state = self.state();
        let running = state.running.iter().map(|folder| {
            let (phase, done, total) =
                state
                    .progress
                    .get(folder)
                    .copied()
                    .unwrap_or((FolderPhase::Checking, 0, None));
            FolderActivity {
                folder: *folder,
                phase,
                done,
                total,
            }
        });
        let waiting = state
            .waiting
            .iter()
            .filter(|folder| !state.running.contains(folder))
            .map(|folder| FolderActivity {
                folder: *folder,
                phase: FolderPhase::Waiting,
                done: 0,
                total: None,
            });
        let mut folders: Vec<FolderActivity> = running.collect();
        for folder in waiting {
            if !folders.iter().any(|f| f.folder == folder.folder) {
                folders.push(folder);
            }
        }
        SyncActivity {
            folders,
            replaying_ops: state.replaying_ops,
            bodies_queued: state.bodies_queued,
            last_synced: state.last_synced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_running_then_waiting_passes_with_progress() {
        let tracker = ActivityTracker::default();
        assert_eq!(tracker.snapshot(), SyncActivity::default());
        tracker.jobs(
            vec![FolderId(1)],
            vec![FolderId(2), FolderId(1), FolderId(3)],
            false,
            4,
        );
        tracker.phase(FolderId(1), FolderPhase::FetchingHeaders, 200, Some(5_400));
        let snapshot = tracker.snapshot();
        assert_eq!(
            snapshot.folders,
            vec![
                FolderActivity {
                    folder: FolderId(1),
                    phase: FolderPhase::FetchingHeaders,
                    done: 200,
                    total: Some(5_400),
                },
                FolderActivity {
                    folder: FolderId(2),
                    phase: FolderPhase::Waiting,
                    done: 0,
                    total: None,
                },
                FolderActivity {
                    folder: FolderId(3),
                    phase: FolderPhase::Waiting,
                    done: 0,
                    total: None,
                },
            ]
        );
        assert_eq!(snapshot.bodies_queued, 4);

        // A finished pass loses its progress; a new one starts at "checking".
        tracker.jobs(vec![FolderId(2)], vec![], true, 0);
        tracker.synced(1_700_000_000);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.folders.len(), 1);
        assert_eq!(snapshot.folders[0].folder, FolderId(2));
        assert_eq!(snapshot.folders[0].phase, FolderPhase::Checking);
        assert!(snapshot.replaying_ops);
        assert_eq!(snapshot.last_synced, Some(1_700_000_000));
    }
}
