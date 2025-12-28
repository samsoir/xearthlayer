//! Background disk cache eviction daemon.
//!
//! Periodically scans the disk cache and evicts oldest files (by mtime)
//! when the cache exceeds its configured size limit.
//!
//! # Design
//!
//! The daemon uses an LRU approximation based on file modification time (mtime).
//! When the cache exceeds its limit, the oldest files are deleted until the cache
//! is at 90% of the limit (leaving headroom for new writes).
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::cache::{DiskCacheConfig, run_eviction_daemon};
//! use tokio_util::sync::CancellationToken;
//!
//! let config = DiskCacheConfig {
//!     cache_dir: PathBuf::from("/home/user/.cache/xearthlayer"),
//!     max_size_bytes: 20 * 1024 * 1024 * 1024, // 20 GB
//!     daemon_interval_secs: 60,
//!     ..Default::default()
//! };
//!
//! let cancellation = CancellationToken::new();
//! tokio::spawn(run_eviction_daemon(config, cancellation));
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::types::DiskCacheConfig;

/// Target percentage of limit after eviction (0.9 = 90%).
/// This leaves 10% headroom for new writes before the next eviction cycle.
const EVICTION_TARGET_PERCENTAGE: f64 = 0.9;

/// Result of an eviction run.
#[derive(Debug, Clone, Default)]
pub struct EvictionResult {
    /// Number of files deleted
    pub files_deleted: usize,
    /// Total bytes freed
    pub bytes_freed: u64,
    /// Cache size before eviction
    pub size_before: u64,
    /// Cache size after eviction
    pub size_after: u64,
    /// Duration of eviction in milliseconds
    pub duration_ms: u64,
}

/// Run the disk cache eviction daemon.
///
/// This function runs indefinitely, periodically checking the disk cache size
/// and evicting oldest files when over the limit. It responds to cancellation
/// for graceful shutdown.
///
/// # Arguments
///
/// * `config` - Disk cache configuration with size limits and check interval
/// * `cancellation` - Token for graceful shutdown
pub async fn run_eviction_daemon(config: DiskCacheConfig, cancellation: CancellationToken) {
    info!(
        cache_dir = %config.cache_dir.display(),
        max_size_bytes = config.max_size_bytes,
        interval_secs = config.daemon_interval_secs,
        "Starting disk cache eviction daemon"
    );

    // Initial eviction check on startup
    if let Some(result) = evict_if_over_limit(&config).await {
        log_eviction_result(&result);
    }

    // Periodic eviction loop
    let interval = Duration::from_secs(config.daemon_interval_secs);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                info!("Disk cache eviction daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                if let Some(result) = evict_if_over_limit(&config).await {
                    log_eviction_result(&result);
                }
            }
        }
    }
}

/// Check disk cache size and evict if over limit.
///
/// Returns `Some(EvictionResult)` if eviction was performed, `None` if cache is under limit.
async fn evict_if_over_limit(config: &DiskCacheConfig) -> Option<EvictionResult> {
    // Get current cache size (blocking I/O wrapped in spawn_blocking)
    let cache_dir = config.cache_dir.clone();
    let stats_result =
        tokio::task::spawn_blocking(move || super::path::disk_cache_stats(&cache_dir))
            .await
            .ok()?;

    let (file_count, current_size) = stats_result.ok()?;

    if current_size <= config.max_size_bytes as u64 {
        debug!(
            size_bytes = current_size,
            limit_bytes = config.max_size_bytes,
            "Disk cache under limit, no eviction needed"
        );
        return None;
    }

    // Evict until at target percentage of limit
    let target_size = (config.max_size_bytes as f64 * EVICTION_TARGET_PERCENTAGE) as u64;

    info!(
        current_size_bytes = current_size,
        limit_bytes = config.max_size_bytes,
        target_bytes = target_size,
        file_count = file_count,
        "Disk cache over limit, starting eviction"
    );

    Some(evict_to_target(&config.cache_dir, current_size, target_size).await)
}

/// Evict oldest files until at target size.
async fn evict_to_target(cache_dir: &Path, current_size: u64, target_size: u64) -> EvictionResult {
    let start = Instant::now();
    let cache_dir = cache_dir.to_path_buf();

    // Run eviction in blocking task (filesystem operations)
    tokio::task::spawn_blocking(move || {
        evict_to_target_blocking(&cache_dir, current_size, target_size, start)
    })
    .await
    .unwrap_or_default()
}

/// Blocking implementation of LRU eviction.
fn evict_to_target_blocking(
    cache_dir: &Path,
    current_size: u64,
    target_size: u64,
    start: Instant,
) -> EvictionResult {
    // 1. Collect all cache files with mtime
    let mut files = collect_cache_files(cache_dir);

    // Calculate total collectable size for diagnostics
    let total_collectable: u64 = files.iter().map(|(_, _, size)| size).sum();

    info!(
        files_collected = files.len(),
        collectable_bytes = total_collectable,
        reported_size = current_size,
        target_bytes = target_size,
        "Eviction scan complete"
    );

    // 2. Sort by mtime (oldest first) for LRU eviction
    files.sort_by_key(|(_, mtime, _)| *mtime);

    // 3. Delete oldest files until under target
    let mut bytes_freed = 0u64;
    let mut files_deleted = 0usize;
    let mut delete_failures = 0usize;
    let mut remaining_size = current_size;

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
                // Log but continue - file may have been deleted by another process
                delete_failures += 1;
                debug!(
                    path = %path.display(),
                    error = %e,
                    "Failed to delete cache file during eviction"
                );
            }
        }
    }

    if delete_failures > 0 {
        info!(
            delete_failures = delete_failures,
            "Some files could not be deleted during eviction"
        );
    }

    // Warn if we couldn't reach target (all files exhausted or too many failures)
    if remaining_size > target_size {
        warn!(
            remaining_size = remaining_size,
            target_size = target_size,
            shortfall_bytes = remaining_size - target_size,
            "Eviction could not reach target size"
        );
    }

    // 4. Clean up empty directories left after eviction
    cleanup_empty_dirs(cache_dir);

    EvictionResult {
        files_deleted,
        bytes_freed,
        size_before: current_size,
        size_after: remaining_size,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Collect all cache files with their mtime and size.
fn collect_cache_files(cache_dir: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let mut files = Vec::new();
    collect_files_recursive(cache_dir, &mut files);
    files
}

/// Recursively collect files from a directory.
fn collect_files_recursive(dir: &Path, files: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            debug!(
                dir = %dir.display(),
                error = %e,
                "Failed to read directory during eviction scan"
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            collect_files_recursive(&path, files);
        } else if let Ok(metadata) = entry.metadata() {
            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = metadata.len();
            files.push((path, mtime, size));
        }
    }
}

/// Remove empty directories after eviction.
///
/// Walks depth-first and removes directories that become empty after eviction.
fn cleanup_empty_dirs(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Recurse first to clean up nested empty directories
            cleanup_empty_dirs(&path);
            // Try to remove if empty (will fail silently if not empty)
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// Log the result of an eviction run.
fn log_eviction_result(result: &EvictionResult) {
    info!(
        files_deleted = result.files_deleted,
        bytes_freed = result.bytes_freed,
        size_before = result.size_before,
        size_after = result.size_after,
        duration_ms = result.duration_ms,
        "Disk cache eviction complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test file with specific size and mtime.
    fn create_test_file(path: &Path, size: usize, age_secs: u64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, vec![0u8; size]).unwrap();

        // Set mtime to (now - age_secs)
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    #[test]
    fn test_collect_cache_files_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let files = collect_cache_files(temp_dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_cache_files_nested() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create nested structure like real cache
        create_test_file(&root.join("bing/BC1/15/12754/tile1.dds"), 1000, 100);
        create_test_file(&root.join("bing/BC1/15/12754/tile2.dds"), 2000, 50);
        create_test_file(&root.join("google/BC1/16/25508/tile3.dds"), 3000, 200);

        let files = collect_cache_files(root);
        assert_eq!(files.len(), 3);

        // Calculate total size
        let total_size: u64 = files.iter().map(|(_, _, size)| size).sum();
        assert_eq!(total_size, 6000);
    }

    #[test]
    fn test_evict_oldest_first() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create files with different ages
        create_test_file(&root.join("oldest.dds"), 1000, 300); // oldest
        create_test_file(&root.join("middle.dds"), 1000, 200);
        create_test_file(&root.join("newest.dds"), 1000, 100); // newest

        // Evict to 2000 bytes (should delete oldest)
        let result = evict_to_target_blocking(root, 3000, 2000, Instant::now());

        assert_eq!(result.files_deleted, 1);
        assert_eq!(result.bytes_freed, 1000);
        assert_eq!(result.size_after, 2000);

        // Verify oldest was deleted
        assert!(!root.join("oldest.dds").exists());
        assert!(root.join("middle.dds").exists());
        assert!(root.join("newest.dds").exists());
    }

    #[test]
    fn test_evict_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create 5 files with different ages
        for i in 0..5 {
            create_test_file(&root.join(format!("file{}.dds", i)), 1000, (5 - i) * 60);
        }

        // Evict to 2000 bytes (should delete 3 oldest)
        let result = evict_to_target_blocking(root, 5000, 2000, Instant::now());

        assert_eq!(result.files_deleted, 3);
        assert_eq!(result.bytes_freed, 3000);
        assert_eq!(result.size_after, 2000);
    }

    #[test]
    fn test_cleanup_empty_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create nested directory structure
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        // All directories should be removed since they're empty
        cleanup_empty_dirs(root);

        assert!(!root.join("a").exists());
    }

    #[test]
    fn test_cleanup_preserves_nonempty_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create structure with a file in the middle
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("a/b/file.txt"), "content").unwrap();

        cleanup_empty_dirs(root);

        // 'a' and 'a/b' should exist (have content), 'a/b/c' should be deleted
        assert!(root.join("a").exists());
        assert!(root.join("a/b").exists());
        assert!(root.join("a/b/file.txt").exists());
        assert!(!root.join("a/b/c").exists());
    }

    #[test]
    fn test_eviction_result_default() {
        let result = EvictionResult::default();
        assert_eq!(result.files_deleted, 0);
        assert_eq!(result.bytes_freed, 0);
        assert_eq!(result.size_before, 0);
        assert_eq!(result.size_after, 0);
        assert_eq!(result.duration_ms, 0);
    }

    #[tokio::test]
    async fn test_evict_if_over_limit_under_limit() {
        let temp_dir = TempDir::new().unwrap();

        let config = DiskCacheConfig {
            cache_dir: temp_dir.path().to_path_buf(),
            max_size_bytes: 10000, // 10 KB limit
            daemon_interval_secs: 60,
            max_age_days: None,
        };

        // Create small file under limit
        std::fs::write(temp_dir.path().join("small.dds"), vec![0u8; 100]).unwrap();

        let result = evict_if_over_limit(&config).await;
        assert!(result.is_none()); // No eviction needed
    }

    #[tokio::test]
    async fn test_evict_if_over_limit_triggers_eviction() {
        let temp_dir = TempDir::new().unwrap();

        let config = DiskCacheConfig {
            cache_dir: temp_dir.path().to_path_buf(),
            max_size_bytes: 2000, // 2 KB limit
            daemon_interval_secs: 60,
            max_age_days: None,
        };

        // Create files totaling 3 KB (over limit)
        create_test_file(&temp_dir.path().join("old.dds"), 1500, 100);
        create_test_file(&temp_dir.path().join("new.dds"), 1500, 10);

        let result = evict_if_over_limit(&config).await;
        assert!(result.is_some());

        let result = result.unwrap();
        assert!(result.files_deleted > 0);
        assert!(result.bytes_freed > 0);
        // Target is 90% of 2000 = 1800, so size_after should be <= 1800
        assert!(result.size_after <= 1800);
    }
}
