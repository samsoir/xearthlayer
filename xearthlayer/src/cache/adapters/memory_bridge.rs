//! Memory cache bridge adapter.
//!
//! Implements `executor::MemoryCache` trait using `TileCacheClient` from the
//! new cache service infrastructure.

use std::future::Future;
use std::sync::Arc;

use crate::cache::clients::TileCacheClient;
use crate::cache::traits::Cache;
use crate::coord::TileCoord;
use crate::executor::MemoryCache;

/// Bridge adapter implementing `executor::MemoryCache` using the new cache service.
///
/// This adapter allows existing executor code to use the new cache infrastructure
/// without modification.
///
/// Memory cache hit/miss metrics are tracked by the executor daemon
/// (which knows the request origin) rather than the bridge layer.
pub struct MemoryCacheBridge {
    /// The underlying tile cache client.
    client: TileCacheClient,
}

impl MemoryCacheBridge {
    /// Create a new memory cache bridge.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            client: TileCacheClient::new(cache),
        }
    }
}

// The MemoryCache trait uses `impl Future<>` in its signature, which Clippy
// suggests converting to async fn. However, we must match the trait's signature.
#[allow(clippy::manual_async_fn)]
impl MemoryCache for MemoryCacheBridge {
    fn get(&self, row: u32, col: u32, zoom: u8) -> impl Future<Output = Option<Vec<u8>>> + Send {
        async move {
            let tile = TileCoord { row, col, zoom };
            self.client.get(&tile).await
        }
    }

    fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) -> impl Future<Output = ()> + Send {
        async move {
            let tile = TileCoord { row, col, zoom };
            self.client.set(&tile, data).await;
        }
    }

    fn size_bytes(&self) -> usize {
        self.client.size_bytes() as usize
    }

    fn entry_count(&self) -> usize {
        self.client.entry_count() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheService, ServiceCacheConfig};

    #[tokio::test]
    async fn test_memory_bridge_put_and_get() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = MemoryCacheBridge::new(service.cache());

        let data = vec![1, 2, 3, 4, 5];
        bridge.put(100, 200, 15, data.clone()).await;

        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(data));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_memory_bridge_get_missing() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = MemoryCacheBridge::new(service.cache());

        let result = bridge.get(999, 999, 15).await;
        assert!(result.is_none());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_memory_bridge_size_tracking() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = MemoryCacheBridge::new(service.cache());

        bridge.put(100, 200, 15, vec![0u8; 1000]).await;
        bridge.put(101, 200, 15, vec![0u8; 2000]).await;

        // Run gc to sync stats
        let _ = service.cache().gc().await;

        assert!(bridge.size_bytes() >= 3000);
        assert!(bridge.entry_count() >= 2);

        service.shutdown().await;
    }
}
