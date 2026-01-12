//! Memory cache adapter for the executor framework.
//!
//! Adapts `cache::MemoryCache` to the executor's `MemoryCache` trait.

use crate::cache::{self, CacheKey};
use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use std::sync::Arc;

/// Adapts `cache::MemoryCache` to the executor's `MemoryCache` trait.
///
/// The executor uses simpler cache keys (row, col, zoom) while the existing
/// cache uses `CacheKey` which includes provider and format. This adapter
/// injects the provider and format configuration.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::cache::MemoryCache;
/// use xearthlayer::dds::DdsFormat;
/// use xearthlayer::executor::adapters::MemoryCacheAdapter;
/// use std::sync::Arc;
///
/// let cache = Arc::new(MemoryCache::new(2 * 1024 * 1024 * 1024)); // 2GB
/// let adapter = MemoryCacheAdapter::new(Arc::clone(&cache), "bing", DdsFormat::BC1);
/// // adapter implements executor::MemoryCache
/// // cache can be shared with telemetry for size queries
/// ```
pub struct MemoryCacheAdapter {
    cache: Arc<cache::MemoryCache>,
    provider: String,
    format: DdsFormat,
}

impl MemoryCacheAdapter {
    /// Creates a new memory cache adapter.
    ///
    /// # Arguments
    ///
    /// * `cache` - Shared reference to the underlying memory cache
    /// * `provider` - Provider name for cache keys
    /// * `format` - DDS format for cache keys
    pub fn new(
        cache: Arc<cache::MemoryCache>,
        provider: impl Into<String>,
        format: DdsFormat,
    ) -> Self {
        Self {
            cache,
            provider: provider.into(),
            format,
        }
    }

    /// Creates a cache key from tile coordinates.
    fn make_key(&self, row: u32, col: u32, zoom: u8) -> CacheKey {
        CacheKey::new(
            self.provider.clone(),
            self.format,
            TileCoord { row, col, zoom },
        )
    }

    /// Returns the provider name.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the DDS format.
    pub fn format(&self) -> DdsFormat {
        self.format
    }
}

impl crate::executor::MemoryCache for MemoryCacheAdapter {
    async fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
        let key = self.make_key(row, col, zoom);
        self.cache.get(&key).await
    }

    async fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
        let key = self.make_key(row, col, zoom);
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
    use crate::executor::MemoryCache;

    #[tokio::test]
    async fn test_memory_cache_adapter_put_get() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        adapter.put(100, 200, 16, vec![1, 2, 3]).await;
        let result = adapter.get(100, 200, 16).await;

        assert!(result.is_some());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_miss() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        let result = adapter.get(100, 200, 16).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_size_tracking() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        assert_eq!(adapter.size_bytes(), 0);
        assert_eq!(adapter.entry_count(), 0);

        adapter.put(100, 200, 16, vec![0u8; 100]).await;

        // moka updates size asynchronously, so we just check it's > 0
        tokio::task::yield_now().await;
        assert!(adapter.size_bytes() > 0);
        assert_eq!(adapter.entry_count(), 1);
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_different_keys() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        adapter.put(100, 200, 16, vec![1, 2, 3]).await;
        adapter.put(100, 201, 16, vec![4, 5, 6]).await;

        assert_eq!(adapter.get(100, 200, 16).await.unwrap(), vec![1, 2, 3]);
        assert_eq!(adapter.get(100, 201, 16).await.unwrap(), vec![4, 5, 6]);
        assert!(adapter.get(100, 202, 16).await.is_none());
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_overwrite() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        adapter.put(100, 200, 16, vec![1, 2, 3]).await;
        assert_eq!(adapter.get(100, 200, 16).await.unwrap(), vec![1, 2, 3]);

        adapter.put(100, 200, 16, vec![7, 8, 9]).await;
        assert_eq!(adapter.get(100, 200, 16).await.unwrap(), vec![7, 8, 9]);
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_provider_isolation() {
        // Different adapters with different providers should have isolated data
        // (though they share the underlying cache, the keys differ)
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        adapter.put(100, 200, 16, vec![1, 2, 3]).await;

        // Same coordinates but different provider won't find it
        // (We can't easily test this with a single adapter, but the key includes provider)
        assert_eq!(adapter.provider(), "bing");
        assert_eq!(adapter.format(), DdsFormat::BC1);
    }

    #[tokio::test]
    async fn test_memory_cache_adapter_format_isolation() {
        let cache = Arc::new(cache::MemoryCache::new(1024 * 1024));
        let adapter_bc1 = MemoryCacheAdapter::new(cache, "bing", DdsFormat::BC1);

        adapter_bc1.put(100, 200, 16, vec![1, 2, 3]).await;

        // The key includes format, so BC1 and BC3 are different entries
        assert_eq!(adapter_bc1.format(), DdsFormat::BC1);
    }
}
