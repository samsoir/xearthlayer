//! In-memory cache provider using moka.
//!
//! This provider wraps `moka::future::Cache` to provide an async-safe,
//! lock-free in-memory cache with automatic LRU eviction.
//!
//! # Why moka?
//!
//! - Lock-free reads (common case)
//! - Concurrent writes without blocking
//! - Automatic LRU eviction without explicit locking
//! - Memory-bounded with configurable limits
//! - Designed for async contexts
//!
//! # Garbage Collection
//!
//! Moka handles eviction automatically when the cache exceeds its size limit.
//! The `gc()` method runs pending maintenance tasks but typically doesn't need
//! to be called manually.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moka::future::Cache as MokaCache;

use crate::cache::traits::{BoxFuture, Cache, GcResult, ServiceCacheError};
use crate::metrics::MetricsClient;

/// In-memory cache provider using moka.
///
/// Provides fast, async-safe access to cached data with automatic LRU eviction.
/// The underlying moka cache uses lock-free data structures, making it safe
/// for use across multiple async tasks.
pub struct MemoryCacheProvider {
    /// The underlying moka cache.
    cache: MokaCache<String, Vec<u8>>,

    /// Maximum size in bytes.
    max_size_bytes: AtomicU64,

    /// Optional metrics client for self-reporting cache size.
    metrics_client: Option<MetricsClient>,
}

impl MemoryCacheProvider {
    /// Create a new memory cache provider.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum cache size in bytes
    /// * `ttl` - Optional time-to-live for entries
    pub fn new(max_size_bytes: u64, ttl: Option<Duration>) -> Self {
        let mut builder = MokaCache::builder()
            // Weight each entry by its data size
            .weigher(|_key: &String, value: &Vec<u8>| -> u32 {
                // moka uses u32 for weights, cap at u32::MAX for very large entries
                value.len().min(u32::MAX as usize) as u32
            })
            // Maximum total weight (size in bytes)
            .max_capacity(max_size_bytes);

        // Add TTL if specified
        if let Some(ttl_duration) = ttl {
            builder = builder.time_to_live(ttl_duration);
        }

        let cache = builder.build();

        Self {
            cache,
            max_size_bytes: AtomicU64::new(max_size_bytes),
            metrics_client: None,
        }
    }

    /// Set the metrics client for self-reporting cache size.
    ///
    /// When set, the provider emits `MemoryCacheSizeUpdate` events after
    /// every mutation (set, delete, gc), keeping the TUI accurate without
    /// requiring external callers to push size metrics.
    pub fn with_metrics(mut self, client: MetricsClient) -> Self {
        self.metrics_client = Some(client);
        self
    }

    /// Report current cache size to metrics.
    ///
    /// Called internally after every mutation. External callers should
    /// NOT emit cache size metrics — the provider owns this responsibility.
    fn report_size_to_metrics(&self) {
        if let Some(ref client) = self.metrics_client {
            client.memory_cache_size(self.cache.weighted_size());
        }
    }
}

impl Cache for MemoryCacheProvider {
    fn set(&self, key: &str, value: Vec<u8>) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        let key = key.to_string();
        Box::pin(async move {
            self.cache.insert(key, value).await;
            self.report_size_to_metrics();
            Ok(())
        })
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, ServiceCacheError>> {
        let key = key.to_string();
        Box::pin(async move {
            let result = self.cache.get(&key).await;
            match &result {
                Some(data) => {
                    tracing::debug!(
                        key = %key,
                        size = data.len(),
                        "Memory cache hit"
                    );
                }
                None => {
                    tracing::debug!(key = %key, "Memory cache miss");
                }
            }
            Ok(result)
        })
    }

    fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let key = key.to_string();
        Box::pin(async move {
            let existed = self.cache.contains_key(&key);
            self.cache.remove(&key).await;
            self.report_size_to_metrics();
            Ok(existed)
        })
    }

    fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let key = key.to_string();
        Box::pin(async move { Ok(self.cache.contains_key(&key)) })
    }

    fn contains_sync(&self, key: &str) -> bool {
        self.cache.contains_key(key)
    }

    fn size_bytes(&self) -> u64 {
        self.cache.weighted_size()
    }

    fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        Box::pin(async move {
            // Update our tracked max size
            self.max_size_bytes.store(size_bytes, Ordering::Relaxed);

            // moka doesn't support dynamic resize, but we can trigger GC
            // to evict entries if we're over the new limit
            self.cache.run_pending_tasks().await;

            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>> {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let size_before = self.cache.weighted_size();
            let count_before = self.cache.entry_count();

            // Run pending maintenance tasks (eviction, etc.)
            self.cache.run_pending_tasks().await;

            let size_after = self.cache.weighted_size();
            let count_after = self.cache.entry_count();

            self.report_size_to_metrics();

            Ok(GcResult {
                entries_removed: count_before.saturating_sub(count_after) as usize,
                bytes_freed: size_before.saturating_sub(size_after),
                duration_ms: start.elapsed().as_millis() as u64,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_provider_new() {
        let provider = MemoryCacheProvider::new(1_000_000, None);
        assert_eq!(provider.max_size_bytes(), 1_000_000);
        assert_eq!(provider.entry_count(), 0);
        assert_eq!(provider.size_bytes(), 0);
    }

    #[tokio::test]
    async fn test_memory_provider_set_and_get() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn test_memory_provider_get_missing() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        let value = provider.get("nonexistent").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_memory_provider_delete_existing() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        provider.set("key1", vec![1, 2, 3]).await.unwrap();
        assert!(provider.contains("key1").await.unwrap());

        let deleted = provider.delete("key1").await.unwrap();
        assert!(deleted);
        assert!(!provider.contains("key1").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_provider_delete_missing() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        let deleted = provider.delete("nonexistent").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_memory_provider_contains() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        assert!(!provider.contains("key1").await.unwrap());

        provider.set("key1", vec![1]).await.unwrap();

        assert!(provider.contains("key1").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_contains_sync_agrees_with_contains() {
        let provider = MemoryCacheProvider::new(1_000_000, None);
        let key = "tile:15:12754:5279";

        assert!(!provider.contains_sync(key));
        provider.set(key, vec![0u8; 64]).await.unwrap();
        assert!(provider.contains_sync(key));
        assert_eq!(
            provider.contains(key).await.unwrap(),
            provider.contains_sync(key)
        );
    }

    #[tokio::test]
    async fn test_memory_provider_size_tracking() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        provider.set("key1", vec![0u8; 1000]).await.unwrap();
        provider.gc().await.unwrap(); // Ensure size is updated

        let size = provider.size_bytes();
        assert!(size >= 1000, "Expected size >= 1000, got {}", size);

        provider.set("key2", vec![0u8; 2000]).await.unwrap();
        provider.gc().await.unwrap();

        let size = provider.size_bytes();
        assert!(size >= 3000, "Expected size >= 3000, got {}", size);
    }

    #[tokio::test]
    async fn test_memory_provider_gc() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        provider.set("key1", vec![0u8; 1000]).await.unwrap();

        let result = provider.gc().await.unwrap();

        // GC should complete without error
        assert!(result.duration_ms < 1000); // Shouldn't take too long
    }

    #[tokio::test]
    async fn test_memory_provider_replace_existing() {
        let provider = MemoryCacheProvider::new(1_000_000, None);

        provider.set("key1", vec![1, 2, 3]).await.unwrap();
        provider.set("key1", vec![4, 5, 6, 7]).await.unwrap();
        provider.gc().await.unwrap(); // Run pending tasks to sync entry_count

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![4, 5, 6, 7]));
        assert_eq!(provider.entry_count(), 1);
    }

    #[tokio::test]
    async fn test_memory_provider_with_ttl() {
        let provider = MemoryCacheProvider::new(1_000_000, Some(Duration::from_millis(50)));

        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        // Value should exist immediately
        assert!(provider.get("key1").await.unwrap().is_some());

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;
        provider.gc().await.unwrap();

        // Value should be gone
        assert!(provider.get("key1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_provider_lru_eviction() {
        // Cache that can hold about 2.5 entries of 1000 bytes each
        let provider = MemoryCacheProvider::new(2500, None);

        // Add 3 entries (3000 bytes total, exceeds 2500 limit)
        provider.set("key1", vec![0u8; 1000]).await.unwrap();
        provider.set("key2", vec![0u8; 1000]).await.unwrap();
        provider.set("key3", vec![0u8; 1000]).await.unwrap();

        // Run GC to trigger eviction
        provider.gc().await.unwrap();

        // Wait a bit for async eviction
        tokio::time::sleep(Duration::from_millis(50)).await;
        provider.gc().await.unwrap();

        // Cache should be under limit
        assert!(
            provider.size_bytes() <= 2500,
            "Expected size <= 2500, got {}",
            provider.size_bytes()
        );
    }

    #[tokio::test]
    async fn test_memory_provider_concurrent_access() {
        use std::sync::Arc;

        let provider = Arc::new(MemoryCacheProvider::new(10_000_000, None));
        let mut handles = Vec::new();

        for i in 0..50 {
            let provider = Arc::clone(&provider);
            handles.push(tokio::spawn(async move {
                let key = format!("key{}", i);
                let data = vec![i as u8; 100];

                provider.set(&key, data.clone()).await.unwrap();
                let result = provider.get(&key).await.unwrap();
                assert_eq!(result, Some(data));
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Run pending tasks to sync entry_count (moka is eventually consistent)
        provider.gc().await.unwrap();
        assert_eq!(provider.entry_count(), 50);
    }
}
