//! Cache statistics tracking and reporting.

use std::time::Instant;

/// Cache statistics for monitoring and debugging.
#[derive(Debug, Clone)]
pub struct CacheStats {
    // Memory cache metrics
    pub memory_hits: u64,
    pub memory_misses: u64,
    pub memory_size_bytes: usize,
    pub memory_entry_count: usize,
    pub memory_evictions: u64,

    // Disk cache metrics
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub disk_size_bytes: usize,
    pub disk_entry_count: usize,
    pub disk_evictions: u64,
    pub disk_writes: u64,
    pub disk_write_failures: u64,

    // Download metrics
    pub downloads: u64,
    pub download_failures: u64,
    pub bytes_downloaded: u64,

    // Timing
    pub created_at: Instant,
}

impl Default for CacheStats {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheStats {
    /// Create a new statistics tracker.
    pub fn new() -> Self {
        Self {
            memory_hits: 0,
            memory_misses: 0,
            memory_size_bytes: 0,
            memory_entry_count: 0,
            memory_evictions: 0,
            disk_hits: 0,
            disk_misses: 0,
            disk_size_bytes: 0,
            disk_entry_count: 0,
            disk_evictions: 0,
            disk_writes: 0,
            disk_write_failures: 0,
            downloads: 0,
            download_failures: 0,
            bytes_downloaded: 0,
            created_at: Instant::now(),
        }
    }

    /// Calculate memory cache hit rate (0.0 to 1.0).
    pub fn memory_hit_rate(&self) -> f64 {
        let total = self.memory_hits + self.memory_misses;
        if total == 0 {
            0.0
        } else {
            self.memory_hits as f64 / total as f64
        }
    }

    /// Calculate disk cache hit rate (0.0 to 1.0).
    pub fn disk_hit_rate(&self) -> f64 {
        let total = self.disk_hits + self.disk_misses;
        if total == 0 {
            0.0
        } else {
            self.disk_hits as f64 / total as f64
        }
    }

    /// Calculate overall cache hit rate (0.0 to 1.0).
    ///
    /// Includes both memory and disk hits.
    pub fn overall_hit_rate(&self) -> f64 {
        let hits = self.memory_hits + self.disk_hits;
        let total = hits + self.disk_misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    /// Get the uptime duration since statistics started.
    pub fn uptime(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }

    /// Record a memory cache hit.
    pub fn record_memory_hit(&mut self) {
        self.memory_hits += 1;
    }

    /// Record a memory cache miss.
    pub fn record_memory_miss(&mut self) {
        self.memory_misses += 1;
    }

    /// Record a disk cache hit.
    pub fn record_disk_hit(&mut self) {
        self.disk_hits += 1;
    }

    /// Record a disk cache miss.
    pub fn record_disk_miss(&mut self) {
        self.disk_misses += 1;
    }

    /// Record a memory cache eviction.
    pub fn record_memory_eviction(&mut self, count: u64) {
        self.memory_evictions += count;
    }

    /// Record a disk cache eviction.
    pub fn record_disk_eviction(&mut self, count: u64) {
        self.disk_evictions += count;
    }

    /// Record a successful disk write.
    pub fn record_disk_write(&mut self) {
        self.disk_writes += 1;
    }

    /// Record a failed disk write.
    pub fn record_disk_write_failure(&mut self) {
        self.disk_write_failures += 1;
    }

    /// Record a successful download.
    pub fn record_download(&mut self, bytes: u64) {
        self.downloads += 1;
        self.bytes_downloaded += bytes;
    }

    /// Record a failed download.
    pub fn record_download_failure(&mut self) {
        self.download_failures += 1;
    }

    /// Update memory cache size metrics.
    pub fn update_memory_size(&mut self, size_bytes: usize, entry_count: usize) {
        self.memory_size_bytes = size_bytes;
        self.memory_entry_count = entry_count;
    }

    /// Update disk cache size metrics.
    pub fn update_disk_size(&mut self, size_bytes: usize, entry_count: usize) {
        self.disk_size_bytes = size_bytes;
        self.disk_entry_count = entry_count;
    }
}

/// Snapshot of cache statistics for reporting.
#[derive(Debug, Clone)]
pub struct CacheStatistics {
    pub stats: CacheStats,
    pub memory_hit_rate_percent: f64,
    pub disk_hit_rate_percent: f64,
    pub overall_hit_rate_percent: f64,
    pub uptime_secs: u64,
}

impl CacheStatistics {
    /// Create a statistics snapshot from current stats.
    pub fn from_stats(stats: &CacheStats) -> Self {
        Self {
            stats: stats.clone(),
            memory_hit_rate_percent: stats.memory_hit_rate() * 100.0,
            disk_hit_rate_percent: stats.disk_hit_rate() * 100.0,
            overall_hit_rate_percent: stats.overall_hit_rate() * 100.0,
            uptime_secs: stats.uptime().as_secs(),
        }
    }

    /// Format statistics as a human-readable string.
    pub fn format(&self, provider: &str) -> String {
        let stats = &self.stats;

        format!(
            r#"XEarthLayer Cache Statistics
Provider: {}

MEMORY CACHE
  Entries:     {}
  Size:        {:.2} MB
  Hits:        {}
  Misses:      {}
  Hit Rate:    {:.1}%
  Evictions:   {}

DISK CACHE
  Entries:     {}
  Size:        {:.2} GB
  Hits:        {}
  Misses:      {}
  Hit Rate:    {:.1}%
  Writes:      {}
  Failures:    {}
  Evictions:   {}

DOWNLOADS
  Total:       {}
  Failures:    {}
  Bytes:       {:.2} MB

OVERALL
  Hit Rate:    {:.1}%
  Uptime:      {}s
"#,
            provider,
            stats.memory_entry_count,
            stats.memory_size_bytes as f64 / (1024.0 * 1024.0),
            stats.memory_hits,
            stats.memory_misses,
            self.memory_hit_rate_percent,
            stats.memory_evictions,
            stats.disk_entry_count,
            stats.disk_size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            stats.disk_hits,
            stats.disk_misses,
            self.disk_hit_rate_percent,
            stats.disk_writes,
            stats.disk_write_failures,
            stats.disk_evictions,
            stats.downloads,
            stats.download_failures,
            stats.bytes_downloaded as f64 / (1024.0 * 1024.0),
            self.overall_hit_rate_percent,
            self.uptime_secs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_stats_default() {
        let stats = CacheStats::default();

        assert_eq!(stats.memory_hits, 0);
        assert_eq!(stats.memory_misses, 0);
        assert_eq!(stats.disk_hits, 0);
        assert_eq!(stats.disk_misses, 0);
        assert_eq!(stats.downloads, 0);
    }

    #[test]
    fn test_memory_hit_rate_no_requests() {
        let stats = CacheStats::new();
        assert_eq!(stats.memory_hit_rate(), 0.0);
    }

    #[test]
    fn test_memory_hit_rate_all_hits() {
        let mut stats = CacheStats::new();
        stats.memory_hits = 100;
        stats.memory_misses = 0;

        assert_eq!(stats.memory_hit_rate(), 1.0);
    }

    #[test]
    fn test_memory_hit_rate_mixed() {
        let mut stats = CacheStats::new();
        stats.memory_hits = 75;
        stats.memory_misses = 25;

        assert_eq!(stats.memory_hit_rate(), 0.75);
    }

    #[test]
    fn test_disk_hit_rate() {
        let mut stats = CacheStats::new();
        stats.disk_hits = 80;
        stats.disk_misses = 20;

        assert_eq!(stats.disk_hit_rate(), 0.8);
    }

    #[test]
    fn test_overall_hit_rate() {
        let mut stats = CacheStats::new();
        stats.memory_hits = 70; // Memory hits
        stats.disk_hits = 20; // Disk hits
        stats.disk_misses = 10; // Full misses

        // Total requests: 70 + 20 + 10 = 100
        // Total hits: 70 + 20 = 90
        // Hit rate: 90/100 = 0.9
        assert_eq!(stats.overall_hit_rate(), 0.9);
    }

    #[test]
    fn test_record_memory_hit() {
        let mut stats = CacheStats::new();
        stats.record_memory_hit();
        stats.record_memory_hit();

        assert_eq!(stats.memory_hits, 2);
    }

    #[test]
    fn test_record_memory_miss() {
        let mut stats = CacheStats::new();
        stats.record_memory_miss();

        assert_eq!(stats.memory_misses, 1);
    }

    #[test]
    fn test_record_disk_operations() {
        let mut stats = CacheStats::new();
        stats.record_disk_hit();
        stats.record_disk_miss();
        stats.record_disk_write();
        stats.record_disk_write_failure();

        assert_eq!(stats.disk_hits, 1);
        assert_eq!(stats.disk_misses, 1);
        assert_eq!(stats.disk_writes, 1);
        assert_eq!(stats.disk_write_failures, 1);
    }

    #[test]
    fn test_record_evictions() {
        let mut stats = CacheStats::new();
        stats.record_memory_eviction(5);
        stats.record_disk_eviction(3);

        assert_eq!(stats.memory_evictions, 5);
        assert_eq!(stats.disk_evictions, 3);
    }

    #[test]
    fn test_record_downloads() {
        let mut stats = CacheStats::new();
        stats.record_download(1_000_000);
        stats.record_download(2_000_000);
        stats.record_download_failure();

        assert_eq!(stats.downloads, 2);
        assert_eq!(stats.bytes_downloaded, 3_000_000);
        assert_eq!(stats.download_failures, 1);
    }

    #[test]
    fn test_update_memory_size() {
        let mut stats = CacheStats::new();
        stats.update_memory_size(500_000_000, 45);

        assert_eq!(stats.memory_size_bytes, 500_000_000);
        assert_eq!(stats.memory_entry_count, 45);
    }

    #[test]
    fn test_update_disk_size() {
        let mut stats = CacheStats::new();
        stats.update_disk_size(10_000_000_000, 900);

        assert_eq!(stats.disk_size_bytes, 10_000_000_000);
        assert_eq!(stats.disk_entry_count, 900);
    }

    #[test]
    fn test_uptime_increases() {
        let stats = CacheStats::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let uptime = stats.uptime();

        assert!(uptime.as_millis() >= 10);
    }

    #[test]
    fn test_cache_statistics_from_stats() {
        let mut stats = CacheStats::new();
        stats.memory_hits = 90;
        stats.memory_misses = 10;

        let snapshot = CacheStatistics::from_stats(&stats);

        assert_eq!(snapshot.memory_hit_rate_percent, 90.0);
        assert_eq!(snapshot.stats.memory_hits, 90);
    }

    #[test]
    fn test_cache_statistics_format() {
        let mut stats = CacheStats::new();
        stats.memory_hits = 100;
        stats.memory_misses = 10;
        stats.disk_hits = 5;
        stats.disk_misses = 5;
        stats.memory_entry_count = 50;
        stats.memory_size_bytes = 500_000_000; // 500 MB

        let snapshot = CacheStatistics::from_stats(&stats);
        let formatted = snapshot.format("bing");

        assert!(formatted.contains("Provider: bing"));
        assert!(formatted.contains("Entries:     50"));
        assert!(formatted.contains("MEMORY CACHE"));
        assert!(formatted.contains("DISK CACHE"));
        assert!(formatted.contains("DOWNLOADS"));
        assert!(formatted.contains("OVERALL"));
    }
}
