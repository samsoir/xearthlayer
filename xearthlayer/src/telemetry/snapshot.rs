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
    /// Memory cache hits
    pub memory_cache_hits: u64,
    /// Memory cache misses
    pub memory_cache_misses: u64,
    /// Memory cache hit rate (0.0 - 1.0)
    pub memory_cache_hit_rate: f64,
    /// Memory cache current size in bytes
    pub memory_cache_size_bytes: u64,
    /// Disk cache hits
    pub disk_cache_hits: u64,
    /// Disk cache misses
    pub disk_cache_misses: u64,
    /// Disk cache hit rate (0.0 - 1.0)
    pub disk_cache_hit_rate: f64,
    /// Disk cache current size in bytes
    pub disk_cache_size_bytes: u64,

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

    /// Returns the coalescing rate (0.0 - 1.0).
    ///
    /// Ratio of coalesced jobs to total jobs submitted.
    pub fn coalescing_rate(&self) -> f64 {
        if self.jobs_submitted == 0 {
            0.0
        } else {
            self.jobs_coalesced as f64 / self.jobs_submitted as f64
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
            self.jobs_completed, self.jobs_per_second
        )?;
        writeln!(f, "  Active: {}", self.jobs_active)?;
        writeln!(f, "  Timed out: {}", self.jobs_timed_out)?;
        writeln!(f, "  Errors: {}", self.jobs_failed)?;
        writeln!(
            f,
            "  Coalesced: {} ({:.1}%)",
            self.jobs_coalesced,
            self.coalescing_rate() * 100.0
        )?;
        writeln!(f)?;

        // Downloads
        writeln!(f, "Downloads:")?;
        writeln!(
            f,
            "  Chunks: {} ({:.0}/s)",
            self.chunks_downloaded, self.chunks_per_second
        )?;
        writeln!(f, "  Throughput: {}", self.throughput_human())?;
        writeln!(
            f,
            "  Failed: {} ({:.2}%)",
            self.chunks_failed,
            self.chunk_failure_rate() * 100.0
        )?;
        writeln!(f, "  Retries: {}", self.chunks_retried)?;
        writeln!(f)?;

        // Cache
        writeln!(f, "Cache:")?;
        writeln!(
            f,
            "  Memory: {:.1}% hit rate ({} hits, {} misses)",
            self.memory_cache_hit_rate * 100.0,
            self.memory_cache_hits,
            self.memory_cache_misses
        )?;
        writeln!(
            f,
            "  Disk: {:.1}% hit rate ({} hits, {} misses)",
            self.disk_cache_hit_rate * 100.0,
            self.disk_cache_hits,
            self.disk_cache_misses
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
            memory_cache_size_bytes: 500_000_000,
            disk_cache_hits: 5000,
            disk_cache_misses: 18040,
            disk_cache_hit_rate: 0.217,
            disk_cache_size_bytes: 5_000_000_000,
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
        // 20 coalesced out of 100 submitted
        assert!((snapshot.coalescing_rate() - 0.2).abs() < 0.001);
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
    fn test_display() {
        let snapshot = test_snapshot();
        let output = format!("{}", snapshot);

        assert!(output.contains("Pipeline Telemetry"));
        assert!(output.contains("Jobs:"));
        assert!(output.contains("Downloads:"));
        assert!(output.contains("Cache:"));
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
}
