//! Metrics reporting layer.
//!
//! This module provides the trait and implementations for transforming
//! aggregated metrics state into presentation formats.
//!
//! # Architecture
//!
//! Reporters read from the shared state handle and produce output in various
//! formats. The trait allows for multiple reporter implementations:
//!
//! - [`TuiReporter`]: Transforms state into [`TelemetrySnapshot`] for the TUI
//! - Future: `PrometheusReporter`, `JsonReporter`, etc.
//!
//! # Example
//!
//! ```ignore
//! let reporter = TuiReporter::new();
//! let snapshot = reporter.report(&state, &history);
//! ```

use super::daemon::MetricsStateSnapshot;
use super::snapshot::TelemetrySnapshot;
use super::state::{AggregatedState, TimeSeriesHistory};

// =============================================================================
// Reporter Trait
// =============================================================================

/// Trait for metrics reporters.
///
/// Reporters transform raw aggregated state into presentation formats.
/// Each reporter produces a specific output type tailored to its use case.
pub trait MetricsReporter {
    /// The output type produced by this reporter.
    type Output;

    /// Transforms aggregated state into the output format.
    ///
    /// # Arguments
    ///
    /// * `state` - The aggregated counters and gauges
    /// * `history` - The time-series history for sparklines
    fn report(&self, state: &AggregatedState, history: &TimeSeriesHistory) -> Self::Output;

    /// Convenience method to report from a snapshot.
    fn report_snapshot(&self, snapshot: &MetricsStateSnapshot) -> Self::Output {
        self.report(&snapshot.state, &snapshot.history)
    }
}

// =============================================================================
// TUI Reporter
// =============================================================================

/// Reporter that produces [`TelemetrySnapshot`] for the TUI dashboard.
///
/// This is the primary reporter used by the CLI to display real-time metrics.
/// It transforms the raw state into the format expected by the TUI components.
#[derive(Clone, Debug, Default)]
pub struct TuiReporter {
    // Configuration fields can be added here if needed
    // e.g., memory_cache_max, disk_cache_max for percentage calculations
}

impl TuiReporter {
    /// Creates a new TUI reporter.
    pub fn new() -> Self {
        Self {}
    }
}

impl MetricsReporter for TuiReporter {
    type Output = TelemetrySnapshot;

    fn report(&self, state: &AggregatedState, history: &TimeSeriesHistory) -> TelemetrySnapshot {
        let uptime = state.uptime();
        let uptime_secs = uptime.as_secs_f64().max(0.001);

        // Calculate cache hit rates
        let memory_total = state.memory_cache_hits + state.memory_cache_misses;
        let memory_hit_rate = if memory_total > 0 {
            state.memory_cache_hits as f64 / memory_total as f64
        } else {
            0.0
        };

        let disk_total = state.disk_cache_hits + state.disk_cache_misses;
        let disk_hit_rate = if disk_total > 0 {
            state.disk_cache_hits as f64 / disk_total as f64
        } else {
            0.0
        };

        // Get latest rates from history, or calculate from totals
        let bytes_per_second = history
            .network_throughput
            .last()
            .unwrap_or(state.bytes_downloaded as f64 / uptime_secs);

        let jobs_per_second = history
            .job_rate
            .last()
            .unwrap_or(state.jobs_completed as f64 / uptime_secs);

        let chunks_per_second = if uptime_secs > 0.0 {
            state.chunks_downloaded as f64 / uptime_secs
        } else {
            0.0
        };

        let fuse_jobs_per_second = if uptime_secs > 0.0 {
            state.fuse_jobs_submitted as f64 / uptime_secs
        } else {
            0.0
        };

        TelemetrySnapshot {
            uptime,

            // FUSE metrics
            fuse_requests_active: state.fuse_requests_active as usize,
            fuse_requests_waiting: state.fuse_requests_waiting as usize,

            // Job metrics
            jobs_submitted: state.jobs_submitted,
            fuse_jobs_submitted: state.fuse_jobs_submitted,
            jobs_completed: state.jobs_completed,
            jobs_failed: state.jobs_failed,
            jobs_timed_out: state.jobs_timed_out,
            jobs_active: state.jobs_active as usize,
            jobs_coalesced: state.jobs_coalesced,

            // Download metrics
            chunks_downloaded: state.chunks_downloaded,
            chunks_failed: state.chunks_failed,
            chunks_retried: state.chunks_retried,
            bytes_downloaded: state.bytes_downloaded,
            downloads_active: state.downloads_active as usize,

            // Cache metrics
            memory_cache_hits: state.memory_cache_hits,
            memory_cache_misses: state.memory_cache_misses,
            memory_cache_hit_rate: memory_hit_rate,
            memory_cache_size_bytes: state.memory_cache_size_bytes,
            disk_cache_hits: state.disk_cache_hits,
            disk_cache_misses: state.disk_cache_misses,
            disk_cache_hit_rate: disk_hit_rate,
            disk_cache_size_bytes: state.disk_cache_size_bytes,
            disk_bytes_written: state.disk_bytes_written,
            disk_bytes_read: state.disk_bytes_read,

            // Encode metrics
            encodes_completed: state.encodes_completed,
            encodes_active: state.encodes_active as usize,
            bytes_encoded: state.bytes_encoded,

            // Computed rates
            jobs_per_second,
            fuse_jobs_per_second,
            chunks_per_second,
            bytes_per_second,
            peak_bytes_per_second: state.peak_bytes_per_second,

            // Timing totals (convert from microseconds to milliseconds)
            total_download_time_ms: state.download_time_us / 1000,
            total_assembly_time_ms: state.assembly_time_us / 1000,
            total_encode_time_ms: state.encode_time_us / 1000,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn create_test_state() -> AggregatedState {
        let mut state = AggregatedState::new();
        state.chunks_downloaded = 1000;
        state.chunks_failed = 10;
        state.chunks_retried = 25;
        state.bytes_downloaded = 50_000_000;

        state.memory_cache_hits = 30;
        state.memory_cache_misses = 70;
        state.disk_cache_hits = 200;
        state.disk_cache_misses = 800;

        state.jobs_submitted = 100;
        state.fuse_jobs_submitted = 80;
        state.jobs_completed = 90;
        state.jobs_failed = 5;
        state.jobs_timed_out = 3;
        state.jobs_active = 2;
        state.jobs_coalesced = 10;

        state.encodes_completed = 90;
        state.bytes_encoded = 900_000_000;

        state.download_time_us = 120_000_000; // 120 seconds
        state.assembly_time_us = 15_000_000; // 15 seconds
        state.encode_time_us = 45_000_000; // 45 seconds

        state
    }

    #[test]
    fn test_tui_reporter_basic() {
        let state = create_test_state();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        let snapshot = reporter.report(&state, &history);

        assert_eq!(snapshot.chunks_downloaded, 1000);
        assert_eq!(snapshot.chunks_failed, 10);
        assert_eq!(snapshot.jobs_completed, 90);
        assert_eq!(snapshot.encodes_completed, 90);
    }

    #[test]
    fn test_cache_hit_rate_calculation() {
        let state = create_test_state();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        let snapshot = reporter.report(&state, &history);

        // Memory: 30 / (30 + 70) = 0.3
        assert!((snapshot.memory_cache_hit_rate - 0.3).abs() < 0.001);

        // Disk: 200 / (200 + 800) = 0.2
        assert!((snapshot.disk_cache_hit_rate - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_cache_hit_rate_zero_total() {
        let state = AggregatedState::new();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        let snapshot = reporter.report(&state, &history);

        // Should be 0.0, not NaN
        assert_eq!(snapshot.memory_cache_hit_rate, 0.0);
        assert_eq!(snapshot.disk_cache_hit_rate, 0.0);
    }

    #[test]
    fn test_timing_conversion() {
        let state = create_test_state();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        let snapshot = reporter.report(&state, &history);

        // Microseconds to milliseconds
        assert_eq!(snapshot.total_download_time_ms, 120_000);
        assert_eq!(snapshot.total_assembly_time_ms, 15_000);
        assert_eq!(snapshot.total_encode_time_ms, 45_000);
    }

    #[test]
    fn test_rates_from_history() {
        let state = create_test_state();
        let mut history = TimeSeriesHistory::new(10);

        // Push some rate samples
        history.network_throughput.push(1_000_000.0); // 1 MB/s
        history.job_rate.push(5.0); // 5 jobs/sec

        let reporter = TuiReporter::new();
        let snapshot = reporter.report(&state, &history);

        // Should use the latest history values
        assert!((snapshot.bytes_per_second - 1_000_000.0).abs() < 0.001);
        assert!((snapshot.jobs_per_second - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_report_snapshot_convenience() {
        let state = create_test_state();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        let metrics_snapshot = MetricsStateSnapshot { state, history };

        let result = reporter.report_snapshot(&metrics_snapshot);
        assert_eq!(result.chunks_downloaded, 1000);
    }

    #[test]
    fn test_snapshot_uptime() {
        let state = AggregatedState::new();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        std::thread::sleep(Duration::from_millis(10));

        let snapshot = reporter.report(&state, &history);
        assert!(snapshot.uptime >= Duration::from_millis(10));
    }

    #[test]
    fn test_disk_cache_size_uses_absolute_value() {
        let mut state = AggregatedState::new();
        let history = TimeSeriesHistory::default();
        let reporter = TuiReporter::new();

        // Set the authoritative disk cache size (from LRU index)
        state.disk_cache_size_bytes = 5_000_000_000;

        // These legacy fields should NOT affect the reported size
        state.initial_disk_cache_bytes = 1_000_000_000;
        state.disk_bytes_written = 10_000_000_000;
        state.disk_bytes_evicted = 500_000_000;

        let snapshot = reporter.report(&state, &history);

        // Should use the absolute value, not the formula
        assert_eq!(
            snapshot.disk_cache_size_bytes, 5_000_000_000,
            "Reporter should use disk_cache_size_bytes directly, not initial+written-evicted"
        );
    }
}
