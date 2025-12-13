//! Cache trait definition for dependency injection.

use crate::cache::types::{CacheError, CacheKey};
use crate::cache::CacheStatistics;
use std::any::Any;

/// Cache abstraction for DDS tiles.
///
/// Enables different caching strategies (two-tier, memory-only, no-op)
/// to be used interchangeably, following the Liskov Substitution Principle.
///
/// # Example
///
/// ```
/// use xearthlayer::cache::{Cache, CacheSystem, CacheConfig, CacheKey};
/// use xearthlayer::coord::TileCoord;
/// use xearthlayer::dds::DdsFormat;
///
/// fn process_with_cache(cache: &dyn Cache) {
///     let key = CacheKey::new("bing", DdsFormat::BC1, TileCoord { row: 100, col: 200, zoom: 15 });
///
///     if let Some(data) = cache.get(&key) {
///         // Use cached data
///     } else {
///         // Generate and cache
///         let data = vec![1, 2, 3];
///         cache.put(key, data).ok();
///     }
/// }
/// ```
pub trait Cache: Send + Sync {
    /// Get cached data for the given key.
    ///
    /// Returns `Some(data)` if found in cache, `None` otherwise.
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>>;

    /// Store data in the cache.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key identifying the tile
    /// * `data` - Tile data to cache
    fn put(&self, key: CacheKey, data: Vec<u8>) -> Result<(), CacheError>;

    /// Check if a key exists in the cache.
    fn contains(&self, key: &CacheKey) -> bool;

    /// Clear all entries from the cache.
    fn clear(&self) -> Result<(), CacheError>;

    /// Get cache statistics.
    ///
    /// Returns aggregated statistics for all cache tiers.
    fn stats(&self) -> CacheStatistics;

    /// Get the provider name.
    fn provider(&self) -> &str;

    /// Get a reference to self as Any for downcasting.
    ///
    /// This enables runtime type inspection for cache implementations,
    /// which is useful for accessing implementation-specific features.
    fn as_any(&self) -> &dyn Any;
}

/// No-op cache implementation that never caches.
///
/// Always returns cache misses. Useful for:
/// - Testing tile generation without caching overhead
/// - Debugging cache-related issues
/// - Comparing performance with/without caching
/// - Development workflows
///
/// # Example
///
/// ```
/// use xearthlayer::cache::{Cache, NoOpCache, CacheKey};
/// use xearthlayer::coord::TileCoord;
/// use xearthlayer::dds::DdsFormat;
///
/// let cache = NoOpCache::new("bing");
/// let key = CacheKey::new("bing", DdsFormat::BC1, TileCoord { row: 100, col: 200, zoom: 15 });
///
/// // Always returns None
/// assert_eq!(cache.get(&key), None);
///
/// // Put succeeds but doesn't store
/// cache.put(key.clone(), vec![1, 2, 3]).unwrap();
/// assert_eq!(cache.get(&key), None);
/// ```
#[derive(Debug, Clone)]
pub struct NoOpCache {
    provider: String,
}

impl NoOpCache {
    /// Create a new no-op cache.
    ///
    /// # Arguments
    ///
    /// * `provider` - Provider name (used for statistics only)
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
        }
    }
}

impl Cache for NoOpCache {
    fn get(&self, _key: &CacheKey) -> Option<Vec<u8>> {
        None // Always miss
    }

    fn put(&self, _key: CacheKey, _data: Vec<u8>) -> Result<(), CacheError> {
        Ok(()) // Accept but don't store
    }

    fn contains(&self, _key: &CacheKey) -> bool {
        false // Never contains
    }

    fn clear(&self) -> Result<(), CacheError> {
        Ok(()) // Nothing to clear
    }

    fn stats(&self) -> CacheStatistics {
        use crate::cache::CacheStats;
        CacheStatistics::from_stats(&CacheStats::new())
    }

    fn provider(&self) -> &str {
        &self.provider
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::dds::DdsFormat;

    fn create_test_key() -> CacheKey {
        CacheKey::new(
            "test",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_noop_cache_new() {
        let cache = NoOpCache::new("bing");
        assert_eq!(cache.provider(), "bing");
    }

    #[test]
    fn test_noop_cache_always_misses() {
        let cache = NoOpCache::new("bing");
        let key = create_test_key();

        assert_eq!(cache.get(&key), None);
    }

    #[test]
    fn test_noop_cache_put_succeeds() {
        let cache = NoOpCache::new("bing");
        let key = create_test_key();
        let data = vec![1, 2, 3, 4, 5];

        // Put succeeds
        assert!(cache.put(key.clone(), data).is_ok());

        // But doesn't actually cache
        assert_eq!(cache.get(&key), None);
    }

    #[test]
    fn test_noop_cache_never_contains() {
        let cache = NoOpCache::new("bing");
        let key = create_test_key();

        assert!(!cache.contains(&key));

        cache.put(key.clone(), vec![1, 2, 3]).unwrap();
        assert!(!cache.contains(&key));
    }

    #[test]
    fn test_noop_cache_clear() {
        let cache = NoOpCache::new("bing");
        assert!(cache.clear().is_ok());
    }

    #[test]
    fn test_noop_cache_stats() {
        let cache = NoOpCache::new("bing");
        let stats = cache.stats();

        assert_eq!(stats.stats.memory_hits, 0);
        assert_eq!(stats.stats.memory_misses, 0);
        assert_eq!(stats.stats.disk_hits, 0);
        assert_eq!(stats.stats.disk_misses, 0);
    }

    #[test]
    fn test_noop_cache_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoOpCache>();
    }

    #[test]
    fn test_noop_cache_as_trait_object() {
        let cache: Box<dyn Cache> = Box::new(NoOpCache::new("bing"));
        let key = create_test_key();

        assert_eq!(cache.get(&key), None);
        assert!(cache.put(key, vec![1, 2, 3]).is_ok());
    }
}
