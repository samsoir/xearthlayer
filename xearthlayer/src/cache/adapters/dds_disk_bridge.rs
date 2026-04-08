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
use crate::executor::{DdsDiskCache, DdsDiskCacheChecker};

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

impl DdsDiskCacheChecker for DdsDiskCacheBridge {
    fn tile_exists(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let tile = TileCoord { row, col, zoom };
            self.client.contains(&tile).await
        })
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

    /// Simulates the fire-and-forget write pattern from BuildAndCacheDdsTask,
    /// then reads back through the same bridge (as the executor daemon would).
    /// This reproduces the exact production scenario where prefetch writes
    /// a tile and FUSE later reads it.
    #[tokio::test]
    async fn test_fire_and_forget_write_then_read() {
        let (bridge, service, _dir) = create_disk_bridge().await;
        let bridge = Arc::new(bridge);

        // Simulate fire-and-forget write (as BuildAndCacheDdsTask does)
        let write_bridge = Arc::clone(&bridge);
        let data = vec![42u8; 1024];
        let write_data = data.clone();
        let handle = tokio::spawn(async move {
            write_bridge.put(1477, 980, 12, write_data).await;
        });

        // Wait for the fire-and-forget write to complete
        handle.await.unwrap();

        // Now read back through the same bridge (as executor daemon would)
        let result = bridge.get(1477, 980, 12).await;
        assert_eq!(
            result,
            Some(data),
            "DDS disk cache should find tile written by fire-and-forget task"
        );

        service.shutdown().await;
    }

    /// Tests that a tile written via one Arc clone of the bridge can be
    /// read via a different Arc clone (simulating factory vs daemon usage).
    #[tokio::test]
    async fn test_shared_bridge_write_read_different_clones() {
        let (bridge, service, _dir) = create_disk_bridge().await;
        let bridge = Arc::new(bridge);

        // Writer clone (simulates factory/task path)
        let writer = Arc::clone(&bridge);
        // Reader clone (simulates daemon path)
        let reader = Arc::clone(&bridge);

        let data = vec![99u8; 2048];
        writer.put(1530, 950, 12, data.clone()).await;

        let result = reader.get(1530, 950, 12).await;
        assert_eq!(
            result,
            Some(data),
            "Reader clone should find tile written by writer clone"
        );

        service.shutdown().await;
    }
}
