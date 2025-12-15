//! Disk cache adapter for the async pipeline.
//!
//! Adapts disk cache operations to the pipeline's `DiskCache` trait.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Adapts disk cache operations to the pipeline's `DiskCache` trait.
///
/// The pipeline's disk cache operates at the chunk level (individual 256x256
/// images) while the existing disk cache operates at the tile level. This
/// adapter provides chunk-level caching with a hierarchical directory structure.
///
/// # Directory Structure
///
/// ```text
/// {cache_dir}/chunks/{provider}/{zoom}/{tile_row}/{tile_col}/{chunk_row}_{chunk_col}.jpg
/// ```
///
/// # Example
///
/// ```ignore
/// use xearthlayer::pipeline::adapters::DiskCacheAdapter;
/// use std::path::PathBuf;
///
/// let adapter = DiskCacheAdapter::new(
///     PathBuf::from("/home/user/.cache/xearthlayer"),
///     "bing",
/// );
/// // adapter implements pipeline::DiskCache
/// ```
pub struct DiskCacheAdapter {
    cache_dir: PathBuf,
    provider: String,
    /// Approximate bytes written to disk cache (tracked atomically).
    bytes_written: AtomicU64,
}

impl DiskCacheAdapter {
    /// Creates a new disk cache adapter.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Root directory for the cache
    /// * `provider` - Provider name for directory hierarchy
    pub fn new(cache_dir: PathBuf, provider: impl Into<String>) -> Self {
        Self {
            cache_dir,
            provider: provider.into(),
            bytes_written: AtomicU64::new(0),
        }
    }

    /// Returns the cache directory.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// Returns the provider name.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the approximate bytes written to the disk cache.
    ///
    /// This tracks bytes written during this session only, not the total
    /// disk usage of the cache directory.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// Constructs the path for a chunk file.
    fn chunk_path(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> PathBuf {
        self.cache_dir
            .join("chunks")
            .join(&self.provider)
            .join(zoom.to_string())
            .join(tile_row.to_string())
            .join(tile_col.to_string())
            .join(format!("{}_{}.jpg", chunk_row, chunk_col))
    }
}

impl crate::pipeline::DiskCache for DiskCacheAdapter {
    async fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> Option<Vec<u8>> {
        let path = self.chunk_path(tile_row, tile_col, zoom, chunk_row, chunk_col);

        // Use tokio's async file I/O
        tokio::fs::read(&path).await.ok()
    }

    async fn put(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        let path = self.chunk_path(tile_row, tile_col, zoom, chunk_row, chunk_col);
        let data_len = data.len() as u64;

        // Create parent directories
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write the chunk data
        tokio::fs::write(&path, data).await?;

        // Track bytes written
        self.bytes_written.fetch_add(data_len, Ordering::Relaxed);

        Ok(())
    }
}

/// A no-op disk cache that never caches anything.
///
/// Useful for testing or when disk caching is disabled.
pub struct NullDiskCache;

impl crate::pipeline::DiskCache for NullDiskCache {
    async fn get(
        &self,
        _tile_row: u32,
        _tile_col: u32,
        _zoom: u8,
        _chunk_row: u8,
        _chunk_col: u8,
    ) -> Option<Vec<u8>> {
        None
    }

    async fn put(
        &self,
        _tile_row: u32,
        _tile_col: u32,
        _zoom: u8,
        _chunk_row: u8,
        _chunk_col: u8,
        _data: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::DiskCache;

    #[test]
    fn test_disk_cache_adapter_path_construction() {
        let adapter = DiskCacheAdapter::new(PathBuf::from("/cache"), "bing");

        let path = adapter.chunk_path(100, 200, 16, 5, 10);

        assert_eq!(
            path,
            PathBuf::from("/cache/chunks/bing/16/100/200/5_10.jpg")
        );
    }

    #[test]
    fn test_disk_cache_adapter_path_with_different_coords() {
        let adapter = DiskCacheAdapter::new(PathBuf::from("/cache"), "google");

        // Different tile coordinates
        assert_eq!(
            adapter.chunk_path(0, 0, 1, 0, 0),
            PathBuf::from("/cache/chunks/google/1/0/0/0_0.jpg")
        );

        // Max chunk coordinates (15, 15)
        assert_eq!(
            adapter.chunk_path(12345, 67890, 20, 15, 15),
            PathBuf::from("/cache/chunks/google/20/12345/67890/15_15.jpg")
        );
    }

    #[test]
    fn test_disk_cache_adapter_accessors() {
        let adapter = DiskCacheAdapter::new(PathBuf::from("/my/cache"), "bing");

        assert_eq!(adapter.cache_dir(), &PathBuf::from("/my/cache"));
        assert_eq!(adapter.provider(), "bing");
    }

    #[tokio::test]
    async fn test_null_disk_cache_always_misses() {
        let cache = NullDiskCache;

        let result = cache.get(100, 200, 16, 0, 0).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_null_disk_cache_put_succeeds() {
        let cache = NullDiskCache;

        let result = cache.put(100, 200, 16, 0, 0, vec![1, 2, 3]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_null_disk_cache_no_persistence() {
        let cache = NullDiskCache;

        // Put data
        cache.put(100, 200, 16, 0, 0, vec![1, 2, 3]).await.unwrap();

        // Should still miss
        let result = cache.get(100, 200, 16, 0, 0).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_disk_cache_adapter_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let adapter = DiskCacheAdapter::new(temp_dir.path().to_path_buf(), "test");

        // Initially empty
        let result = adapter.get(100, 200, 16, 0, 0).await;
        assert!(result.is_none());

        // Put some data
        let data = vec![0xFF, 0xD8, 0xFF, 0xE0]; // JPEG magic bytes
        adapter.put(100, 200, 16, 0, 0, data.clone()).await.unwrap();

        // Should now be cached
        let result = adapter.get(100, 200, 16, 0, 0).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap(), data);
    }

    #[tokio::test]
    async fn test_disk_cache_adapter_multiple_chunks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let adapter = DiskCacheAdapter::new(temp_dir.path().to_path_buf(), "test");

        // Store multiple chunks for the same tile
        for chunk_row in 0..3u8 {
            for chunk_col in 0..3u8 {
                let data = vec![chunk_row, chunk_col];
                adapter
                    .put(100, 200, 16, chunk_row, chunk_col, data)
                    .await
                    .unwrap();
            }
        }

        // Verify all chunks can be retrieved
        for chunk_row in 0..3u8 {
            for chunk_col in 0..3u8 {
                let result = adapter.get(100, 200, 16, chunk_row, chunk_col).await;
                assert!(result.is_some());
                assert_eq!(result.unwrap(), vec![chunk_row, chunk_col]);
            }
        }

        // Non-existent chunk should still be None
        let result = adapter.get(100, 200, 16, 10, 10).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_disk_cache_adapter_creates_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let adapter = DiskCacheAdapter::new(temp_dir.path().to_path_buf(), "test");

        let data = vec![1, 2, 3];
        adapter.put(100, 200, 16, 5, 10, data).await.unwrap();

        // Verify directory structure was created
        let expected_dir = temp_dir.path().join("chunks/test/16/100/200");
        assert!(expected_dir.exists());
        assert!(expected_dir.is_dir());

        let expected_file = expected_dir.join("5_10.jpg");
        assert!(expected_file.exists());
        assert!(expected_file.is_file());
    }
}
