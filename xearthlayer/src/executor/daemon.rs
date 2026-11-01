//! Job executor daemon for processing DDS tile requests.
//!
//! The [`ExecutorDaemon`] is a long-running background service that:
//! - Receives tile requests via a channel
//! - Checks memory cache for fast-path cache hits
//! - Uses request coalescing to prevent duplicate work
//! - Creates and executes DDS generation jobs
//! - Returns results to callers
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                       ExecutorDaemon                             │
//! │                                                                  │
//! │  JobRequest ──► ┌─────────────┐                                 │
//! │                 │ Cache Check │──► Hit ──► Return immediately   │
//! │                 └──────┬──────┘                                 │
//! │                        │ Miss                                   │
//! │                        ▼                                        │
//! │                 ┌─────────────┐                                 │
//! │                 │  Coalescer  │──► Coalesced ──► Wait for result│
//! │                 └──────┬──────┘                                 │
//! │                        │ New                                    │
//! │                        ▼                                        │
//! │                 ┌─────────────┐                                 │
//! │                 │   Factory   │──► Create DdsGenerateJob        │
//! │                 └──────┬──────┘                                 │
//! │                        ▼                                        │
//! │                 ┌─────────────┐                                 │
//! │                 │  Executor   │──► Run job, wait for completion │
//! │                 └──────┬──────┘                                 │
//! │                        ▼                                        │
//! │                 ┌─────────────┐                                 │
//! │                 │ Cache Read  │──► Return DDS data              │
//! │                 └─────────────┘                                 │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{ExecutorDaemon, ExecutorDaemonConfig};
//!
//! let config = ExecutorDaemonConfig::default();
//! let (daemon, request_tx) = ExecutorDaemon::new(config, factory, memory_cache, dds_disk_cache);
//!
//! // Start daemon
//! let shutdown = CancellationToken::new();
//! tokio::spawn(daemon.run(shutdown.clone()));
//!
//! // Submit request
//! let (request, response_rx) = JobRequest::fuse(tile, CancellationToken::new());
//! request_tx.send(request).await?;
//! let response = response_rx.await?;
//! ```

use crate::coord::TileCoord;
use crate::executor::{
    DemandGuard, DemandTracker, ExecutorConfig, Job, JobExecutor, JobHandle, JobStatus,
    JobSubmitter as ExecutorSubmitter, SubmitError, TelemetrySink, TracingTelemetrySink,
};
use crate::jobs::DdsJobFactory;
use crate::metrics::MetricsClient;
use crate::runtime::{DdsResponse, JobRequest};
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn, Instrument};

// =============================================================================
// Debug map helpers
// =============================================================================

/// Record a tile activity event for the debug map overlay.
#[cfg(feature = "debug-map")]
fn record_debug_tile_activity(
    tile_lat: f64,
    tile_lon: f64,
    row: u32,
    col: u32,
    zoom: u8,
    is_fuse: bool,
    result: crate::debug_map::activity::TileCacheResult,
) {
    use crate::debug_map::activity::{TileActivityTracker, TileOrigin};

    let origin = if is_fuse {
        TileOrigin::Fuse
    } else {
        TileOrigin::Prefetch
    };
    TileActivityTracker::global()
        .record_with_tile(tile_lat, tile_lon, row, col, zoom, origin, result);
}

// =============================================================================
// Configuration
// =============================================================================

/// Default channel capacity for job requests.
pub const DEFAULT_REQUEST_CHANNEL_CAPACITY: usize = 1000;

/// Configuration for the executor daemon.
#[derive(Clone, Debug)]
pub struct ExecutorDaemonConfig {
    /// Job executor configuration.
    pub executor: ExecutorConfig,

    /// Request channel capacity.
    pub channel_capacity: usize,

    /// Wait for room in the executor's job queue instead of shedding the job.
    ///
    /// The prefetch lane sets this: a shed prefetch job is lost without the
    /// coordinator knowing, which counted its tile as submitted and never asked
    /// for it again. Waiting holds the daemon instead, the request channel
    /// fills, and the coordinator sees a full lane and keeps the tile pending.
    /// The demand lane leaves it off, so a FUSE request on a saturated executor
    /// is answered at once rather than waiting out its timeout.
    pub wait_for_executor: bool,
}

impl Default for ExecutorDaemonConfig {
    fn default() -> Self {
        Self {
            executor: ExecutorConfig::default(),
            channel_capacity: DEFAULT_REQUEST_CHANNEL_CAPACITY,
            wait_for_executor: false,
        }
    }
}

// =============================================================================
// Memory Cache Trait (minimal interface for daemon)
// =============================================================================

/// Minimal interface for memory cache operations.
///
/// This trait allows the daemon to check cache and read results without
/// depending on the full `MemoryCache` trait from the pipeline module.
pub trait DaemonMemoryCache: Send + Sync + 'static {
    /// Gets a tile from the cache.
    fn get(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Bytes>> + Send + '_>>;

    /// Stores a tile in the memory cache.
    ///
    /// Used by the daemon to promote DDS disk cache hits back to memory.
    fn put(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
        data: Bytes,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;

    /// Checks if a tile exists in the cache without loading data.
    ///
    /// This is more efficient than `get` when you only need to know if a tile
    /// is cached. Default implementation calls `get` and checks for Some.
    fn contains(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.get(row, col, zoom).await.is_some() })
    }
}

/// Blanket implementation for any type implementing the executor's MemoryCache trait.
impl<T> DaemonMemoryCache for T
where
    T: crate::executor::MemoryCache,
{
    fn get(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Bytes>> + Send + '_>> {
        Box::pin(async move { crate::executor::MemoryCache::get(self, row, col, zoom).await })
    }

    fn put(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
        data: Bytes,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move { crate::executor::MemoryCache::put(self, row, col, zoom, data).await })
    }
}

// =============================================================================
// Request Coalescing
// =============================================================================

/// Result broadcast for coalesced requests.
///
/// Contains the DDS data and metadata shared between coalesced requests.
#[derive(Clone, Debug)]
struct CoalescedDdsResult {
    /// The DDS data.
    data: Bytes,
    /// How long the request took.
    duration: Duration,
    /// Whether the job succeeded.
    job_succeeded: bool,
}

impl CoalescedDdsResult {
    fn new(data: Bytes, duration: Duration, job_succeeded: bool) -> Self {
        Self {
            data,
            duration,
            job_succeeded,
        }
    }

    fn into_response(self, cache_hit: bool) -> DdsResponse {
        DdsResponse::new(self.data, cache_hit, self.duration, self.job_succeeded)
    }
}

/// Daemon-level request coalescer.
///
/// Tracks in-flight jobs to prevent duplicate work when multiple requests
/// arrive for the same tile. When a job is already in-flight, new requests
/// subscribe to the same result instead of creating another job.
///
/// This coalescer operates at the daemon level, meaning it coalesces requests
/// from ALL consumers (FUSE, Prefetch, Prewarm) - not just within a single
/// consumer like the FUSE-level coalescer.
struct DaemonCoalescer {
    /// In-flight jobs: tile -> broadcast sender for result
    in_flight: DashMap<TileCoord, broadcast::Sender<CoalescedDdsResult>>,
}

impl DaemonCoalescer {
    fn new() -> Self {
        Self {
            in_flight: DashMap::new(),
        }
    }

    /// Attempts to register a request for the given tile.
    ///
    /// Returns `Ok(receiver)` if an in-flight job exists (caller should wait).
    /// Returns `Err(sender)` if this is a new request (caller should process).
    fn register(
        &self,
        tile: TileCoord,
    ) -> Result<broadcast::Receiver<CoalescedDdsResult>, broadcast::Sender<CoalescedDdsResult>>
    {
        match self.in_flight.entry(tile) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                // Job already in-flight, subscribe to result
                let rx = entry.get().subscribe();
                debug!(
                    tile = ?tile,
                    waiters = entry.get().receiver_count(),
                    "Request coalesced - waiting for in-flight job"
                );
                Ok(rx)
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // New request, create broadcast channel
                let (tx, _rx) = broadcast::channel(16);
                entry.insert(tx.clone());
                debug!(
                    tile = ?tile,
                    in_flight_count = self.in_flight.len(),
                    "New request - starting job"
                );
                Err(tx)
            }
        }
    }

    /// Completes a job, broadcasting the result to all waiters.
    fn complete(&self, tile: TileCoord, result: CoalescedDdsResult) {
        if let Some((_, tx)) = self.in_flight.remove(&tile) {
            let subscriber_count = tx.receiver_count();
            let _ = tx.send(result);

            if subscriber_count > 0 {
                debug!(
                    tile = ?tile,
                    waiters = subscriber_count,
                    "Broadcast result to coalesced waiters"
                );
            }
        }
    }

    /// Whether any request is waiting on the in-flight job for `tile`.
    fn has_waiters(&self, tile: TileCoord) -> bool {
        self.in_flight
            .get(&tile)
            .is_some_and(|tx| tx.receiver_count() > 0)
    }

    /// Cancels a job, removing it from in-flight.
    fn cancel(&self, tile: TileCoord) {
        if let Some((_, _tx)) = self.in_flight.remove(&tile) {
            debug!(tile = ?tile, "Cancelled in-flight job");
        }
    }
}

// =============================================================================
// Executor Daemon
// =============================================================================

/// The job executor daemon.
///
/// Owns the job executor and receives requests from producers via channel.
/// Runs as a long-lived background task.
///
/// Uses daemon-level request coalescing to prevent duplicate job creation
/// when multiple consumers (FUSE, Prefetch) request the same tile concurrently.
///
/// # Type Parameters
///
/// * `F` - Factory type for creating DDS generation jobs
/// * `M` - Memory cache type for cache lookups
/// * `DD` - DDS disk cache type for persistent tile lookups
pub struct ExecutorDaemon<F, M, DD>
where
    F: DdsJobFactory,
    M: DaemonMemoryCache,
    DD: super::DdsDiskCache,
{
    /// The job executor.
    executor: JobExecutor,

    /// Job submitter for sending jobs to executor.
    submitter: ExecutorSubmitter,

    /// Factory for creating DDS jobs.
    factory: Arc<F>,

    /// Memory cache for fast-path cache hits.
    memory_cache: Arc<M>,

    /// DDS disk cache for medium-path cache hits (avoids re-encoding).
    dds_disk_cache: Arc<DD>,

    /// Channel receiver for requests.
    request_rx: mpsc::Receiver<JobRequest>,

    /// Optional metrics client for event-based metrics.
    metrics_client: Option<MetricsClient>,

    /// Counts submitted jobs as FUSE demand (demand lane only).
    demand: Option<DemandTracker>,
    /// Wait for executor room instead of shedding (prefetch lane only).
    wait_for_executor: bool,
}

/// The shared dependencies `run` hands to every request, built once.
struct RequestContext<'a, F, M, DD> {
    submitter: &'a ExecutorSubmitter,
    factory: &'a Arc<F>,
    memory_cache: &'a Arc<M>,
    dds_disk_cache: &'a Arc<DD>,
    coalescer: &'a Arc<DaemonCoalescer>,
    metrics_client: &'a Option<MetricsClient>,
    demand: &'a Option<DemandTracker>,
    wait_for_executor: bool,
}

impl<F, M, DD> ExecutorDaemon<F, M, DD>
where
    F: DdsJobFactory,
    M: DaemonMemoryCache,
    DD: super::DdsDiskCache,
{
    /// Creates a new daemon with its channel.
    ///
    /// Returns the daemon and a sender that can be cloned for producers.
    /// Uses a default `TracingTelemetrySink` for logging.
    ///
    /// # Arguments
    ///
    /// * `config` - Daemon configuration
    /// * `factory` - Factory for creating DDS jobs
    /// * `memory_cache` - Memory cache for fast-path lookups
    pub fn new(
        config: ExecutorDaemonConfig,
        factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
    ) -> (Self, mpsc::Sender<JobRequest>) {
        Self::with_telemetry(
            config,
            factory,
            memory_cache,
            dds_disk_cache,
            Arc::new(TracingTelemetrySink),
        )
    }

    /// Creates a new daemon with a custom telemetry sink.
    ///
    /// Returns the daemon and a sender that can be cloned for producers.
    ///
    /// # Arguments
    ///
    /// * `config` - Daemon configuration
    /// * `factory` - Factory for creating DDS jobs
    /// * `memory_cache` - Memory cache for fast-path lookups
    /// * `telemetry` - Telemetry sink for emitting executor events
    pub fn with_telemetry(
        config: ExecutorDaemonConfig,
        factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        telemetry: Arc<dyn TelemetrySink>,
    ) -> (Self, mpsc::Sender<JobRequest>) {
        let (request_tx, request_rx) = mpsc::channel(config.channel_capacity);
        let (executor, submitter) = JobExecutor::with_telemetry(config.executor, telemetry);

        let daemon = Self {
            executor,
            submitter,
            factory,
            memory_cache,
            dds_disk_cache,
            request_rx,
            metrics_client: None,
            demand: None,
            wait_for_executor: config.wait_for_executor,
        };

        (daemon, request_tx)
    }

    /// Creates a new daemon with the event-based metrics client.
    ///
    /// This constructor enables metrics collection via the `MetricsClient`
    /// for fire-and-forget event emission. The client is passed through to
    /// the `JobExecutor` so tasks receive it via `TaskContext`.
    ///
    /// # Arguments
    ///
    /// * `config` - Daemon configuration
    /// * `factory` - Factory for creating DDS jobs
    /// * `memory_cache` - Memory cache for fast-path lookups
    /// * `telemetry` - Telemetry sink for emitting executor events
    /// * `metrics_client` - Metrics client for event emission
    pub fn with_metrics_client(
        config: ExecutorDaemonConfig,
        factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        telemetry: Arc<dyn TelemetrySink>,
        metrics_client: MetricsClient,
    ) -> (Self, mpsc::Sender<JobRequest>) {
        let (request_tx, request_rx) = mpsc::channel(config.channel_capacity);
        // Pass metrics_client to executor so tasks receive it via TaskContext
        let (executor, submitter) = JobExecutor::with_telemetry_and_metrics(
            config.executor,
            telemetry,
            Some(metrics_client.clone()),
        );

        let daemon = Self {
            executor,
            submitter,
            factory,
            memory_cache,
            dds_disk_cache,
            request_rx,
            metrics_client: Some(metrics_client),
            demand: None,
            wait_for_executor: config.wait_for_executor,
        };

        (daemon, request_tx)
    }

    /// Counts this daemon's submitted jobs as FUSE demand (demand lane only).
    pub fn with_demand_tracker(mut self, tracker: DemandTracker) -> Self {
        self.demand = Some(tracker);
        self
    }

    /// Returns a clone of the internal job submitter.
    ///
    /// This allows external components (like the GC scheduler) to submit
    /// arbitrary jobs to the executor. The submitter is cloneable and
    /// can be passed to background tasks.
    ///
    /// # Note
    ///
    /// This must be called before `run()` consumes the daemon.
    pub fn job_submitter(&self) -> ExecutorSubmitter {
        self.submitter.clone()
    }

    /// Jobs running and jobs held waiting for admission, refreshed every
    /// executor loop. The prefetch lane's load and gauges read these; the
    /// stall watchdog deliberately does not.
    pub fn job_counts(&self) -> super::JobCounts {
        self.executor.job_counts.clone()
    }

    /// Returns a shared reference to the executor's resource pools.
    ///
    /// Used by the circuit breaker to monitor pool utilization and trip
    /// when any pool is near saturation.
    pub fn resource_pools(&self) -> Arc<super::resource_pool::ResourcePools> {
        Arc::clone(&self.executor.resource_pools)
    }

    /// Runs the daemon until shutdown is signalled.
    ///
    /// This is the main event loop that:
    /// - Receives new job requests
    /// - Checks cache for hits
    /// - Uses daemon-level coalescing to prevent duplicate jobs
    /// - Runs jobs via the executor
    /// - Returns results to callers
    pub async fn run(self, shutdown: CancellationToken) {
        info!("Executor daemon starting");

        let Self {
            executor,
            submitter,
            factory,
            memory_cache,
            dds_disk_cache,
            mut request_rx,
            metrics_client,
            demand,
            wait_for_executor,
        } = self;

        // Create shared coalescer for daemon-level request deduplication
        let coalescer = Arc::new(DaemonCoalescer::new());

        let ctx = RequestContext {
            submitter: &submitter,
            factory: &factory,
            memory_cache: &memory_cache,
            dds_disk_cache: &dds_disk_cache,
            coalescer: &coalescer,
            metrics_client: &metrics_client,
            demand: &demand,
            wait_for_executor,
        };

        // Spawn the executor in a separate task
        let executor_shutdown = shutdown.clone();
        let executor_handle = tokio::spawn(async move {
            executor.run(executor_shutdown).await;
        });

        // Main request loop — requests are processed inline (cache check +
        // coalescer + job submission are all <10µs). Long-running work (waiting
        // for job completion) is already spawned inside handle_request. This
        // keeps the executor pipeline fed without deferring job submission to
        // the Tokio scheduler, which can starve the pipeline under load.
        loop {
            tokio::select! {
                biased;

                // Check for shutdown
                _ = shutdown.cancelled() => {
                    info!("Executor daemon shutting down");
                    break;
                }

                // Receive new job requests
                Some(request) = request_rx.recv() => {
                    Self::handle_request(request, &ctx).await;
                }
            }
        }

        // Wait for executor to finish
        let _ = executor_handle.await;
        info!("Executor daemon stopped");
    }

    /// Submits `job`, holding a demand guard for as long as it is in flight.
    /// On error the guard is dropped here, which is how the shed and
    /// failed-submit exits release their demand. With `wait_for_executor`
    /// the submit waits for queue room and never sheds.
    async fn submit_tracked(
        submitter: &ExecutorSubmitter,
        job: Box<dyn Job>,
        demand: &Option<DemandTracker>,
        wait_for_executor: bool,
    ) -> Result<(JobHandle, Option<DemandGuard>), SubmitError> {
        let guard = demand.as_ref().map(DemandTracker::begin);
        let submitted = if wait_for_executor {
            submitter.submit_boxed_waiting(job).await
        } else {
            submitter.try_submit_boxed(job)
        };
        submitted.map(|handle| (handle, guard))
    }

    async fn handle_request(request: JobRequest, ctx: &RequestContext<'_, F, M, DD>) {
        let RequestContext {
            submitter,
            factory,
            memory_cache,
            dds_disk_cache,
            coalescer,
            metrics_client,
            demand,
            wait_for_executor,
        } = *ctx;
        // The producer's clock, so every duration below counts the time the
        // request sat queued on the channel: X-Plane was already waiting.
        let start = request.created;
        let tile = request.tile;
        let priority = request.priority;
        let origin = request.origin;

        // Calculate geographic context for analysis logging
        let (tile_lat, tile_lon) = tile.to_lat_lon();
        let dsf_tile = tile.to_dsf_tile_name();

        debug!(
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            tile_lat = format!("{:.4}", tile_lat),
            tile_lon = format!("{:.4}", tile_lon),
            dsf_tile = %dsf_tile,
            priority = ?priority,
            origin = %origin,
            "DDS request received"
        );

        // Check for cancellation first
        if request.cancellation.is_cancelled() {
            debug!(tile = ?tile, "Request already cancelled");
            // A FUSE request cancelled before the daemon reached it timed out
            // while queued on the channel. X-Plane waited the whole time, so
            // it is reported like any other FUSE timeout; otherwise queue
            // stalls would vanish from the no-harm measure.
            if origin.is_fuse() {
                if let Some(client) = metrics_client {
                    client.job_timed_out(start.elapsed().as_micros() as u64);
                }
            }
            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::empty(start.elapsed()));
            }
            return;
        }

        // Fast path: check memory cache first
        let cache_key = format!("tile:{}:{}:{}", tile.zoom, tile.row, tile.col);
        debug!(
            cache_key = %cache_key,
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            origin = %origin,
            "Executor: checking cache tiers"
        );
        let cache_result = memory_cache
            .get(tile.row, tile.col, tile.zoom)
            .instrument(tracing::trace_span!(target: "profiling", "cache_check"))
            .await;
        if let Some(data) = cache_result {
            let duration = start.elapsed();
            debug!(
                tile_row = tile.row,
                tile_col = tile.col,
                tile_zoom = tile.zoom,
                dsf_tile = %dsf_tile,
                origin = %origin,
                cache_status = "hit",
                latency_ms = duration.as_millis(),
                data_size = data.len(),
                "DDS request completed"
            );

            // Track cache hit
            if let Some(client) = metrics_client {
                client.memory_cache_hit(origin.is_fuse());
                if origin.is_fuse() {
                    client.fuse_tile_served();
                }
            }

            #[cfg(feature = "debug-map")]
            record_debug_tile_activity(
                tile_lat,
                tile_lon,
                tile.row,
                tile.col,
                tile.zoom,
                origin.is_fuse(),
                crate::debug_map::activity::TileCacheResult::CacheHit,
            );

            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::cache_hit(data, duration));
            }
            return;
        }

        // Medium path: check DDS disk cache (avoids re-encoding)
        let dds_disk_result = dds_disk_cache
            .get(tile.row, tile.col, tile.zoom)
            .instrument(tracing::trace_span!(target: "profiling", "dds_disk_cache_check"))
            .await;
        if dds_disk_result.is_none() {
            // Track DDS disk cache miss in metrics.
            // Passes origin.is_fuse() so the daemon can maintain a FUSE-only
            // counter separate from prefetch/prewarm traffic (see #171).
            if let Some(client) = metrics_client {
                client.dds_disk_cache_miss(origin.is_fuse());
            }
            debug!(
                tile_row = tile.row,
                tile_col = tile.col,
                tile_zoom = tile.zoom,
                dsf_tile = %dsf_tile,
                origin = %origin,
                "DDS disk cache miss"
            );
        }
        if let Some(data) = dds_disk_result {
            let duration = start.elapsed();
            debug!(
                tile_row = tile.row,
                tile_col = tile.col,
                tile_zoom = tile.zoom,
                dsf_tile = %dsf_tile,
                origin = %origin,
                cache_status = "dds_disk_hit",
                latency_ms = duration.as_millis(),
                data_size = data.len(),
                "DDS disk cache hit — serving without re-encode"
            );

            // Track DDS disk cache hit in metrics.
            // Passes origin.is_fuse() so the daemon can maintain a FUSE-only
            // counter separate from prefetch/prewarm traffic (see #171).
            if let Some(client) = metrics_client {
                client.dds_disk_cache_hit(data.len() as u64, origin.is_fuse());
                if origin.is_fuse() {
                    client.fuse_tile_served();
                }
            }

            #[cfg(feature = "debug-map")]
            record_debug_tile_activity(
                tile_lat,
                tile_lon,
                tile.row,
                tile.col,
                tile.zoom,
                origin.is_fuse(),
                crate::debug_map::activity::TileCacheResult::CacheHit,
            );

            // Promote back to memory cache (fire-and-forget).
            // Size metrics are reported internally by the provider.
            let cache = Arc::clone(memory_cache);
            let data_for_promote = data.clone();
            let tile_for_promote = tile;
            tokio::spawn(async move {
                cache
                    .put(
                        tile_for_promote.row,
                        tile_for_promote.col,
                        tile_for_promote.zoom,
                        data_for_promote,
                    )
                    .await;
            });

            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::cache_hit(data, duration));
            }
            return;
        }

        // Check coalescer for in-flight job
        match coalescer.register(tile) {
            Ok(mut receiver) => {
                // Job already in-flight, wait for result
                debug!(
                    tile_row = tile.row,
                    tile_col = tile.col,
                    tile_zoom = tile.zoom,
                    dsf_tile = %dsf_tile,
                    origin = %origin,
                    "Coalesced with in-flight job"
                );

                // Spawn task to wait for coalesced result
                tokio::spawn(async move {
                    match receiver.recv().await {
                        Ok(result) => {
                            if let Some(tx) = request.response_tx {
                                let _ = tx.send(result.into_response(false));
                            }
                        }
                        Err(_) => {
                            // Channel closed without result - job was cancelled or failed
                            if let Some(tx) = request.response_tx {
                                let _ = tx.send(DdsResponse::empty(start.elapsed()));
                            }
                        }
                    }
                });
                return;
            }
            Err(_sender) => {
                // New request, proceed with job creation
                // (sender is kept in coalescer's DashMap)
            }
        }

        // Track cache miss
        debug!(
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            dsf_tile = %dsf_tile,
            origin = %origin,
            cache_status = "miss",
            "Cache miss - submitting job"
        );
        if let Some(client) = metrics_client {
            client.memory_cache_miss(origin.is_fuse());
        }

        // Create and submit job
        let job = factory.create_job(tile, priority);
        let job_id = job.id();
        debug!(job_id = %job_id, tile = ?tile, "Created DDS generation job");

        let handle = Self::submit_tracked(submitter, job, demand, wait_for_executor)
            .instrument(tracing::trace_span!(target: "profiling", "job_submit"))
            .await;

        match handle {
            Ok((mut handle, demand_guard)) => {
                debug!(job_id = %job_id, "Job submitted to executor");

                // Track job submitted (FUSE requests are from RequestOrigin::Fuse)
                let is_fuse = origin.is_fuse();
                if let Some(client) = metrics_client {
                    client.job_submitted(is_fuse);
                }

                let memory_cache = Arc::clone(memory_cache);
                let dds_disk_cache = Arc::clone(dds_disk_cache);
                let coalescer = Arc::clone(coalescer);
                let cancellation = request.cancellation.clone();
                let metrics_for_completion = metrics_client.clone();
                let dsf_tile_for_log = dsf_tile.clone();

                tokio::spawn(async move {
                    // Released when this task ends, on the success, failure
                    // and cancellation arms alike.
                    let _demand_guard = demand_guard;

                    // Wait for job completion
                    tokio::select! {
                        job_result = handle.wait() => {
                            let status = handle.status();
                            let duration = start.elapsed();

                            // Track job completion
                            let success = status == JobStatus::Succeeded;
                            if let Some(ref client) = metrics_for_completion {
                                client.job_completed(success, duration.as_micros() as u64, origin.is_fuse());
                                if origin.is_fuse() {
                                    client.fuse_tile_served();
                                }
                            }

                            // Get DDS data directly from job result (avoids cache race conditions)
                            let (data, served_from_cache) = if success {
                                match job_result.output_data {
                                    Some(d) => {
                                        debug!(
                                            tile_row = tile.row,
                                            tile_col = tile.col,
                                            tile_zoom = tile.zoom,
                                            dsf_tile = %dsf_tile_for_log,
                                            origin = %origin,
                                            cache_status = "generated",
                                            latency_ms = duration.as_millis(),
                                            data_size = d.len(),
                                            "DDS request completed"
                                        );

                                        #[cfg(feature = "debug-map")]
                                        record_debug_tile_activity(
                                            tile_lat, tile_lon, tile.row, tile.col, tile.zoom,
                                            origin.is_fuse(),
                                            crate::debug_map::activity::TileCacheResult::Generated,
                                        );

                                        (d, false)
                                    }
                                    // A job skipped by the prefetch lane's cache probe
                                    // succeeds without output: its tile was cached while it
                                    // waited, often on DDS disk only. With nobody to answer,
                                    // reading the tile back would cost a whole-tile read for
                                    // nothing. A waiter that joins after this check is told
                                    // the job succeeded, which is true.
                                    None if request.response_tx.is_none()
                                        && !coalescer.has_waiters(tile) =>
                                    {
                                        (Bytes::new(), true)
                                    }
                                    None => {
                                        let cached = match memory_cache
                                            .get(tile.row, tile.col, tile.zoom)
                                            .await
                                        {
                                            Some(d) => Some(d),
                                            None => {
                                                dds_disk_cache
                                                    .get(tile.row, tile.col, tile.zoom)
                                                    .await
                                            }
                                        };
                                        match cached {
                                            Some(d) => (d, true),
                                            None => {
                                                warn!(
                                                    tile_row = tile.row,
                                                    tile_col = tile.col,
                                                    tile_zoom = tile.zoom,
                                                    dsf_tile = %dsf_tile_for_log,
                                                    duration_ms = duration.as_millis(),
                                                    "Job succeeded with no output and the tile is in no cache"
                                                );
                                                (Bytes::new(), false)
                                            }
                                        }
                                    }
                                }
                            } else {
                                warn!(
                                    tile_row = tile.row,
                                    tile_col = tile.col,
                                    tile_zoom = tile.zoom,
                                    dsf_tile = %dsf_tile_for_log,
                                    latency_ms = duration.as_millis(),
                                    "Job failed - returning empty data"
                                );
                                (Bytes::new(), false)
                            };

                            // Broadcast result to coalesced waiters
                            let coalesced_result = CoalescedDdsResult::new(
                                data.clone(),
                                duration,
                                success,
                            );
                            coalescer.complete(tile, coalesced_result);

                            // Create response with job_succeeded flag set correctly
                            let response = if served_from_cache {
                                DdsResponse::cache_hit(data, duration)
                            } else {
                                DdsResponse::new(data, false, duration, success)
                            };

                            // Send response if requested
                            if let Some(tx) = request.response_tx {
                                let _ = tx.send(response);
                            }
                        }
                        _ = cancellation.cancelled() => {
                            debug!(
                                tile_row = tile.row,
                                tile_col = tile.col,
                                tile_zoom = tile.zoom,
                                dsf_tile = %dsf_tile_for_log,
                                latency_ms = start.elapsed().as_millis(),
                                "DDS request cancelled"
                            );
                            handle.kill();

                            // A cancelled FUSE request is a FUSE timeout. Its
                            // wait feeds the latency window: a stall must not
                            // vanish from the no-harm measure.
                            if origin.is_fuse() {
                                if let Some(ref client) = metrics_for_completion {
                                    client.job_timed_out(start.elapsed().as_micros() as u64);
                                }
                            }

                            // Remove from coalescer so other waiters know job was cancelled
                            coalescer.cancel(tile);

                            if let Some(tx) = request.response_tx {
                                let _ = tx.send(DdsResponse::empty(start.elapsed()));
                            }
                        }
                    }
                });
            }
            Err(err) if err.is_back_pressure() => {
                // Not a failure. The executor's queue is full, so this tile is
                // shed: the requester gets an empty answer now and asks again
                // if it still wants the tile. Only a daemon without
                // `wait_for_executor` sheds, which is the demand lane; FUSE
                // re-requests a tile it still needs. The prefetch lane waits
                // instead, because nothing re-requested a shed prefetch tile.
                // Reporting this as a possible shutdown told operators their
                // executor had died while it was working normally (#272).
                debug!(
                    tile = ?tile,
                    reason = %err,
                    "Job shed at the executor boundary under back pressure"
                );

                // Remove from coalescer
                coalescer.cancel(tile);

                if let Some(tx) = request.response_tx {
                    let _ = tx.send(DdsResponse::empty(start.elapsed()));
                }
            }
            Err(err) => {
                warn!(tile = ?tile, reason = %err, "Failed to submit job - executor may be shutdown");

                // Remove from coalescer
                coalescer.cancel(tile);

                if let Some(tx) = request.response_tx {
                    let _ = tx.send(DdsResponse::empty(start.elapsed()));
                }
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::{ErrorPolicy, Job, JobId, JobResult, Priority, Task, TaskContext};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    fn test_tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }
    }

    /// Mock memory cache for testing
    struct MockMemoryCache {
        data: Mutex<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockMemoryCache {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }

        fn insert(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
            self.data.lock().unwrap().insert((row, col, zoom), data);
        }
    }

    impl DaemonMemoryCache for MockMemoryCache {
        fn get(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Bytes>> + Send + '_>>
        {
            let data = self.data.lock().unwrap().get(&(row, col, zoom)).cloned();
            Box::pin(async move { data.map(Bytes::from) })
        }

        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            data: Bytes,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            self.data
                .lock()
                .unwrap()
                .insert((row, col, zoom), data.to_vec());
            Box::pin(async {})
        }
    }

    /// Mock job factory that tracks job creation count
    struct MockJobFactory {
        jobs_created: AtomicUsize,
        fail: bool,
    }

    impl MockJobFactory {
        fn new() -> Self {
            Self {
                jobs_created: AtomicUsize::new(0),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                jobs_created: AtomicUsize::new(0),
                fail: true,
            }
        }

        fn jobs_created(&self) -> usize {
            self.jobs_created.load(Ordering::SeqCst)
        }
    }

    /// Mock job that simulates work with configurable delay and output
    struct MockJob {
        id: JobId,
        priority: Priority,
        tile: TileCoord,
        delay: Duration,
        fail: bool,
    }

    impl MockJob {
        fn new(tile: TileCoord, priority: Priority, delay: Duration) -> Self {
            Self {
                id: JobId::new(format!("mock-{}_{}_ZL{}", tile.row, tile.col, tile.zoom)),
                priority,
                tile,
                delay,
                fail: false,
            }
        }
    }

    /// Task that always fails, for the failure exit of a job.
    struct FailingMockTask;

    impl Task for FailingMockTask {
        fn name(&self) -> &str {
            "BuildAndCacheDds"
        }

        fn resource_type(&self) -> crate::executor::ResourceType {
            crate::executor::ResourceType::CPU
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a mut TaskContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::executor::TaskResult> + Send + 'a>,
        > {
            Box::pin(async {
                crate::executor::TaskResult::Failed(crate::executor::TaskError::new("mock failure"))
            })
        }
    }

    impl Job for MockJob {
        fn id(&self) -> JobId {
            self.id.clone()
        }

        fn name(&self) -> &str {
            "MockDdsGenerate"
        }

        fn error_policy(&self) -> ErrorPolicy {
            ErrorPolicy::FailFast
        }

        fn priority(&self) -> Priority {
            self.priority
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            if self.fail {
                return vec![Box::new(FailingMockTask)];
            }
            // Create a task that simulates work and produces output
            vec![Box::new(MockDdsTask {
                tile: self.tile,
                delay: self.delay,
            })]
        }

        fn on_complete(&self, result: &JobResult) -> JobStatus {
            if result.failed_tasks.is_empty() {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
    }

    /// Mock task that simulates DDS generation with delay
    struct MockDdsTask {
        tile: TileCoord,
        delay: Duration,
    }

    impl Task for MockDdsTask {
        fn name(&self) -> &str {
            // Must match the name used in executor.rs:348 to extract output_data
            "BuildAndCacheDds"
        }

        fn resource_type(&self) -> crate::executor::ResourceType {
            crate::executor::ResourceType::CPU
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a mut TaskContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::executor::TaskResult> + Send + 'a>,
        > {
            let delay = self.delay;
            let tile = self.tile;
            Box::pin(async move {
                // Simulate work
                tokio::time::sleep(delay).await;
                // Return DDS data
                let mut output = crate::executor::TaskOutput::new();
                // Must be Bytes: `TaskOutput::get` is a typed downcast, so a
                // mock storing Vec<u8> silently yields None to the real reader.
                let data = Bytes::from(
                    format!("DDS-{}-{}-{}", tile.row, tile.col, tile.zoom).into_bytes(),
                );
                output.set("dds_data", data);
                crate::executor::TaskResult::SuccessWithOutput(output)
            })
        }
    }

    impl DdsJobFactory for MockJobFactory {
        fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job> {
            self.jobs_created.fetch_add(1, Ordering::SeqCst);
            // Use 50ms delay to simulate real job timing
            let mut job = MockJob::new(tile, priority, Duration::from_millis(50));
            job.fail = self.fail;
            Box::new(job)
        }
    }

    type TestDaemon =
        ExecutorDaemon<MockJobFactory, MockMemoryCache, crate::executor::NullDdsDiskCache>;

    fn demand_daemon(
        factory: Arc<MockJobFactory>,
    ) -> (TestDaemon, mpsc::Sender<JobRequest>, DemandTracker) {
        let tracker = DemandTracker::new();
        let (daemon, tx) = ExecutorDaemon::new(
            ExecutorDaemonConfig::default(),
            factory,
            Arc::new(MockMemoryCache::new()),
            Arc::new(super::super::NullDdsDiskCache),
        );
        (daemon.with_demand_tracker(tracker.clone()), tx, tracker)
    }

    async fn settle(tracker: &DemandTracker) {
        for _ in 0..200 {
            if tracker.in_flight() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "demand count never returned to zero: {}",
            tracker.in_flight()
        );
    }

    #[tokio::test]
    async fn test_demand_count_returns_to_zero_after_success() {
        let (daemon, tx, tracker) = demand_daemon(Arc::new(MockJobFactory::new()));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));
        let (request, rx) = JobRequest::fuse(test_tile(), CancellationToken::new());
        tx.send(request).await.unwrap();
        assert!(rx.await.unwrap().is_success());
        settle(&tracker).await;
        shutdown.cancel();
        let _ = run.await;
    }

    #[tokio::test]
    async fn test_demand_count_returns_to_zero_after_failure() {
        let (daemon, tx, tracker) = demand_daemon(Arc::new(MockJobFactory::failing()));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));
        let (request, rx) = JobRequest::fuse(test_tile(), CancellationToken::new());
        tx.send(request).await.unwrap();
        assert!(!rx.await.unwrap().is_success());
        settle(&tracker).await;
        shutdown.cancel();
        let _ = run.await;
    }

    #[tokio::test]
    async fn test_demand_count_returns_to_zero_after_cancellation() {
        let (daemon, tx, tracker) = demand_daemon(Arc::new(MockJobFactory::new()));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));
        let token = CancellationToken::new();
        let (request, _rx) = JobRequest::fuse(test_tile(), token.clone());
        tx.send(request).await.unwrap();
        for _ in 0..200 {
            if tracker.in_flight() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            tracker.in_flight(),
            1,
            "the job must be counted while it runs"
        );
        token.cancel();
        settle(&tracker).await;
        shutdown.cancel();
        let _ = run.await;
    }

    fn mock_job() -> Box<dyn Job> {
        Box::new(MockJob::new(
            test_tile(),
            Priority::ON_DEMAND,
            Duration::ZERO,
        ))
    }

    #[tokio::test]
    async fn test_shed_submit_releases_demand() {
        let config = ExecutorConfig {
            job_channel_capacity: 1,
            ..Default::default()
        };
        let (executor, submitter) = JobExecutor::new(config);
        let tracker = DemandTracker::new();
        let demand = Some(tracker.clone());
        let _first = TestDaemon::submit_tracked(&submitter, mock_job(), &demand, false)
            .await
            .unwrap();
        let shed = TestDaemon::submit_tracked(&submitter, mock_job(), &demand, false).await;
        assert!(shed
            .err()
            .expect("second submit is shed")
            .is_back_pressure());
        assert_eq!(tracker.in_flight(), 1, "only the accepted job is in flight");
        drop(executor);
    }

    #[tokio::test]
    async fn test_failed_submit_releases_demand() {
        let (executor, submitter) = JobExecutor::new(Default::default());
        drop(executor);
        let tracker = DemandTracker::new();
        let result =
            TestDaemon::submit_tracked(&submitter, mock_job(), &Some(tracker.clone()), false).await;
        assert!(result.is_err());
        assert_eq!(tracker.in_flight(), 0);
    }

    /// The prefetch lane waits for room instead of shedding: a shed prefetch
    /// job was lost without the coordinator knowing, so its tile was counted
    /// as submitted and never requested again (2,871 sheds on one stressed
    /// flight, 66 of them later timing out under FUSE). Waiting holds the
    /// daemon, the lane's request channel fills, and the coordinator keeps
    /// the tile pending.
    #[tokio::test]
    async fn test_waiting_submit_holds_a_job_until_the_executor_has_room() {
        let config = ExecutorConfig {
            job_channel_capacity: 1,
            ..Default::default()
        };
        let (mut executor, submitter) = JobExecutor::new(config);
        let _first = TestDaemon::submit_tracked(&submitter, mock_job(), &None, true)
            .await
            .unwrap();

        let waiting_submitter = submitter.clone();
        let second = tokio::spawn(async move {
            TestDaemon::submit_tracked(&waiting_submitter, mock_job(), &None, true)
                .await
                .map(|_| ())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "with the channel full, a waiting submit must hold, not shed"
        );

        executor
            .job_receiver
            .recv()
            .await
            .expect("first job queued");
        let accepted = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("room freed, the waiting submit must complete")
            .unwrap();
        assert!(accepted.is_ok(), "the held job is accepted, not lost");
    }

    #[tokio::test]
    async fn test_waiting_submit_fails_only_when_the_executor_is_gone() {
        let (executor, submitter) = JobExecutor::new(Default::default());
        drop(executor);
        let result = TestDaemon::submit_tracked(&submitter, mock_job(), &None, true).await;
        assert!(matches!(result, Err(err) if !err.is_back_pressure()));
    }

    /// DDS disk cache holding at most one tile, written by the job itself.
    /// Counts reads, because a whole-tile read nobody wanted is the cost.
    #[derive(Default)]
    struct OneTileDdsDisk {
        tile: Mutex<Option<(TileCoord, Bytes)>>,
        gets: AtomicUsize,
    }

    impl OneTileDdsDisk {
        fn holds_a_tile(&self) -> bool {
            self.tile.lock().unwrap().is_some()
        }
    }

    impl crate::executor::DdsDiskCache for OneTileDdsDisk {
        async fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Bytes> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            let held = self.tile.lock().unwrap().clone();
            held.filter(|(t, _)| (t.row, t.col, t.zoom) == (row, col, zoom))
                .map(|(_, data)| data)
        }

        async fn put(&self, row: u32, col: u32, zoom: u8, data: Bytes) {
            *self.tile.lock().unwrap() = Some((TileCoord { row, col, zoom }, data));
        }

        async fn contains(&self, row: u32, col: u32, zoom: u8) -> bool {
            let held = self.tile.lock().unwrap().clone();
            held.is_some_and(|(t, _)| (t.row, t.col, t.zoom) == (row, col, zoom))
        }
    }

    /// Stands in for a probe-skipped job: while it waited, another lane put
    /// its tile in the DDS disk cache, so it succeeds with no output.
    struct SkippedByProbeTask {
        disk: Arc<OneTileDdsDisk>,
        tile: TileCoord,
    }

    impl Task for SkippedByProbeTask {
        fn name(&self) -> &str {
            "CachedTileProbe"
        }

        fn resource_type(&self) -> crate::executor::ResourceType {
            crate::executor::ResourceType::CPU
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a mut TaskContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::executor::TaskResult> + Send + 'a>,
        > {
            Box::pin(async move {
                use crate::executor::DdsDiskCache;
                let t = self.tile;
                self.disk
                    .put(t.row, t.col, t.zoom, Bytes::from_static(b"ON-DISK"))
                    .await;
                crate::executor::TaskResult::Success
            })
        }
    }

    struct SkippedByProbeJob {
        disk: Arc<OneTileDdsDisk>,
        tile: TileCoord,
    }

    impl Job for SkippedByProbeJob {
        fn id(&self) -> JobId {
            JobId::new("skipped-by-probe")
        }

        fn name(&self) -> &str {
            "SkippedByProbe"
        }

        fn priority(&self) -> Priority {
            Priority::PREFETCH
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            vec![Box::new(SkippedByProbeTask {
                disk: Arc::clone(&self.disk),
                tile: self.tile,
            })]
        }

        fn on_complete(&self, _result: &JobResult) -> JobStatus {
            JobStatus::Succeeded
        }
    }

    struct SkippedByProbeFactory {
        disk: Arc<OneTileDdsDisk>,
    }

    impl DdsJobFactory for SkippedByProbeFactory {
        fn create_job(&self, tile: TileCoord, _priority: Priority) -> Box<dyn Job> {
            Box::new(SkippedByProbeJob {
                disk: Arc::clone(&self.disk),
                tile,
            })
        }
    }

    #[tokio::test]
    async fn test_probe_skipped_job_is_served_from_dds_disk_as_a_cache_hit() {
        let disk = Arc::new(OneTileDdsDisk::default());
        let (daemon, tx) = ExecutorDaemon::new(
            ExecutorDaemonConfig::default(),
            Arc::new(SkippedByProbeFactory {
                disk: Arc::clone(&disk),
            }),
            Arc::new(MockMemoryCache::new()),
            disk,
        );
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));

        let (request, rx) = JobRequest::prewarm(test_tile());
        tx.send(request).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(response.data, Bytes::from_static(b"ON-DISK"));
        assert!(response.cache_hit, "a skipped job reports AlreadyCached");
        assert!(response.is_success());

        shutdown.cancel();
        let _ = run.await;
    }

    /// X-Plane starts waiting when FUSE makes the request, not when the
    /// daemon takes it off the channel, so time spent queued is latency.
    #[tokio::test]
    async fn test_fuse_latency_includes_time_queued_before_the_daemon() {
        use crate::metrics::MetricEvent;
        const QUEUED: Duration = Duration::from_millis(150);

        let (metrics_tx, mut metrics_rx) = mpsc::unbounded_channel();
        let (daemon, tx) = ExecutorDaemon::with_metrics_client(
            ExecutorDaemonConfig::default(),
            Arc::new(MockJobFactory::new()),
            Arc::new(MockMemoryCache::new()),
            Arc::new(super::super::NullDdsDiskCache),
            Arc::new(crate::executor::NullTelemetrySink),
            MetricsClient::new(metrics_tx),
        );
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));

        let (request, rx) = JobRequest::fuse(test_tile(), CancellationToken::new());
        tokio::time::sleep(QUEUED).await;
        tx.send(request).await.unwrap();
        assert!(rx.await.unwrap().is_success());

        let mut latency_us = None;
        while let Ok(event) = metrics_rx.try_recv() {
            if let MetricEvent::JobCompleted {
                duration_us,
                is_fuse: true,
                ..
            } = event
            {
                latency_us = Some(duration_us);
            }
        }
        let latency = Duration::from_micros(latency_us.expect("a FUSE job completed"));
        assert!(
            latency >= QUEUED,
            "latency {latency:?} must include the {QUEUED:?} the request sat queued"
        );

        shutdown.cancel();
        let _ = run.await;
    }

    /// A FUSE request can time out while still queued on the demand channel,
    /// before the daemon reaches it. X-Plane waited the whole time and got a
    /// placeholder, so it is a timeout like any other: if it went unreported,
    /// queue stalls would vanish from the no-harm measure.
    #[tokio::test]
    async fn test_fuse_request_cancelled_while_queued_reports_a_timeout() {
        use crate::metrics::MetricEvent;
        const QUEUED: Duration = Duration::from_millis(150);

        let (metrics_tx, mut metrics_rx) = mpsc::unbounded_channel();
        let (daemon, tx) = ExecutorDaemon::with_metrics_client(
            ExecutorDaemonConfig::default(),
            Arc::new(MockJobFactory::new()),
            Arc::new(MockMemoryCache::new()),
            Arc::new(super::super::NullDdsDiskCache),
            Arc::new(crate::executor::NullTelemetrySink),
            MetricsClient::new(metrics_tx),
        );
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));

        let fuse_token = CancellationToken::new();
        let (fuse_request, fuse_rx) = JobRequest::fuse(test_tile(), fuse_token.clone());
        let prefetch_token = CancellationToken::new();
        let prefetch_request = JobRequest {
            cancellation: prefetch_token.clone(),
            ..JobRequest::prefetch(TileCoord {
                row: 101,
                ..test_tile()
            })
        };
        tokio::time::sleep(QUEUED).await;
        fuse_token.cancel();
        prefetch_token.cancel();
        tx.send(fuse_request).await.unwrap();
        tx.send(prefetch_request).await.unwrap();
        assert!(!fuse_rx.await.unwrap().is_success());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let timeouts: Vec<u64> = std::iter::from_fn(|| metrics_rx.try_recv().ok())
            .filter_map(|event| match event {
                MetricEvent::JobTimedOut { duration_us } => Some(duration_us),
                _ => None,
            })
            .collect();
        assert_eq!(
            timeouts.len(),
            1,
            "the queued FUSE request is one timeout; a cancelled prefetch request is none"
        );
        assert!(
            Duration::from_micros(timeouts[0]) >= QUEUED,
            "the timeout must carry the {QUEUED:?} the request sat queued"
        );

        shutdown.cancel();
        let _ = run.await;
    }

    /// With no response channel and no coalesced waiter, nobody wants the
    /// skipped job's tile, so reading it back from disk is pure cost.
    #[tokio::test]
    async fn test_probe_skipped_job_without_listeners_reads_nothing_back() {
        let disk = Arc::new(OneTileDdsDisk::default());
        let (daemon, tx) = ExecutorDaemon::new(
            ExecutorDaemonConfig::default(),
            Arc::new(SkippedByProbeFactory {
                disk: Arc::clone(&disk),
            }),
            Arc::new(MockMemoryCache::new()),
            Arc::clone(&disk),
        );
        let counts = daemon.job_counts();
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(daemon.run(shutdown.clone()));

        tx.send(JobRequest::prefetch(test_tile())).await.unwrap();
        for _ in 0..200 {
            if disk.holds_a_tile() && counts.running() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(disk.holds_a_tile(), "the job ran");
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            disk.gets.load(Ordering::SeqCst),
            1,
            "only the admission check reads the disk tier"
        );

        shutdown.cancel();
        let _ = run.await;
    }

    #[test]
    fn test_config_default() {
        let config = ExecutorDaemonConfig::default();
        assert_eq!(config.channel_capacity, DEFAULT_REQUEST_CHANNEL_CAPACITY);
    }

    #[tokio::test]
    async fn test_daemon_creation() {
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let (daemon, tx) = ExecutorDaemon::new(
            ExecutorDaemonConfig::default(),
            factory,
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        // Verify channel is open
        assert!(!tx.is_closed());

        // We can't easily verify the daemon's internal state without running it
        // Just verify it was created successfully
        drop(daemon);
    }

    #[tokio::test]
    async fn test_cache_hit_fast_path() {
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        // Pre-populate cache
        let tile = test_tile();
        cache.insert(tile.row, tile.col, tile.zoom, vec![1, 2, 3]);

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            factory,
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // Start daemon
        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // Send request
        let (request, rx) = JobRequest::fuse(tile, CancellationToken::new());
        tx.send(request).await.unwrap();

        // Should get cache hit response quickly
        let response = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(response.data, vec![1, 2, 3]);
        assert!(response.cache_hit);

        // Shutdown
        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn test_prefetch_request_no_response() {
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            factory,
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // Send prefetch request (no response channel)
        let request = JobRequest::prefetch(test_tile());
        tx.send(request).await.unwrap();

        // Give it a moment to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown
        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn test_cancelled_request_returns_empty() {
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            factory,
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // Create already-cancelled request
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (request, rx) = JobRequest::fuse(test_tile(), cancellation);
        tx.send(request).await.unwrap();

        // Should get empty response
        let response = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();

        assert!(!response.has_data());

        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    // =========================================================================
    // Request Coalescing Tests (Bug 4)
    // =========================================================================
    //
    // These tests verify that concurrent requests for the same tile are
    // coalesced into a single job execution.

    #[tokio::test]
    async fn test_concurrent_requests_same_tile_create_one_job() {
        // GIVEN: A daemon with a factory that tracks job creation count
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            Arc::clone(&factory),
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // WHEN: We send 5 concurrent requests for the same tile
        let tile = test_tile();
        let mut receivers = Vec::new();

        for _ in 0..5 {
            let (request, rx) = JobRequest::fuse(tile, CancellationToken::new());
            tx.send(request).await.unwrap();
            receivers.push(rx);
        }

        // Wait for all responses
        for rx in receivers {
            let response = tokio::time::timeout(Duration::from_secs(2), rx)
                .await
                .expect("Response timeout")
                .expect("Response channel closed");

            // All should get valid data
            assert!(response.has_data(), "All waiters should receive data");
        }

        // THEN: Only ONE job should have been created (coalescing worked)
        assert_eq!(
            factory.jobs_created(),
            1,
            "Concurrent requests for same tile should create only one job"
        );

        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn test_different_tiles_create_separate_jobs() {
        // GIVEN: A daemon with a factory that tracks job creation count
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            Arc::clone(&factory),
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // WHEN: We send requests for 3 different tiles
        let mut receivers = Vec::new();

        for i in 0..3 {
            let tile = TileCoord {
                row: 100 + i,
                col: 200,
                zoom: 14,
            };
            let (request, rx) = JobRequest::fuse(tile, CancellationToken::new());
            tx.send(request).await.unwrap();
            receivers.push(rx);
        }

        // Wait for all responses
        for rx in receivers {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx)
                .await
                .expect("Response timeout")
                .expect("Response channel closed");
        }

        // THEN: 3 separate jobs should have been created
        assert_eq!(
            factory.jobs_created(),
            3,
            "Different tiles should each create their own job"
        );

        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn test_coalesced_requests_all_receive_same_result() {
        // GIVEN: A daemon
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            Arc::clone(&factory),
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        // WHEN: We send concurrent requests for the same tile
        let tile = test_tile();
        let mut receivers = Vec::new();

        for _ in 0..3 {
            let (request, rx) = JobRequest::fuse(tile, CancellationToken::new());
            tx.send(request).await.unwrap();
            receivers.push(rx);
        }

        // Collect all responses
        let mut responses = Vec::new();
        for rx in receivers {
            let response = tokio::time::timeout(Duration::from_secs(2), rx)
                .await
                .expect("Response timeout")
                .expect("Response channel closed");
            responses.push(response);
        }

        // THEN: All responses should have the same data
        let first_data = &responses[0].data;
        for response in &responses[1..] {
            assert_eq!(
                &response.data, first_data,
                "All coalesced requests should receive identical data"
            );
        }

        shutdown.cancel();
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn test_sequential_requests_after_completion_create_new_jobs() {
        // GIVEN: A daemon
        let factory = Arc::new(MockJobFactory::new());
        let cache = Arc::new(MockMemoryCache::new());

        let config = ExecutorDaemonConfig::default();
        let (daemon, tx) = ExecutorDaemon::new(
            config,
            Arc::clone(&factory),
            cache,
            Arc::new(super::super::NullDdsDiskCache),
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let daemon_handle = tokio::spawn(async move {
            daemon.run(shutdown_clone).await;
        });

        let tile = test_tile();

        // WHEN: We send a request, wait for it to complete, then send another
        let (request1, rx1) = JobRequest::fuse(tile, CancellationToken::new());
        tx.send(request1).await.unwrap();

        let response1 = tokio::time::timeout(Duration::from_secs(2), rx1)
            .await
            .expect("Response 1 timeout")
            .expect("Response 1 channel closed");
        assert!(response1.has_data());

        // First job completed, now send second request
        let (request2, rx2) = JobRequest::fuse(tile, CancellationToken::new());
        tx.send(request2).await.unwrap();

        let response2 = tokio::time::timeout(Duration::from_secs(2), rx2)
            .await
            .expect("Response 2 timeout")
            .expect("Response 2 channel closed");
        assert!(response2.has_data());

        // THEN: Two separate jobs should have been created
        // (unless cache hit on second - but our mock cache is empty)
        assert_eq!(
            factory.jobs_created(),
            2,
            "Sequential requests after completion should create new jobs"
        );

        shutdown.cancel();
        let _ = daemon_handle.await;
    }
}
