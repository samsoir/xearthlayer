//! Session-wide network statistics tracking.
//!
//! Provides thread-safe accumulation of download metrics across all
//! tile downloads during a session.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Instant;

/// Thread-safe session-wide network statistics.
///
/// Tracks cumulative download metrics that can be safely updated from
/// multiple download threads and read for periodic logging.
///
/// # Example
///
/// ```
/// use xearthlayer::orchestrator::NetworkStats;
///
/// let stats = NetworkStats::new();
///
/// // Record a successful download
/// stats.record_chunk_success(1024);
///
/// // Record a failure
/// stats.record_chunk_failure();
///
/// // Record a retry
/// stats.record_retry();
///
/// // Get current stats for logging
/// let snapshot = stats.snapshot();
/// println!("Downloaded: {} bytes", snapshot.bytes_downloaded);
/// ```
pub struct NetworkStats {
    /// Total bytes downloaded
    bytes_downloaded: AtomicU64,
    /// Number of chunks successfully downloaded
    chunks_downloaded: AtomicU64,
    /// Number of chunk download failures
    chunks_failed: AtomicU64,
    /// Number of retry attempts
    retries: AtomicU64,
    /// Active download time tracking and peak speed
    activity_tracker: RwLock<ActivityTracker>,
}

/// Tracks active download time and peak speed.
///
/// Active time is accumulated only while downloads are occurring, not during
/// idle periods. This gives a more accurate average speed calculation.
struct ActivityTracker {
    /// Peak speed in bytes per second
    peak_bytes_per_sec: f64,
    /// Bytes at last sample (for peak speed calculation)
    last_sample_bytes: u64,
    /// Time of last sample (for peak speed calculation)
    last_sample_time: Instant,
    /// Total accumulated active download time in seconds
    active_time_secs: f64,
    /// Time of last activity (for detecting idle gaps)
    last_activity_time: Option<Instant>,
    /// Idle threshold - if no activity for this duration, stop counting time
    idle_threshold_secs: f64,
}

impl ActivityTracker {
    fn new(now: Instant) -> Self {
        Self {
            peak_bytes_per_sec: 0.0,
            last_sample_bytes: 0,
            last_sample_time: now,
            active_time_secs: 0.0,
            last_activity_time: None,
            idle_threshold_secs: 2.0, // Consider idle after 2 seconds of no activity
        }
    }

    /// Record activity and update peak speed.
    fn record_activity(&mut self, current_bytes: u64, now: Instant) {
        // Update active time tracking
        if let Some(last_time) = self.last_activity_time {
            let elapsed = now.duration_since(last_time).as_secs_f64();
            // Only count time if within idle threshold
            if elapsed <= self.idle_threshold_secs {
                self.active_time_secs += elapsed;
            }
        }
        self.last_activity_time = Some(now);

        // Update peak speed calculation
        let elapsed = now.duration_since(self.last_sample_time).as_secs_f64();

        // Only calculate if enough time has passed (avoid division by tiny numbers)
        if elapsed >= 0.5 {
            let bytes_delta = current_bytes.saturating_sub(self.last_sample_bytes);
            let speed = bytes_delta as f64 / elapsed;

            if speed > self.peak_bytes_per_sec {
                self.peak_bytes_per_sec = speed;
            }

            self.last_sample_bytes = current_bytes;
            self.last_sample_time = now;
        }
    }

    /// Get the total active download time in seconds.
    fn active_time(&self) -> f64 {
        self.active_time_secs
    }

    /// Get the peak speed in bytes per second.
    fn peak_speed(&self) -> f64 {
        self.peak_bytes_per_sec
    }
}

/// Snapshot of network statistics at a point in time.
#[derive(Debug, Clone)]
pub struct NetworkStatsSnapshot {
    /// Total bytes downloaded
    pub bytes_downloaded: u64,
    /// Number of chunks successfully downloaded
    pub chunks_downloaded: u64,
    /// Number of chunk download failures
    pub chunks_failed: u64,
    /// Number of retry attempts
    pub retries: u64,
    /// Active download time in seconds (excludes idle periods)
    pub active_time_secs: f64,
    /// Average download speed in bytes per second (based on active time only)
    pub avg_bytes_per_sec: f64,
    /// Peak download speed in bytes per second
    pub peak_bytes_per_sec: f64,
}

impl NetworkStats {
    /// Create a new network stats tracker.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            bytes_downloaded: AtomicU64::new(0),
            chunks_downloaded: AtomicU64::new(0),
            chunks_failed: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            activity_tracker: RwLock::new(ActivityTracker::new(now)),
        }
    }

    /// Record a successful chunk download.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes downloaded
    pub fn record_chunk_success(&self, bytes: usize) {
        self.bytes_downloaded
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.chunks_downloaded.fetch_add(1, Ordering::Relaxed);

        // Update activity tracker
        if let Ok(mut tracker) = self.activity_tracker.write() {
            let total_bytes = self.bytes_downloaded.load(Ordering::Relaxed);
            tracker.record_activity(total_bytes, Instant::now());
        }
    }

    /// Record a chunk download failure.
    pub fn record_chunk_failure(&self) {
        self.chunks_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a retry attempt.
    pub fn record_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a snapshot of current statistics.
    pub fn snapshot(&self) -> NetworkStatsSnapshot {
        let bytes_downloaded = self.bytes_downloaded.load(Ordering::Relaxed);

        let (active_time_secs, peak_bytes_per_sec) = self
            .activity_tracker
            .read()
            .map(|t| (t.active_time(), t.peak_speed()))
            .unwrap_or((0.0, 0.0));

        let avg_bytes_per_sec = if active_time_secs > 0.0 {
            bytes_downloaded as f64 / active_time_secs
        } else {
            0.0
        };

        NetworkStatsSnapshot {
            bytes_downloaded,
            chunks_downloaded: self.chunks_downloaded.load(Ordering::Relaxed),
            chunks_failed: self.chunks_failed.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            active_time_secs,
            avg_bytes_per_sec,
            peak_bytes_per_sec,
        }
    }
}

impl Default for NetworkStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_new_stats_are_zero() {
        let stats = NetworkStats::new();
        let snapshot = stats.snapshot();

        assert_eq!(snapshot.bytes_downloaded, 0);
        assert_eq!(snapshot.chunks_downloaded, 0);
        assert_eq!(snapshot.chunks_failed, 0);
        assert_eq!(snapshot.retries, 0);
    }

    #[test]
    fn test_record_chunk_success() {
        let stats = NetworkStats::new();

        stats.record_chunk_success(1024);
        stats.record_chunk_success(2048);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.bytes_downloaded, 3072);
        assert_eq!(snapshot.chunks_downloaded, 2);
    }

    #[test]
    fn test_record_chunk_failure() {
        let stats = NetworkStats::new();

        stats.record_chunk_failure();
        stats.record_chunk_failure();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.chunks_failed, 2);
    }

    #[test]
    fn test_record_retry() {
        let stats = NetworkStats::new();

        stats.record_retry();
        stats.record_retry();
        stats.record_retry();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.retries, 3);
    }

    #[test]
    fn test_average_speed_calculation() {
        let stats = NetworkStats::new();

        // Download some bytes with a small delay between chunks
        stats.record_chunk_success(500_000);
        thread::sleep(Duration::from_millis(100));
        stats.record_chunk_success(500_000);

        let snapshot = stats.snapshot();
        // Active time should be approximately 0.1 seconds (the gap between downloads)
        assert!(snapshot.active_time_secs >= 0.05);
        assert!(snapshot.active_time_secs < 1.0); // Should not include startup time
        assert!(snapshot.avg_bytes_per_sec > 0.0);
        // Average speed should be high since we're only counting active time
        // 1MB / ~0.1s = ~10 MB/s
        assert!(snapshot.avg_bytes_per_sec > 1_000_000.0);
    }

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;

        let stats = Arc::new(NetworkStats::new());
        let mut handles = vec![];

        // Spawn multiple threads recording stats
        for _ in 0..10 {
            let stats_clone = Arc::clone(&stats);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    stats_clone.record_chunk_success(100);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.chunks_downloaded, 1000);
        assert_eq!(snapshot.bytes_downloaded, 100_000);
    }

    #[test]
    fn test_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NetworkStats>();
    }
}
