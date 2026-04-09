//! Chunk cache client for JPEG chunk storage.
//!
//! This client wraps a generic `Cache` with:
//! - Key translation: chunk coordinates → `"chunk:{zoom}:{tile_row}:{tile_col}:{chunk_row}:{chunk_col}"`
//! - Metrics injection: cache hit/miss reporting
//!
//! # Key Format
//!
//! Keys follow the format `chunk:{zoom}:{tile_row}:{tile_col}:{chunk_row}:{chunk_col}`.
//! Example: `chunk:15:12754:5279:8:12`

use std::sync::Arc;

use tracing::warn;

use crate::cache::traits::Cache;
use crate::metrics::MetricsClient;

/// Cache client for JPEG chunk storage.
///
/// Translates chunk coordinates to cache keys and optionally reports metrics
/// on cache hits and misses.
pub struct ChunkCacheClient {
    /// The underlying generic cache.
    cache: Arc<dyn Cache>,

    /// Optional metrics client for hit/miss reporting.
    metrics: Option<MetricsClient>,
}

impl ChunkCacheClient {
    /// Create a new chunk cache client without metrics.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            cache,
            metrics: None,
        }
    }

    /// Create a new chunk cache client with metrics.
    ///
    /// # Arguments
    ///
    /// * `cache` - The underlying cache implementation
    /// * `metrics` - Metrics client for reporting
    pub fn with_metrics(cache: Arc<dyn Cache>, metrics: MetricsClient) -> Self {
        Self {
            cache,
            metrics: Some(metrics),
        }
    }

    /// Get a chunk from the cache.
    ///
    /// Reports cache hit/miss to metrics if configured.
    ///
    /// # Arguments
    ///
    /// * `tile_row` - Tile row coordinate
    /// * `tile_col` - Tile column coordinate
    /// * `zoom` - Tile zoom level
    /// * `chunk_row` - Chunk row within tile (0-15)
    /// * `chunk_col` - Chunk column within tile (0-15)
    ///
    /// # Returns
    ///
    /// `Some(data)` if cached, `None` otherwise
    pub async fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> Option<Vec<u8>> {
        let key = Self::chunk_to_key(tile_row, tile_col, zoom, chunk_row, chunk_col);
        match self.cache.get(&key).await {
            Ok(Some(data)) => {
                if let Some(ref m) = self.metrics {
                    m.chunk_disk_cache_hit(data.len() as u64);
                }
                Some(data)
            }
            Ok(None) => {
                if let Some(ref m) = self.metrics {
                    m.chunk_disk_cache_miss();
                }
                None
            }
            Err(e) => {
                warn!(error = %e, key = %key, "Chunk cache get failed");
                if let Some(ref m) = self.metrics {
                    m.chunk_disk_cache_miss();
                }
                None
            }
        }
    }

    /// Store a chunk in the cache.
    ///
    /// # Arguments
    ///
    /// * `tile_row` - Tile row coordinate
    /// * `tile_col` - Tile column coordinate
    /// * `zoom` - Tile zoom level
    /// * `chunk_row` - Chunk row within tile (0-15)
    /// * `chunk_col` - Chunk column within tile (0-15)
    /// * `data` - The JPEG chunk data
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, `Err` on failure
    pub async fn set(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        let key = Self::chunk_to_key(tile_row, tile_col, zoom, chunk_row, chunk_col);
        self.cache
            .set(&key, data)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// Check if a chunk exists in the cache.
    ///
    /// # Arguments
    ///
    /// * `tile_row` - Tile row coordinate
    /// * `tile_col` - Tile column coordinate
    /// * `zoom` - Tile zoom level
    /// * `chunk_row` - Chunk row within tile (0-15)
    /// * `chunk_col` - Chunk column within tile (0-15)
    pub async fn contains(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> bool {
        let key = Self::chunk_to_key(tile_row, tile_col, zoom, chunk_row, chunk_col);
        self.cache.contains(&key).await.unwrap_or(false)
    }

    /// Get the current cache size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.cache.size_bytes()
    }

    /// Get the current number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Convert chunk coordinates to a cache key.
    ///
    /// Format: `chunk:{zoom}:{tile_row}:{tile_col}:{chunk_row}:{chunk_col}`
    fn chunk_to_key(
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> String {
        format!(
            "chunk:{}:{}:{}:{}:{}",
            zoom, tile_row, tile_col, chunk_row, chunk_col
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheService, ServiceCacheConfig};

    #[test]
    fn test_chunk_to_key() {
        let key = ChunkCacheClient::chunk_to_key(12754, 5279, 15, 8, 12);
        assert_eq!(key, "chunk:15:12754:5279:8:12");
    }

    #[tokio::test]
    async fn test_chunk_client_set_and_get() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = ChunkCacheClient::new(service.cache());

        let data = vec![1, 2, 3, 4, 5];

        client.set(100, 200, 15, 8, 12, data.clone()).await.unwrap();

        let result = client.get(100, 200, 15, 8, 12).await;
        assert_eq!(result, Some(data));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_chunk_client_get_missing() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = ChunkCacheClient::new(service.cache());

        let result = client.get(999, 999, 15, 8, 12).await;
        assert!(result.is_none());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_chunk_client_contains() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = ChunkCacheClient::new(service.cache());

        assert!(!client.contains(100, 200, 15, 8, 12).await);

        client
            .set(100, 200, 15, 8, 12, vec![1, 2, 3])
            .await
            .unwrap();

        assert!(client.contains(100, 200, 15, 8, 12).await);

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_chunk_client_different_chunks_same_tile() {
        let service = CacheService::start(ServiceCacheConfig::memory(1_000_000, None))
            .await
            .unwrap();
        let client = ChunkCacheClient::new(service.cache());

        // Same tile, different chunk positions
        client.set(100, 200, 15, 0, 0, vec![1, 1, 1]).await.unwrap();
        client.set(100, 200, 15, 0, 1, vec![2, 2, 2]).await.unwrap();
        client
            .set(100, 200, 15, 15, 15, vec![3, 3, 3])
            .await
            .unwrap();

        assert_eq!(client.get(100, 200, 15, 0, 0).await, Some(vec![1, 1, 1]));
        assert_eq!(client.get(100, 200, 15, 0, 1).await, Some(vec![2, 2, 2]));
        assert_eq!(client.get(100, 200, 15, 15, 15).await, Some(vec![3, 3, 3]));

        service.shutdown().await;
    }
}
