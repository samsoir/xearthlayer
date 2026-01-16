//! Memory cache adapter for the executor daemon.
//!
//! This module provides a simple adapter that bridges the `cache::MemoryCache`
//! to the `DaemonMemoryCache` trait used by the executor daemon.
//!
//! # Design Rationale
//!
//! The executor daemon needs a minimal interface for memory cache operations
//! (just `get` for fast-path lookups). This adapter implements the
//! `executor::MemoryCache` trait and works directly with `cache::MemoryCache`.
//!
//! The `DaemonMemoryCache` trait has a blanket impl for any `executor::MemoryCache`,
//! so this adapter gets `DaemonMemoryCache` for free.

use crate::cache::{CacheKey, MemoryCache};
use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use std::sync::Arc;

/// Adapter that bridges `cache::MemoryCache` to the executor's `MemoryCache` trait.
///
/// This adapter automatically implements `DaemonMemoryCache` via the blanket impl
/// in `executor/daemon.rs` that covers all `executor::MemoryCache` implementors.
///
/// Handles cache key generation with provider and format context.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::executor::ExecutorCacheAdapter;
/// use xearthlayer::cache::MemoryCache;
/// use xearthlayer::dds::DdsFormat;
///
/// let cache = Arc::new(MemoryCache::new(1024 * 1024 * 1024)); // 1GB
/// let adapter = ExecutorCacheAdapter::new(cache, "bing", DdsFormat::BC1);
///
/// // Adapter implements DaemonMemoryCache (via blanket impl)
/// // Can be used with ExecutorDaemon
/// ```
#[derive(Clone)]
pub struct ExecutorCacheAdapter {
    /// The underlying memory cache.
    cache: Arc<MemoryCache>,

    /// Provider name for cache key generation.
    provider: String,

    /// DDS format for cache key generation.
    format: DdsFormat,
}

impl ExecutorCacheAdapter {
    /// Creates a new executor cache adapter.
    ///
    /// # Arguments
    ///
    /// * `cache` - The memory cache to wrap
    /// * `provider` - Provider name (e.g., "bing", "go2")
    /// * `format` - DDS format (BC1 or BC3)
    pub fn new(cache: Arc<MemoryCache>, provider: impl Into<String>, format: DdsFormat) -> Self {
        Self {
            cache,
            provider: provider.into(),
            format,
        }
    }

    /// Gets the underlying memory cache.
    ///
    /// Useful for size queries and other cache management operations.
    pub fn inner(&self) -> &Arc<MemoryCache> {
        &self.cache
    }

    /// Creates a cache key for the given tile coordinates.
    fn make_key(&self, row: u32, col: u32, zoom: u8) -> CacheKey {
        let tile = TileCoord { row, col, zoom };
        CacheKey::new(&self.provider, self.format, tile)
    }

    /// Stores a tile in the cache (sync version).
    ///
    /// This is a convenience method for sync contexts.
    pub fn put_sync(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
        let key = self.make_key(row, col, zoom);
        // MemoryCache has both async put() and sync put_sync().
        // Use the sync version directly.
        self.cache.put_sync(key, data);
    }

    /// Checks if a tile is in the cache.
    pub fn contains(&self, row: u32, col: u32, zoom: u8) -> bool {
        let key = self.make_key(row, col, zoom);
        self.cache.get_sync(&key).is_some()
    }

    /// Gets the cache size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.cache.size_bytes()
    }
}

// Implement executor::MemoryCache trait.
// This also provides DaemonMemoryCache via the blanket impl in daemon.rs.
impl super::MemoryCache for ExecutorCacheAdapter {
    async fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
        let key = self.make_key(row, col, zoom);
        self.cache.get(&key).await
    }

    async fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
        let key = self.make_key(row, col, zoom);
        // Use async put to ensure write completes (put_sync can drop writes!)
        self.cache.put(key, data).await;
    }

    fn size_bytes(&self) -> usize {
        self.cache.size_bytes()
    }

    fn entry_count(&self) -> usize {
        self.cache.entry_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_adapter() -> ExecutorCacheAdapter {
        let cache = Arc::new(MemoryCache::new(1024 * 1024)); // 1MB
        ExecutorCacheAdapter::new(cache, "test_provider", DdsFormat::BC1)
    }

    #[test]
    fn test_adapter_creation() {
        let adapter = create_test_adapter();
        assert_eq!(adapter.provider, "test_provider");
        assert_eq!(adapter.format, DdsFormat::BC1);
    }

    #[tokio::test]
    async fn test_get_returns_none_for_empty_cache() {
        use crate::executor::MemoryCache;

        let adapter = create_test_adapter();
        let result = MemoryCache::get(&adapter, 100, 200, 14).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_put_and_get() {
        use crate::executor::MemoryCache;

        let adapter = create_test_adapter();

        // Put some data
        MemoryCache::put(&adapter, 100, 200, 14, vec![1, 2, 3, 4]).await;

        // Get it back
        let result = MemoryCache::get(&adapter, 100, 200, 14).await;
        assert_eq!(result, Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn test_put_sync_and_contains() {
        let adapter = create_test_adapter();

        assert!(!adapter.contains(100, 200, 14));

        adapter.put_sync(100, 200, 14, vec![1, 2, 3]);

        assert!(adapter.contains(100, 200, 14));
    }

    #[test]
    fn test_different_coords_different_keys() {
        let adapter = create_test_adapter();

        adapter.put_sync(100, 200, 14, vec![1, 2, 3]);
        adapter.put_sync(101, 200, 14, vec![4, 5, 6]);

        assert!(adapter.contains(100, 200, 14));
        assert!(adapter.contains(101, 200, 14));
        assert!(!adapter.contains(100, 201, 14)); // Different col
    }

    #[test]
    fn test_size_bytes() {
        let adapter = create_test_adapter();

        // Initially empty
        let initial_size = adapter.size_bytes();

        // Add some data
        adapter.put_sync(100, 200, 14, vec![0; 1000]);

        // Size should increase
        assert!(adapter.size_bytes() > initial_size);
    }

    #[test]
    fn test_inner_returns_cache() {
        let adapter = create_test_adapter();
        let inner = adapter.inner();
        assert_eq!(inner.size_bytes(), adapter.size_bytes());
    }
}
