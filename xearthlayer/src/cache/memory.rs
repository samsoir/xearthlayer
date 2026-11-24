//! In-memory cache using moka, with moka's default TinyLFU eviction.
//!
//! Not on the production path: the DDS memory tier is `MemoryCacheProvider`.
//!
//! This module provides an async-safe memory cache backed by `moka::future::Cache`.
//! Moka uses lock-free data structures internally, making it safe to use from
//! async contexts without risk of blocking the Tokio runtime.
//!
//! # Why moka?
//!
//! The previous implementation used `std::sync::Mutex` which blocks the OS thread
//! when the lock is contended. In an async context, this can starve the Tokio
//! runtime if many tasks try to access the cache simultaneously.
//!
//! Moka provides:
//! - Lock-free reads (common case)
//! - Concurrent writes without blocking
//! - Automatic eviction without explicit locking
//! - Memory-bounded with configurable limits

use crate::cache::types::CacheKey;
use crate::cache::CacheStats;
use moka::future::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// In-memory cache for DDS tiles.
///
/// Provides fast, async-safe access to recently used tiles with automatic
/// eviction (moka's default, TinyLFU) when memory limits are exceeded.
///
/// This implementation uses `moka::future::Cache` which is designed for
/// async contexts and uses lock-free data structures internally.
pub struct MemoryCache {
    /// The underlying moka cache
    cache: Cache<CacheKey, Arc<Vec<u8>>>,
    /// Maximum size in bytes
    max_size_bytes: u64,
    /// Statistics - using atomics for lock-free updates
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl MemoryCache {
    /// Create a new memory cache with the given size limit.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum memory size in bytes (default: 2GB)
    pub fn new(max_size_bytes: usize) -> Self {
        let max_bytes = max_size_bytes as u64;

        // Build the moka cache with size-based eviction
        let cache = Cache::builder()
            // Weight each entry by its data size
            .weigher(|_key: &CacheKey, value: &Arc<Vec<u8>>| -> u32 {
                // moka uses u32 for weights, cap at u32::MAX for very large entries
                value.len().min(u32::MAX as usize) as u32
            })
            // Maximum total weight (size in bytes)
            .max_capacity(max_bytes)
            // Enable entry eviction notifications for stats
            .eviction_listener(|_key, _value, _cause| {
                // Note: We can't easily update our atomic counter here because
                // the listener doesn't have access to self. We'll track evictions
                // via the weighted_size changes instead.
            })
            .build();

        Self {
            cache,
            max_size_bytes: max_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Get a cached tile.
    ///
    /// Returns `Some(data)` if the tile is in cache, `None` otherwise.
    /// This operation is async-safe and non-blocking.
    pub async fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        match self.cache.get(key).await {
            Some(arc_data) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                // Clone the data out of the Arc
                Some((*arc_data).clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Get a cached tile (synchronous version for compatibility).
    ///
    /// This is a blocking operation that should only be used from sync contexts.
    /// Prefer `get()` in async code.
    pub fn get_sync(&self, key: &CacheKey) -> Option<Vec<u8>> {
        // moka's get is actually non-blocking for reads, so this is safe
        // We use blocking_get for synchronous contexts
        match self.cache.get(key).now_or_never() {
            Some(Some(arc_data)) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some((*arc_data).clone())
            }
            _ => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Put a tile into the cache.
    ///
    /// Eviction happens automatically when the cache exceeds its size limit.
    /// This operation is async-safe and non-blocking.
    pub async fn put(&self, key: CacheKey, data: Vec<u8>) {
        // Track evictions by checking size before and after
        let size_before = self.cache.weighted_size();

        self.cache.insert(key, Arc::new(data)).await;

        // Run pending maintenance (eviction) tasks
        self.cache.run_pending_tasks().await;

        let size_after = self.cache.weighted_size();
        if size_after < size_before {
            // Size decreased, meaning eviction occurred
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Put a tile into the cache (synchronous version for compatibility).
    ///
    /// This is a blocking operation that should only be used from sync contexts.
    /// Prefer `put()` in async code.
    pub fn put_sync(&self, key: CacheKey, data: Vec<u8>) {
        // moka's insert is designed to be fast and non-blocking
        // Use now_or_never to execute synchronously
        let size_before = self.cache.weighted_size();

        // Insert synchronously
        let _ = self.cache.insert(key, Arc::new(data)).now_or_never();

        // Run pending tasks synchronously
        let _ = self.cache.run_pending_tasks().now_or_never();

        let size_after = self.cache.weighted_size();
        if size_after < size_before {
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Check if a key exists in the cache.
    pub fn contains(&self, key: &CacheKey) -> bool {
        self.cache.contains_key(key)
    }

    /// Get the current number of entries in the cache.
    pub fn entry_count(&self) -> usize {
        self.cache.entry_count() as usize
    }

    /// Get the current size of the cache in bytes.
    pub fn size_bytes(&self) -> usize {
        self.cache.weighted_size() as usize
    }

    /// Get the maximum size of the cache in bytes.
    pub fn max_size_bytes(&self) -> usize {
        self.max_size_bytes as usize
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::new();
        stats.memory_hits = self.hits.load(Ordering::Relaxed);
        stats.memory_misses = self.misses.load(Ordering::Relaxed);
        stats.memory_evictions = self.evictions.load(Ordering::Relaxed);
        stats.memory_size_bytes = self.size_bytes();
        stats.memory_entry_count = self.entry_count();
        stats
    }

    /// Clear all entries from the cache.
    pub fn clear(&self) {
        self.cache.invalidate_all();
        // Run pending tasks to complete the invalidation
        let _ = self.cache.run_pending_tasks().now_or_never();
    }

    /// Evict entries until the cache is under the size limit.
    ///
    /// Note: With moka, this is typically not needed as eviction is automatic.
    /// This method is provided for API compatibility and runs pending maintenance.
    pub fn evict_if_over_limit(&self) -> Result<(), crate::cache::CacheError> {
        // Run any pending eviction tasks
        let _ = self.cache.run_pending_tasks().now_or_never();
        Ok(())
    }
}

// Implement FutureExt::now_or_never for convenience
trait FutureExtNowOrNever {
    type Output;
    fn now_or_never(self) -> Option<Self::Output>;
}

impl<F: std::future::Future> FutureExtNowOrNever for F {
    type Output = F::Output;

    fn now_or_never(self) -> Option<Self::Output> {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        // Create a no-op waker
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        // Pin the future and poll once
        let mut pinned = std::pin::pin!(self);
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::dds::DdsFormat;

    fn create_test_key(col: u32) -> CacheKey {
        CacheKey::new(
            "test",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_memory_cache_new() {
        let cache = MemoryCache::new(1_000_000);
        assert_eq!(cache.max_size_bytes(), 1_000_000);
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
    }

    #[tokio::test]
    async fn test_memory_cache_put_and_get() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put(key.clone(), data.clone()).await;

        let retrieved = cache.get(&key).await;
        assert_eq!(retrieved, Some(data));
        assert_eq!(cache.entry_count(), 1);
    }

    #[tokio::test]
    async fn test_memory_cache_miss() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);

        let retrieved = cache.get(&key).await;
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_memory_cache_contains() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        assert!(!cache.contains(&key));
        cache.put_sync(key.clone(), data);
        assert!(cache.contains(&key));
    }

    #[tokio::test]
    async fn test_memory_cache_size_tracking() {
        let cache = MemoryCache::new(1_000_000);
        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let data1 = vec![0u8; 1000];
        let data2 = vec![0u8; 2000];

        cache.put(key1, data1).await;
        // Wait for size to update
        tokio::task::yield_now().await;
        let size1 = cache.size_bytes();
        assert!(size1 >= 1000, "size should be at least 1000, got {}", size1);

        cache.put(key2, data2).await;
        tokio::task::yield_now().await;
        let size2 = cache.size_bytes();
        assert!(size2 >= 3000, "size should be at least 3000, got {}", size2);
        assert_eq!(cache.entry_count(), 2);
    }

    #[test]
    fn test_memory_cache_clear() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put_sync(key.clone(), data);
        assert_eq!(cache.entry_count(), 1);

        cache.clear();
        assert_eq!(cache.entry_count(), 0);
        assert!(!cache.contains(&key));
    }

    #[tokio::test]
    async fn test_memory_cache_lru_eviction() {
        // Create cache that can hold ~2.5 entries of 1000 bytes each
        let cache = MemoryCache::new(2500);

        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let key3 = create_test_key(3);
        let data = vec![0u8; 1000];

        // Add 3 entries (3000 bytes total, exceeds 2500 limit)
        cache.put(key1.clone(), data.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        cache.put(key2.clone(), data.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        cache.put(key3.clone(), data.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Run pending maintenance tasks
        cache.cache.run_pending_tasks().await;

        // Cache should have evicted at least one entry to stay under limit
        assert!(
            cache.size_bytes() <= 2500,
            "Cache should be under limit, got {} bytes",
            cache.size_bytes()
        );
    }

    #[tokio::test]
    async fn test_memory_cache_statistics_hits() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        cache.put(key.clone(), data).await;

        cache.get(&key).await;
        cache.get(&key).await;

        let stats = cache.stats();
        assert_eq!(stats.memory_hits, 2);
        assert_eq!(stats.memory_misses, 0);
    }

    #[tokio::test]
    async fn test_memory_cache_statistics_misses() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);

        cache.get(&key).await;
        cache.get(&key).await;

        let stats = cache.stats();
        assert_eq!(stats.memory_hits, 0);
        assert_eq!(stats.memory_misses, 2);
    }

    #[tokio::test]
    async fn test_memory_cache_replace_existing() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data1 = vec![1, 2, 3];
        let data2 = vec![4, 5, 6, 7, 8];

        cache.put(key.clone(), data1).await;
        cache.put(key.clone(), data2.clone()).await;

        let retrieved = cache.get(&key).await;
        assert_eq!(retrieved, Some(data2));
        assert_eq!(cache.entry_count(), 1);
    }

    #[tokio::test]
    async fn test_memory_cache_concurrent_access() {
        use std::sync::Arc;

        let cache = Arc::new(MemoryCache::new(10_000_000));
        let mut handles = Vec::new();

        // Spawn multiple tasks that read and write concurrently
        for i in 0..100 {
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                let key = create_test_key(i);
                let data = vec![i as u8; 100];

                // Write
                cache.put(key.clone(), data.clone()).await;

                // Read
                let result = cache.get(&key).await;
                assert_eq!(result, Some(data));
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // All entries should be present
        assert_eq!(cache.entry_count(), 100);
    }

    #[test]
    fn test_sync_operations() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put_sync(key.clone(), data.clone());

        let retrieved = cache.get_sync(&key);
        assert_eq!(retrieved, Some(data));
    }
}
