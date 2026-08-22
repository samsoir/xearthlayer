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
use super::memory_probe::{MemoryProbe, ProcessMemoryProbe};
use super::state::{AggregatedState, TimeSeriesHistory, DEFAULT_HISTORY_CAPACITY};
use crate::cache::DiskTier;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Interval between time-series samples (100ms).
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Interval between process memory samples (60s).
///
/// Deliberately not configurable: traces are pooled across users and machines,
/// so a fixed cadence removes one variable. A 12h flight adds 720 lines.
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

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

    /// Last jobs completed (for rate calculation).
    last_jobs_completed: u64,

    /// Last FUSE requests completed (for rate calculation).
    last_fuse_completed: u64,

    /// Probe for reading process memory.
    memory_probe: Arc<dyn MemoryProbe>,

    /// Set once the probe has failed, so the warning is logged only once.
    memory_probe_failed: bool,
}

impl MetricsDaemon {
    /// Creates a new metrics daemon with the production memory probe.
    ///
    /// # Arguments
    ///
    /// * `rx` - Channel receiver for incoming events
    pub fn new(rx: mpsc::UnboundedReceiver<MetricEvent>) -> Self {
        Self::with_memory_probe(rx, Arc::new(ProcessMemoryProbe::new()))
    }

    /// Creates a new metrics daemon with an injected memory probe.
    ///
    /// # Arguments
    ///
    /// * `rx` - Channel receiver for incoming events
    /// * `memory_probe` - Probe used for periodic memory sampling
    pub fn with_memory_probe(
        rx: mpsc::UnboundedReceiver<MetricEvent>,
        memory_probe: Arc<dyn MemoryProbe>,
    ) -> Self {
        let shared_state = Arc::new(RwLock::new(MetricsStateSnapshot::default()));

        Self {
            rx,
            state: AggregatedState::new(),
            history: TimeSeriesHistory::new(DEFAULT_HISTORY_CAPACITY),
            shared_state,
            last_sample: Instant::now(),
            last_bytes_downloaded: 0,
            last_jobs_completed: 0,
            last_fuse_completed: 0,
            memory_probe,
            memory_probe_failed: false,
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

        let mut memory_interval = tokio::time::interval(MEMORY_SAMPLE_INTERVAL);
        memory_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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

                // Sample process memory
                _ = memory_interval.tick() => {
                    self.log_memory_sample();
                    self.log_prefetch_sample();
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

            // Chunk disk cache events
            MetricEvent::ChunkDiskCacheHit { bytes } => {
                self.state.chunk_disk_cache_hits += 1;
                self.state.chunk_disk_bytes_read += bytes;
            }
            MetricEvent::ChunkDiskCacheMiss => {
                self.state.chunk_disk_cache_misses += 1;
            }
            MetricEvent::DiskWriteStarted => {
                self.state.disk_writes_active += 1;
            }
            MetricEvent::DiskWriteCompleted { bytes, tier } => {
                self.state.disk_writes_active = self.state.disk_writes_active.saturating_sub(1);
                match tier {
                    DiskTier::Chunk => self.state.chunk_disk_bytes_written += bytes,
                    DiskTier::Dds => self.state.dds_disk_bytes_written += bytes,
                }
            }
            MetricEvent::DiskCacheInitialSize { bytes } => {
                self.state.initial_disk_cache_bytes = bytes;
            }
            MetricEvent::DiskCacheEvicted { bytes_freed } => {
                self.state.disk_bytes_evicted += bytes_freed;
            }
            MetricEvent::DiskCacheSizeUpdate { bytes } => {
                self.state.chunk_disk_cache_size_bytes = bytes;
            }
            MetricEvent::DdsDiskCacheSizeUpdate { bytes } => {
                self.state.dds_disk_cache_size_bytes = bytes;
            }
            MetricEvent::ChunkIndexEntriesUpdate { entries } => {
                self.state.chunk_index_entries = entries;
            }
            MetricEvent::DdsDiskCacheHit { bytes, is_fuse } => {
                self.state.dds_disk_cache_hits += 1;
                self.state.dds_disk_bytes_read += bytes;
                if is_fuse {
                    self.state.fuse_dds_disk_cache_hits += 1;
                }
            }
            MetricEvent::DdsDiskCacheMiss { is_fuse } => {
                self.state.dds_disk_cache_misses += 1;
                if is_fuse {
                    self.state.fuse_dds_disk_cache_misses += 1;
                }
            }

            // Memory cache events
            MetricEvent::MemoryCacheHit { is_fuse } => {
                self.state.memory_cache_hits += 1;
                if is_fuse {
                    self.state.fuse_memory_cache_hits += 1;
                }
            }
            MetricEvent::MemoryCacheMiss { is_fuse } => {
                self.state.memory_cache_misses += 1;
                if is_fuse {
                    self.state.fuse_memory_cache_misses += 1;
                }
            }
            MetricEvent::MemoryCacheSizeUpdate { bytes } => {
                self.state.memory_cache_size_bytes = bytes;
            }
            MetricEvent::MemCacheWriteStarted => {
                self.state.mem_cache_writes_active += 1;
            }
            MetricEvent::MemCacheWriteCompleted => {
                self.state.mem_cache_writes_active =
                    self.state.mem_cache_writes_active.saturating_sub(1);
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
            MetricEvent::FuseTileServed => {
                self.state.fuse_tiles_served += 1;
            }
            MetricEvent::FuseRead {
                returned,
                materialised,
                virtual_dds,
            } => {
                if virtual_dds {
                    self.state.fuse_dds_reads += 1;
                    self.state.fuse_dds_read_bytes += returned;
                    self.state.fuse_dds_alloc_bytes += materialised;
                } else {
                    self.state.fuse_file_reads += 1;
                    self.state.fuse_file_read_bytes += returned;
                    self.state.fuse_file_alloc_bytes += materialised;
                }
            }
            MetricEvent::FuseHandlesUpdate {
                open,
                pinned_bytes,
                peak_open,
                peak_pinned_bytes,
            } => {
                // Gauges: assigned, not accumulated.
                self.state.fuse_handles_open = open;
                self.state.fuse_handles_pinned_bytes = pinned_bytes;
                // Peaks are owned by the filesystem, which sees every
                // transition; take them as reported rather than deriving a
                // maximum from the samples that happen to arrive here.
                self.state.fuse_handles_peak_open = peak_open;
                self.state.fuse_handles_peak_pinned_bytes = peak_pinned_bytes;
            }
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

            // Prefetch region state events (#176)
            MetricEvent::PrefetchRegionState {
                in_progress,
                prefetched,
                no_coverage,
                deferred,
            } => {
                // Gauge: assigns, does not increment. The coordinator reports
                // the full distribution each maintenance cycle, so the latest
                // event is always the authoritative value.
                self.state.prefetch_regions_in_progress = in_progress;
                self.state.prefetch_regions_prefetched = prefetched;
                self.state.prefetch_regions_nocoverage = no_coverage;
                self.state.prefetch_regions_deferred_active = deferred;
            }
            MetricEvent::PrefetchStateDiverged => {
                self.state.prefetch_state_diverged += 1;
            }
            MetricEvent::PrefetchRegionDemoted => {
                self.state.prefetch_regions_demoted += 1;
            }
            MetricEvent::PrefetchRegionDeferred => {
                self.state.prefetch_regions_deferred += 1;
            }
            MetricEvent::PrefetchDeferralCleared => {
                self.state.prefetch_deferrals_cleared += 1;
            }
            MetricEvent::PrefetchRegionsPromotedNormal { count } => {
                self.state.prefetch_promotions_normal += count as u64;
            }
            MetricEvent::PrefetchRegionPromotedRescue => {
                self.state.prefetch_promotions_rescue += 1;
            }
        }
    }

    /// Emits one memory sample line.
    ///
    /// Every context field defeats a specific confounder: cache occupancy and
    /// GC pressure differed by orders of magnitude between the two issue #209
    /// flights, which is what made them incomparable. `disk_writes_active`
    /// correlated against `rss_mb` is what discriminates the candidate causes.
    fn log_memory_sample(&mut self) {
        // A sample with `rss_bytes == 0` is treated exactly like `None`: it is
        // the signature of the memory-stats init race the `Once` in
        // `memory_probe.rs` guards against (see its doc comment). Emitting it
        // as `rss_mb=0` would read as a healthy, near-empty process instead of
        // "unavailable" — the same misleading shape as swap being invisible.
        let sample = self
            .memory_probe
            .sample()
            .filter(|sample| sample.rss_bytes != 0);

        let Some(sample) = sample else {
            if !self.memory_probe_failed {
                self.memory_probe_failed = true;
                tracing::warn!("Memory probe unavailable; memory samples disabled");
            }
            return;
        };

        const MB: u64 = 1024 * 1024;
        let state = &self.state;

        tracing::info!(
            uptime_s = state.uptime().as_secs(),
            rss_mb = sample.rss_bytes / MB,
            vm_mb = sample.vm_bytes / MB,
            // Anonymous memory swapped out. This is the field that actually
            // caught #209: rss_mb alone read a healthy 9.3 GB while 54.7 GB
            // was swapped out. 0 means "not readable on this platform", not
            // "nothing swapped" — same convention as `threads`.
            swap_mb = sample.swap_bytes.unwrap_or(0) / MB,
            // 0 means "not readable on this platform", not "no threads".
            threads = sample.threads.unwrap_or(0),
            tiles_done = state.encodes_completed,
            encodes_active = state.encodes_active,
            chunks_ok = state.chunks_downloaded,
            chunks_failed = state.chunks_failed,
            mem_cache_mb = state.memory_cache_size_bytes / MB,
            dds_disk_mb = state.dds_disk_cache_size_bytes / MB,
            chunk_disk_mb = state.chunk_disk_cache_size_bytes / MB,
            gc_evicted_mb = state.disk_bytes_evicted / MB,
            chunk_index_entries = state.chunk_index_entries,
            disk_writes_active = state.disk_writes_active,
            // Mirrors disk_writes_active for the memory-cache spawn, which
            // previously had no in-flight gauge at all (see MemCacheWriteStarted
            // doc comment in metrics/event.rs for why that was a diagnosis hole).
            mem_cache_writes_active = state.mem_cache_writes_active,
            // FUSE read amplification (#233 / #234). `*_alloc_mb` is what the
            // read handler allocated; `*_read_mb` is what reached X-Plane. The
            // kernel caps each read at 1 MiB, so a handler that materialises the
            // whole object per call inflates alloc against read -- 12-23x for an
            // 11.17 MB object before those fixes, ~1x after.
            fuse_file_reads = state.fuse_file_reads,
            fuse_file_read_mb = state.fuse_file_read_bytes / MB,
            fuse_file_alloc_mb = state.fuse_file_alloc_bytes / MB,
            fuse_dds_reads = state.fuse_dds_reads,
            fuse_dds_read_mb = state.fuse_dds_read_bytes / MB,
            fuse_dds_alloc_mb = state.fuse_dds_alloc_bytes / MB,
            dds_handles_open = state.fuse_handles_open,
            dds_pinned_mb = state.fuse_handles_pinned_bytes / MB,
            dds_handles_peak = state.fuse_handles_peak_open,
            dds_pinned_peak_mb = state.fuse_handles_peak_pinned_bytes / MB,
            "Memory sample"
        );
    }

    /// Emit the periodic prefetch-state line.
    ///
    /// Runs on the same 60s cadence as the memory sample. The per-cycle
    /// distribution stays at `debug!` in the coordinator; this exists so a
    /// default-level flight log is enough to judge the #176 acceptance
    /// criteria without running an entire flight at `--debug`.
    fn log_prefetch_sample(&self) {
        let state = &self.state;
        tracing::info!(
            uptime_s = state.uptime().as_secs(),
            regions_in_progress = state.prefetch_regions_in_progress,
            regions_prefetched = state.prefetch_regions_prefetched,
            regions_nocoverage = state.prefetch_regions_nocoverage,
            // #226: how many regions are deferred RIGHT NOW (gauge). Distinct
            // from `regions_deferred` below, which is a cumulative count of
            // every deferral since process start. Without this, deferred
            // regions would be invisible in the sample line — or, worse,
            // counted as `regions_nocoverage` and read as missing scenery.
            regions_deferred_active = state.prefetch_regions_deferred_active,
            promotions_normal = state.prefetch_promotions_normal,
            promotions_rescue = state.prefetch_promotions_rescue,
            state_diverged = state.prefetch_state_diverged,
            regions_demoted = state.prefetch_regions_demoted,
            // #226: how often a region was skipped for making no progress.
            // Cumulative since process start — difference successive samples
            // for a rate — so it only ever climbs. It does NOT climb
            // every maintenance cycle: a stuck region is moved to `Deferred`
            // immediately, and `is_stale()` only re-evaluates `InProgress`
            // regions, so the region sits off-cycle until its deferral expires
            // (the 20/30/40/60s ladder), gets re-planned back to `InProgress`,
            // and ages another `stale_region_timeout` (default 120s) before it
            // can increment again. Realistic floor is ~140s per region per
            // increment. Read the rate of change between samples, not the
            // absolute value: a flat value across successive lines is the
            // healthy signal. Non-zero under a cold-cache backlog is expected.
            regions_deferred = state.prefetch_regions_deferred,
            // #226: how often the sim demanded a tile inside a region that
            // was still Deferred, clearing the deferral early. The post-fix
            // analogue of `regions_demoted` — measures whether the
            // 20/30/40/60s ladder is well-tuned, not whether prefetch's state
            // was wrong. Cumulative since process start, same as
            // `regions_deferred` above.
            deferrals_cleared = state.prefetch_deferrals_cleared,
            // Criterion 5's user-visible outcome (the others above are
            // mechanism): on-demand FUSE generations should fall during
            // cruise. Per-tile FUSE logging is debug-only (#209), so this
            // aggregated counter is the only default-level source.
            fuse_generations = state.fuse_jobs_submitted,
            "Prefetch sample"
        );
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
    use super::super::client::MetricsClient;
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

        daemon.process_event(MetricEvent::ChunkDiskCacheHit { bytes: 1024 });
        daemon.process_event(MetricEvent::ChunkDiskCacheMiss);
        daemon.process_event(MetricEvent::ChunkDiskCacheMiss);

        assert_eq!(daemon.state.chunk_disk_cache_hits, 1);
        assert_eq!(daemon.state.chunk_disk_cache_misses, 2);

        daemon.process_event(MetricEvent::MemoryCacheHit { is_fuse: true });
        daemon.process_event(MetricEvent::MemoryCacheMiss { is_fuse: false });
        daemon.process_event(MetricEvent::MemoryCacheSizeUpdate { bytes: 1_000_000 });

        assert_eq!(daemon.state.memory_cache_hits, 1);
        assert_eq!(daemon.state.memory_cache_misses, 1);
        assert_eq!(daemon.state.memory_cache_size_bytes, 1_000_000);
    }

    #[test]
    fn test_fuse_read_events_are_split_by_path() {
        let (mut daemon, _tx) = create_daemon();

        // A real file on disk: two ranged calls out of one 4 MiB file.
        daemon.process_event(MetricEvent::FuseRead {
            returned: 1_048_576,
            materialised: 4_194_304,
            virtual_dds: false,
        });
        daemon.process_event(MetricEvent::FuseRead {
            returned: 1_048_576,
            materialised: 4_194_304,
            virtual_dds: false,
        });
        // A generated DDS tile: one ranged call out of an 11.17 MB tile.
        daemon.process_event(MetricEvent::FuseRead {
            returned: 1_048_576,
            materialised: 11_712_512,
            virtual_dds: true,
        });

        assert_eq!(daemon.state.fuse_file_reads, 2);
        assert_eq!(daemon.state.fuse_file_read_bytes, 2_097_152);
        assert_eq!(daemon.state.fuse_file_alloc_bytes, 8_388_608);

        // The DDS event must not leak into the file counters, and vice versa:
        // the two issues are verified independently from one flight.
        assert_eq!(daemon.state.fuse_dds_reads, 1);
        assert_eq!(daemon.state.fuse_dds_read_bytes, 1_048_576);
        assert_eq!(daemon.state.fuse_dds_alloc_bytes, 11_712_512);
    }

    #[test]
    fn test_fuse_read_counters_accumulate_rather_than_replace() {
        // These are counters, not gauges. A single event cannot distinguish
        // `0 += x` from `0 = x`, so send two and assert on the sum.
        let (mut daemon, _tx) = create_daemon();

        for _ in 0..3 {
            daemon.process_event(MetricEvent::FuseRead {
                returned: 100,
                materialised: 1_000,
                virtual_dds: false,
            });
        }

        assert_eq!(daemon.state.fuse_file_reads, 3);
        assert_eq!(daemon.state.fuse_file_read_bytes, 300);
        assert_eq!(daemon.state.fuse_file_alloc_bytes, 3_000);
    }

    #[test]
    fn test_process_disk_cache_size_update() {
        let (mut daemon, _tx) = create_daemon();

        assert_eq!(daemon.state.chunk_disk_cache_size_bytes, 0);

        // Initial size report
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 5_000_000_000,
        });
        assert_eq!(daemon.state.chunk_disk_cache_size_bytes, 5_000_000_000);

        // Size increases after write
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 5_001_000_000,
        });
        assert_eq!(daemon.state.chunk_disk_cache_size_bytes, 5_001_000_000);

        // Size decreases after eviction
        daemon.process_event(MetricEvent::DiskCacheSizeUpdate {
            bytes: 4_000_000_000,
        });
        assert_eq!(daemon.state.chunk_disk_cache_size_bytes, 4_000_000_000);
    }

    #[test]
    fn chunk_index_entries_update_sets_gauge() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::new(rx);

        daemon.process_event(MetricEvent::ChunkIndexEntriesUpdate {
            entries: 12_046_463,
        });

        assert_eq!(daemon.state.chunk_index_entries, 12_046_463);
    }

    #[test]
    fn test_process_dds_disk_cache_events() {
        let (mut daemon, _tx) = create_daemon();
        daemon.process_event(MetricEvent::DdsDiskCacheHit {
            bytes: 11_000_000,
            is_fuse: false,
        });
        daemon.process_event(MetricEvent::DdsDiskCacheHit {
            bytes: 11_000_000,
            is_fuse: false,
        });
        daemon.process_event(MetricEvent::DdsDiskCacheMiss { is_fuse: false });
        assert_eq!(daemon.state.dds_disk_cache_hits, 2);
        assert_eq!(daemon.state.dds_disk_cache_misses, 1);
        assert_eq!(daemon.state.dds_disk_bytes_read, 22_000_000);
    }

    #[test]
    fn test_dds_disk_cache_events_segregate_fuse_from_aggregate() {
        // Regression for #171: DDS disk cache events tagged with `is_fuse: true`
        // must increment both aggregate and FUSE-only counters; events with
        // `is_fuse: false` (prefetch, prewarm) must only hit the aggregate.
        let (mut daemon, _tx) = create_daemon();

        // FUSE hit → both counters
        daemon.process_event(MetricEvent::DdsDiskCacheHit {
            bytes: 1_000,
            is_fuse: true,
        });
        assert_eq!(daemon.state.dds_disk_cache_hits, 1);
        assert_eq!(daemon.state.fuse_dds_disk_cache_hits, 1);

        // Prefetch hit → aggregate only
        daemon.process_event(MetricEvent::DdsDiskCacheHit {
            bytes: 2_000,
            is_fuse: false,
        });
        assert_eq!(daemon.state.dds_disk_cache_hits, 2);
        assert_eq!(
            daemon.state.fuse_dds_disk_cache_hits, 1,
            "prefetch hits must NOT touch the FUSE-only counter"
        );

        // FUSE miss → both counters
        daemon.process_event(MetricEvent::DdsDiskCacheMiss { is_fuse: true });
        assert_eq!(daemon.state.dds_disk_cache_misses, 1);
        assert_eq!(daemon.state.fuse_dds_disk_cache_misses, 1);

        // Prefetch miss → aggregate only
        daemon.process_event(MetricEvent::DdsDiskCacheMiss { is_fuse: false });
        assert_eq!(daemon.state.dds_disk_cache_misses, 2);
        assert_eq!(
            daemon.state.fuse_dds_disk_cache_misses, 1,
            "prefetch misses must NOT touch the FUSE-only counter"
        );
    }

    #[test]
    fn test_process_chunk_disk_cache_events() {
        let (mut daemon, _tx) = create_daemon();
        daemon.process_event(MetricEvent::ChunkDiskCacheHit { bytes: 1024 });
        daemon.process_event(MetricEvent::ChunkDiskCacheMiss);
        daemon.process_event(MetricEvent::ChunkDiskCacheMiss);
        assert_eq!(daemon.state.chunk_disk_cache_hits, 1);
        assert_eq!(daemon.state.chunk_disk_cache_misses, 2);
        assert_eq!(daemon.state.chunk_disk_bytes_read, 1024);
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
        daemon.state.chunk_disk_bytes_written = 5_000;
        daemon.state.jobs_completed = 2;

        // Wait a bit and sample
        std::thread::sleep(Duration::from_millis(10));
        daemon.sample_time_series();

        assert_eq!(daemon.history.network_throughput.len(), 1);
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

    #[test]
    fn memory_sample_is_skipped_when_probe_unavailable() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(crate::metrics::memory_probe::StaticMemoryProbe::unavailable()),
        );

        assert!(!daemon.memory_probe_failed);
        daemon.log_memory_sample();
        assert!(
            daemon.memory_probe_failed,
            "first failure must latch the warn-once flag"
        );

        // Second call must not reset the flag (the flag latches after the
        // first failure and stays latched; see
        // `memory_sample_emits_no_event_when_probe_unavailable` for the
        // companion assertion that no event is actually emitted).
        daemon.log_memory_sample();
        assert!(daemon.memory_probe_failed);
    }

    #[test]
    fn memory_sample_runs_with_a_working_probe() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(crate::metrics::memory_probe::StaticMemoryProbe::new(
                5_412 * 1024 * 1024,
                6_218 * 1024 * 1024,
            )),
        );

        daemon.process_event(MetricEvent::ChunkIndexEntriesUpdate { entries: 99 });
        daemon.log_memory_sample();

        assert!(
            !daemon.memory_probe_failed,
            "working probe must not latch failure"
        );
        assert_eq!(daemon.state.chunk_index_entries, 99);
    }

    // =========================================================================
    // Tracing capture harness for asserting on the emitted "Memory sample"
    // event itself (field names, sources, and MB truncation), not just that
    // `log_memory_sample` runs without panicking.
    // =========================================================================

    /// Visitor that records every field of a tracing event as a string,
    /// keyed by field name, using `record_debug` as the sole capture point.
    ///
    /// `Visit`'s other `record_*` methods (u64, i64, bool, str, ...) all have
    /// default implementations that delegate to `record_debug`, so this one
    /// override captures every field type emitted by `log_memory_sample`.
    /// The implicit message field (`"Memory sample"`) arrives as a `Debug`
    /// value too: `std::fmt::Arguments`'s `Debug` impl forwards to `Display`,
    /// so the captured string has no surrounding quotes.
    struct RecordingVisitor<'a>(&'a mut std::collections::HashMap<String, String>);

    impl tracing::field::Visit for RecordingVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    /// A `tracing_subscriber::Layer` that appends every event's fields to a
    /// shared buffer, in emission order.
    struct RecordingLayer {
        events: std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordingLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::HashMap::new();
            event.record(&mut RecordingVisitor(&mut fields));
            self.events.lock().unwrap().push(fields);
        }
    }

    /// Runs `f` under a tracing subscriber that captures every event's
    /// fields, returning them in emission order. Uses only
    /// `tracing`/`tracing-subscriber`, both already direct dependencies of
    /// this crate (see Cargo.toml) — no new dependency is introduced.
    fn capture_events<F: FnOnce()>(f: F) -> Vec<std::collections::HashMap<String, String>> {
        use tracing_subscriber::layer::SubscriberExt;

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = RecordingLayer {
            events: std::sync::Arc::clone(&events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f);
        let captured = events.lock().unwrap().clone();
        captured
    }

    /// Finds the first captured event whose message is "Memory sample".
    fn find_memory_sample(
        events: &[std::collections::HashMap<String, String>],
    ) -> Option<&std::collections::HashMap<String, String>> {
        events
            .iter()
            .find(|fields| fields.get("message").map(String::as_str) == Some("Memory sample"))
    }

    #[test]
    fn memory_sample_line_carries_every_field_from_its_correct_source() {
        const MB: u64 = 1024 * 1024;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(
                crate::metrics::memory_probe::StaticMemoryProbe::new(
                    5 * MB + 1, // deliberately not a whole number of MB
                    6 * MB,
                )
                .with_swap_bytes(7 * MB),
            ),
        );

        // Every numeric field is seeded to a distinct value so that a
        // swapped source, a renamed/reordered field, or a silently dropped
        // field cannot pass this test: any of those would make at least one
        // of the per-field assertions below fail.
        daemon.state.encodes_completed = 10; // tiles_done
        daemon.state.encodes_active = 11;
        daemon.state.chunks_downloaded = 12; // chunks_ok
        daemon.state.chunks_failed = 13;
        daemon.state.memory_cache_size_bytes = 14 * MB + 999_999; // mem_cache_mb, also not a whole MB
        daemon.state.dds_disk_cache_size_bytes = 15 * MB; // dds_disk_mb
        daemon.state.chunk_disk_cache_size_bytes = 16 * MB; // chunk_disk_mb
        daemon.state.disk_bytes_evicted = 17 * MB; // gc_evicted_mb
        daemon.state.chunk_index_entries = 18;
        daemon.state.disk_writes_active = 19;
        daemon.state.mem_cache_writes_active = 20;

        let events = capture_events(|| daemon.log_memory_sample());
        let sample = find_memory_sample(&events)
            .expect("log_memory_sample must emit a \"Memory sample\" event");

        // Field-by-field: name -> expected value. `rss_mb` and `mem_cache_mb`
        // are seeded from byte counts that are NOT whole multiples of MB, so
        // this also proves the emit line truncates to MB rather than leaking
        // raw bytes through under an "_mb" name.
        let expected: &[(&str, &str)] = &[
            ("rss_mb", "5"),
            ("vm_mb", "6"),
            ("swap_mb", "7"),  // StaticMemoryProbe::with_swap_bytes
            ("threads", "42"), // StaticMemoryProbe::new fixes threads at 42
            ("tiles_done", "10"),
            ("encodes_active", "11"),
            ("chunks_ok", "12"),
            ("chunks_failed", "13"),
            ("mem_cache_mb", "14"),
            ("dds_disk_mb", "15"),
            ("chunk_disk_mb", "16"),
            ("gc_evicted_mb", "17"),
            ("chunk_index_entries", "18"),
            ("disk_writes_active", "19"),
            ("mem_cache_writes_active", "20"),
        ];

        for (field, value) in expected {
            assert_eq!(
                sample.get(*field).map(String::as_str),
                Some(*value),
                "field `{field}` did not carry the expected value from its source"
            );
        }

        assert!(
            sample.contains_key("uptime_s"),
            "uptime_s field must be present"
        );

        // Belt-and-braces: confirm the fixture values (including the
        // dynamically-read uptime_s) are pairwise distinct. If they were
        // not, a swap between two fields sharing a value could pass the
        // assertions above by coincidence.
        let mut all_values: Vec<&str> = expected.iter().map(|(_, v)| *v).collect();
        all_values.push(sample.get("uptime_s").unwrap().as_str());
        let mut sorted = all_values.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all_values.len(),
            "fixture values must be pairwise distinct or a swapped source could go undetected"
        );
    }

    #[test]
    fn memory_sample_emits_no_event_when_probe_unavailable() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(crate::metrics::memory_probe::StaticMemoryProbe::unavailable()),
        );

        let events = capture_events(|| daemon.log_memory_sample());

        assert!(
            find_memory_sample(&events).is_none(),
            "no \"Memory sample\" event may be emitted when the probe is unavailable"
        );
    }

    // =========================================================================
    // rss_bytes == 0 guard (minor fix a)
    //
    // A `Some(sample)` with `rss_bytes == 0` must be treated exactly like a
    // `None` sample: it is the observable signature of the memory-stats init
    // race described on `MEMORY_STATS_INIT` in memory_probe.rs. Without this
    // guard, that race would silently emit `rss_mb=0` — a plausible-looking
    // but wrong reading — instead of latching the same "unavailable" path
    // that `None` takes.
    // =========================================================================

    #[test]
    fn memory_sample_is_skipped_when_rss_bytes_is_zero() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(crate::metrics::memory_probe::StaticMemoryProbe::new(
                0,
                6 * 1024 * 1024,
            )),
        );

        assert!(!daemon.memory_probe_failed);
        daemon.log_memory_sample();
        assert!(
            daemon.memory_probe_failed,
            "a zeroed rss_bytes sample must be treated as unavailable, latching the warn-once flag"
        );
    }

    #[test]
    fn memory_sample_emits_no_event_when_rss_bytes_is_zero() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut daemon = MetricsDaemon::with_memory_probe(
            rx,
            std::sync::Arc::new(crate::metrics::memory_probe::StaticMemoryProbe::new(
                0,
                6 * 1024 * 1024,
            )),
        );

        let events = capture_events(|| daemon.log_memory_sample());

        assert!(
            find_memory_sample(&events).is_none(),
            "no \"Memory sample\" event may be emitted when rss_bytes is zero"
        );
    }

    #[test]
    fn chunk_tier_write_credits_only_the_chunk_counter() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::DiskWriteCompleted {
            bytes: 1_000,
            tier: DiskTier::Chunk,
        });

        assert_eq!(daemon.state.chunk_disk_bytes_written, 1_000);
        assert_eq!(daemon.state.dds_disk_bytes_written, 0);
    }

    #[test]
    fn dds_tier_write_credits_only_the_dds_counter() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::DiskWriteCompleted {
            bytes: 11_174_016,
            tier: DiskTier::Dds,
        });

        assert_eq!(daemon.state.dds_disk_bytes_written, 11_174_016);
        assert_eq!(daemon.state.chunk_disk_bytes_written, 0);
    }

    // Guards the cross-tier invariant the #209 memory telemetry depends on:
    // disk_writes_active must count in-flight writes from BOTH tiers.
    #[test]
    fn both_tiers_decrement_the_shared_in_flight_gauge() {
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::DiskWriteStarted);
        daemon.process_event(MetricEvent::DiskWriteStarted);
        assert_eq!(daemon.state.disk_writes_active, 2);

        daemon.process_event(MetricEvent::DiskWriteCompleted {
            bytes: 1,
            tier: DiskTier::Chunk,
        });
        assert_eq!(
            daemon.state.disk_writes_active, 1,
            "chunk completion must decrement"
        );

        daemon.process_event(MetricEvent::DiskWriteCompleted {
            bytes: 1,
            tier: DiskTier::Dds,
        });
        assert_eq!(
            daemon.state.disk_writes_active, 0,
            "dds completion must decrement too"
        );
    }

    // =========================================================================
    // Prefetch sample tests (#176)
    //
    // Region-state counters are a gauge (assigned wholesale each maintenance
    // cycle); divergence/demotion counters accumulate. Mixing up assign vs.
    // increment for the gauge would produce a plausible-looking but
    // monotonically-growing region count.
    // =========================================================================

    /// Finds the first captured event whose message is "Prefetch sample".
    fn find_prefetch_sample(
        events: &[std::collections::HashMap<String, String>],
    ) -> Option<&std::collections::HashMap<String, String>> {
        events
            .iter()
            .find(|fields| fields.get("message").map(String::as_str) == Some("Prefetch sample"))
    }

    #[test]
    fn test_prefetch_sample_reports_divergence_and_region_state() {
        let (mut daemon, tx) = create_daemon();
        let client = MetricsClient::new(tx);

        client.prefetch_region_state(3, 7, 2, 5);
        client.prefetch_state_diverged();
        client.prefetch_state_diverged();
        client.prefetch_region_demoted();

        // The daemon's event loop isn't running in this unit test, so drain
        // the channel into the daemon's state directly before sampling.
        while let Ok(event) = daemon.rx.try_recv() {
            daemon.process_event(event);
        }

        let events = capture_events(|| daemon.log_prefetch_sample());
        let sample =
            find_prefetch_sample(&events).expect("log_prefetch_sample must emit a sample line");

        assert_eq!(
            sample.get("regions_in_progress").map(String::as_str),
            Some("3")
        );
        assert_eq!(
            sample.get("regions_prefetched").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            sample.get("regions_nocoverage").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            sample.get("regions_deferred_active").map(String::as_str),
            Some("5"),
            "the deferred gauge must be its own field, distinct from regions_nocoverage"
        );
        assert_eq!(sample.get("state_diverged").map(String::as_str), Some("2"));
        assert_eq!(sample.get("regions_demoted").map(String::as_str), Some("1"));
    }

    #[test]
    fn test_prefetch_sample_reports_fuse_generations() {
        // Criterion 5 ("on-demand FUSE generations fall during cruise") has
        // no field in the sample line unless fuse_jobs_submitted is surfaced
        // here — per-tile FUSE logging is debug-only (#209), so this counter
        // is the only default-level source for that criterion.
        let (mut daemon, tx) = create_daemon();
        let client = MetricsClient::new(tx);

        client.job_submitted(true);
        client.job_submitted(true);
        client.job_submitted(false); // non-FUSE submission must not count

        while let Ok(event) = daemon.rx.try_recv() {
            daemon.process_event(event);
        }

        let events = capture_events(|| daemon.log_prefetch_sample());
        let sample =
            find_prefetch_sample(&events).expect("log_prefetch_sample must emit a sample line");

        assert_eq!(
            sample.get("fuse_generations").map(String::as_str),
            Some("2"),
            "fuse_generations must carry state.fuse_jobs_submitted"
        );
    }

    #[test]
    fn test_prefetch_sample_reports_regions_deferred() {
        // regions_deferred is a COUNTER, unlike the region-state gauges
        // above: it accumulates across the daemon's own event stream rather
        // than being assigned wholesale from GeoIndex each cycle. Nothing
        // windows it, so in a running process it is cumulative since start —
        // difference successive log samples for a rate.
        let (mut daemon, _tx) = create_daemon();
        daemon.state.prefetch_regions_deferred = 7;
        daemon.state.prefetch_regions_nocoverage = 3;
        daemon.state.prefetch_regions_deferred_active = 4;
        daemon.state.prefetch_deferrals_cleared = 5;

        let events = capture_events(|| daemon.log_prefetch_sample());
        let sample =
            find_prefetch_sample(&events).expect("log_prefetch_sample must emit a sample line");
        assert_eq!(
            sample.get("regions_deferred").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            sample.get("deferrals_cleared").map(String::as_str),
            Some("5"),
            "deferrals_cleared must be its own field, distinct from regions_deferred"
        );
        // Same subject, different semantics — the two must never collapse
        // into one field. Distinct seeded values make a mix-up visible.
        assert_eq!(
            sample.get("regions_deferred_active").map(String::as_str),
            Some("4"),
            "the gauge is a separate field from the counter"
        );
    }

    #[test]
    fn test_prefetch_region_deferred_event_increments_the_counter_only() {
        // Closes the daemon end of the `regions_deferred` chain (#226 review
        // I-3). The sample-line test seeds the field directly, so it cannot
        // tell whether `process_event` routes this variant anywhere at all —
        // and the most plausible mis-wiring, incrementing the *gauge*
        // `prefetch_regions_deferred_active`, would leave the counter
        // permanently zero while the sample line still looked populated.
        let (mut daemon, _tx) = create_daemon();
        daemon.state.prefetch_regions_deferred_active = 4;

        daemon.process_event(MetricEvent::PrefetchRegionDeferred);
        daemon.process_event(MetricEvent::PrefetchRegionDeferred);

        assert_eq!(
            daemon.state.prefetch_regions_deferred, 2,
            "each event must increment the counter"
        );
        assert_eq!(
            daemon.state.prefetch_regions_deferred_active, 4,
            "the event must leave the gauge alone — it is assigned wholesale \
             from the GeoIndex by PrefetchRegionState"
        );
        // The neighbouring prefetch counters share a shape; a copy-paste in
        // the match arm would land in one of them.
        assert_eq!(daemon.state.prefetch_regions_demoted, 0);
        assert_eq!(daemon.state.prefetch_state_diverged, 0);
        assert_eq!(daemon.state.prefetch_promotions_normal, 0);
        assert_eq!(daemon.state.prefetch_promotions_rescue, 0);
        assert_eq!(daemon.state.prefetch_deferrals_cleared, 0);
    }

    #[test]
    fn test_prefetch_deferral_cleared_event_increments_the_counter_only() {
        // Closes the daemon end of the `deferrals_cleared` chain (#226,
        // mirroring PrefetchRegionDeferred above end to end). The most
        // plausible mis-wiring is routing this into the *gauge*
        // `prefetch_regions_deferred_active`, which would leave the counter
        // permanently zero while the gauge looked populated.
        let (mut daemon, _tx) = create_daemon();
        daemon.state.prefetch_regions_deferred_active = 4;

        daemon.process_event(MetricEvent::PrefetchDeferralCleared);
        daemon.process_event(MetricEvent::PrefetchDeferralCleared);

        assert_eq!(
            daemon.state.prefetch_deferrals_cleared, 2,
            "each event must increment the counter"
        );
        assert_eq!(
            daemon.state.prefetch_regions_deferred_active, 4,
            "the event must leave the gauge alone"
        );
        // The neighbouring prefetch counters share a shape; a copy-paste in
        // the match arm would land in one of them.
        assert_eq!(daemon.state.prefetch_regions_deferred, 0);
        assert_eq!(daemon.state.prefetch_regions_demoted, 0);
        assert_eq!(daemon.state.prefetch_state_diverged, 0);
        assert_eq!(daemon.state.prefetch_promotions_normal, 0);
        assert_eq!(daemon.state.prefetch_promotions_rescue, 0);
    }

    #[test]
    fn test_prefetch_region_state_is_a_gauge_not_a_counter() {
        // Regression guard: a second PrefetchRegionState event must replace
        // the previous values, not add to them. If this were mistakenly
        // implemented as an increment, region counts would grow without
        // bound across maintenance cycles.
        let (mut daemon, _tx) = create_daemon();

        daemon.process_event(MetricEvent::PrefetchRegionState {
            in_progress: 5,
            prefetched: 10,
            no_coverage: 1,
            deferred: 4,
        });
        assert_eq!(daemon.state.prefetch_regions_in_progress, 5);
        assert_eq!(daemon.state.prefetch_regions_prefetched, 10);
        assert_eq!(daemon.state.prefetch_regions_nocoverage, 1);
        assert_eq!(daemon.state.prefetch_regions_deferred_active, 4);

        daemon.process_event(MetricEvent::PrefetchRegionState {
            in_progress: 2,
            prefetched: 3,
            no_coverage: 0,
            deferred: 0,
        });
        assert_eq!(daemon.state.prefetch_regions_in_progress, 2);
        assert_eq!(daemon.state.prefetch_regions_prefetched, 3);
        assert_eq!(daemon.state.prefetch_regions_nocoverage, 0);
        assert_eq!(
            daemon.state.prefetch_regions_deferred_active, 0,
            "the deferred gauge must be assigned, not accumulated"
        );
    }

    #[test]
    fn test_promotion_counters_accumulate_and_are_separate() {
        let (mut daemon, _tx) = create_daemon();

        // Two events, not one: from a zero start `0 += n` and `0 = n` are
        // indistinguishable, so a single event cannot prove these are counters.
        daemon.process_event(MetricEvent::PrefetchRegionsPromotedNormal { count: 3 });
        daemon.process_event(MetricEvent::PrefetchRegionsPromotedNormal { count: 2 });
        daemon.process_event(MetricEvent::PrefetchRegionPromotedRescue);

        assert_eq!(
            daemon.state.prefetch_promotions_normal, 5,
            "must accumulate, not assign"
        );
        assert_eq!(daemon.state.prefetch_promotions_rescue, 1);
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
