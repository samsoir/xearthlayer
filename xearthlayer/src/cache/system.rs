//! Two-tier cache system coordinator.

use crate::cache::daemon::DiskCacheDaemon;
use crate::cache::disk::DiskCache;
use crate::cache::memory::MemoryCache;
use crate::cache::r#trait::Cache;
use crate::cache::types::{CacheConfig, CacheError, CacheKey};
use crate::cache::{CacheStatistics, CacheStats};
use crate::config::format_size;
use crate::log::Logger;
use crate::{log_debug, log_info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Background thread for periodic cache statistics logging.
///
/// Logs cache statistics at a configurable interval using the injected logger.
struct CacheStatsLogger {
    /// Handle to the logger thread
    thread_handle: Option<JoinHandle<()>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl CacheStatsLogger {
    /// Start the stats logger thread.
    ///
    /// # Arguments
    ///
    /// * `memory` - Arc to the memory cache
    /// * `disk` - Arc to the disk cache
    /// * `logger` - Logger for output
    /// * `provider` - Provider name for log messages
    /// * `interval_secs` - Logging interval in seconds
    fn start(
        memory: Arc<MemoryCache>,
        disk: Arc<DiskCache>,
        logger: Arc<dyn Logger>,
        provider: String,
        interval_secs: u64,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let thread_handle = thread::Builder::new()
            .name("cache-stats".to_string())
            .spawn(move || {
                Self::run_loop(
                    memory,
                    disk,
                    logger,
                    provider,
                    interval_secs,
                    shutdown_clone,
                );
            })
            .expect("Failed to spawn cache stats logger thread");

        Self {
            thread_handle: Some(thread_handle),
            shutdown,
        }
    }

    /// The main logger loop.
    fn run_loop(
        memory: Arc<MemoryCache>,
        disk: Arc<DiskCache>,
        logger: Arc<dyn Logger>,
        provider: String,
        interval_secs: u64,
        shutdown: Arc<AtomicBool>,
    ) {
        let interval = Duration::from_secs(interval_secs);

        // Use shorter sleep intervals for responsive shutdown
        let check_interval = Duration::from_secs(1);
        let mut elapsed = Duration::ZERO;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                log_debug!(logger, "Cache stats logger received shutdown signal");
                break;
            }

            thread::sleep(check_interval);
            elapsed += check_interval;

            if elapsed >= interval {
                elapsed = Duration::ZERO;
                Self::log_stats(&memory, &disk, &logger, &provider);
            }
        }
    }

    /// Log current cache statistics.
    fn log_stats(memory: &MemoryCache, disk: &DiskCache, logger: &Arc<dyn Logger>, provider: &str) {
        let mem_stats = memory.stats();
        let disk_stats = disk.stats();

        // Memory cache stats
        let mem_entries = memory.entry_count();
        let mem_size = memory.size_bytes();
        let mem_hit_rate = mem_stats.memory_hit_rate() * 100.0;

        log_info!(
            logger,
            "[CACHE] Memory: {} tiles ({}), hits: {} ({:.1}%), evictions: {}",
            mem_entries,
            format_size(mem_size),
            mem_stats.memory_hits,
            mem_hit_rate,
            mem_stats.memory_evictions
        );

        // Disk cache stats
        let disk_entries = disk.entry_count();
        let disk_size = disk.size_bytes();
        let disk_max = disk.max_size_bytes();
        let disk_hit_rate = disk_stats.disk_hit_rate() * 100.0;

        log_info!(
            logger,
            "[CACHE] Disk: {} tiles ({} / {}), hits: {} ({:.1}%), writes: {}, evictions: {}",
            disk_entries,
            format_size(disk_size),
            format_size(disk_max),
            disk_stats.disk_hits,
            disk_hit_rate,
            disk_stats.disk_writes,
            disk_stats.disk_evictions
        );

        // Overall hit rate (only if there have been requests)
        let total_hits = mem_stats.memory_hits + disk_stats.disk_hits;
        let total_requests = total_hits + disk_stats.disk_misses;

        if total_requests > 0 {
            let overall_hit_rate = (total_hits as f64 / total_requests as f64) * 100.0;
            log_info!(
                logger,
                "[CACHE] Overall: {:.1}% hit rate ({} hits / {} requests), provider: {}",
                overall_hit_rate,
                total_hits,
                total_requests,
                provider
            );
        }
    }

    /// Signal shutdown.
    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Wait for thread to finish.
    fn join(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CacheStatsLogger {
    fn drop(&mut self) {
        self.shutdown();
        self.join();
    }
}

/// Two-tier cache system coordinating memory and disk caches.
///
/// Implements the cache lookup strategy:
/// 1. Check memory cache (fast: <1ms)
/// 2. If miss, check disk cache (medium: 10-50ms)
/// 3. If miss, caller generates tile and caches it
///
/// The cache system runs two background threads:
/// - Disk cache garbage collection daemon
/// - Statistics logger (logs cache stats periodically)
///
/// # Example
///
/// ```
/// use xearthlayer::cache::{CacheSystem, CacheConfig, CacheKey};
/// use xearthlayer::coord::TileCoord;
/// use xearthlayer::dds::DdsFormat;
/// use xearthlayer::log::NoOpLogger;
/// use std::sync::Arc;
///
/// let config = CacheConfig::new("bing");
/// let logger = Arc::new(NoOpLogger);
/// let cache = CacheSystem::new(config, logger).unwrap();
///
/// let key = CacheKey::new("bing", DdsFormat::BC1, TileCoord { row: 100, col: 200, zoom: 15 });
///
/// // Try to get from cache
/// if let Some(data) = cache.get(&key) {
///     // Cache hit - use data
/// } else {
///     // Cache miss - generate tile
///     let data = vec![1, 2, 3]; // Generate tile data
///     cache.put(key, data).unwrap();
/// }
/// ```
pub struct CacheSystem {
    /// Memory cache (Tier 1: fast)
    memory: Arc<MemoryCache>,
    /// Disk cache (Tier 2: persistent)
    disk: Arc<DiskCache>,
    /// Provider name
    provider: String,
    /// Background daemon for disk cache garbage collection
    #[allow(dead_code)]
    daemon: DiskCacheDaemon,
    /// Background stats logger (optional, None if interval is 0)
    #[allow(dead_code)]
    stats_logger: Option<CacheStatsLogger>,
}

impl CacheSystem {
    /// Create a new two-tier cache system.
    ///
    /// This starts background threads for:
    /// - Disk cache garbage collection
    /// - Statistics logging (if interval > 0)
    ///
    /// # Arguments
    ///
    /// * `config` - Cache configuration with memory and disk settings
    /// * `logger` - Logger for statistics output
    pub fn new(config: CacheConfig, logger: Arc<dyn Logger>) -> Result<Self, CacheError> {
        let memory = Arc::new(MemoryCache::new(config.memory.max_size_bytes));

        let disk = Arc::new(DiskCache::new(
            config.disk.cache_dir.clone(),
            config.disk.max_size_bytes,
        )?);

        // Start background daemon for disk cache garbage collection
        let daemon = DiskCacheDaemon::start(disk.clone(), config.disk.daemon_interval_secs);

        // Start stats logger if interval > 0
        let stats_logger = if config.stats_interval_secs > 0 {
            Some(CacheStatsLogger::start(
                memory.clone(),
                disk.clone(),
                logger,
                config.provider.clone(),
                config.stats_interval_secs,
            ))
        } else {
            None
        };

        Ok(Self {
            memory,
            disk,
            provider: config.provider,
            daemon,
            stats_logger,
        })
    }

    /// Get a cached tile.
    ///
    /// Checks memory cache first, then disk cache. If found in disk cache,
    /// promotes to memory cache for faster future access.
    ///
    /// Returns `Some(data)` if found in either cache, `None` otherwise.
    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        // Try memory cache first (fast path)
        if let Some(data) = self.memory.get(key) {
            return Some(data);
        }

        // Try disk cache (slower path)
        if let Some(data) = self.disk.get(key) {
            // Promote to memory cache for faster future access
            // Ignore errors - if memory cache is full, eviction will handle it
            let _ = self.memory.put(key.clone(), data.clone());
            return Some(data);
        }

        // Complete miss
        None
    }

    /// Cache a tile in both memory and disk.
    ///
    /// Uses write-through strategy: writes to memory immediately and disk synchronously.
    /// For async disk writes, use the disk cache write thread (future enhancement).
    pub fn put(&self, key: CacheKey, data: Vec<u8>) -> Result<(), CacheError> {
        // Write to memory cache
        self.memory.put(key.clone(), data.clone())?;

        // Write to disk cache (synchronous for now)
        self.disk.put_sync(key, data)?;

        Ok(())
    }

    /// Check if a key exists in either cache.
    pub fn contains(&self, key: &CacheKey) -> bool {
        self.memory.contains(key) || self.disk.contains(key)
    }

    /// Get memory cache statistics.
    pub fn memory_stats(&self) -> CacheStats {
        self.memory.stats()
    }

    /// Get disk cache statistics.
    pub fn disk_stats(&self) -> CacheStats {
        self.disk.stats()
    }

    /// Get combined cache statistics.
    pub fn stats(&self) -> CacheStatistics {
        let memory_stats = self.memory.stats();
        let disk_stats = self.disk.stats();

        // Combine statistics
        let combined = CacheStats {
            memory_hits: memory_stats.memory_hits,
            memory_misses: memory_stats.memory_misses,
            memory_size_bytes: memory_stats.memory_size_bytes,
            memory_entry_count: memory_stats.memory_entry_count,
            memory_evictions: memory_stats.memory_evictions,
            disk_hits: disk_stats.disk_hits,
            disk_misses: disk_stats.disk_misses,
            disk_size_bytes: disk_stats.disk_size_bytes,
            disk_entry_count: disk_stats.disk_entry_count,
            disk_evictions: disk_stats.disk_evictions,
            disk_writes: disk_stats.disk_writes,
            disk_write_failures: disk_stats.disk_write_failures,
            downloads: memory_stats.downloads.max(disk_stats.downloads),
            download_failures: memory_stats
                .download_failures
                .max(disk_stats.download_failures),
            bytes_downloaded: memory_stats
                .bytes_downloaded
                .max(disk_stats.bytes_downloaded),
            created_at: memory_stats.created_at.min(disk_stats.created_at),
        };

        CacheStatistics::from_stats(&combined)
    }

    /// Get formatted statistics string.
    pub fn format_stats(&self) -> String {
        self.stats().format(&self.provider)
    }

    /// Clear both memory and disk caches.
    pub fn clear(&self) -> Result<(), CacheError> {
        self.memory.clear();
        self.disk.clear()?;
        Ok(())
    }

    /// Get the provider name.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Get memory cache entry count.
    pub fn memory_entry_count(&self) -> usize {
        self.memory.entry_count()
    }

    /// Get disk cache entry count.
    pub fn disk_entry_count(&self) -> usize {
        self.disk.entry_count()
    }

    /// Get memory cache size in bytes.
    pub fn memory_size_bytes(&self) -> usize {
        self.memory.size_bytes()
    }

    /// Get disk cache size in bytes.
    pub fn disk_size_bytes(&self) -> usize {
        self.disk.size_bytes()
    }

    /// Run eviction on memory cache if over limit.
    ///
    /// Called by cache daemon thread.
    pub fn evict_memory_if_needed(&self) -> Result<(), CacheError> {
        self.memory.evict_if_over_limit()
    }

    /// Run eviction on disk cache if over limit.
    ///
    /// Called by cache daemon thread.
    pub fn evict_disk_if_needed(&self) -> Result<(), CacheError> {
        self.disk.evict_if_over_limit()
    }
}

// Implement Cache trait for CacheSystem
impl Cache for CacheSystem {
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        self.get(key)
    }

    fn put(&self, key: CacheKey, data: Vec<u8>) -> Result<(), CacheError> {
        self.put(key, data)
    }

    fn contains(&self, key: &CacheKey) -> bool {
        self.contains(key)
    }

    fn clear(&self) -> Result<(), CacheError> {
        self.clear()
    }

    fn stats(&self) -> CacheStatistics {
        self.stats()
    }

    fn provider(&self) -> &str {
        self.provider()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::dds::DdsFormat;
    use crate::log::NoOpLogger;
    use tempfile::TempDir;

    fn create_test_cache() -> (CacheSystem, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = CacheConfig::new("bing")
            .with_memory_size(10_000)
            .with_disk_size(100_000)
            .with_cache_dir(temp_dir.path().to_path_buf())
            .with_stats_interval(0); // Disable stats logging in tests

        let logger: Arc<dyn Logger> = Arc::new(NoOpLogger);
        let cache = CacheSystem::new(config, logger).unwrap();
        (cache, temp_dir)
    }

    fn create_test_key(col: u32) -> CacheKey {
        CacheKey::new(
            "bing",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_cache_system_new() {
        let (cache, _temp) = create_test_cache();
        assert_eq!(cache.provider(), "bing");
        assert_eq!(cache.memory_entry_count(), 0);
        assert_eq!(cache.disk_entry_count(), 0);
    }

    #[test]
    fn test_cache_system_put_and_get() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put(key.clone(), data.clone()).unwrap();

        // Should be in both caches
        assert_eq!(cache.get(&key), Some(data));
        assert_eq!(cache.memory_entry_count(), 1);
        assert_eq!(cache.disk_entry_count(), 1);
    }

    #[test]
    fn test_cache_system_miss() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);

        assert_eq!(cache.get(&key), None);
    }

    #[test]
    fn test_cache_system_contains() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        assert!(!cache.contains(&key));
        cache.put(key.clone(), data).unwrap();
        assert!(cache.contains(&key));
    }

    #[test]
    fn test_cache_system_memory_hit() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put(key.clone(), data.clone()).unwrap();

        // First get - should hit memory
        assert_eq!(cache.get(&key), Some(data.clone()));

        // Check statistics
        let stats = cache.memory_stats();
        assert_eq!(stats.memory_hits, 1);
    }

    #[test]
    fn test_cache_system_disk_promotion() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        // Put in cache
        cache.put(key.clone(), data.clone()).unwrap();

        // Clear memory cache to simulate eviction
        cache.memory.clear();
        assert_eq!(cache.memory_entry_count(), 0);
        assert_eq!(cache.disk_entry_count(), 1);

        // Get should hit disk and promote to memory
        assert_eq!(cache.get(&key), Some(data));
        assert_eq!(cache.memory_entry_count(), 1);
    }

    #[test]
    fn test_cache_system_statistics() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        cache.put(key.clone(), data).unwrap();
        cache.get(&key);

        let stats = cache.stats();
        assert!(stats.memory_hit_rate_percent > 0.0);
    }

    #[test]
    fn test_cache_system_format_stats() {
        let (cache, _temp) = create_test_cache();
        let formatted = cache.format_stats();

        assert!(formatted.contains("Provider: bing"));
        assert!(formatted.contains("MEMORY CACHE"));
        assert!(formatted.contains("DISK CACHE"));
    }

    #[test]
    fn test_cache_system_clear() {
        let (cache, _temp) = create_test_cache();
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        cache.put(key.clone(), data).unwrap();
        assert_eq!(cache.memory_entry_count(), 1);
        assert_eq!(cache.disk_entry_count(), 1);

        cache.clear().unwrap();
        assert_eq!(cache.memory_entry_count(), 0);
        assert_eq!(cache.disk_entry_count(), 0);
    }

    #[test]
    fn test_cache_system_size_tracking() {
        let (cache, _temp) = create_test_cache();
        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let data = vec![0u8; 1000];

        cache.put(key1, data.clone()).unwrap();
        cache.put(key2, data.clone()).unwrap();

        assert_eq!(cache.memory_size_bytes(), 2000);
        assert_eq!(cache.disk_size_bytes(), 2000);
    }

    #[test]
    fn test_cache_system_eviction() {
        let (cache, _temp) = create_test_cache();
        let data = vec![0u8; 5000]; // Each entry 5KB

        // Add 3 entries (15KB total, over 10KB memory limit)
        for i in 1..=3 {
            cache.put(create_test_key(i), data.clone()).unwrap();
        }

        // Memory should have evicted entries
        assert!(cache.memory_size_bytes() <= 10_000);

        // Disk should have all entries
        assert_eq!(cache.disk_entry_count(), 3);
    }

    #[test]
    fn test_cache_system_evict_memory_if_needed() {
        let (cache, _temp) = create_test_cache();
        let data = vec![0u8; 5000];

        // Add entries
        for i in 1..=3 {
            cache.put(create_test_key(i), data.clone()).unwrap();
        }

        // Run manual eviction
        cache.evict_memory_if_needed().unwrap();

        // Should be under limit
        assert!(cache.memory_size_bytes() <= 10_000);
    }

    #[test]
    fn test_cache_system_evict_disk_if_needed() {
        let temp_dir = TempDir::new().unwrap();
        let config = CacheConfig::new("bing")
            .with_memory_size(1_000_000)
            .with_disk_size(10_000) // Small disk limit
            .with_cache_dir(temp_dir.path().to_path_buf())
            .with_stats_interval(0);

        let logger: Arc<dyn Logger> = Arc::new(NoOpLogger);
        let cache = CacheSystem::new(config, logger).unwrap();
        let data = vec![0u8; 5000];

        // Add entries that exceed disk limit
        for i in 1..=5 {
            cache.put(create_test_key(i), data.clone()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Run manual eviction
        cache.evict_disk_if_needed().unwrap();

        // Should be under or near limit
        assert!(cache.disk_size_bytes() <= 10_000);
    }

    #[test]
    fn test_cache_system_multiple_providers() {
        let temp_dir = TempDir::new().unwrap();
        let logger: Arc<dyn Logger> = Arc::new(NoOpLogger);

        // Create caches for different providers
        let config_bing = CacheConfig::new("bing")
            .with_cache_dir(temp_dir.path().to_path_buf())
            .with_stats_interval(0);
        let cache_bing = CacheSystem::new(config_bing, logger.clone()).unwrap();

        let config_google = CacheConfig::new("google")
            .with_cache_dir(temp_dir.path().to_path_buf())
            .with_stats_interval(0);
        let cache_google = CacheSystem::new(config_google, logger).unwrap();

        let key_bing = CacheKey::new(
            "bing",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
        );
        let key_google = CacheKey::new(
            "google",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
        );

        let data = vec![1, 2, 3];

        // Cache data in both
        cache_bing.put(key_bing.clone(), data.clone()).unwrap();
        cache_google.put(key_google.clone(), data.clone()).unwrap();

        // Both should be retrievable
        assert_eq!(cache_bing.get(&key_bing), Some(data.clone()));
        assert_eq!(cache_google.get(&key_google), Some(data));

        // Cross-provider lookup should not work
        assert_eq!(cache_bing.get(&key_google), None);
        assert_eq!(cache_google.get(&key_bing), None);
    }
}
