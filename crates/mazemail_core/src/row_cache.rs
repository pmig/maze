//! The LRU of thread rows behind [`CoreHandle::rows`](crate::CoreHandle::rows)
//! (PLAN §4.3, "Thread list model").
//!
//! The thread list keeps only keys (16 bytes per row); the visible range asks for row
//! data, which comes from here or from one batched store query. Rows depend on the
//! folder they're shown in (dates and counts per folder), so entries are keyed by
//! `(thread, context folder)`.
//!
//! Invalidation is by thread: every store change set drops the rows of the threads it
//! touched, in every context. A row loaded from the store while an invalidation ran
//! could be older than the change, so loads carry the cache epoch they started at and
//! are only inserted if no invalidation happened in between.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mazemail_types::{FolderId, ThreadId};
use store::ThreadRow;

/// Default capacity (PLAN §4.3).
pub const DEFAULT_ROW_CACHE_CAPACITY: usize = 2_000;

type Key = (ThreadId, Option<FolderId>);

#[derive(Default)]
struct State {
    rows: HashMap<Key, (u64, ThreadRow)>,
    /// Use stamp → key, oldest first.
    order: BTreeMap<u64, Key>,
    /// Contexts cached per thread, for invalidation.
    contexts: HashMap<ThreadId, Vec<Option<FolderId>>>,
    next_stamp: u64,
    epoch: u64,
    hits: u64,
    misses: u64,
}

impl State {
    fn stamp(&mut self) -> u64 {
        self.next_stamp += 1;
        self.next_stamp
    }

    fn remove(&mut self, key: &Key) {
        if let Some((stamp, _)) = self.rows.remove(key) {
            self.order.remove(&stamp);
        }
        if let Some(contexts) = self.contexts.get_mut(&key.0) {
            contexts.retain(|c| *c != key.1);
            if contexts.is_empty() {
                self.contexts.remove(&key.0);
            }
        }
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowCacheStats {
    /// Rows cached.
    pub len: usize,
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that went to the store.
    pub misses: u64,
}

/// A bounded LRU of [`ThreadRow`]s.
pub(crate) struct RowCache {
    capacity: usize,
    state: Mutex<State>,
}

impl std::fmt::Debug for RowCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowCache")
            .field("capacity", &self.capacity)
            .field("stats", &self.stats())
            .finish()
    }
}

impl RowCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks up rows. Returns one slot per id (`None` = miss) and the epoch to pass to
    /// [`RowCache::insert`] for the rows loaded to fill the misses.
    pub(crate) fn get(
        &self,
        ids: &[ThreadId],
        context: Option<FolderId>,
    ) -> (Vec<Option<ThreadRow>>, u64) {
        let mut state = self.lock();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let key = (*id, context);
            let stamp = state.stamp();
            let found = match state.rows.get_mut(&key) {
                Some((old, row)) => {
                    let previous = std::mem::replace(old, stamp);
                    Some((previous, row.clone()))
                }
                None => None,
            };
            match found {
                Some((previous, row)) => {
                    state.order.remove(&previous);
                    state.order.insert(stamp, key);
                    state.hits += 1;
                    out.push(Some(row));
                }
                None => {
                    state.misses += 1;
                    out.push(None);
                }
            }
        }
        (out, state.epoch)
    }

    /// Inserts rows loaded at `epoch`; dropped if an invalidation happened since.
    pub(crate) fn insert(&self, rows: &[ThreadRow], context: Option<FolderId>, epoch: u64) {
        let mut state = self.lock();
        if state.epoch != epoch {
            return;
        }
        for row in rows {
            let key = (row.id, context);
            state.remove(&key);
            let stamp = state.stamp();
            state.rows.insert(key, (stamp, row.clone()));
            state.order.insert(stamp, key);
            state.contexts.entry(row.id).or_default().push(context);
        }
        while state.rows.len() > self.capacity {
            let Some((_, key)) = state.order.pop_first() else {
                break;
            };
            state.remove(&key);
        }
    }

    /// Drops every cached row of these threads.
    pub(crate) fn invalidate(&self, threads: impl IntoIterator<Item = ThreadId>) {
        let mut state = self.lock();
        state.epoch += 1;
        for thread in threads {
            if let Some(contexts) = state.contexts.remove(&thread) {
                for context in contexts {
                    if let Some((stamp, _)) = state.rows.remove(&(thread, context)) {
                        state.order.remove(&stamp);
                    }
                }
            }
        }
    }

    /// Drops everything (after lost change sets).
    pub(crate) fn clear(&self) {
        let mut state = self.lock();
        state.epoch += 1;
        state.rows.clear();
        state.order.clear();
        state.contexts.clear();
    }

    pub(crate) fn stats(&self) -> RowCacheStats {
        let state = self.lock();
        RowCacheStats {
            len: state.rows.len(),
            hits: state.hits,
            misses: state.misses,
        }
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::AccountId;

    use super::*;

    fn row(id: i64) -> ThreadRow {
        ThreadRow {
            id: ThreadId(id),
            account: AccountId(1),
            subject: None,
            participants: Vec::new(),
            date: id,
            message_count: 1,
            unread_count: 0,
            flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            snippet: None,
            last_message: None,
        }
    }

    #[test]
    fn evicts_least_recently_used() {
        let cache = RowCache::new(2);
        let (_, epoch) = cache.get(&[], None);
        cache.insert(&[row(1), row(2)], None, epoch);
        // Touch 1, so 2 is the oldest.
        assert!(cache.get(&[ThreadId(1)], None).0[0].is_some());
        cache.insert(&[row(3)], None, epoch);
        let (got, _) = cache.get(&[ThreadId(1), ThreadId(2), ThreadId(3)], None);
        assert!(got[0].is_some());
        assert!(got[1].is_none());
        assert!(got[2].is_some());
        assert_eq!(cache.stats().len, 2);
    }

    #[test]
    fn contexts_are_separate_and_invalidated_together() {
        let cache = RowCache::new(10);
        let (_, epoch) = cache.get(&[], None);
        cache.insert(&[row(1)], None, epoch);
        cache.insert(&[row(1)], Some(FolderId(5)), epoch);
        assert!(cache.get(&[ThreadId(1)], Some(FolderId(5))).0[0].is_some());
        assert!(cache.get(&[ThreadId(1)], Some(FolderId(6))).0[0].is_none());
        cache.invalidate([ThreadId(1)]);
        assert!(cache.get(&[ThreadId(1)], None).0[0].is_none());
        assert!(cache.get(&[ThreadId(1)], Some(FolderId(5))).0[0].is_none());
        assert_eq!(cache.stats().len, 0);
    }

    #[test]
    fn stale_loads_are_not_inserted() {
        let cache = RowCache::new(10);
        let (_, epoch) = cache.get(&[ThreadId(1)], None);
        cache.invalidate([ThreadId(9)]);
        cache.insert(&[row(1)], None, epoch);
        assert!(cache.get(&[ThreadId(1)], None).0[0].is_none());
        cache.clear();
        assert_eq!(cache.stats().len, 0);
    }
}
