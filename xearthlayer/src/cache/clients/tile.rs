//! Tile cache client for DDS tile storage.
//!
//! This client wraps a generic `Cache` with:
//! - Key translation: `TileCoord` → `"tile:{zoom}:{row}:{col}"`
//! - Metrics injection: cache hit/miss reporting
//!
//! # Key Format
//!
//! Keys follow the format `tile:{zoom}:{row}:{col}` for debuggability.
//! Example: `tile:15:12754:5279`

use std::sync::Arc;

use tracing::warn;

use crate::cache::traits::Cache;
use crate::coord::TileCoord;

/// Cache client for DDS tile storage.
///
/// Translates `TileCoord` to cache keys.
///
/// Memory cache hit/miss metrics are tracked by the executor daemon
/// (which knows the request origin) rather than here.
pub struct TileCacheClient {
    /// The underlying generic cache.
    cache: Arc<dyn Cache>,
}

impl TileCacheClient {
    /// Create a new tile cache client.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Get a tile from the cache.
    ///
    /// Reports cache hit/miss to metrics if configured.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates
    ///
    /// # Returns
    ///
    /// `Some(data)` if cached, `None` otherwise
    pub async fn get(&self, tile: &TileCoord) -> Option<Vec<u8>> {
        let key = Self::tile_to_key(tile);
        match self.cache.get(&key).await {
            Ok(Some(data)) => Some(data),
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, key = %key, "Tile cache get failed");
                None
            }
        }
    }

    /// Store a tile in the cache.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates
    /// * `data` - The DDS tile data
    pub async fn set(&self, tile: &TileCoord, data: Vec<u8>) {
        let key = Self::tile_to_key(tile);
        if let Err(e) = self.cache.set(&key, data).await {
            warn!(error = %e, key = %key, "Tile cache set failed");
        }
    }

    /// Check if a tile exists in the cache.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates
    pub async fn contains(&self, tile: &TileCoord) -> bool {
        let key = Self::tile_to_key(tile);
        self.cache.contains(&key).await.unwrap_or(false)
    }

    /// Delete a tile from the cache.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates
    ///
    /// # Returns
    ///
    /// `true` if the tile was deleted, `false` if it didn't exist
    pub async fn delete(&self, tile: &TileCoord) -> bool {
        let key = Self::tile_to_key(tile);
        self.cache.delete(&key).await.unwrap_or(false)
    }

    /// Get the current cache size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.cache.size_bytes()
    }

    /// Get the current number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Convert tile coordinates to a cache key.
    ///
    /// Format: `tile:{zoom}:{row}:{col}`
    fn tile_to_key(tile: &TileCoord) -> String {
        format!("tile:{}:{}:{}", tile.zoom, tile.row, tile.col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheService, ServiceCacheConfig};

    #[tokio::test]
    async fn test_tile_to_key() {
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };
        let key = TileCacheClient::tile_to_key(&tile);
        assert_eq!(key, "tile:15:12754:5279");
    }

    #[tokio::test]
    async fn test_tile_client_set_and_get() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = TileCacheClient::new(service.cache());

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };
        let data = vec![1, 2, 3, 4, 5];

        client.set(&tile, data.clone()).await;

        let result = client.get(&tile).await;
        assert_eq!(result, Some(data));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_tile_client_get_missing() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = TileCacheClient::new(service.cache());

        let tile = TileCoord {
            row: 999,
            col: 999,
            zoom: 15,
        };

        let result = client.get(&tile).await;
        assert!(result.is_none());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_tile_client_contains() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = TileCacheClient::new(service.cache());

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        assert!(!client.contains(&tile).await);

        client.set(&tile, vec![1, 2, 3]).await;

        assert!(client.contains(&tile).await);

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_tile_client_delete() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = TileCacheClient::new(service.cache());

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        client.set(&tile, vec![1, 2, 3]).await;
        assert!(client.contains(&tile).await);

        let deleted = client.delete(&tile).await;
        assert!(deleted);
        assert!(!client.contains(&tile).await);

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_tile_client_size_tracking() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = TileCacheClient::new(service.cache());

        let tile1 = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };
        let tile2 = TileCoord {
            row: 101,
            col: 200,
            zoom: 15,
        };

        client.set(&tile1, vec![0u8; 1000]).await;
        client.set(&tile2, vec![0u8; 2000]).await;

        // Run gc to sync stats
        let _ = service.cache().gc().await;

        assert!(client.size_bytes() >= 3000);
        assert!(client.entry_count() >= 2);

        service.shutdown().await;
    }
}
