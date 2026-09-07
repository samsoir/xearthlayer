//! Disk cache bridge adapter.
//!
//! Implements `executor::DiskCache` trait using `ChunkCacheClient` from the
//! new cache service infrastructure.

use bytes::Bytes;
use std::future::Future;
use std::sync::Arc;

use crate::cache::clients::ChunkCacheClient;
use crate::cache::traits::Cache;
use crate::executor::DiskCache;
use crate::metrics::MetricsClient;

/// Bridge adapter implementing `executor::DiskCache` using the new cache service.
///
/// This adapter allows existing executor code to use the new cache infrastructure
/// without modification.
pub struct DiskCacheBridge {
    /// The underlying chunk cache client.
    client: ChunkCacheClient,
}

impl DiskCacheBridge {
    /// Create a new disk cache bridge.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            client: ChunkCacheClient::new(cache),
        }
    }

    /// Create a new disk cache bridge with metrics.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    /// * `metrics` - Metrics client for reporting
    pub fn with_metrics(cache: Arc<dyn Cache>, metrics: MetricsClient) -> Self {
        Self {
            client: ChunkCacheClient::with_metrics(cache, metrics),
        }
    }
}

// The DiskCache trait uses `impl Future<>` in its signature, which Clippy
// suggests converting to async fn. However, we must match the trait's signature.
#[allow(clippy::manual_async_fn)]
impl DiskCache for DiskCacheBridge {
    fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> impl Future<Output = Option<Bytes>> + Send {
        async move {
            self.client
                .get(tile_row, tile_col, zoom, chunk_row, chunk_col)
                .await
        }
    }

    fn put(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Bytes,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
        async move {
            self.client
                .set(tile_row, tile_col, zoom, chunk_row, chunk_col, data)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheService, ServiceCacheConfig};

    #[tokio::test]
    async fn test_disk_bridge_put_and_get() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = DiskCacheBridge::new(service.cache());

        let data = vec![1, 2, 3, 4, 5];
        bridge
            .put(100, 200, 15, 8, 12, data.clone().into())
            .await
            .unwrap();

        let result = bridge.get(100, 200, 15, 8, 12).await;
        assert_eq!(result, Some(Bytes::from(data)));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_bridge_get_missing() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = DiskCacheBridge::new(service.cache());

        let result = bridge.get(999, 999, 15, 8, 12).await;
        assert!(result.is_none());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_bridge_multiple_chunks() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let bridge = DiskCacheBridge::new(service.cache());

        // Store multiple chunks for the same tile
        bridge
            .put(100, 200, 15, 0, 0, vec![1, 1].into())
            .await
            .unwrap();
        bridge
            .put(100, 200, 15, 0, 1, vec![2, 2].into())
            .await
            .unwrap();
        bridge
            .put(100, 200, 15, 15, 15, vec![3, 3].into())
            .await
            .unwrap();

        // Verify they're all retrievable
        assert_eq!(
            bridge.get(100, 200, 15, 0, 0).await,
            Some(Bytes::from(vec![1, 1]))
        );
        assert_eq!(
            bridge.get(100, 200, 15, 0, 1).await,
            Some(Bytes::from(vec![2, 2]))
        );
        assert_eq!(
            bridge.get(100, 200, 15, 15, 15).await,
            Some(Bytes::from(vec![3, 3]))
        );

        service.shutdown().await;
    }
}
