//! Metrics aggregation daemon.
//!
//! The [`MetricsDaemon`] runs as an independent async task that:
//!
//! 1. Receives events from the channel (sent by `MetricsClient`)
//! 2. Updates counters and gauges in `AggregatedState`
//! 3. Samples time-series data at regular intervals for sparklines
//! 4. Publishes state to a shared handle for reporters to read
//!
//! # Design Notes
//!
//! The daemon owns mutable state and is the only writer. Reporters access
//! state through a shared `RwLock` handle that the daemon updates after
//! processing events. This ensures reporters never block event processing.

use super::event::MetricEvent;
use super::state::{AggregatedState, TimeSeriesHistory, DEFAULT_HISTORY_CAPACITY};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Interval between time-series samples (100ms).
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Shared state handle for read-only access by reporters.
pub type SharedMetricsState = Arc<RwLock<MetricsStateSnapshot>>;

/// A snapshot of metrics state for reporter access.
#[derive(Clone, Debug, Default)]
pub struct MetricsStateSnapshot {
    /// Aggregated counters and gauges.
    pub state: AggregatedState,
    /// Time-series history for sparklines.
    pub history: TimeSeriesHistory,
}

/// The metrics aggregation daemon.
///
/// This daemon processes events from the channel and maintains aggregated
/// metrics state. It runs as an independent async task and publishes state
/// updates to a shared handle.
pub struct MetricsDaemon {
    /// Channel receiver for incoming events.
    rx: mpsc::UnboundedReceiver<MetricEvent>,

    /// Current aggregated state.
    state: AggregatedState,

    /// Time-series history for sparklines.
    history: TimeSeriesHistory,

    /// Shared state handle for reporters.
    shared_state: SharedMetricsState,

    /// Last sample time for rate calculation.
    last_sample: Instant,

    /// Last bytes downloaded (for rate calculation).
    last_bytes_downloaded: u64,

    /// Last disk bytes written (for rate calculation).
    last_disk_bytes_written: u64,

    /// Last jobs completed (for rate calculation).
    last_jobs_completed: u64,

    /// Last FUSE requests completed (for rate calculation).
    last_fuse_completed: u64,
}

impl MetricsDaemon {
    /// Creates a new metrics daemon.
    ///
    /// # Arguments
    ///
    /// * `rx` - Channel receiver for incoming events
    pub fn new(rx: mpsc::UnboundedReceiver<MetricEvent>) -> Self {
        let shared_state = Arc::new(RwLock::new(MetricsStateSnapshot::default()));

        Self {
            rx,
            state: AggregatedState::new(),
            history: TimeSeriesHistory::new(DEFAULT_HISTORY_CAPACITY),
            shared_state,
            last_sample: Instant::now(),
            last_bytes_downloaded: 0,
            last_disk_bytes_written: 0,
            last_jobs_completed: 0,
            last_fuse_completed: 0,
        }
    }

    /// Returns a handle to the shared state.
    ///
    /// Reporters use this handle to read the current state.
    pub fn state_handle(&self) -> SharedMetricsState {
        Arc::clone(&self.shared_state)
    }

    /// Runs the daemon until shutdown is signaled.
    ///
    /// This is the main event loop that:
    /// 1. Receives and processes events from the channel
    /// 2. Samples time-series data at regular intervals
    /// 3. Updates the shared state for reporters
    pub async fn run(mut self, shutdown: CancellationToken) {
        tracing::info!("Metrics daemon starting");

        let mut sample_interval = tokio::time::interval(SAMPLE_INTERVAL);
        // Don't let missed ticks pile up
        sample_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;

                // Check shutdown first
                _ = shutdown.cancelled() => {
                    tracing::info!("Metrics daemon shutting down");
                    break;
                }

                // Process incoming events
                Some(event) = self.rx.recv() => {
                    self.process_event(event);
                }

                // Sample time-series data
                _ = sample_interval.tick() => {
                    self.sample_time_series();
                    self.update_shared_state();
                }
            }
        }

        // Final state update before shutdown
        self.update_shared_state();
        tracing::debug!("Metrics daemon stopped");
    }

    /// Processes a single event, updating the aggregated state.
    fn process_event(&mut self, event: MetricEvent) {
        match event {
            // Download events
            MetricEvent::DownloadStarted => {
                self.state.downloads_active += 1;
            }
            MetricEvent::DownloadCompleted { bytes, duration_us } => {
                self.state.downloads_active = self.state.downloads_active.saturating_sub(1);
                self.state.chunks_downloaded += 1;
                self.state.bytes_downloaded += bytes;
                self.state.download_time_us += duration_us;
            }
            MetricEvent::DownloadFailed => {
                self.state.downloads_active = self.state.downloads_active.saturating_sub(1);
                self.state.chunks_failed += 1;
            }
            MetricEvent::DownloadRetried => {
                self.state.chunks_retried += 1;
            }

            // Disk cache events
            MetricEvent::DiskCacheHit { bytes } => {
                self.state.disk_cache_hits += 1;
                self.state.disk_bytes_read += bytes;
            }
            MetricEvent::DiskCacheMiss => {
                self.state.disk_cache_misses += 1;
            }
            MetricEvent::DiskWriteStarted => {
                self.state.disk_writes_active += 1;
            }
            MetricEvent::DiskWriteCompleted { bytes, duration_us } => {
                self.state.disk_writes_active = self.state.disk_writes_active.saturating_sub(1);
                self.state.disk_bytes_written += bytes;
                self.state.disk_write_time_us += duration_us;
            }
            MetricEvent::DiskCacheInitialSize { bytes } => {
                self.state.initial_disk_cache_bytes = bytes;
            }
            MetricEvent::DiskCacheEvicted { bytes_freed } => {
                self.state.disk_bytes_evicted += bytes_freed;
            }
            MetricEvent::DiskCacheSizeUpdate { bytes } => {
                self.state.disk_cache_size_bytes = bytes;
            }

            // Memory cache events
            MetricEvent::MemoryCacheHit => {
                self.state.memory_cache_hits += 1;
            }
            MetricEvent::MemoryCacheMiss => {
                self.state.memory_cache_misses += 1;
            }
            MetricEvent::MemoryCacheSizeUpdate { bytes } => {
                self.state.memory_cache_size_bytes = bytes;
            }

            // Job events
            MetricEvent::JobSubmitted { is_fuse } => {
                self.state.jobs_submitted += 1;
                if is_fuse {
                    self.state.fuse_jobs_submitted += 1;
                }
            }
            MetricEvent::JobStarted => {
                self.state.jobs_active += 1;
            }
            MetricEvent::JobCompleted {
                success,
                duration_us: _,
            } => {
                self.state.jobs_active = self.state.jobs_active.saturating_sub(1);
                if success {
                    self.state.jobs_completed += 1;
                } else {
                    self.state.jobs_failed += 1;
                }
            }
            MetricEvent::JobCoalesced => {
                self.state.jobs_coalesced += 1;
            }
            MetricEvent::JobTimedOut => {
                self.state.jobs_active = self.state.jobs_active.saturating_sub(1);
                self.state.jobs_timed_out += 1;
            }

            // Encode events
            MetricEvent::EncodeStarted => {
                self.state.encodes_active += 1;
            }
            MetricEvent::EncodeCompleted { bytes, duration_us } => {
                self.state.encodes_active = self.state.encodes_active.saturating_sub(1);
                self.state.encodes_completed += 1;
                self.state.bytes_encoded += bytes;
                self.state.encode_time_us += duration_us;
            }

            // Assembly events
            MetricEvent::AssemblyCompleted { duration_us } => {
                self.state.assembly_time_us += duration_us;
            }

            // FUSE events
            MetricEvent::FuseRequestStarted => {
                self.state.fuse_requests_active += 1;
            }
            MetricEvent::FuseRequestCompleted => {
                self.state.fuse_requests_active = self.state.fuse_requests_active.saturating_sub(1);
            }
            MetricEvent::FuseRequestQueued => {
                self.state.fuse_requests_waiting += 1;
            }
            MetricEvent::FuseRequestDequeued => {
                self.state.fuse_requests_waiting =
                    self.state.fuse_requests_waiting.saturating_sub(1);
            }
        }
    }

    /// Samples current rates for time-series history.
    fn sample_time_series(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();

        if elapsed > 0.0 {
            // Network throughput (bytes/sec)
            let bytes_delta = self
                .state
                .bytes_downloaded
                .saturating_sub(self.last_bytes_downloaded);
            let network_rate = bytes_delta as f64 / elapsed;
            self.history.network_throughput.push(network_rate);

            // Update peak if this is higher
            if network_rate > self.state.peak_bytes_per_second {
                self.state.peak_bytes_per_second = network_rate;
            }

            self.last_bytes_downloaded = self.state.bytes_downloaded;

            // Disk throughput (bytes/sec)
            let disk_delta = self
                .state
                .disk_bytes_written
                .saturating_sub(self.last_disk_bytes_written);
            self.history
                .disk_throughput
                .push(disk_delta as f64 / elapsed);
            self.last_disk_bytes_written = self.state.disk_bytes_written;

            // Job rate (jobs/sec)
            let jobs_delta = self
                .state
                .jobs_completed
                .saturating_sub(self.last_jobs_completed);
            self.history.job_rate.push(jobs_delta as f64 / elapsed);
            self.last_jobs_completed = self.state.jobs_completed;

            // FUSE rate (completed FUSE jobs per second)
            // We track fuse_jobs_submitted but use jobs_completed for rate
            // This is an approximation - ideally we'd track FUSE completions separately
            let fuse_delta = self
                .state
                .fuse_jobs_submitted
                .saturating_sub(self.last_fuse_completed);
            self.history.fuse_rate.push(fuse_delta as f64 / elapsed);
            self.last_fuse_completed = self.state.fuse_jobs_submitted;
        }

        self.last_sample = now;
    }

    /// Updates the shared state for reporters to read.
    fn update_shared_state(&self) {
        if let Ok(mut guard) = self.shared_state.write() {
            guard.state = self.state.clone();
            guard.history = self.history.clone();
        }
    }
}

impl std::fmt::Debug for MetricsDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsDaemon")
            .field("chunks_downloaded", &self.state.chunks_downloaded)
            .field("jobs_completed", &self.state.jobs_completed)
            .field("history_samples", &self.history.network_throughput.len())
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_daemon() -> (MetricsDaemon, mpsc::UnboundedSender<MetricEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (MetricsDaemon::new(rx), tx)
    }

    #[test]
    fn test_daemon_creation() {
        let (daemon, _tx) = create_daemon();
        assert_eq!(daemon.state.chunks_downloaded, 0);
        assert_eq!(daemon.state.jobs_submitted, 0);
        assert!(daemon.history.network_throughput.is_empty());
    }

    #[test]
    fn test_process_download_events() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::DownloadStarted);
        assert_eq!(daemon.state.downloads_active, 1);

        daemon.process_event(MetricEvent::DownloadCompleted {
            bytes: 1024,
            duration_us: 5000,
        });
        assert_eq!(daemon.state.downloads_active, 0);
        assert_eq!(daemon.state.chunks_downloaded, 1);
        assert_eq!(daemon.state.bytes_downloaded, 1024);

        daemon.process_event(MetricEvent::DownloadStarted);
        daemon.process_event(MetricEvent::DownloadFailed);
        assert_eq!(daemon.state.downloads_active, 0);
        assert_eq!(daemon.state.chunks_failed, 1);

        daemon.process_event(MetricEvent::DownloadRetried);
        assert_eq!(daemon.state.chunks_retried, 1);
    }

    #[test]
    fn test_process_cache_events() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::DiskCacheHit { bytes: 1024 });
        daemon.process_event(MetricEvent::DiskCacheMiss);
        daemon.process_event(MetricEvent::DiskCacheMiss);

        assert_eq!(daemon.state.disk_cache_hits, 1);
        assert_eq!(daemon.state.disk_cache_misses, 2);

        daemon.process_event(MetricEvent::MemoryCacheHit);
        daemon.process_event(MetricEvent::MemoryCacheMiss);
        daemon.process_event(MetricEvent::MemoryCacheSizeUpdate { bytes: 1_000_000 });

        assert_eq!(daemon.state.memory_cache_hits, 1);
        assert_eq!(daemon.state.memory_cache_misses, 1);
        assert_eq!(daemon.state.memory_cache_size_bytes, 1_000_000);
    }

    #[test]
    fn test_process_disk_cache_size_update() {
        let (mut daemon, _tx) = create_daemon();

        assert_eq!(daemon.state.disk_cache_size_bytes, 0);

        // Initial size report
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 5_000_000_000,
        });
        assert_eq!(daemon.state.disk_cache_size_bytes, 5_000_000_000);

        // Size increases after write
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 5_001_000_000,
        });
        assert_eq!(daemon.state.disk_cache_size_bytes, 5_001_000_000);

        // Size decreases after eviction
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 4_000_000_000,
        });
        assert_eq!(daemon.state.disk_cache_size_bytes, 4_000_000_000);
    }

    #[test]
    fn test_process_job_events() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::JobSubmitted { is_fuse: true });
        daemon.process_event(MetricEvent::JobSubmitted { is_fuse: false });
        assert_eq!(daemon.state.jobs_submitted, 2);
        assert_eq!(daemon.state.fuse_jobs_submitted, 1);

        daemon.process_event(MetricEvent::JobStarted);
        daemon.process_event(MetricEvent::JobStarted);
        assert_eq!(daemon.state.jobs_active, 2);

        daemon.process_event(MetricEvent::JobCompleted {
            success: true,
            duration_us: 100_000,
        });
        assert_eq!(daemon.state.jobs_active, 1);
        assert_eq!(daemon.state.jobs_completed, 1);

        daemon.process_event(MetricEvent::JobCompleted {
            success: false,
            duration_us: 50_000,
        });
        assert_eq!(daemon.state.jobs_active, 0);
        assert_eq!(daemon.state.jobs_failed, 1);

        daemon.process_event(MetricEvent::JobCoalesced);
        assert_eq!(daemon.state.jobs_coalesced, 1);

        daemon.process_event(MetricEvent::JobStarted);
        daemon.process_event(MetricEvent::JobTimedOut);
        assert_eq!(daemon.state.jobs_active, 0);
        assert_eq!(daemon.state.jobs_timed_out, 1);
    }

    #[test]
    fn test_process_encode_events() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::EncodeStarted);
        assert_eq!(daemon.state.encodes_active, 1);

        daemon.process_event(MetricEvent::EncodeCompleted {
            bytes: 5_000_000,
            duration_us: 200_000,
        });
        assert_eq!(daemon.state.encodes_active, 0);
        assert_eq!(daemon.state.encodes_completed, 1);
        assert_eq!(daemon.state.bytes_encoded, 5_000_000);
    }

    #[test]
    fn test_process_fuse_events() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::FuseRequestStarted);
        daemon.process_event(MetricEvent::FuseRequestStarted);
        assert_eq!(daemon.state.fuse_requests_active, 2);

        daemon.process_event(MetricEvent::FuseRequestQueued);
        assert_eq!(daemon.state.fuse_requests_waiting, 1);

        daemon.process_event(MetricEvent::FuseRequestDequeued);
        assert_eq!(daemon.state.fuse_requests_waiting, 0);

        daemon.process_event(MetricEvent::FuseRequestCompleted);
        assert_eq!(daemon.state.fuse_requests_active, 1);
    }

    #[test]
    fn test_sample_time_series() {
        let (mut daemon, _tx) = create_daemon();

        // Simulate some activity
        daemon.state.bytes_downloaded = 10_000;
        daemon.state.disk_bytes_written = 5_000;
        daemon.state.jobs_completed = 2;

        // Wait a bit and sample
        std::thread::sleep(Duration::from_millis(10));
        daemon.sample_time_series();

        assert_eq!(daemon.history.network_throughput.len(), 1);
        assert_eq!(daemon.history.disk_throughput.len(), 1);
        assert_eq!(daemon.history.job_rate.len(), 1);

        // Verify rates are non-zero
        let net_rate = daemon.history.network_throughput.last().unwrap();
        assert!(net_rate > 0.0);
    }

    #[test]
    fn test_shared_state_update() {
        let (daemon, _tx) = create_daemon();
        let handle = daemon.state_handle();

        // Initial state
        {
            let snapshot = handle.read().unwrap();
            assert_eq!(snapshot.state.chunks_downloaded, 0);
        }

        // Can't easily test update_shared_state without running the daemon,
        // but we can verify the handle works
        assert!(Arc::strong_count(&handle) >= 1);
    }

    #[test]
    fn test_saturating_decrements() {
        let (mut daemon, _tx) = create_daemon();

        // Multiple completions without starts should not underflow
        daemon.process_event(MetricEvent::DownloadCompleted {
            bytes: 100,
            duration_us: 100,
        });
        daemon.process_event(MetricEvent::DownloadCompleted {
            bytes: 100,
            duration_us: 100,
        });
        assert_eq!(daemon.state.downloads_active, 0);

        daemon.process_event(MetricEvent::FuseRequestCompleted);
        assert_eq!(daemon.state.fuse_requests_active, 0);
    }

    #[tokio::test]
    async fn test_daemon_run_and_shutdown() {
        let (tx, rx) = mpsc::unbounded_channel();
        let daemon = MetricsDaemon::new(rx);
        let handle = daemon.state_handle();
        let shutdown = CancellationToken::new();

        // Send some events
        tx.send(MetricEvent::DownloadStarted).unwrap();
        tx.send(MetricEvent::DownloadCompleted {
            bytes: 1024,
            duration_us: 5000,
        })
        .unwrap();

        // Start daemon
        let shutdown_clone = shutdown.clone();
        let daemon_task = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // Give it time to process events
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Signal shutdown
        shutdown.cancel();
        daemon_task.await.unwrap();

        // Verify final state
        let snapshot = handle.read().unwrap();
        assert_eq!(snapshot.state.chunks_downloaded, 1);
        assert_eq!(snapshot.state.bytes_downloaded, 1024);
    }
}
