//! Lock-free atomic metrics collection.
//!
//! Uses `AtomicU64` and `AtomicUsize` for high-performance, thread-safe
//! metrics collection without locks.

use super::TelemetrySnapshot;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Lock-free metrics collection for the pipeline.
///
/// All operations use `Relaxed` ordering for maximum performance.
/// This is appropriate because we don't need strict ordering between
/// different counters - they're independent measurements.
pub struct PipelineMetrics {
    /// When metrics collection started
    start_time: Instant,

    // === FUSE metrics ===
    /// Currently active FUSE requests being handled
    fuse_requests_active: AtomicUsize,
    /// FUSE requests waiting in queue
    fuse_requests_waiting: AtomicUsize,

    // === Job metrics ===
    /// Total jobs submitted to the pipeline
    jobs_submitted: AtomicU64,
    /// Jobs completed successfully (includes partial success)
    jobs_completed: AtomicU64,
    /// Jobs that failed entirely
    jobs_failed: AtomicU64,
    /// Currently active jobs in the pipeline
    jobs_active: AtomicUsize,
    /// Jobs that were coalesced (waited for existing work)
    jobs_coalesced: AtomicU64,

    // === Download metrics ===
    /// Total chunks downloaded successfully
    chunks_downloaded: AtomicU64,
    /// Chunks that failed after all retries
    chunks_failed: AtomicU64,
    /// Total retry attempts
    chunks_retried: AtomicU64,
    /// Total bytes downloaded
    bytes_downloaded: AtomicU64,
    /// Currently active download tasks
    downloads_active: AtomicUsize,

    // === Cache metrics ===
    /// Memory cache hits (tile level)
    memory_cache_hits: AtomicU64,
    /// Memory cache misses (tile level)
    memory_cache_misses: AtomicU64,
    /// Memory cache current size in bytes
    memory_cache_size_bytes: AtomicU64,
    /// Disk cache hits (chunk level)
    disk_cache_hits: AtomicU64,
    /// Disk cache misses (chunk level)
    disk_cache_misses: AtomicU64,
    /// Disk cache current size in bytes
    disk_cache_size_bytes: AtomicU64,

    // === Encoding metrics ===
    /// Encode operations completed
    encodes_completed: AtomicU64,
    /// Currently active encode operations
    encodes_active: AtomicUsize,
    /// Total bytes encoded (DDS output)
    bytes_encoded: AtomicU64,

    // === Timing metrics (stored as microseconds) ===
    /// Total download time in microseconds
    download_time_us: AtomicU64,
    /// Total assembly time in microseconds
    assembly_time_us: AtomicU64,
    /// Total encode time in microseconds
    encode_time_us: AtomicU64,

    // === Peak tracking ===
    /// Peak bytes per second (updated during snapshot calculation)
    peak_bytes_per_second: AtomicU64,
}

impl PipelineMetrics {
    /// Creates a new metrics instance.
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            fuse_requests_active: AtomicUsize::new(0),
            fuse_requests_waiting: AtomicUsize::new(0),
            jobs_submitted: AtomicU64::new(0),
            jobs_completed: AtomicU64::new(0),
            jobs_failed: AtomicU64::new(0),
            jobs_active: AtomicUsize::new(0),
            jobs_coalesced: AtomicU64::new(0),
            chunks_downloaded: AtomicU64::new(0),
            chunks_failed: AtomicU64::new(0),
            chunks_retried: AtomicU64::new(0),
            bytes_downloaded: AtomicU64::new(0),
            downloads_active: AtomicUsize::new(0),
            memory_cache_hits: AtomicU64::new(0),
            memory_cache_misses: AtomicU64::new(0),
            memory_cache_size_bytes: AtomicU64::new(0),
            disk_cache_hits: AtomicU64::new(0),
            disk_cache_misses: AtomicU64::new(0),
            disk_cache_size_bytes: AtomicU64::new(0),
            encodes_completed: AtomicU64::new(0),
            encodes_active: AtomicUsize::new(0),
            bytes_encoded: AtomicU64::new(0),
            download_time_us: AtomicU64::new(0),
            assembly_time_us: AtomicU64::new(0),
            encode_time_us: AtomicU64::new(0),
            peak_bytes_per_second: AtomicU64::new(0),
        }
    }

    // === FUSE tracking ===

    /// Record a FUSE request starting.
    pub fn fuse_request_started(&self) {
        self.fuse_requests_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a FUSE request completing.
    pub fn fuse_request_completed(&self) {
        self.fuse_requests_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a request being added to the FUSE wait queue.
    pub fn fuse_request_queued(&self) {
        self.fuse_requests_waiting.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a request being removed from the FUSE wait queue.
    pub fn fuse_request_dequeued(&self) {
        self.fuse_requests_waiting.fetch_sub(1, Ordering::Relaxed);
    }

    // === Job tracking ===

    /// Record a job being submitted to the pipeline.
    ///
    /// Call this when a FUSE request arrives, before coalescing check.
    pub fn job_submitted(&self) {
        self.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.fuse_requests_waiting.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a job starting active processing.
    ///
    /// Call this when a job actually starts processing (after coalescing check).
    /// The job moves from "waiting" to "active" state.
    pub fn job_started(&self) {
        self.fuse_requests_waiting.fetch_sub(1, Ordering::Relaxed);
        self.jobs_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a job completing successfully.
    pub fn job_completed(&self) {
        self.jobs_completed.fetch_add(1, Ordering::Relaxed);
        self.jobs_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a job failing.
    pub fn job_failed(&self) {
        self.jobs_failed.fetch_add(1, Ordering::Relaxed);
        self.jobs_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a job being coalesced (waiting for existing work).
    pub fn job_coalesced(&self) {
        self.jobs_coalesced.fetch_add(1, Ordering::Relaxed);
    }

    // === Download tracking ===

    /// Record a download starting.
    pub fn download_started(&self) {
        self.downloads_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a successful chunk download.
    pub fn download_completed(&self, bytes: u64, duration_us: u64) {
        self.chunks_downloaded.fetch_add(1, Ordering::Relaxed);
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
        self.download_time_us
            .fetch_add(duration_us, Ordering::Relaxed);
        self.downloads_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a failed chunk download.
    pub fn download_failed(&self) {
        self.chunks_failed.fetch_add(1, Ordering::Relaxed);
        self.downloads_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a download retry.
    pub fn download_retried(&self) {
        self.chunks_retried.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a download being cancelled (balances download_started).
    ///
    /// This is called when a download is aborted due to cancellation (e.g., FUSE timeout).
    /// It decrements the active counter without counting as success or failure.
    pub fn download_cancelled(&self) {
        self.downloads_active.fetch_sub(1, Ordering::Relaxed);
    }

    // === Cache tracking ===

    /// Record a memory cache hit.
    pub fn memory_cache_hit(&self) {
        self.memory_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a memory cache miss.
    pub fn memory_cache_miss(&self) {
        self.memory_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a disk cache hit.
    pub fn disk_cache_hit(&self) {
        self.disk_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a disk cache miss.
    pub fn disk_cache_miss(&self) {
        self.disk_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Update memory cache size.
    pub fn set_memory_cache_size(&self, bytes: u64) {
        self.memory_cache_size_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Update disk cache size.
    pub fn set_disk_cache_size(&self, bytes: u64) {
        self.disk_cache_size_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Add bytes to the disk cache size counter.
    ///
    /// Use this when chunks are written to disk cache to track cumulative size.
    pub fn add_disk_cache_bytes(&self, bytes: u64) {
        self.disk_cache_size_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    // === Encode tracking ===

    /// Record an encode operation starting.
    pub fn encode_started(&self) {
        self.encodes_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an encode operation completing.
    pub fn encode_completed(&self, bytes: u64, duration_us: u64) {
        self.encodes_completed.fetch_add(1, Ordering::Relaxed);
        self.bytes_encoded.fetch_add(bytes, Ordering::Relaxed);
        self.encode_time_us
            .fetch_add(duration_us, Ordering::Relaxed);
        self.encodes_active.fetch_sub(1, Ordering::Relaxed);
    }

    // === Assembly tracking ===

    /// Record assembly time.
    pub fn assembly_completed(&self, duration_us: u64) {
        self.assembly_time_us
            .fetch_add(duration_us, Ordering::Relaxed);
    }

    // === Snapshot ===

    /// Take a point-in-time snapshot of all metrics.
    ///
    /// This is the primary interface for views and displays.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let uptime = self.start_time.elapsed();
        let uptime_secs = uptime.as_secs_f64().max(0.001); // Avoid division by zero

        let jobs_completed = self.jobs_completed.load(Ordering::Relaxed);
        let chunks_downloaded = self.chunks_downloaded.load(Ordering::Relaxed);
        let bytes_downloaded = self.bytes_downloaded.load(Ordering::Relaxed);

        let memory_cache_hits = self.memory_cache_hits.load(Ordering::Relaxed);
        let memory_cache_misses = self.memory_cache_misses.load(Ordering::Relaxed);
        let memory_cache_total = memory_cache_hits + memory_cache_misses;

        let disk_cache_hits = self.disk_cache_hits.load(Ordering::Relaxed);
        let disk_cache_misses = self.disk_cache_misses.load(Ordering::Relaxed);
        let disk_cache_total = disk_cache_hits + disk_cache_misses;

        let bytes_per_second = bytes_downloaded as f64 / uptime_secs;

        // Update peak tracking
        let current_bps_u64 = bytes_per_second as u64;
        let mut peak_bps = self.peak_bytes_per_second.load(Ordering::Relaxed);
        while current_bps_u64 > peak_bps {
            match self.peak_bytes_per_second.compare_exchange_weak(
                peak_bps,
                current_bps_u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => peak_bps = actual,
            }
        }
        let peak_bytes_per_second = self.peak_bytes_per_second.load(Ordering::Relaxed) as f64;

        TelemetrySnapshot {
            uptime,

            // FUSE metrics
            fuse_requests_active: self.fuse_requests_active.load(Ordering::Relaxed),
            fuse_requests_waiting: self.fuse_requests_waiting.load(Ordering::Relaxed),

            // Job metrics
            jobs_submitted: self.jobs_submitted.load(Ordering::Relaxed),
            jobs_completed,
            jobs_failed: self.jobs_failed.load(Ordering::Relaxed),
            jobs_active: self.jobs_active.load(Ordering::Relaxed),
            jobs_coalesced: self.jobs_coalesced.load(Ordering::Relaxed),

            // Download metrics
            chunks_downloaded,
            chunks_failed: self.chunks_failed.load(Ordering::Relaxed),
            chunks_retried: self.chunks_retried.load(Ordering::Relaxed),
            bytes_downloaded,
            downloads_active: self.downloads_active.load(Ordering::Relaxed),

            // Cache metrics
            memory_cache_hits,
            memory_cache_misses,
            memory_cache_hit_rate: if memory_cache_total > 0 {
                memory_cache_hits as f64 / memory_cache_total as f64
            } else {
                0.0
            },
            memory_cache_size_bytes: self.memory_cache_size_bytes.load(Ordering::Relaxed),
            disk_cache_hits,
            disk_cache_misses,
            disk_cache_hit_rate: if disk_cache_total > 0 {
                disk_cache_hits as f64 / disk_cache_total as f64
            } else {
                0.0
            },
            disk_cache_size_bytes: self.disk_cache_size_bytes.load(Ordering::Relaxed),

            // Encode metrics
            encodes_completed: self.encodes_completed.load(Ordering::Relaxed),
            encodes_active: self.encodes_active.load(Ordering::Relaxed),
            bytes_encoded: self.bytes_encoded.load(Ordering::Relaxed),

            // Computed rates
            jobs_per_second: jobs_completed as f64 / uptime_secs,
            chunks_per_second: chunks_downloaded as f64 / uptime_secs,
            bytes_per_second,
            peak_bytes_per_second,

            // Timing metrics (convert from microseconds)
            total_download_time_ms: self.download_time_us.load(Ordering::Relaxed) / 1000,
            total_assembly_time_ms: self.assembly_time_us.load(Ordering::Relaxed) / 1000,
            total_encode_time_ms: self.encode_time_us.load(Ordering::Relaxed) / 1000,
        }
    }

    /// Reset all counters to zero.
    ///
    /// Useful for periodic reporting or testing.
    pub fn reset(&self) {
        self.fuse_requests_active.store(0, Ordering::Relaxed);
        self.fuse_requests_waiting.store(0, Ordering::Relaxed);
        self.jobs_submitted.store(0, Ordering::Relaxed);
        self.jobs_completed.store(0, Ordering::Relaxed);
        self.jobs_failed.store(0, Ordering::Relaxed);
        self.jobs_active.store(0, Ordering::Relaxed);
        self.jobs_coalesced.store(0, Ordering::Relaxed);
        self.chunks_downloaded.store(0, Ordering::Relaxed);
        self.chunks_failed.store(0, Ordering::Relaxed);
        self.chunks_retried.store(0, Ordering::Relaxed);
        self.bytes_downloaded.store(0, Ordering::Relaxed);
        self.downloads_active.store(0, Ordering::Relaxed);
        self.memory_cache_hits.store(0, Ordering::Relaxed);
        self.memory_cache_misses.store(0, Ordering::Relaxed);
        self.memory_cache_size_bytes.store(0, Ordering::Relaxed);
        self.disk_cache_hits.store(0, Ordering::Relaxed);
        self.disk_cache_misses.store(0, Ordering::Relaxed);
        self.disk_cache_size_bytes.store(0, Ordering::Relaxed);
        self.encodes_completed.store(0, Ordering::Relaxed);
        self.encodes_active.store(0, Ordering::Relaxed);
        self.bytes_encoded.store(0, Ordering::Relaxed);
        self.download_time_us.store(0, Ordering::Relaxed);
        self.assembly_time_us.store(0, Ordering::Relaxed);
        self.encode_time_us.store(0, Ordering::Relaxed);
        self.peak_bytes_per_second.store(0, Ordering::Relaxed);
    }
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_lifecycle() {
        let metrics = PipelineMetrics::new();

        // Job submitted - now waiting
        metrics.job_submitted();
        assert_eq!(metrics.jobs_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.fuse_requests_waiting.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.jobs_active.load(Ordering::Relaxed), 0);

        // Job starts processing - moves from waiting to active
        metrics.job_started();
        assert_eq!(metrics.fuse_requests_waiting.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.jobs_active.load(Ordering::Relaxed), 1);

        // Job completes
        metrics.job_completed();
        assert_eq!(metrics.jobs_active.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.jobs_completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_job_failure() {
        let metrics = PipelineMetrics::new();

        metrics.job_submitted();
        metrics.job_started();
        metrics.job_failed();

        assert_eq!(metrics.jobs_active.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.jobs_failed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_download_tracking() {
        let metrics = PipelineMetrics::new();

        metrics.download_started();
        assert_eq!(metrics.downloads_active.load(Ordering::Relaxed), 1);

        metrics.download_completed(1024, 5000);
        assert_eq!(metrics.downloads_active.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.chunks_downloaded.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bytes_downloaded.load(Ordering::Relaxed), 1024);
    }

    #[test]
    fn test_cache_hit_rate() {
        let metrics = PipelineMetrics::new();

        // 3 hits, 1 miss = 75% hit rate
        metrics.memory_cache_hit();
        metrics.memory_cache_hit();
        metrics.memory_cache_hit();
        metrics.memory_cache_miss();

        let snapshot = metrics.snapshot();
        assert!((snapshot.memory_cache_hit_rate - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_snapshot_rates() {
        let metrics = PipelineMetrics::new();

        // Complete some work
        for _ in 0..10 {
            metrics.job_submitted();
            metrics.job_started();
            metrics.job_completed();
        }

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.jobs_completed, 10);
        assert!(snapshot.jobs_per_second > 0.0);
    }

    #[test]
    fn test_reset() {
        let metrics = PipelineMetrics::new();

        metrics.job_submitted();
        metrics.job_started();
        metrics.job_completed();
        metrics.download_started();
        metrics.download_completed(1024, 5000);

        metrics.reset();

        assert_eq!(metrics.jobs_submitted.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.jobs_completed.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.chunks_downloaded.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_coalescing_tracking() {
        let metrics = PipelineMetrics::new();

        metrics.job_coalesced();
        metrics.job_coalesced();
        metrics.job_coalesced();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.jobs_coalesced, 3);
    }
}
