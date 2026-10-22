//! The shared, bounded cache of decrypted index blocks.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use quick_cache::sync::{Cache, DefaultLifecycle};
use quick_cache::{DefaultHashBuilder, OptionsBuilder, Weighter};
use vault::block::NONCE_PREFIX_LEN;

/// Default block cache budget: 32 MiB of decrypted blocks.
pub const DEFAULT_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// Few shards: searches rarely run in parallel, and every shard gets a slice of the
/// capacity, which bounds the largest entry it can hold.
const SHARDS: usize = 4;

/// Identifies cached plaintext by the file's random nonce prefix plus a position.
///
/// The nonce prefix is unique per file and files are write-once, so a key never refers
/// to stale data, even after a file is deleted and its name reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockKey {
    /// One decrypted block.
    Block {
        file: [u8; NONCE_PREFIX_LEN],
        index: u64,
    },
    /// A large byte range read as a whole. Tantivy re-reads some slices on every query
    /// (field norms, fast field columns) where `mmap` would make that free.
    Range {
        file: [u8; NONCE_PREFIX_LEN],
        start: u64,
        end: u64,
    },
}

#[derive(Clone, Copy, Debug)]
struct BlockWeighter;

impl Weighter<BlockKey, Arc<[u8]>> for BlockWeighter {
    fn weight(&self, _key: &BlockKey, block: &Arc<[u8]>) -> u64 {
        // Count the bookkeeping too, so tiny blocks can't overrun the budget.
        block.len() as u64 + 64
    }
}

/// A bounded cache of decrypted index blocks (and of large ranges that Tantivy reads
/// repeatedly), shared by every file of one or more
/// [`EncryptedDirectory`](crate::EncryptedDirectory) instances.
///
/// The capacity is a memory budget in bytes of plaintext (PLAN §8: the block cache
/// replaces the OS page cache that `mmap` would use). Eviction is `quick_cache`'s
/// CLOCK-Pro-style policy, which approximates LRU but resists one-off scans, and the cache
/// is sharded so concurrent searches don't contend on one lock. A capacity of 0 disables
/// caching.
pub struct BlockCache {
    cache: Option<Cache<BlockKey, Arc<[u8]>, BlockWeighter>>,
    capacity: u64,
    max_entry: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// A snapshot of [`BlockCache`] counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockCacheStats {
    /// Reads served from the cache.
    pub hits: u64,
    /// Reads that had to decrypt a block from disk.
    pub misses: u64,
    /// Blocks currently cached.
    pub blocks: u64,
    /// Bytes currently accounted against the capacity.
    pub bytes: u64,
    /// The capacity in bytes.
    pub capacity: u64,
}

impl BlockCache {
    /// A cache holding at most `capacity_bytes` of decrypted blocks.
    pub fn new(capacity_bytes: u64) -> Self {
        let estimated_items = usize::try_from(capacity_bytes / (16 * 1024))
            .unwrap_or(usize::MAX)
            .max(16);
        let cache = OptionsBuilder::new()
            .shards(SHARDS)
            .estimated_items_capacity(estimated_items)
            .weight_capacity(capacity_bytes)
            .build()
            .ok()
            .filter(|_| capacity_bytes > 0)
            .map(|options| {
                Cache::with_options(
                    options,
                    BlockWeighter,
                    DefaultHashBuilder::default(),
                    DefaultLifecycle::default(),
                )
            });
        // A shard refuses entries larger than (most of) its share of the capacity; stay
        // well below that so large entries don't flush a shard either.
        let max_entry = cache.as_ref().map_or(0, |cache| cache.shard_capacity() / 4);
        Self {
            cache,
            capacity: capacity_bytes,
            max_entry,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// The capacity in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The largest plaintext range worth caching as one entry.
    pub(crate) fn max_entry(&self) -> u64 {
        self.max_entry
    }

    /// Current counters.
    pub fn stats(&self) -> BlockCacheStats {
        BlockCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            blocks: self.cache.as_ref().map_or(0, |cache| cache.len() as u64),
            bytes: self.cache.as_ref().map_or(0, Cache::weight),
            capacity: self.capacity,
        }
    }

    /// Drops every cached block (for example after the app goes idle).
    pub fn clear(&self) {
        if let Some(cache) = &self.cache {
            cache.clear();
        }
    }

    /// Returns the cached block or decrypts it with `load` and caches the result.
    pub(crate) fn get_or_load(
        &self,
        key: BlockKey,
        load: impl FnOnce() -> io::Result<Arc<[u8]>>,
    ) -> io::Result<Arc<[u8]>> {
        let Some(cache) = &self.cache else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return load();
        };
        let mut missed = false;
        let block = cache.get_or_insert_with(&key, || {
            missed = true;
            load()
        })?;
        let counter = if missed { &self.misses } else { &self.hits };
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(block)
    }
}

impl fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockCache")
            .field("stats", &self.stats())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(index: u64) -> BlockKey {
        BlockKey::Block {
            file: [1; NONCE_PREFIX_LEN],
            index,
        }
    }

    #[test]
    fn caches_and_counts() {
        let cache = BlockCache::new(1024 * 1024);
        let load = || Ok(Arc::from(vec![7u8; 100]));
        assert_eq!(&*cache.get_or_load(key(0), load).unwrap(), &[7u8; 100][..]);
        cache
            .get_or_load(key(0), || panic!("must be cached"))
            .unwrap();
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.blocks), (1, 1, 1));
        cache.clear();
        assert_eq!(cache.stats().blocks, 0);
    }

    #[test]
    fn stays_within_budget() {
        let cache = BlockCache::new(64 * 1024);
        for block in 0..100 {
            cache
                .get_or_load(key(block), || Ok(Arc::from(vec![0u8; 4096])))
                .unwrap();
        }
        assert!(cache.stats().bytes <= 64 * 1024);
    }

    #[test]
    fn zero_capacity_disables_caching() {
        let cache = BlockCache::new(0);
        for _ in 0..2 {
            cache
                .get_or_load(key(0), || Ok(Arc::from(vec![1u8; 10])))
                .unwrap();
        }
        assert_eq!(cache.stats().misses, 2);
    }

    #[test]
    fn load_errors_are_not_cached() {
        let cache = BlockCache::new(1024 * 1024);
        assert!(
            cache
                .get_or_load(key(3), || Err(io::Error::other("boom")))
                .is_err()
        );
        assert!(
            cache
                .get_or_load(key(3), || Ok(Arc::from(vec![1u8])))
                .is_ok()
        );
    }
}
