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
//! let (daemon, request_tx) = ExecutorDaemon::new(config, factory, memory_cache);
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
    ExecutorConfig, JobExecutor, JobStatus, JobSubmitter as ExecutorSubmitter, TelemetrySink,
    TracingTelemetrySink,
};
use crate::jobs::DdsJobFactory;
use crate::metrics::MetricsClient;
use crate::runtime::{DdsResponse, JobRequest};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
}

impl Default for ExecutorDaemonConfig {
    fn default() -> Self {
        Self {
            executor: ExecutorConfig::default(),
            channel_capacity: DEFAULT_REQUEST_CHANNEL_CAPACITY,
        }
    }
}

impl From<&crate::config::ExecutorSettings> for ExecutorDaemonConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            executor: ExecutorConfig::from(settings),
            channel_capacity: settings.request_channel_capacity,
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>;

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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>> {
        Box::pin(async move { crate::executor::MemoryCache::get(self, row, col, zoom).await })
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
    data: Vec<u8>,
    /// How long the request took.
    duration: Duration,
    /// Whether the job succeeded.
    job_succeeded: bool,
}

impl CoalescedDdsResult {
    fn new(data: Vec<u8>, duration: Duration, job_succeeded: bool) -> Self {
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

    /// Cancels a job, removing it from in-flight.
    #[allow(dead_code)]
    fn cancel(&self, tile: TileCoord) {
        if let Some((_, _tx)) = self.in_flight.remove(&tile) {
            debug!(tile = ?tile, "Cancelled in-flight job");
        }
    }

    /// Returns the number of currently in-flight jobs.
    #[allow(dead_code)]
    fn in_flight_count(&self) -> usize {
        self.in_flight.len()
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
pub struct ExecutorDaemon<F, M>
where
    F: DdsJobFactory,
    M: DaemonMemoryCache,
{
    /// The job executor.
    executor: JobExecutor,

    /// Job submitter for sending jobs to executor.
    submitter: ExecutorSubmitter,

    /// Factory for creating DDS jobs.
    factory: Arc<F>,

    /// Memory cache for fast-path cache hits.
    memory_cache: Arc<M>,

    /// Channel receiver for requests.
    request_rx: mpsc::Receiver<JobRequest>,

    /// Optional metrics client for event-based metrics.
    metrics_client: Option<MetricsClient>,
}

impl<F, M> ExecutorDaemon<F, M>
where
    F: DdsJobFactory,
    M: DaemonMemoryCache,
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
    ) -> (Self, mpsc::Sender<JobRequest>) {
        Self::with_telemetry(
            config,
            factory,
            memory_cache,
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
        telemetry: Arc<dyn TelemetrySink>,
    ) -> (Self, mpsc::Sender<JobRequest>) {
        let (request_tx, request_rx) = mpsc::channel(config.channel_capacity);
        let (executor, submitter) = JobExecutor::with_telemetry(config.executor, telemetry);

        let daemon = Self {
            executor,
            submitter,
            factory,
            memory_cache,
            request_rx,
            metrics_client: None,
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
            request_rx,
            metrics_client: Some(metrics_client),
        };

        (daemon, request_tx)
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
            mut request_rx,
            metrics_client,
        } = self;

        // Create shared coalescer for daemon-level request deduplication
        let coalescer = Arc::new(DaemonCoalescer::new());

        // Spawn the executor in a separate task
        let executor_shutdown = shutdown.clone();
        let executor_handle = tokio::spawn(async move {
            executor.run(executor_shutdown).await;
        });

        // Main request loop
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
                    Self::handle_request(
                        request,
                        &submitter,
                        &factory,
                        &memory_cache,
                        &coalescer,
                        metrics_client.as_ref(),
                    ).await;
                }
            }
        }

        // Wait for executor to finish
        let _ = executor_handle.await;
        info!("Executor daemon stopped");
    }

    async fn handle_request(
        request: JobRequest,
        submitter: &ExecutorSubmitter,
        factory: &Arc<F>,
        memory_cache: &Arc<M>,
        coalescer: &Arc<DaemonCoalescer>,
        metrics_client: Option<&MetricsClient>,
    ) {
        let start = Instant::now();
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
            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::empty(start.elapsed()));
            }
            return;
        }

        // Fast path: check memory cache first
        if let Some(data) = memory_cache.get(tile.row, tile.col, tile.zoom).await {
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
                client.memory_cache_hit();
            }

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
            client.memory_cache_miss();
        }

        // Create and submit job
        let job = factory.create_job(tile, priority);
        let job_id = job.id();
        debug!(job_id = %job_id, tile = ?tile, "Created DDS generation job");

        let handle = submitter.try_submit_boxed(job);

        match handle {
            Some(mut handle) => {
                debug!(job_id = %job_id, "Job submitted to executor");

                // Track job submitted (FUSE requests are from RequestOrigin::Fuse)
                let is_fuse = origin.is_fuse();
                if let Some(client) = metrics_client {
                    client.job_submitted(is_fuse);
                }

                let memory_cache = Arc::clone(memory_cache);
                let coalescer = Arc::clone(coalescer);
                let cancellation = request.cancellation.clone();
                let metrics_for_completion = metrics_client.cloned();
                let dsf_tile_for_log = dsf_tile.clone();

                tokio::spawn(async move {
                    // Wait for job completion
                    tokio::select! {
                        job_result = handle.wait() => {
                            let status = handle.status();
                            let duration = start.elapsed();

                            // Track job completion
                            let success = status == JobStatus::Succeeded;
                            if let Some(ref client) = metrics_for_completion {
                                client.job_completed(success, duration.as_micros() as u64);
                            }

                            // Get DDS data directly from job result (avoids cache race conditions)
                            let data = if success {
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
                                        d
                                    }
                                    None => {
                                        // Job succeeded but no output data - should not happen
                                        // but fall back to cache read for safety
                                        warn!(
                                            tile_row = tile.row,
                                            tile_col = tile.col,
                                            tile_zoom = tile.zoom,
                                            dsf_tile = %dsf_tile_for_log,
                                            duration_ms = duration.as_millis(),
                                            "Job succeeded but output_data was None - falling back to cache"
                                        );
                                        memory_cache.get(tile.row, tile.col, tile.zoom).await.unwrap_or_default()
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
                                Vec::new()
                            };

                            // Broadcast result to coalesced waiters
                            let coalesced_result = CoalescedDdsResult::new(
                                data.clone(),
                                duration,
                                success,
                            );
                            coalescer.complete(tile, coalesced_result);

                            // Create response with job_succeeded flag set correctly
                            let response = DdsResponse::new(data, false, duration, success);

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

                            // Remove from coalescer so other waiters know job was cancelled
                            coalescer.cancel(tile);

                            if let Some(tx) = request.response_tx {
                                let _ = tx.send(DdsResponse::empty(start.elapsed()));
                            }
                        }
                    }
                });
            }
            None => {
                warn!(tile = ?tile, "Failed to submit job - executor may be shutdown");

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
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>
        {
            let data = self.data.lock().unwrap().get(&(row, col, zoom)).cloned();
            Box::pin(async move { data })
        }
    }

    /// Mock job factory that tracks job creation count
    struct MockJobFactory {
        jobs_created: AtomicUsize,
    }

    impl MockJobFactory {
        fn new() -> Self {
            Self {
                jobs_created: AtomicUsize::new(0),
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
    }

    impl MockJob {
        fn new(tile: TileCoord, priority: Priority, delay: Duration) -> Self {
            Self {
                id: JobId::new(format!("mock-{}_{}_ZL{}", tile.row, tile.col, tile.zoom)),
                priority,
                tile,
                delay,
            }
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
                let data = format!("DDS-{}-{}-{}", tile.row, tile.col, tile.zoom).into_bytes();
                output.set("dds_data", data);
                crate::executor::TaskResult::SuccessWithOutput(output)
            })
        }
    }

    impl DdsJobFactory for MockJobFactory {
        fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job> {
            self.jobs_created.fetch_add(1, Ordering::SeqCst);
            // Use 50ms delay to simulate real job timing
            Box::new(MockJob::new(tile, priority, Duration::from_millis(50)))
        }
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

        let (daemon, tx) = ExecutorDaemon::new(ExecutorDaemonConfig::default(), factory, cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, factory, cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, factory, cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, factory, cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, Arc::clone(&factory), cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, Arc::clone(&factory), cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, Arc::clone(&factory), cache);

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
        let (daemon, tx) = ExecutorDaemon::new(config, Arc::clone(&factory), cache);

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
