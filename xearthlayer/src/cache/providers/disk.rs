//! On-disk cache provider with LRU index tracking.
//!
//! This provider stores cache entries as files on disk and uses an in-memory
//! LRU index for efficient garbage collection without filesystem scanning.
//!
//! # Key Changes (v0.4)
//!
//! - **LRU Index**: Uses in-memory index for O(1) cache tracking
//! - **Reversible Filenames**: Keys encoded as `key.replace(':', '_')` instead of hashing
//! - **External GC**: GC is managed externally via [`CacheGcJob`], not an internal daemon
//!
//! # File Layout
//!
//! Files are stored in 1°×1° DSF region subdirectories with reversible names:
//! ```text
//! {cache_dir}/{region}/{key_with_underscores}.cache
//! ```
//!
//! The region is derived from the tile's geographic center (e.g., `+33-119`).
//! This enables parallel scanning on startup via rayon.
//!
//! Example: `tile:15:12754:5279` → `+33-119/tile_15_12754_5279.cache`
//!
//! # Migration Note
//!
//! Caches from versions prior to v0.3 (flat layout or hashed filenames) are
//! not automatically migrated. Run `xearthlayer cache migrate` to move flat
//! files into region subdirectories, or `xearthlayer cache clear` to start fresh.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::config::DiskProviderConfig;
use crate::cache::lru_index::LruIndex;
use crate::cache::traits::{BoxFuture, Cache, GcResult, ServiceCacheError};
use crate::metrics::{MetricsClient, OptionalMetrics};

/// On-disk cache provider with LRU index tracking.
///
/// This provider stores cache entries as files and maintains an in-memory
/// LRU index for efficient garbage collection. GC is handled externally
/// via `CacheGcJob` rather than an internal daemon.
///
/// # Lifecycle
///
/// 1. Create via `start()` or `start_with_index()`
/// 2. Index is populated from disk on startup
/// 3. External GC scheduler submits `CacheGcJob` when needed
/// 4. Call `shutdown()` for graceful cleanup
pub struct DiskCacheProvider {
    /// Cache directory path.
    directory: PathBuf,

    /// Maximum size in bytes.
    max_size_bytes: AtomicU64,

    /// In-memory LRU index for cache tracking.
    lru_index: Arc<LruIndex>,

    /// Cancellation token for graceful shutdown.
    shutdown: CancellationToken,

    /// Optional metrics client for reporting cache size updates.
    metrics_client: Option<MetricsClient>,
}

impl DiskCacheProvider {
    /// Start a new disk cache provider.
    ///
    /// This creates the cache directory if needed and populates the LRU index
    /// from existing cache files on disk.
    ///
    /// # Arguments
    ///
    /// * `config` - Disk cache configuration
    ///
    /// # Returns
    ///
    /// A new `DiskCacheProvider` ready for use.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache directory cannot be created.
    pub async fn start(config: DiskProviderConfig) -> Result<Arc<Self>, ServiceCacheError> {
        // Create cache directory if it doesn't exist
        tokio::fs::create_dir_all(&config.directory)
            .await
            .map_err(ServiceCacheError::Io)?;

        let shutdown = CancellationToken::new();

        // Create LRU index
        let lru_index = Arc::new(LruIndex::new(config.directory.clone()));

        let provider = Arc::new(Self {
            directory: config.directory.clone(),
            max_size_bytes: AtomicU64::new(config.max_size_bytes),
            lru_index,
            shutdown,
            metrics_client: config.metrics_client,
        });

        // Populate LRU index from disk
        match provider.lru_index.populate_from_disk().await {
            Ok(stats) => {
                info!(
                    dir = %config.directory.display(),
                    files = stats.files_indexed,
                    size_mb = stats.total_bytes / 1_000_000,
                    max_mb = config.max_size_bytes / 1_000_000,
                    "Disk cache provider started"
                );
            }
            Err(e) => {
                warn!(
                    dir = %config.directory.display(),
                    error = %e,
                    "Failed to populate LRU index from disk, starting empty"
                );
            }
        }

        Ok(provider)
    }

    /// Start a new disk cache provider with an existing LRU index.
    ///
    /// Use this when you want to share the LRU index with other components
    /// (e.g., the GC scheduler).
    ///
    /// # Arguments
    ///
    /// * `config` - Disk cache configuration
    /// * `lru_index` - Pre-existing LRU index
    pub async fn start_with_index(
        config: DiskProviderConfig,
        lru_index: Arc<LruIndex>,
    ) -> Result<Arc<Self>, ServiceCacheError> {
        // Create cache directory if it doesn't exist
        tokio::fs::create_dir_all(&config.directory)
            .await
            .map_err(ServiceCacheError::Io)?;

        let shutdown = CancellationToken::new();

        let provider = Arc::new(Self {
            directory: config.directory.clone(),
            max_size_bytes: AtomicU64::new(config.max_size_bytes),
            lru_index,
            shutdown,
            metrics_client: config.metrics_client,
        });

        info!(
            dir = %config.directory.display(),
            max_mb = config.max_size_bytes / 1_000_000,
            "Disk cache provider started with shared LRU index"
        );

        Ok(provider)
    }

    /// Returns a reference to the LRU index.
    ///
    /// This can be used by external GC schedulers to query cache state
    /// and create `CacheGcJob` instances.
    pub fn lru_index(&self) -> Arc<LruIndex> {
        Arc::clone(&self.lru_index)
    }

    /// Returns the cache directory path.
    pub fn directory(&self) -> &PathBuf {
        &self.directory
    }

    /// Returns the maximum cache size in bytes.
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    /// Shutdown the provider.
    ///
    /// This signals any pending operations to stop and cleans up resources.
    pub async fn shutdown(&self) {
        info!("Disk cache provider shutting down");
        self.shutdown.cancel();
        info!("Disk cache provider shutdown complete");
    }

    /// Returns the current cache size from the LRU index.
    ///
    /// The LRU index is populated from disk during `start()`, so this
    /// returns an accurate size without re-scanning. This avoids the
    /// double-counting bug that occurred when `populate_from_disk()` was
    /// called twice (once in `start()` and again here).
    ///
    /// Returns the total size in bytes.
    pub async fn scan_initial_size(&self) -> Result<u64, ServiceCacheError> {
        let size = self.lru_index.total_size();
        let count = self.lru_index.entry_count();

        // Seed the absolute disk cache size metric
        self.metrics_client.disk_cache_size(size);

        info!(
            files = count,
            size_mb = size / 1_000_000,
            "Disk cache initial size from LRU index"
        );

        Ok(size)
    }

    /// Check if garbage collection is needed.
    ///
    /// Returns `true` if the cache is over 95% of the maximum size.
    pub fn needs_gc(&self) -> bool {
        let current = self.lru_index.total_size();
        let max = self.max_size_bytes.load(Ordering::Relaxed);
        let threshold = (max as f64 * 0.95) as u64;
        current > threshold
    }

    /// Returns the target size for GC (80% of max).
    pub fn gc_target_size(&self) -> u64 {
        let max = self.max_size_bytes.load(Ordering::Relaxed);
        (max as f64 * 0.80) as u64
    }

    /// Get the file path for a cache key.
    ///
    /// Delegates to `LruIndex::key_to_path()` as the single source of truth
    /// for path resolution, ensuring region subdirectories are used.
    fn key_path(&self, key: &str) -> PathBuf {
        self.lru_index.key_to_path(key)
    }
}

impl Cache for DiskCacheProvider {
    fn set(&self, key: &str, value: Vec<u8>) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        let path = self.key_path(key);
        let size = value.len() as u64;
        let key_owned = key.to_string();

        Box::pin(async move {
            // Ensure parent directory exists (creates region dir on first write)
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(ServiceCacheError::Io)?;
            }

            // Write atomically via temp file
            let temp_path = path.with_extension("tmp");
            tokio::fs::write(&temp_path, &value)
                .await
                .map_err(ServiceCacheError::Io)?;
            tokio::fs::rename(&temp_path, &path)
                .await
                .map_err(ServiceCacheError::Io)?;

            // Update LRU index after successful write
            self.lru_index.record(&key_owned, size);

            // Report authoritative cache size to metrics
            self.metrics_client
                .disk_cache_size(self.lru_index.total_size());

            debug!(key = %key_owned, size, "Cache set");
            Ok(())
        })
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, ServiceCacheError>> {
        let path = self.key_path(key);
        let key_owned = key.to_string();

        Box::pin(async move {
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    // Update LRU index access time
                    self.lru_index.touch(&key_owned);
                    debug!(key = %key_owned, size = data.len(), "Cache hit");
                    Ok(Some(data))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File not on disk - ensure index is clean
                    if self.lru_index.contains(&key_owned) {
                        self.lru_index.remove(&key_owned);
                        debug!(key = %key_owned, "Removed stale index entry");
                    }
                    Ok(None)
                }
                Err(e) => Err(ServiceCacheError::Io(e)),
            }
        })
    }

    fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let path = self.key_path(key);
        let key_owned = key.to_string();

        Box::pin(async move {
            // Remove from index first
            self.lru_index.remove(&key_owned);

            // Then delete file
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    // Report authoritative cache size to metrics
                    self.metrics_client
                        .disk_cache_size(self.lru_index.total_size());
                    debug!(key = %key_owned, "Cache delete");
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(ServiceCacheError::Io(e)),
            }
        })
    }

    fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        // Check index first (faster than filesystem)
        let in_index = self.lru_index.contains(key);
        if !in_index {
            return Box::pin(async move { Ok(false) });
        }

        // Verify file exists (index might be stale)
        let path = self.key_path(key);
        let key_owned = key.to_string();

        Box::pin(async move {
            if tokio::fs::metadata(&path).await.is_ok() {
                Ok(true)
            } else {
                // File doesn't exist - clean up stale index entry
                self.lru_index.remove(&key_owned);
                Ok(false)
            }
        })
    }

    fn size_bytes(&self) -> u64 {
        self.lru_index.total_size()
    }

    fn entry_count(&self) -> u64 {
        self.lru_index.entry_count()
    }

    fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        Box::pin(async move {
            self.max_size_bytes.store(size_bytes, Ordering::Relaxed);
            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>> {
        // GC is now handled externally via CacheGcJob.
        // This method returns a no-op result.
        Box::pin(async move {
            Ok(GcResult {
                entries_removed: 0,
                bytes_freed: 0,
                duration_ms: 0,
            })
        })
    }
}

impl Drop for DiskCacheProvider {
    fn drop(&mut self) {
        // Signal shutdown if not already done
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    async fn create_test_provider(max_size: u64) -> (TempDir, Arc<DiskCacheProvider>) {
        let temp_dir = TempDir::new().unwrap();
        let config = DiskProviderConfig {
            directory: temp_dir.path().to_path_buf(),
            max_size_bytes: max_size,
            gc_interval: Duration::from_secs(3600), // Not used anymore
            provider_name: "test".to_string(),
            metrics_client: None,
        };

        let provider = DiskCacheProvider::start(config).await.unwrap();

        (temp_dir, provider)
    }

    #[tokio::test]
    async fn test_disk_provider_set_and_get() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![1, 2, 3]));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_get_missing() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        let value = provider.get("nonexistent").await.unwrap();
        assert!(value.is_none());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_delete() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        let deleted = provider.delete("key1").await.unwrap();
        assert!(deleted);

        let value = provider.get("key1").await.unwrap();
        assert!(value.is_none());

        // Delete non-existent
        let deleted = provider.delete("key1").await.unwrap();
        assert!(!deleted);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_contains() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        assert!(!provider.contains("key1").await.unwrap());

        provider.set("key1", vec![1]).await.unwrap();

        assert!(provider.contains("key1").await.unwrap());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_replace_existing() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3]).await.unwrap();
        provider.set("key1", vec![4, 5, 6, 7]).await.unwrap();

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![4, 5, 6, 7]));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_size_tracking() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        assert_eq!(provider.size_bytes(), 0);
        assert_eq!(provider.entry_count(), 0);

        provider.set("key1", vec![0u8; 1000]).await.unwrap();

        assert_eq!(provider.size_bytes(), 1000);
        assert_eq!(provider.entry_count(), 1);

        provider.set("key2", vec![0u8; 2000]).await.unwrap();

        assert_eq!(provider.size_bytes(), 3000);
        assert_eq!(provider.entry_count(), 2);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_needs_gc() {
        let (_temp_dir, provider) = create_test_provider(1000).await;

        assert!(!provider.needs_gc());

        // Add data to exceed 95% threshold (950 bytes)
        provider.set("key1", vec![0u8; 960]).await.unwrap();

        assert!(provider.needs_gc());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_lru_index_access() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        let lru_index = provider.lru_index();
        assert!(lru_index.contains("key1"));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_atomic_write() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        // Write should be atomic (temp file + rename)
        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        // No temp files should remain (check in the region directory)
        let cache_path = provider.lru_index().key_to_path("key1");
        let parent = cache_path.parent().unwrap();
        let files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();

        assert!(files.is_empty(), "Temp files should not remain");

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_stale_index_cleanup_on_get() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        // Set a value
        provider.set("key1", vec![1, 2, 3]).await.unwrap();
        assert!(provider.lru_index().contains("key1"));

        // Manually delete the file using the provider's path resolution
        let path = provider.lru_index().key_to_path("key1");
        std::fs::remove_file(path).unwrap();

        // Get should return None and clean up index
        let value = provider.get("key1").await.unwrap();
        assert!(value.is_none());
        assert!(!provider.lru_index().contains("key1"));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_stale_index_cleanup_on_contains() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        // Set a value
        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        // Manually delete the file using the provider's path resolution
        let path = provider.lru_index().key_to_path("key1");
        std::fs::remove_file(path).unwrap();

        // Contains should return false and clean up index
        assert!(!provider.contains("key1").await.unwrap());
        assert!(!provider.lru_index().contains("key1"));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_scan_initial_size_does_not_double_count() {
        let temp_dir = TempDir::new().unwrap();

        // Pre-populate disk with cache files in region subdirectories
        let path1 = crate::cache::key_to_full_path(temp_dir.path(), "chunk:15:100:200:8:12");
        let path2 = crate::cache::key_to_full_path(temp_dir.path(), "chunk:15:100:201:0:0");
        std::fs::create_dir_all(path1.parent().unwrap()).unwrap();
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path1, vec![0u8; 1000]).unwrap();
        std::fs::write(&path2, vec![0u8; 2000]).unwrap();

        let config = DiskProviderConfig {
            directory: temp_dir.path().to_path_buf(),
            max_size_bytes: 1_000_000,
            gc_interval: Duration::from_secs(3600),
            provider_name: "test".to_string(),
            metrics_client: None,
        };

        // start() internally calls populate_from_disk()
        let provider = DiskCacheProvider::start(config).await.unwrap();

        // Verify initial state is correct
        assert_eq!(provider.size_bytes(), 3000);
        assert_eq!(provider.entry_count(), 2);

        // scan_initial_size() must return the same value, NOT double it
        let scanned_size = provider.scan_initial_size().await.unwrap();
        assert_eq!(
            scanned_size, 3000,
            "scan_initial_size() must not double-count after start()"
        );

        // And the internal state must remain consistent
        assert_eq!(
            provider.size_bytes(),
            3000,
            "size_bytes() must remain consistent after scan_initial_size()"
        );
        assert_eq!(provider.entry_count(), 2);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let key = "tile:15:100:200";

        // Create provider, add data, shutdown
        {
            let config = DiskProviderConfig {
                directory: temp_dir.path().to_path_buf(),
                max_size_bytes: 1_000_000,
                gc_interval: Duration::from_secs(3600),
                provider_name: "test".to_string(),
                metrics_client: None,
            };
            let provider = DiskCacheProvider::start(config).await.unwrap();

            provider.set(key, vec![1, 2, 3, 4, 5]).await.unwrap();
            provider.shutdown().await;
        }

        // Create new provider, verify data persists
        {
            let config = DiskProviderConfig {
                directory: temp_dir.path().to_path_buf(),
                max_size_bytes: 1_000_000,
                gc_interval: Duration::from_secs(3600),
                provider_name: "test".to_string(),
                metrics_client: None,
            };
            let provider = DiskCacheProvider::start(config).await.unwrap();

            // Index should be populated from disk
            assert!(provider.lru_index().contains(key));
            assert_eq!(provider.entry_count(), 1);

            let value = provider.get(key).await.unwrap();
            assert_eq!(value, Some(vec![1, 2, 3, 4, 5]));

            provider.shutdown().await;
        }
    }
}
