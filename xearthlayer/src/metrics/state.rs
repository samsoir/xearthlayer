//! Aggregated metrics state.
//!
//! This module contains the state structures maintained by the metrics daemon.
//! The daemon owns mutable state and updates it based on incoming events.
//! Reporters read from a shared copy of this state.
//!
//! # Data Structures
//!
//! - `AggregatedState`: Counters and gauges for all metrics
//! - `TimeSeriesHistory`: Ring buffers for rate sampling (sparklines)
//! - `RingBuffer<T>`: Fixed-size circular buffer for time-series data

use std::time::Instant;

// =============================================================================
// Ring Buffer
// =============================================================================

/// A fixed-size circular buffer for time-series data.
///
/// Used to store recent samples for sparkline visualization (e.g., last 60
/// samples at 100ms intervals = 6 seconds of history).
#[derive(Clone, Debug)]
pub struct RingBuffer<T: Clone> {
    data: Vec<T>,
    capacity: usize,
    head: usize,
    len: usize,
}

impl<T: Clone + Default> RingBuffer<T> {
    /// Creates a new ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            data: vec![T::default(); capacity],
            capacity,
            head: 0,
            len: 0,
        }
    }

    /// Pushes a value into the buffer, overwriting the oldest value if full.
    pub fn push(&mut self, value: T) {
        self.data[self.head] = value;
        self.head = (self.head + 1) % self.capacity;
        if self.len < self.capacity {
            self.len += 1;
        }
    }

    /// Returns the most recently pushed value, if any.
    pub fn last(&self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let idx = if self.head == 0 {
            self.capacity - 1
        } else {
            self.head - 1
        };
        Some(self.data[idx].clone())
    }

    /// Returns the values in order from oldest to newest.
    pub fn as_slice(&self) -> Vec<T> {
        if self.len == 0 {
            return Vec::new();
        }

        let mut result = Vec::with_capacity(self.len);
        if self.len < self.capacity {
            // Buffer not full yet - data starts at index 0
            result.extend_from_slice(&self.data[..self.len]);
        } else {
            // Buffer is full - data wraps around
            result.extend_from_slice(&self.data[self.head..]);
            result.extend_from_slice(&self.data[..self.head]);
        }
        result
    }

    /// Returns the number of values in the buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clears all values from the buffer.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        for item in &mut self.data {
            *item = T::default();
        }
    }
}

// =============================================================================
// Aggregated State
// =============================================================================

/// Aggregated metrics state maintained by the daemon.
///
/// This struct contains all counters and gauges that are updated by events.
/// It's designed to be cheap to clone for snapshot generation.
#[derive(Clone, Debug)]
pub struct AggregatedState {
    /// When the metrics system started (for uptime calculation).
    pub uptime_start: Instant,

    // =========================================================================
    // Download Metrics
    // =========================================================================
    /// Currently active downloads.
    pub downloads_active: u64,
    /// Total chunks downloaded successfully.
    pub chunks_downloaded: u64,
    /// Total chunks that failed.
    pub chunks_failed: u64,
    /// Total retry attempts.
    pub chunks_retried: u64,
    /// Total bytes downloaded.
    pub bytes_downloaded: u64,
    /// Total download time in microseconds (for average calculation).
    pub download_time_us: u64,

    // =========================================================================
    // Disk Cache Metrics
    // =========================================================================
    /// Disk cache hits.
    pub disk_cache_hits: u64,
    /// Disk cache misses.
    pub disk_cache_misses: u64,
    /// Active disk writes.
    pub disk_writes_active: u64,
    /// Total bytes written to disk cache.
    pub disk_bytes_written: u64,
    /// Total bytes read from disk cache (cache hits).
    pub disk_bytes_read: u64,
    /// Total disk write time in microseconds.
    pub disk_write_time_us: u64,
    /// Initial disk cache size (scanned on startup, not reset).
    pub initial_disk_cache_bytes: u64,
    /// Total bytes evicted from disk cache by the GC daemon.
    pub disk_bytes_evicted: u64,
    /// Current disk cache size in bytes (absolute value from LRU index).
    ///
    /// Updated directly via `DiskCacheSizeUpdate` events from the
    /// `DiskCacheProvider` after writes and evictions. This is the
    /// authoritative value, replacing the fragile formula
    /// `initial + written - evicted`.
    pub disk_cache_size_bytes: u64,

    // =========================================================================
    // Memory Cache Metrics
    // =========================================================================
    /// Memory cache hits.
    pub memory_cache_hits: u64,
    /// Memory cache misses.
    pub memory_cache_misses: u64,
    /// Current memory cache size in bytes.
    pub memory_cache_size_bytes: u64,

    // =========================================================================
    // Job Metrics
    // =========================================================================
    /// Total jobs submitted.
    pub jobs_submitted: u64,
    /// Jobs submitted from FUSE (X-Plane requests).
    pub fuse_jobs_submitted: u64,
    /// Jobs completed successfully.
    pub jobs_completed: u64,
    /// Jobs that failed.
    pub jobs_failed: u64,
    /// Jobs that timed out.
    pub jobs_timed_out: u64,
    /// Currently active jobs.
    pub jobs_active: u64,
    /// Jobs that were coalesced.
    pub jobs_coalesced: u64,

    // =========================================================================
    // Encode Metrics
    // =========================================================================
    /// Currently active encode operations.
    pub encodes_active: u64,
    /// Total encode operations completed.
    pub encodes_completed: u64,
    /// Total bytes encoded (DDS output).
    pub bytes_encoded: u64,
    /// Total encode time in microseconds.
    pub encode_time_us: u64,

    // =========================================================================
    // Assembly Metrics
    // =========================================================================
    /// Total assembly time in microseconds.
    pub assembly_time_us: u64,

    // =========================================================================
    // FUSE Metrics
    // =========================================================================
    /// Currently active FUSE requests.
    pub fuse_requests_active: u64,
    /// FUSE requests waiting in queue.
    pub fuse_requests_waiting: u64,

    // =========================================================================
    // Peak Tracking
    // =========================================================================
    /// Peak bytes per second observed.
    pub peak_bytes_per_second: f64,
}

impl Default for AggregatedState {
    fn default() -> Self {
        Self::new()
    }
}

impl AggregatedState {
    /// Creates a new aggregated state with all counters at zero.
    pub fn new() -> Self {
        Self {
            uptime_start: Instant::now(),
            downloads_active: 0,
            chunks_downloaded: 0,
            chunks_failed: 0,
            chunks_retried: 0,
            bytes_downloaded: 0,
            download_time_us: 0,
            disk_cache_hits: 0,
            disk_cache_misses: 0,
            disk_writes_active: 0,
            disk_bytes_written: 0,
            disk_bytes_read: 0,
            disk_write_time_us: 0,
            initial_disk_cache_bytes: 0,
            disk_bytes_evicted: 0,
            disk_cache_size_bytes: 0,
            memory_cache_hits: 0,
            memory_cache_misses: 0,
            memory_cache_size_bytes: 0,
            jobs_submitted: 0,
            fuse_jobs_submitted: 0,
            jobs_completed: 0,
            jobs_failed: 0,
            jobs_timed_out: 0,
            jobs_active: 0,
            jobs_coalesced: 0,
            encodes_active: 0,
            encodes_completed: 0,
            bytes_encoded: 0,
            encode_time_us: 0,
            assembly_time_us: 0,
            fuse_requests_active: 0,
            fuse_requests_waiting: 0,
            peak_bytes_per_second: 0.0,
        }
    }

    /// Returns the uptime duration.
    pub fn uptime(&self) -> std::time::Duration {
        self.uptime_start.elapsed()
    }

    /// Resets all counters to zero.
    pub fn reset(&mut self) {
        self.uptime_start = Instant::now();
        self.downloads_active = 0;
        self.chunks_downloaded = 0;
        self.chunks_failed = 0;
        self.chunks_retried = 0;
        self.bytes_downloaded = 0;
        self.download_time_us = 0;
        self.disk_cache_hits = 0;
        self.disk_cache_misses = 0;
        self.disk_writes_active = 0;
        self.disk_bytes_written = 0;
        self.disk_bytes_read = 0;
        self.disk_write_time_us = 0;
        self.memory_cache_hits = 0;
        self.memory_cache_misses = 0;
        self.memory_cache_size_bytes = 0;
        self.jobs_submitted = 0;
        self.fuse_jobs_submitted = 0;
        self.jobs_completed = 0;
        self.jobs_failed = 0;
        self.jobs_timed_out = 0;
        self.jobs_active = 0;
        self.jobs_coalesced = 0;
        self.encodes_active = 0;
        self.encodes_completed = 0;
        self.bytes_encoded = 0;
        self.encode_time_us = 0;
        self.assembly_time_us = 0;
        self.fuse_requests_active = 0;
        self.fuse_requests_waiting = 0;
        self.peak_bytes_per_second = 0.0;
    }
}

// =============================================================================
// Time Series History
// =============================================================================

/// Default number of samples to keep (60 samples × 100ms = 6 seconds).
pub const DEFAULT_HISTORY_CAPACITY: usize = 60;

/// Time series history for rate visualization (sparklines).
///
/// Contains ring buffers that are sampled at regular intervals by the daemon.
#[derive(Clone, Debug)]
pub struct TimeSeriesHistory {
    /// Network throughput samples (bytes/sec).
    pub network_throughput: RingBuffer<f64>,
    /// Disk throughput samples (bytes/sec).
    pub disk_throughput: RingBuffer<f64>,
    /// Job completion rate samples (jobs/sec).
    pub job_rate: RingBuffer<f64>,
    /// FUSE request rate samples (requests/sec).
    pub fuse_rate: RingBuffer<f64>,
}

impl TimeSeriesHistory {
    /// Creates a new time series history with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            network_throughput: RingBuffer::new(capacity),
            disk_throughput: RingBuffer::new(capacity),
            job_rate: RingBuffer::new(capacity),
            fuse_rate: RingBuffer::new(capacity),
        }
    }

    /// Clears all time series data.
    pub fn clear(&mut self) {
        self.network_throughput.clear();
        self.disk_throughput.clear();
        self.job_rate.clear();
        self.fuse_rate.clear();
    }
}

impl Default for TimeSeriesHistory {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_CAPACITY)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let mut buf: RingBuffer<i32> = RingBuffer::new(3);

        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.last(), None);

        buf.push(1);
        buf.push(2);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.last(), Some(2));
        assert_eq!(buf.as_slice(), vec![1, 2]);
    }

    #[test]
    fn test_ring_buffer_overflow() {
        let mut buf: RingBuffer<i32> = RingBuffer::new(3);

        buf.push(1);
        buf.push(2);
        buf.push(3);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.as_slice(), vec![1, 2, 3]);

        // Push one more - should overwrite oldest (1)
        buf.push(4);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.as_slice(), vec![2, 3, 4]);
        assert_eq!(buf.last(), Some(4));

        // Push another
        buf.push(5);
        assert_eq!(buf.as_slice(), vec![3, 4, 5]);
    }

    #[test]
    fn test_ring_buffer_clear() {
        let mut buf: RingBuffer<i32> = RingBuffer::new(3);
        buf.push(1);
        buf.push(2);

        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.last(), None);
        assert!(buf.as_slice().is_empty());
    }

    #[test]
    fn test_aggregated_state_default() {
        let state = AggregatedState::default();
        assert_eq!(state.chunks_downloaded, 0);
        assert_eq!(state.jobs_submitted, 0);
        assert!(state.uptime().as_secs() < 1);
    }

    #[test]
    fn test_aggregated_state_reset() {
        let mut state = AggregatedState::default();
        state.chunks_downloaded = 100;
        state.bytes_downloaded = 1_000_000;
        state.jobs_completed = 50;

        state.reset();
        assert_eq!(state.chunks_downloaded, 0);
        assert_eq!(state.bytes_downloaded, 0);
        assert_eq!(state.jobs_completed, 0);
    }

    #[test]
    fn test_time_series_history() {
        let mut history = TimeSeriesHistory::new(10);

        history.network_throughput.push(1000.0);
        history.network_throughput.push(2000.0);
        history.disk_throughput.push(500.0);

        assert_eq!(history.network_throughput.len(), 2);
        assert_eq!(history.network_throughput.last(), Some(2000.0));
        assert_eq!(history.disk_throughput.len(), 1);

        history.clear();
        assert!(history.network_throughput.is_empty());
        assert!(history.disk_throughput.is_empty());
    }
}
