//! Point-in-time telemetry snapshot.
//!
//! Provides an immutable view of metrics for display and reporting.

use std::fmt;
use std::time::Duration;

/// A point-in-time snapshot of pipeline metrics.
///
/// This is an immutable copy of all metrics, safe to use for display
/// without blocking the pipeline. All rates are pre-computed based on
/// uptime at the time of snapshot.
#[derive(Clone, Debug)]
pub struct TelemetrySnapshot {
    /// How long the pipeline has been running
    pub uptime: Duration,

    // === FUSE metrics ===
    /// Total FUSE tiles served (from any source: memory, DDS disk, or job)
    pub fuse_tiles_served: u64,
    /// Currently active FUSE requests
    pub fuse_requests_active: usize,
    /// FUSE requests waiting in queue
    pub fuse_requests_waiting: usize,

    // === Job metrics ===
    /// Total jobs submitted
    pub jobs_submitted: u64,
    /// Jobs submitted from FUSE (X-Plane requests, NOT prefetch).
    /// Used by circuit breaker to detect scenery loading bursts.
    pub fuse_jobs_submitted: u64,
    /// Jobs completed successfully
    pub jobs_completed: u64,
    /// Jobs that failed due to pipeline errors
    pub jobs_failed: u64,
    /// Jobs that timed out (FUSE timeout, work may have completed in background)
    pub jobs_timed_out: u64,
    /// Currently active jobs
    pub jobs_active: usize,
    /// Jobs that were coalesced
    pub jobs_coalesced: u64,

    // === Download metrics ===
    /// Total chunks downloaded
    pub chunks_downloaded: u64,
    /// Chunks that failed after retries
    pub chunks_failed: u64,
    /// Total retry attempts
    pub chunks_retried: u64,
    /// Total bytes downloaded
    pub bytes_downloaded: u64,
    /// Currently active downloads
    pub downloads_active: usize,

    // === Cache metrics ===
    /// Memory cache hits (all origins)
    pub memory_cache_hits: u64,
    /// Memory cache misses (all origins)
    pub memory_cache_misses: u64,
    /// Memory cache hit rate — all origins (0.0 - 1.0)
    pub memory_cache_hit_rate: f64,
    /// Memory cache hit rate — FUSE (X-Plane) requests only (0.0 - 1.0)
    pub fuse_memory_cache_hit_rate: f64,
    /// Memory cache current size in bytes
    pub memory_cache_size_bytes: u64,

    // === DDS Disk Cache metrics ===
    /// DDS disk cache hits
    pub dds_disk_cache_hits: u64,
    /// DDS disk cache misses
    pub dds_disk_cache_misses: u64,
    /// DDS disk cache hit rate (0.0 - 1.0)
    pub dds_disk_cache_hit_rate: f64,
    /// DDS disk cache current size in bytes
    pub dds_disk_cache_size_bytes: u64,
    /// DDS disk bytes read this session (from cache hits)
    pub dds_disk_bytes_read: u64,

    // === Chunk Disk Cache metrics ===
    /// Chunk disk cache hits
    pub chunk_disk_cache_hits: u64,
    /// Chunk disk cache misses
    pub chunk_disk_cache_misses: u64,
    /// Chunk disk cache hit rate (0.0 - 1.0)
    pub chunk_disk_cache_hit_rate: f64,
    /// Chunk disk cache current size in bytes
    pub chunk_disk_cache_size_bytes: u64,
    /// Chunk disk bytes written this session
    pub chunk_disk_bytes_written: u64,
    /// Chunk disk bytes read this session (from cache hits)
    pub chunk_disk_bytes_read: u64,

    // === Encode metrics ===
    /// Encode operations completed
    pub encodes_completed: u64,
    /// Currently active encodes
    pub encodes_active: usize,
    /// Total bytes encoded
    pub bytes_encoded: u64,

    // === Computed rates ===
    /// Jobs per second
    pub jobs_per_second: f64,
    /// FUSE jobs per second (X-Plane requests only, NOT prefetch).
    /// Used by circuit breaker to detect scenery loading bursts.
    pub fuse_jobs_per_second: f64,
    /// Chunks per second
    pub chunks_per_second: f64,
    /// Bytes per second (download throughput)
    pub bytes_per_second: f64,
    /// Peak bytes per second (highest throughput seen)
    pub peak_bytes_per_second: f64,

    // === Timing totals (milliseconds) ===
    /// Total download time
    pub total_download_time_ms: u64,
    /// Total assembly time
    pub total_assembly_time_ms: u64,
    /// Total encode time
    pub total_encode_time_ms: u64,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        Self {
            uptime: Duration::ZERO,
            fuse_tiles_served: 0,
            fuse_requests_active: 0,
            fuse_requests_waiting: 0,
            jobs_submitted: 0,
            fuse_jobs_submitted: 0,
            jobs_completed: 0,
            jobs_failed: 0,
            jobs_timed_out: 0,
            jobs_active: 0,
            jobs_coalesced: 0,
            chunks_downloaded: 0,
            chunks_failed: 0,
            chunks_retried: 0,
            bytes_downloaded: 0,
            downloads_active: 0,
            memory_cache_hits: 0,
            memory_cache_misses: 0,
            memory_cache_hit_rate: 0.0,
            fuse_memory_cache_hit_rate: 0.0,
            memory_cache_size_bytes: 0,
            dds_disk_cache_hits: 0,
            dds_disk_cache_misses: 0,
            dds_disk_cache_hit_rate: 0.0,
            dds_disk_cache_size_bytes: 0,
            dds_disk_bytes_read: 0,
            chunk_disk_cache_hits: 0,
            chunk_disk_cache_misses: 0,
            chunk_disk_cache_hit_rate: 0.0,
            chunk_disk_cache_size_bytes: 0,
            chunk_disk_bytes_written: 0,
            chunk_disk_bytes_read: 0,
            encodes_completed: 0,
            encodes_active: 0,
            bytes_encoded: 0,
            jobs_per_second: 0.0,
            fuse_jobs_per_second: 0.0,
            chunks_per_second: 0.0,
            bytes_per_second: 0.0,
            peak_bytes_per_second: 0.0,
            total_download_time_ms: 0,
            total_assembly_time_ms: 0,
            total_encode_time_ms: 0,
        }
    }
}

impl TelemetrySnapshot {
    /// Returns the total number of tiles generated (completed jobs).
    pub fn tiles_generated(&self) -> u64 {
        self.jobs_completed
    }

    /// Returns the error rate (0.0 - 1.0).
    ///
    /// This only counts actual pipeline errors, not timeouts.
    pub fn error_rate(&self) -> f64 {
        let total = self.jobs_completed + self.jobs_failed + self.jobs_timed_out;
        if total == 0 {
            0.0
        } else {
            self.jobs_failed as f64 / total as f64
        }
    }

    /// Returns the timeout rate (0.0 - 1.0).
    pub fn timeout_rate(&self) -> f64 {
        let total = self.jobs_completed + self.jobs_failed + self.jobs_timed_out;
        if total == 0 {
            0.0
        } else {
            self.jobs_timed_out as f64 / total as f64
        }
    }

    /// Returns the chunk failure rate (0.0 - 1.0).
    pub fn chunk_failure_rate(&self) -> f64 {
        let total = self.chunks_downloaded + self.chunks_failed;
        if total == 0 {
            0.0
        } else {
            self.chunks_failed as f64 / total as f64
        }
    }

    /// Returns the coalescing savings rate (0.0 - 1.0).
    ///
    /// Percentage of total requests that were avoided via coalescing.
    /// Formula: coalesced / (coalesced + completed)
    pub fn coalescing_rate(&self) -> f64 {
        let total_requests = self.jobs_coalesced + self.jobs_completed;
        if total_requests == 0 {
            0.0
        } else {
            self.jobs_coalesced as f64 / total_requests as f64
        }
    }

    /// Returns download throughput in human-readable format.
    pub fn throughput_human(&self) -> String {
        format_bytes_per_second(self.bytes_per_second)
    }

    /// Returns uptime in human-readable format.
    pub fn uptime_human(&self) -> String {
        format_duration(self.uptime)
    }

    /// Returns total bytes downloaded in human-readable format.
    pub fn bytes_downloaded_human(&self) -> String {
        format_bytes(self.bytes_downloaded)
    }
}

impl fmt::Display for TelemetrySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Pipeline Telemetry (uptime: {})", self.uptime_human())?;
        writeln!(f, "─────────────────────────────────────────")?;
        writeln!(f)?;

        // Jobs
        writeln!(f, "Jobs:")?;
        writeln!(
            f,
            "  Completed: {} ({:.1}/s)",
            format_number(self.jobs_completed),
            self.jobs_per_second
        )?;
        writeln!(f, "  Active: {}", format_number(self.jobs_active))?;
        writeln!(f, "  Timed out: {}", format_number(self.jobs_timed_out))?;
        writeln!(f, "  Errors: {}", format_number(self.jobs_failed))?;
        writeln!(
            f,
            "  Coalesced: {} ({:.1}%)",
            format_number(self.jobs_coalesced),
            self.coalescing_rate() * 100.0
        )?;
        writeln!(f)?;

        // Downloads
        writeln!(f, "Downloads:")?;
        writeln!(
            f,
            "  Chunks: {} ({:.0}/s)",
            format_number(self.chunks_downloaded),
            self.chunks_per_second
        )?;
        writeln!(f, "  Throughput: {}", self.throughput_human())?;
        writeln!(
            f,
            "  Failed: {} ({:.2}%)",
            format_number(self.chunks_failed),
            self.chunk_failure_rate() * 100.0
        )?;
        writeln!(f, "  Retries: {}", format_number(self.chunks_retried))?;
        writeln!(f)?;

        // Cache
        writeln!(f, "Cache:")?;
        writeln!(
            f,
            "  Memory: {:.1}% hit rate ({} hits, {} misses) | FUSE: {:.1}%",
            self.memory_cache_hit_rate * 100.0,
            format_number(self.memory_cache_hits),
            format_number(self.memory_cache_misses),
            self.fuse_memory_cache_hit_rate * 100.0,
        )?;
        writeln!(
            f,
            "  DDS Disk: {:.1}% hit rate ({} hits, {} misses)",
            self.dds_disk_cache_hit_rate * 100.0,
            format_number(self.dds_disk_cache_hits),
            format_number(self.dds_disk_cache_misses)
        )?;
        writeln!(
            f,
            "  Chunks: {:.1}% hit rate ({} hits, {} misses)",
            self.chunk_disk_cache_hit_rate * 100.0,
            format_number(self.chunk_disk_cache_hits),
            format_number(self.chunk_disk_cache_misses)
        )?;

        Ok(())
    }
}

/// Format bytes per second in human-readable form.
fn format_bytes_per_second(bps: f64) -> String {
    if bps >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

/// Format bytes in human-readable form.
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Format an integer with thousand separators (e.g., 23040 → "23,040").
fn format_number(n: impl fmt::Display) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Format duration in human-readable form.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_snapshot() -> TelemetrySnapshot {
        TelemetrySnapshot {
            uptime: Duration::from_secs(3661), // 1:01:01
            fuse_tiles_served: 5000,
            fuse_requests_active: 8,
            fuse_requests_waiting: 12,
            jobs_submitted: 100,
            fuse_jobs_submitted: 80, // 80 from FUSE, 20 from prefetch
            jobs_completed: 90,
            jobs_failed: 2,
            jobs_timed_out: 3,
            jobs_active: 5,
            jobs_coalesced: 20,
            chunks_downloaded: 23040, // 90 tiles × 256 chunks
            chunks_failed: 10,
            chunks_retried: 25,
            bytes_downloaded: 70_000_000, // ~70 MB
            downloads_active: 128,
            memory_cache_hits: 30,
            memory_cache_misses: 60,
            memory_cache_hit_rate: 0.333,
            fuse_memory_cache_hit_rate: 0.6,
            memory_cache_size_bytes: 500_000_000,
            dds_disk_cache_hits: 17300,
            dds_disk_cache_misses: 1200,
            dds_disk_cache_hit_rate: 17300.0 / 18500.0,
            dds_disk_cache_size_bytes: 289_000_000_000,
            dds_disk_bytes_read: 190_000_000_000,
            chunk_disk_cache_hits: 5000,
            chunk_disk_cache_misses: 18040,
            chunk_disk_cache_hit_rate: 5000.0 / 23040.0,
            chunk_disk_cache_size_bytes: 5_000_000_000,
            chunk_disk_bytes_written: 500_000_000,
            chunk_disk_bytes_read: 750_000_000,
            encodes_completed: 90,
            encodes_active: 2,
            bytes_encoded: 1_000_000_000, // ~1 GB
            jobs_per_second: 0.0246,
            fuse_jobs_per_second: 0.0218, // 80 FUSE jobs / 3661 seconds
            chunks_per_second: 6.3,
            bytes_per_second: 19_125.0,
            peak_bytes_per_second: 50_000.0,
            total_download_time_ms: 120_000,
            total_assembly_time_ms: 15_000,
            total_encode_time_ms: 45_000,
        }
    }

    #[test]
    fn test_default() {
        let snapshot = TelemetrySnapshot::default();
        assert_eq!(snapshot.uptime, Duration::ZERO);
        assert_eq!(snapshot.jobs_submitted, 0);
        assert_eq!(snapshot.bytes_per_second, 0.0);
    }

    #[test]
    fn test_error_rate() {
        let snapshot = test_snapshot();
        // 2 failed out of 95 total (90 completed + 2 failed + 3 timed_out)
        let expected = 2.0 / 95.0;
        assert!((snapshot.error_rate() - expected).abs() < 0.001);
    }

    #[test]
    fn test_timeout_rate() {
        let snapshot = test_snapshot();
        // 3 timed_out out of 95 total (90 completed + 2 failed + 3 timed_out)
        let expected = 3.0 / 95.0;
        assert!((snapshot.timeout_rate() - expected).abs() < 0.001);
    }

    #[test]
    fn test_coalescing_rate() {
        let snapshot = test_snapshot();
        // 20 coalesced out of 110 total requests (20 coalesced + 90 completed)
        let expected = 20.0 / 110.0; // ~0.1818
        assert!((snapshot.coalescing_rate() - expected).abs() < 0.001);
    }

    #[test]
    fn test_chunk_failure_rate() {
        let snapshot = test_snapshot();
        // 10 failed out of 23050 total
        let expected = 10.0 / 23050.0;
        assert!((snapshot.chunk_failure_rate() - expected).abs() < 0.001);
    }

    #[test]
    fn test_format_bytes_per_second() {
        assert_eq!(format_bytes_per_second(500.0), "500 B/s");
        assert_eq!(format_bytes_per_second(1_500.0), "1.5 KB/s");
        assert_eq!(format_bytes_per_second(1_500_000.0), "1.5 MB/s");
        assert_eq!(format_bytes_per_second(1_500_000_000.0), "1.5 GB/s");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(61)), "01:01");
        assert_eq!(format_duration(Duration::from_secs(3661)), "01:01:01");
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(23_040), "23,040");
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(5_000_000_000_u64), "5,000,000,000");
    }

    #[test]
    fn test_display() {
        let snapshot = test_snapshot();
        let output = format!("{}", snapshot);

        assert!(output.contains("Pipeline Telemetry"));
        assert!(output.contains("Jobs:"));
        assert!(output.contains("Downloads:"));
        assert!(output.contains("Cache:"));
        // Verify thousand separators
        assert!(output.contains("23,040")); // chunks_downloaded
        // Verify per-tier disk cache lines
        assert!(output.contains("DDS Disk"));
        assert!(output.contains("Chunks"));
        assert!(output.contains("17,300 hits")); // dds_disk_cache_hits
        assert!(output.contains("1,200 misses")); // dds_disk_cache_misses
        assert!(output.contains("5,000 hits")); // chunk_disk_cache_hits
        assert!(output.contains("18,040 misses")); // chunk_disk_cache_misses
    }

    #[test]
    fn test_uptime_human() {
        let snapshot = test_snapshot();
        assert_eq!(snapshot.uptime_human(), "01:01:01");
    }

    #[test]
    fn test_throughput_human() {
        let snapshot = test_snapshot();
        assert_eq!(snapshot.throughput_human(), "19.1 KB/s");
    }

    #[test]
    fn test_dds_disk_cache_hit_rate() {
        let snapshot = TelemetrySnapshot {
            dds_disk_cache_hits: 93,
            dds_disk_cache_misses: 7,
            dds_disk_cache_hit_rate: 0.93,
            ..Default::default()
        };
        assert!((snapshot.dds_disk_cache_hit_rate - 0.93).abs() < 0.001);
    }

    #[test]
    fn test_chunk_disk_cache_hit_rate() {
        let snapshot = TelemetrySnapshot {
            chunk_disk_cache_hits: 5000,
            chunk_disk_cache_misses: 18000,
            chunk_disk_cache_hit_rate: 5000.0 / 23000.0,
            ..Default::default()
        };
        assert!((snapshot.chunk_disk_cache_hit_rate - 0.2174).abs() < 0.001);
    }
}
