//! DDS disk cache bridge adapter.
//!
//! Implements `executor::DdsDiskCache` trait using `TileCacheClient` from the
//! cache service infrastructure. Uses the same tile-level key format as the
//! memory cache bridge (`"tile:{zoom}:{row}:{col}"`), but backed by a
//! `DiskCacheProvider` for persistent DDS tile storage.

use std::future::Future;
use std::sync::Arc;

use crate::cache::clients::TileCacheClient;
use crate::cache::traits::Cache;
use crate::coord::TileCoord;
use crate::executor::DdsDiskCache;

/// Bridge adapter implementing `executor::DdsDiskCache` using the cache service.
///
/// This adapter sits between the executor and the DDS disk cache provider,
/// translating tile coordinates into cache keys. It enables the executor daemon
/// and build tasks to read/write DDS tiles from/to disk without knowing about
/// the underlying cache infrastructure.
pub struct DdsDiskCacheBridge {
    /// The underlying tile cache client.
    client: TileCacheClient,
}

impl DdsDiskCacheBridge {
    /// Create a new DDS disk cache bridge.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation (backed by DiskCacheProvider)
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            client: TileCacheClient::new(cache),
        }
    }
}

#[allow(clippy::manual_async_fn)]
impl DdsDiskCache for DdsDiskCacheBridge {
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

    fn contains(&self, row: u32, col: u32, zoom: u8) -> impl Future<Output = bool> + Send {
        async move {
            let tile = TileCoord { row, col, zoom };
            self.client.contains(&tile).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheService, ServiceCacheConfig};
    use tempfile::TempDir;

    async fn create_disk_bridge() -> (DdsDiskCacheBridge, CacheService, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = ServiceCacheConfig::disk(
            100_000_000,
            dir.path().to_path_buf(),
            std::time::Duration::from_secs(3600),
            "test".to_string(),
        );
        let service = CacheService::start(config).await.unwrap();
        let bridge = DdsDiskCacheBridge::new(service.cache());
        (bridge, service, dir)
    }

    #[tokio::test]
    async fn test_dds_disk_bridge_put_and_get() {
        let (bridge, service, _dir) = create_disk_bridge().await;

        let data = vec![1, 2, 3, 4, 5];
        bridge.put(100, 200, 15, data.clone()).await;

        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(data));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_dds_disk_bridge_get_missing() {
        let (bridge, service, _dir) = create_disk_bridge().await;

        let result = bridge.get(999, 999, 15).await;
        assert!(result.is_none());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_dds_disk_bridge_contains() {
        let (bridge, service, _dir) = create_disk_bridge().await;

        assert!(!bridge.contains(100, 200, 15).await);

        bridge.put(100, 200, 15, vec![1, 2, 3]).await;

        assert!(bridge.contains(100, 200, 15).await);

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_dds_disk_bridge_contains_missing() {
        let (bridge, service, _dir) = create_disk_bridge().await;

        assert!(!bridge.contains(100, 200, 15).await);

        service.shutdown().await;
    }
}
