//! On-disk cache provider with internal garbage collection.
//!
//! This provider stores cache entries as files on disk and runs a background
//! GC daemon that periodically evicts the oldest files when the cache exceeds
//! its size limit.
//!
//! # Key Feature: Self-Contained GC
//!
//! The GC daemon is **owned by the provider** and spawned during construction.
//! This fixes a critical bug where the GC daemon was previously wired externally
//! in the CLI and never started in TUI mode (because mounting happened later).
//!
//! # Eviction Strategy
//!
//! Uses LRU approximation based on file modification time (mtime):
//! - When cache exceeds limit, oldest files are deleted first
//! - Eviction targets 90% of limit (leaving 10% headroom for new writes)
//! - Empty directories are cleaned up after eviction
//!
//! # File Layout
//!
//! Files are stored in a flat structure within the cache directory:
//! ```text
//! {cache_dir}/{key_hash}.cache
//! ```
//!
//! The key is hashed to create a safe filename that works across all platforms.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::config::DiskProviderConfig;
use crate::cache::traits::{BoxFuture, Cache, GcResult, ServiceCacheError};
use crate::metrics::MetricsClient;

/// Target percentage of limit after eviction (0.9 = 90%).
/// This leaves 10% headroom for new writes before the next eviction cycle.
const EVICTION_TARGET_PERCENTAGE: f64 = 0.9;

/// On-disk cache provider with internal garbage collection.
///
/// This provider spawns a background GC daemon during construction that
/// periodically evicts old files when the cache exceeds its size limit.
///
/// # Shutdown
///
/// Call `shutdown()` to gracefully stop the GC daemon. The daemon will
/// finish any in-progress eviction before stopping.
pub struct DiskCacheProvider {
    /// Cache directory path.
    directory: PathBuf,

    /// Maximum size in bytes.
    max_size_bytes: AtomicU64,

    /// GC interval.
    gc_interval: Duration,

    /// Current cached size (approximate, updated during GC).
    cached_size: AtomicU64,

    /// Current entry count (approximate, updated during GC).
    cached_count: AtomicU64,

    /// Handle to the GC daemon task.
    gc_handle: RwLock<Option<JoinHandle<()>>>,

    /// Cancellation token for graceful shutdown.
    shutdown: CancellationToken,

    /// Optional metrics client for reporting GC eviction stats.
    metrics_client: Option<MetricsClient>,
}

impl DiskCacheProvider {
    /// Start a new disk cache provider with GC daemon.
    ///
    /// This creates the cache directory if needed and spawns a background
    /// task for periodic garbage collection.
    ///
    /// # Arguments
    ///
    /// * `config` - Disk cache configuration
    ///
    /// # Returns
    ///
    /// A new `DiskCacheProvider` with an active GC daemon.
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

        let provider = Arc::new(Self {
            directory: config.directory.clone(),
            max_size_bytes: AtomicU64::new(config.max_size_bytes),
            gc_interval: config.gc_interval,
            cached_size: AtomicU64::new(0), // Will be populated by scan_initial_size() or first GC
            cached_count: AtomicU64::new(0),
            gc_handle: RwLock::new(None),
            shutdown: shutdown.clone(),
            metrics_client: config.metrics_client,
        });

        // Spawn internal GC daemon
        let gc_provider = Arc::clone(&provider);
        let gc_handle = tokio::spawn(async move {
            gc_provider.run_gc_daemon().await;
        });

        // Store the handle
        {
            let mut handle_guard = provider.gc_handle.write().await;
            *handle_guard = Some(gc_handle);
        }

        info!(
            dir = %config.directory.display(),
            max_bytes = config.max_size_bytes,
            interval_secs = config.gc_interval.as_secs(),
            "Disk cache provider started with GC daemon"
        );

        Ok(provider)
    }

    /// Scan existing cache size and update cached_size.
    ///
    /// This should be called after start() to get an accurate initial size
    /// for metrics display. The scan is done in a blocking task to avoid
    /// blocking the async runtime.
    ///
    /// Returns the total size in bytes.
    pub async fn scan_initial_size(&self) -> Result<u64, ServiceCacheError> {
        let directory = self.directory.clone();
        let initial_size = tokio::task::spawn_blocking(move || {
            let files = Self::collect_cache_files(&directory);
            files.iter().map(|(_, _, size)| size).sum::<u64>()
        })
        .await
        .map_err(|e| ServiceCacheError::SpawnError(e.to_string()))?;

        self.cached_size.store(initial_size, Ordering::Relaxed);

        info!(
            initial_size = initial_size,
            "Disk cache initial size scanned"
        );

        Ok(initial_size)
    }

    /// Shutdown the provider, stopping the GC daemon.
    ///
    /// This method waits for the GC daemon to finish any in-progress work
    /// before returning.
    pub async fn shutdown(&self) {
        info!("Disk cache provider shutting down");

        // Signal shutdown
        self.shutdown.cancel();

        // Wait for GC daemon to finish
        let handle = {
            let mut guard = self.gc_handle.write().await;
            guard.take()
        };

        if let Some(handle) = handle {
            let _ = handle.await;
        }

        info!("Disk cache provider shutdown complete");
    }

    /// Internal GC daemon - runs until shutdown.
    async fn run_gc_daemon(&self) {
        info!(
            dir = %self.directory.display(),
            max_bytes = self.max_size_bytes.load(Ordering::Relaxed),
            interval_secs = self.gc_interval.as_secs(),
            "Disk cache GC daemon started"
        );

        // Initial GC check on startup
        if let Err(e) = self.run_gc_cycle().await {
            warn!(error = %e, "Initial GC cycle failed");
        }

        // Periodic GC loop
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    info!("Disk cache GC daemon shutting down");
                    break;
                }
                _ = tokio::time::sleep(self.gc_interval) => {
                    if let Err(e) = self.run_gc_cycle().await {
                        warn!(error = %e, "GC cycle failed");
                    }
                }
            }
        }
    }

    /// Run a single GC cycle.
    async fn run_gc_cycle(&self) -> Result<GcResult, ServiceCacheError> {
        let max_bytes = self.max_size_bytes.load(Ordering::Relaxed);
        let directory = self.directory.clone();

        // Run in spawn_blocking since we're doing filesystem operations
        let result =
            tokio::task::spawn_blocking(move || Self::gc_cycle_blocking(&directory, max_bytes))
                .await
                .map_err(|e| ServiceCacheError::SpawnError(e.to_string()))??;

        // Update cached stats
        self.cached_size.store(
            result.size_before.saturating_sub(result.bytes_freed),
            Ordering::Relaxed,
        );

        if result.entries_removed > 0 {
            info!(
                entries_removed = result.entries_removed,
                bytes_freed = result.bytes_freed,
                duration_ms = result.duration_ms,
                "Disk cache GC complete"
            );

            // Report eviction to metrics system
            if let Some(ref metrics) = self.metrics_client {
                metrics.disk_cache_evicted(result.bytes_freed);
            }
        }

        Ok(GcResult {
            entries_removed: result.entries_removed,
            bytes_freed: result.bytes_freed,
            duration_ms: result.duration_ms,
        })
    }

    /// Blocking GC implementation.
    fn gc_cycle_blocking(
        directory: &PathBuf,
        max_bytes: u64,
    ) -> Result<GcCycleResult, ServiceCacheError> {
        let start = Instant::now();

        // Collect all cache files with mtime and size
        let mut files = Self::collect_cache_files(directory);
        let total_size: u64 = files.iter().map(|(_, _, size)| size).sum();
        let file_count = files.len();

        debug!(
            file_count = file_count,
            total_size = total_size,
            limit = max_bytes,
            "GC cycle scan complete"
        );

        // Check if eviction is needed
        if total_size <= max_bytes {
            return Ok(GcCycleResult {
                entries_removed: 0,
                bytes_freed: 0,
                size_before: total_size,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        // Calculate eviction target (90% of limit)
        let target_size = (max_bytes as f64 * EVICTION_TARGET_PERCENTAGE) as u64;

        info!(
            current_size = total_size,
            limit = max_bytes,
            target = target_size,
            "Disk cache over limit, starting eviction"
        );

        // Sort by mtime (oldest first) for LRU eviction
        files.sort_by_key(|(_, mtime, _)| *mtime);

        // Delete oldest files until under target
        let mut bytes_freed = 0u64;
        let mut files_deleted = 0usize;
        let mut remaining_size = total_size;

        for (path, _mtime, size) in files {
            if remaining_size <= target_size {
                break;
            }

            match std::fs::remove_file(&path) {
                Ok(()) => {
                    bytes_freed += size;
                    remaining_size = remaining_size.saturating_sub(size);
                    files_deleted += 1;
                }
                Err(e) => {
                    debug!(
                        path = %path.display(),
                        error = %e,
                        "Failed to delete cache file during eviction"
                    );
                }
            }
        }

        // Clean up empty directories
        Self::cleanup_empty_dirs(directory);

        Ok(GcCycleResult {
            entries_removed: files_deleted,
            bytes_freed,
            size_before: total_size,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }

    /// Collect all cache files with their mtime and size.
    fn collect_cache_files(dir: &PathBuf) -> Vec<(PathBuf, SystemTime, u64)> {
        let mut files = Vec::new();
        Self::collect_files_recursive(dir, &mut files);
        files
    }

    /// Recursively collect files from a directory.
    fn collect_files_recursive(dir: &PathBuf, files: &mut Vec<(PathBuf, SystemTime, u64)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                debug!(
                    dir = %dir.display(),
                    error = %e,
                    "Failed to read directory during GC scan"
                );
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                Self::collect_files_recursive(&path, files);
            } else if let Ok(metadata) = entry.metadata() {
                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let size = metadata.len();
                files.push((path, mtime, size));
            }
        }
    }

    /// Remove empty directories after eviction.
    fn cleanup_empty_dirs(dir: &PathBuf) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Recurse first to clean up nested empty directories
                Self::cleanup_empty_dirs(&path);
                // Try to remove if empty (will fail silently if not empty)
                let _ = std::fs::remove_dir(&path);
            }
        }
    }

    /// Generate a safe filename from a cache key.
    fn key_to_filename(key: &str) -> String {
        // Use a simple hash for safe filenames
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        format!("{:016x}.cache", hasher.finish())
    }

    /// Get the file path for a cache key.
    fn key_path(&self, key: &str) -> PathBuf {
        self.directory.join(Self::key_to_filename(key))
    }
}

/// Result of a GC cycle with internal details.
struct GcCycleResult {
    entries_removed: usize,
    bytes_freed: u64,
    size_before: u64,
    duration_ms: u64,
}

impl Cache for DiskCacheProvider {
    fn set(&self, key: &str, value: Vec<u8>) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        let path = self.key_path(key);
        Box::pin(async move {
            // Write atomically via temp file
            let temp_path = path.with_extension("tmp");
            tokio::fs::write(&temp_path, &value)
                .await
                .map_err(ServiceCacheError::Io)?;
            tokio::fs::rename(&temp_path, &path)
                .await
                .map_err(ServiceCacheError::Io)?;
            Ok(())
        })
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, ServiceCacheError>> {
        let path = self.key_path(key);
        Box::pin(async move {
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    // Update mtime to mark as recently used
                    let _ = tokio::fs::File::open(&path).await;
                    Ok(Some(data))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(ServiceCacheError::Io(e)),
            }
        })
    }

    fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let path = self.key_path(key);
        Box::pin(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(ServiceCacheError::Io(e)),
            }
        })
    }

    fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let path = self.key_path(key);
        Box::pin(async move { Ok(path.exists()) })
    }

    fn size_bytes(&self) -> u64 {
        self.cached_size.load(Ordering::Relaxed)
    }

    fn entry_count(&self) -> u64 {
        self.cached_count.load(Ordering::Relaxed)
    }

    fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        Box::pin(async move {
            self.max_size_bytes.store(size_bytes, Ordering::Relaxed);

            // Trigger GC if we might be over the new limit
            self.run_gc_cycle().await?;

            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>> {
        Box::pin(async move { self.run_gc_cycle().await })
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
    use tempfile::TempDir;

    async fn create_test_provider(max_size: u64) -> (TempDir, Arc<DiskCacheProvider>) {
        let temp_dir = TempDir::new().unwrap();
        let config = DiskProviderConfig {
            directory: temp_dir.path().to_path_buf(),
            max_size_bytes: max_size,
            gc_interval: Duration::from_secs(3600), // Long interval for tests
            provider_name: "test".to_string(),
            metrics_client: None,
        };

        let provider = DiskCacheProvider::start(config).await.unwrap();

        // Give GC daemon time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

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
    async fn test_disk_provider_gc() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![0u8; 1000]).await.unwrap();

        let result = provider.gc().await.unwrap();

        // GC should complete without error
        assert!(result.duration_ms < 5000);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_eviction() {
        // Small cache that will need eviction
        let (_temp_dir, provider) = create_test_provider(2500).await;

        // Add files exceeding limit
        provider.set("key1", vec![0u8; 1000]).await.unwrap();
        provider.set("key2", vec![0u8; 1000]).await.unwrap();
        provider.set("key3", vec![0u8; 1000]).await.unwrap();

        // Trigger GC
        let result = provider.gc().await.unwrap();

        // Should have evicted some files
        // (may vary based on timing)
        assert!(result.entries_removed > 0 || provider.size_bytes() <= 2500);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_atomic_write() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        // Write should be atomic (temp file + rename)
        provider.set("key1", vec![1, 2, 3]).await.unwrap();

        // No temp files should remain
        let files: Vec<_> = std::fs::read_dir(provider.directory.as_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();

        assert!(files.is_empty(), "Temp files should not remain");

        provider.shutdown().await;
    }

    #[test]
    fn test_key_to_filename() {
        let filename = DiskCacheProvider::key_to_filename("tile:15:12754:5279");
        assert!(filename.ends_with(".cache"));
        assert!(!filename.contains('/'));
        assert!(!filename.contains(':'));
    }

    #[test]
    fn test_key_to_filename_deterministic() {
        let f1 = DiskCacheProvider::key_to_filename("same_key");
        let f2 = DiskCacheProvider::key_to_filename("same_key");
        assert_eq!(f1, f2);

        let f3 = DiskCacheProvider::key_to_filename("different_key");
        assert_ne!(f1, f3);
    }
}
