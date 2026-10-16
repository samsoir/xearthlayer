//! XEarthLayer runtime orchestrator.
//!
//! This module provides the central runtime that coordinates all daemons
//! and manages their lifecycles. The runtime owns the job channel and
//! provides DdsClient access to producers (FUSE, Prefetch).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      XEarthLayerRuntime                         │
//! │                                                                  │
//! │  ┌───────────────┐                    ┌──────────────────────┐  │
//! │  │ DdsClient     │──────────────────► │  demand daemon       │  │
//! │  │ (FUSE)        │    JobRequest      │  (counts in flight)  │  │
//! │  └───────────────┘                    └──────────┬───────────┘  │
//! │                                        DemandTracker (pause)    │
//! │  ┌───────────────┐                    ┌──────────▼───────────┐  │
//! │  │ PrefetchLane  │──────────────────► │  prefetch daemon     │  │
//! │  │ (prefetch,GC) │    JobRequest      │  (gated, smaller)    │  │
//! │  └───────────────┘                    └──────────────────────┘  │
//! │                                                                  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The two daemons share the caches but nothing else: each has its own
//! resource pools, HTTP semaphore and coalescer, so prefetch can never hold
//! a permit a FUSE request is waiting for. The prefetch lane admits no new
//! job while any demand job is in flight (#296).
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::runtime::XEarthLayerRuntime;
//!
//! // Create runtime with dependencies
//! let runtime = XEarthLayerRuntime::new(
//!     demand_factory, prefetch_factory, memory_cache, dds_disk_cache,
//!     config, handle,
//! );
//!
//! // Get client for producers
//! let client = runtime.dds_client();
//!
//! // When shutting down
//! runtime.shutdown().await;
//! ```

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::DEFAULT_PREFETCH_CAPACITY_SHARE;
use crate::executor::{
    ChannelDdsClient, DaemonMemoryCache, DdsClient, DdsDiskCache, DemandTracker, ExecutorConfig,
    ExecutorDaemon, ExecutorDaemonConfig, JobConcurrencyLimits, JobCounts, LaneSizing,
    MultiplexTelemetrySink, TelemetrySink, TracingTelemetrySink, TILE_GENERATION_GROUP,
};
use crate::jobs::DdsJobFactory;
use crate::metrics::MetricsClient;
use crate::pinned_budget::PinnedTileBudget;
use crate::runtime::{
    LocalPrefetchLane, SharedTileProgressTracker, TileProgressSink, TileProgressTracker,
};

/// Configuration for the XEarthLayer runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Configuration for the executor daemon.
    pub daemon_config: ExecutorDaemonConfig,
    /// Pool capacities of both lanes.
    pub sizing: LaneSizing,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::new(ExecutorDaemonConfig::default())
    }
}

impl RuntimeConfig {
    /// Create a new runtime configuration with custom settings and lanes
    /// sized at the default `prefetch.capacity_share`.
    pub fn new(daemon_config: ExecutorDaemonConfig) -> Self {
        Self {
            daemon_config,
            sizing: LaneSizing::from_configured_share(DEFAULT_PREFETCH_CAPACITY_SHARE),
        }
    }

    /// Set the pool capacities of both lanes.
    pub fn with_sizing(mut self, sizing: LaneSizing) -> Self {
        self.sizing = sizing;
        self
    }

    /// Set the channel capacity for job requests.
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.daemon_config.channel_capacity = capacity;
        self
    }
}

/// The main XEarthLayer runtime.
///
/// This struct orchestrates the daemon architecture, owning the job channels
/// and managing daemon lifecycles. It provides:
///
/// - A `DdsClient` for FUSE to submit tile requests to the demand lane
/// - A `PrefetchLane` for prefetch work and cache GC
/// - Background execution of both `ExecutorDaemon`s
/// - Graceful shutdown coordination
///
/// # Lifecycle
///
/// 1. **Creation**: `new()` creates the channels and spawns both daemon tasks
/// 2. **Operation**: Producers use `dds_client()` or `prefetch_lane()`
/// 3. **Shutdown**: `shutdown()` cancels the daemons and waits for completion
pub struct XEarthLayerRuntime {
    /// DDS client for the demand lane.
    dds_client: Arc<ChannelDdsClient>,

    /// The prefetch lane, which also carries cache GC jobs.
    prefetch_lane: Arc<LocalPrefetchLane>,

    /// Demand jobs in flight. Held here because the prefetch lane's
    /// admission gate is a receiver of this tracker's sender: dropping the
    /// last tracker while it reads paused would leave prefetch paused forever.
    demand: DemandTracker,

    /// Memory pinned by open virtual DDS handles (#246).
    pinned_budget: Arc<PinnedTileBudget>,

    /// Each lane's pools; lets tests check lane isolation.
    #[cfg(test)]
    demand_pools: Arc<crate::executor::ResourcePools>,
    #[cfg(test)]
    prefetch_pools: Arc<crate::executor::ResourcePools>,

    /// The demand and prefetch daemon tasks, and the lane-load reporter when
    /// metrics are on, each named for the shutdown log.
    daemon_handles: Vec<(&'static str, JoinHandle<()>)>,

    /// Shutdown token for graceful termination.
    shutdown_token: CancellationToken,

    /// Tile progress tracker for TUI display.
    tile_progress_tracker: SharedTileProgressTracker,
}

/// How often the prefetch lane's load and backlog are sampled for metrics.
const LANE_LOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Publishes the prefetch lane's load and job counts until shutdown. A gauge
/// polled on a timer rather than an event per change: the sample line only
/// needs the latest value. The queue figure is the jobs held waiting for
/// admission, which is where the lane's work sits while demand has it
/// paused.
async fn report_lane_load(
    client: MetricsClient,
    lane: Arc<LocalPrefetchLane>,
    counts: JobCounts,
    shutdown: CancellationToken,
) {
    use crate::runtime::PrefetchLane;
    let mut tick = tokio::time::interval(LANE_LOAD_INTERVAL);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => client.prefetch_lane_load(
                lane.load(),
                counts.running(),
                counts.waiting(),
            ),
        }
    }
}

impl XEarthLayerRuntime {
    /// Create a new runtime with the given dependencies.
    ///
    /// This starts both executor daemons in background tasks immediately.
    ///
    /// # Arguments
    ///
    /// * `demand_factory` - Factory for FUSE demand jobs
    /// * `prefetch_factory` - Factory for prefetch jobs, with its own HTTP
    ///   semaphore and the admission cache probe
    /// * `memory_cache` - Memory cache shared by both lanes
    /// * `dds_disk_cache` - DDS disk cache shared by both lanes
    /// * `config` - Runtime configuration, including both lanes' sizing
    /// * `handle` - Handle to the Tokio runtime for spawning tasks
    pub fn new<F, M, DD>(
        demand_factory: Arc<F>,
        prefetch_factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        config: RuntimeConfig,
        handle: tokio::runtime::Handle,
    ) -> Self
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
        DD: DdsDiskCache + 'static,
    {
        // Default to tracing-only telemetry, no metrics
        Self::with_metrics_client(
            demand_factory,
            prefetch_factory,
            memory_cache,
            dds_disk_cache,
            config,
            handle,
            None,
        )
    }

    /// Create a new runtime with the event-based metrics client.
    ///
    /// As [`Self::new`]; when metrics are provided, per-chunk and job events
    /// from both lanes flow to the metrics daemon.
    pub fn with_metrics_client<F, M, DD>(
        demand_factory: Arc<F>,
        prefetch_factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        config: RuntimeConfig,
        handle: tokio::runtime::Handle,
        metrics_client: Option<MetricsClient>,
    ) -> Self
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
        DD: DdsDiskCache + 'static,
    {
        let RuntimeConfig {
            daemon_config,
            sizing,
        } = config;
        info!(
            demand_cpu = sizing.demand.cpu,
            prefetch_cpu = sizing.prefetch.cpu,
            prefetch_tile_generation = sizing.prefetch_tile_generation,
            prefetch_http_permits = sizing.prefetch_http_permits,
            "Starting XEarthLayer runtime"
        );

        // Create tile progress tracker for TUI display
        let tile_progress_tracker = TileProgressTracker::new();
        let tile_progress_sink = TileProgressSink::new(Arc::clone(&tile_progress_tracker));

        // Combine tracing sink with tile progress sink
        let telemetry: Arc<dyn TelemetrySink> = Arc::new(MultiplexTelemetrySink::new(vec![
            Arc::new(TracingTelemetrySink) as Arc<dyn TelemetrySink>,
            tile_progress_sink as Arc<dyn TelemetrySink>,
        ]));

        let demand = match &metrics_client {
            Some(client) => DemandTracker::new().with_metrics(client.clone()),
            None => DemandTracker::new(),
        };

        let (demand_daemon_config, prefetch_daemon_config) =
            Self::lane_daemon_configs(daemon_config, &sizing, demand.subscribe());

        let (demand_daemon, demand_tx) = Self::build_daemon(
            demand_daemon_config,
            demand_factory,
            Arc::clone(&memory_cache),
            Arc::clone(&dds_disk_cache),
            Arc::clone(&telemetry),
            metrics_client.clone(),
        );
        // Only the demand daemon counts demand; the prefetch daemon counting
        // its own jobs would pause itself.
        let demand_daemon = demand_daemon.with_demand_tracker(demand.clone());

        let (prefetch_daemon, prefetch_tx) = Self::build_daemon(
            prefetch_daemon_config,
            prefetch_factory,
            memory_cache,
            dds_disk_cache,
            telemetry,
            metrics_client.clone(),
        );
        let prefetch_counts = prefetch_daemon.job_counts();
        #[cfg(test)]
        let prefetch_pools = prefetch_daemon.resource_pools();

        // Created here, before the filesystem exists, because the client is
        // what carries it to the mount (#246).
        let pinned_budget = Arc::new(PinnedTileBudget::new());
        #[cfg(test)]
        let demand_pools = demand_daemon.resource_pools();
        let dds_client = Arc::new(
            ChannelDdsClient::new(demand_tx).with_pinned_budget(Arc::clone(&pinned_budget)),
        );
        let prefetch_lane = Arc::new(LocalPrefetchLane::new(
            prefetch_tx,
            prefetch_counts.clone(),
            sizing.prefetch_tile_generation,
            prefetch_daemon.job_submitter(),
        ));

        // Create shutdown token for coordinating shutdown
        let shutdown_token = CancellationToken::new();

        // Use handle.spawn() instead of tokio::spawn() so we don't require
        // the calling thread to be in a runtime context.
        let demand_shutdown = shutdown_token.clone();
        let prefetch_shutdown = shutdown_token.clone();
        let mut daemon_handles = vec![
            (
                "Demand executor daemon",
                handle.spawn(async move { demand_daemon.run(demand_shutdown).await }),
            ),
            (
                "Prefetch executor daemon",
                handle.spawn(async move { prefetch_daemon.run(prefetch_shutdown).await }),
            ),
        ];
        if let Some(client) = metrics_client {
            daemon_handles.push((
                "Prefetch lane load reporter",
                handle.spawn(report_lane_load(
                    client,
                    Arc::clone(&prefetch_lane),
                    prefetch_counts,
                    shutdown_token.clone(),
                )),
            ));
        }

        info!("XEarthLayer runtime started");

        Self {
            dds_client,
            prefetch_lane,
            demand,
            pinned_budget,
            #[cfg(test)]
            demand_pools,
            #[cfg(test)]
            prefetch_pools,
            daemon_handles,
            shutdown_token,
            tile_progress_tracker,
        }
    }

    /// Daemon configuration for each lane, from the shared base.
    ///
    /// The prefetch lane is gated by demand, sized by the share, registers its
    /// own tile-generation group (prefetch jobs declare it, and a job in an
    /// unregistered group is held forever; GC jobs declare no group), and
    /// waits for executor room so it never sheds a job the coordinator counts
    /// as submitted.
    fn lane_daemon_configs(
        daemon_config: ExecutorDaemonConfig,
        sizing: &LaneSizing,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> (ExecutorDaemonConfig, ExecutorDaemonConfig) {
        let mut demand = daemon_config.clone();
        demand.executor.resource_pools = sizing.demand.clone();

        let mut prefetch = daemon_config;
        prefetch.executor = ExecutorConfig {
            resource_pools: sizing.prefetch.clone(),
            job_concurrency_limits: JobConcurrencyLimits::empty()
                .with_group(TILE_GENERATION_GROUP, sizing.prefetch_tile_generation),
            ..prefetch.executor
        }
        .with_admission_gate(gate);
        prefetch.wait_for_executor = true;
        (demand, prefetch)
    }

    fn build_daemon<F, M, DD>(
        config: ExecutorDaemonConfig,
        factory: Arc<F>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        telemetry: Arc<dyn TelemetrySink>,
        metrics_client: Option<MetricsClient>,
    ) -> (
        ExecutorDaemon<F, M, DD>,
        tokio::sync::mpsc::Sender<crate::runtime::JobRequest>,
    )
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
        DD: DdsDiskCache + 'static,
    {
        match metrics_client {
            Some(client) => ExecutorDaemon::with_metrics_client(
                config,
                factory,
                memory_cache,
                dds_disk_cache,
                telemetry,
                client,
            ),
            None => ExecutorDaemon::with_telemetry(
                config,
                factory,
                memory_cache,
                dds_disk_cache,
                telemetry,
            ),
        }
    }

    /// Get the DDS client for the demand lane.
    ///
    /// This client can be cloned and shared with FUSE handlers and other
    /// components that need to request tiles.
    pub fn dds_client(&self) -> Arc<dyn DdsClient> {
        Arc::clone(&self.dds_client) as Arc<dyn DdsClient>
    }

    /// Get a concrete reference to the channel-based DDS client.
    ///
    /// This is useful when you need the concrete type rather than
    /// the trait object.
    pub fn channel_dds_client(&self) -> Arc<ChannelDdsClient> {
        Arc::clone(&self.dds_client)
    }

    /// The prefetch lane. Its job submitter also carries cache GC jobs.
    pub fn prefetch_lane(&self) -> Arc<LocalPrefetchLane> {
        Arc::clone(&self.prefetch_lane)
    }

    /// Demand jobs in flight; the prefetch lane pauses while any are.
    pub fn demand(&self) -> DemandTracker {
        self.demand.clone()
    }

    /// The pinned-tile budget the FUSE mount accounts against (#246).
    pub fn pinned_budget(&self) -> Arc<PinnedTileBudget> {
        Arc::clone(&self.pinned_budget)
    }

    /// Check if the runtime is still running.
    ///
    /// Returns `false` if either lane's daemon has been shut down or crashed.
    pub fn is_running(&self) -> bool {
        self.dds_client.is_connected() && self.prefetch_lane.is_connected()
    }

    /// Get the shutdown token for external coordination.
    ///
    /// Other components can listen to this token to know when
    /// the runtime is shutting down.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Get the tile progress tracker for TUI display.
    ///
    /// The tracker provides a snapshot of active tile generation
    /// progress for rendering in the dashboard.
    pub fn tile_progress_tracker(&self) -> SharedTileProgressTracker {
        Arc::clone(&self.tile_progress_tracker)
    }

    /// Shutdown the runtime gracefully.
    ///
    /// This cancels both daemons and waits for them to complete any
    /// in-flight work before returning.
    pub async fn shutdown(mut self) {
        info!("Shutting down XEarthLayer runtime");

        // Signal shutdown to the daemons
        self.shutdown_token.cancel();

        for (name, handle) in self.daemon_handles.drain(..) {
            match handle.await {
                Ok(()) => info!("{name} shut down cleanly"),
                Err(e) => tracing::error!("{name} task failed: {e}"),
            }
        }

        info!("XEarthLayer runtime stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the prefetch lane waits for executor room. The demand lane keeps
    /// shedding, so a FUSE request on a saturated executor gets its answer at
    /// once instead of waiting out the generation timeout behind it.
    #[test]
    fn only_the_prefetch_lane_waits_for_executor_room() {
        let sizing = LaneSizing::from_configured_share(0.35);
        let (_gate_tx, gate) = tokio::sync::watch::channel(false);
        let (demand, prefetch) =
            XEarthLayerRuntime::lane_daemon_configs(ExecutorDaemonConfig::default(), &sizing, gate);
        assert!(prefetch.wait_for_executor, "prefetch must never lose a job");
        assert!(
            !demand.wait_for_executor,
            "demand keeps its existing behaviour"
        );
        assert!(prefetch.executor.admission_gate.is_some());
        assert!(demand.executor.admission_gate.is_none());
    }
    use crate::coord::TileCoord;
    use crate::executor::{
        Job, JobId, JobResult, JobStatus, NullDdsDiskCache, Priority, ResourcePoolConfig,
        ResourceType, Task, TaskContext, TaskOutput, TaskResult,
    };
    use crate::runtime::{JobRequest, LaneOutcome, PrefetchLane};
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    /// Mock job factory whose jobs take `delay` to produce a tile.
    struct MockJobFactory {
        delay: Duration,
    }

    impl MockJobFactory {
        fn with_delay(delay: Duration) -> Self {
            Self { delay }
        }
    }

    impl DdsJobFactory for MockJobFactory {
        fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job> {
            Box::new(MockJob {
                id: JobId::new(format!("mock-{}_{}_ZL{}", tile.row, tile.col, tile.zoom)),
                priority,
                delay: self.delay,
            })
        }
    }

    /// Mock job: one CPU task that sleeps, then yields a tile.
    struct MockJob {
        id: JobId,
        priority: Priority,
        delay: Duration,
    }

    impl Job for MockJob {
        fn id(&self) -> JobId {
            self.id.clone()
        }

        fn name(&self) -> &str {
            "MockJob"
        }

        fn priority(&self) -> Priority {
            self.priority
        }

        // As the real DDS job does, so job admission is exercised too.
        fn concurrency_group(&self) -> Option<&str> {
            Some(TILE_GENERATION_GROUP)
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            vec![Box::new(MockTask { delay: self.delay })]
        }

        fn on_complete(&self, result: &JobResult) -> JobStatus {
            if result.failed_tasks.is_empty() {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
    }

    struct MockTask {
        delay: Duration,
    }

    impl Task for MockTask {
        fn name(&self) -> &str {
            // The executor reads `dds_data` from the task of this name.
            "BuildAndCacheDds"
        }

        fn resource_type(&self) -> ResourceType {
            ResourceType::CPU
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a mut TaskContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TaskResult> + Send + 'a>> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let mut output = TaskOutput::new();
                output.set("dds_data", Bytes::from_static(b"DDS"));
                TaskResult::SuccessWithOutput(output)
            })
        }
    }

    /// Mock memory cache for testing.
    /// Always returns cache miss - used for testing the runtime without real caching.
    struct MockMemoryCache;

    impl DaemonMemoryCache for MockMemoryCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Bytes>> + Send + '_>>
        {
            Box::pin(async { None }) // Always miss for testing
        }

        fn put(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
            _data: Bytes,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {}) // No-op for testing
        }
    }

    const JOB_DELAY: Duration = Duration::from_millis(50);

    fn build_test_runtime() -> XEarthLayerRuntime {
        XEarthLayerRuntime::new(
            Arc::new(MockJobFactory::with_delay(JOB_DELAY)),
            Arc::new(MockJobFactory::with_delay(JOB_DELAY)),
            Arc::new(MockMemoryCache),
            Arc::new(NullDdsDiskCache),
            RuntimeConfig::default()
                .with_sizing(LaneSizing::derive(ResourcePoolConfig::default(), 0.35)),
            tokio::runtime::Handle::current(),
        )
    }

    fn tile_at(i: u32) -> TileCoord {
        TileCoord {
            row: i,
            col: i,
            zoom: 12,
        }
    }

    /// Queues `queued` distinct tiles on the prefetch lane, then times one
    /// FUSE request on the demand lane to completion.
    async fn time_one_fuse_request(runtime: XEarthLayerRuntime, queued: u32) -> Duration {
        let lane = runtime.prefetch_lane();
        for i in 0..queued {
            lane.submit(tile_at(1000 + i))
                .expect("prefetch lane accepts");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        let start = Instant::now();
        let response = runtime
            .dds_client()
            .request_dds(tile_at(1), CancellationToken::new())
            .await
            .expect("demand lane answers");
        let elapsed = start.elapsed();
        assert!(response.is_success());
        runtime.shutdown().await;
        elapsed
    }

    #[tokio::test]
    async fn fuse_request_is_not_slowed_by_a_saturated_prefetch_lane() {
        let idle = time_one_fuse_request(build_test_runtime(), 0).await;
        let busy = time_one_fuse_request(build_test_runtime(), 200).await;
        assert!(
            busy <= idle + Duration::from_millis(40),
            "demand took {busy:?} behind a full prefetch lane vs {idle:?} idle"
        );
    }

    /// Structural, so it fails on a shared executor where priority dispatch
    /// would let the timing test pass anyway: prefetch work must occupy only
    /// the prefetch lane's pools.
    #[tokio::test]
    async fn prefetch_work_never_occupies_the_demand_pools() {
        let runtime = build_test_runtime();
        let lane = runtime.prefetch_lane();
        let demand_pools = Arc::clone(&runtime.demand_pools);
        let prefetch_pools = Arc::clone(&runtime.prefetch_pools);
        assert!(
            !Arc::ptr_eq(&demand_pools, &prefetch_pools),
            "the lanes must not share resource pools"
        );

        for i in 0..200 {
            lane.submit(tile_at(1000 + i))
                .expect("prefetch lane accepts");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            prefetch_pools.in_flight(ResourceType::CPU) > 0,
            "prefetch work should be running on the prefetch pools"
        );
        assert_eq!(
            demand_pools.in_flight(ResourceType::CPU),
            0,
            "prefetch work must not hold demand pool permits"
        );
        runtime.shutdown().await;
    }

    /// The mount pins against the budget on the DDS client and the prefetch
    /// coordinator reads the one on the runtime. If these ever diverge, both
    /// halves still compile and report plausible numbers, and only the FUSE
    /// back-pressure silently stops working (#246).
    #[tokio::test]
    async fn the_mount_and_the_coordinator_share_one_pinned_budget() {
        let runtime = build_test_runtime();
        let from_client = runtime
            .dds_client()
            .pinned_budget()
            .expect("the demand client carries the budget");
        assert!(Arc::ptr_eq(&runtime.pinned_budget(), &from_client));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn demand_in_flight_pauses_prefetch_admission() {
        let runtime = build_test_runtime();
        let paused = runtime.demand().subscribe();
        let (req, rx) = JobRequest::fuse(tile_at(1), CancellationToken::new());
        runtime.dds_client().submit(req).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(*paused.borrow(), "a FUSE job in flight must pause prefetch");
        let _ = rx.await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!*paused.borrow());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn prefetch_requests_never_count_as_demand() {
        let runtime = build_test_runtime();
        let receipt = runtime
            .prefetch_lane()
            .submit_with_receipt(tile_at(7))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(runtime.demand().in_flight(), 0);
        assert_eq!(receipt.outcome().await, LaneOutcome::Generated);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn test_runtime_creation_and_shutdown() {
        let runtime = build_test_runtime();

        // Runtime should be running
        assert!(runtime.is_running());

        // Shutdown should complete without hanging
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
            .await
            .expect("Shutdown should complete within 5 seconds");
    }

    /// FUSE alone working is not a running runtime: prefetch and cache GC
    /// both go through the prefetch lane.
    #[tokio::test]
    async fn runtime_is_not_running_once_the_prefetch_lane_stops() {
        let runtime = build_test_runtime();
        runtime.daemon_handles[1].1.abort();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(runtime.dds_client().is_connected(), "demand lane still up");
        assert!(!runtime.is_running());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn test_runtime_multiple_instances() {
        // Test that multiple runtimes can be created and shut down independently
        let runtime1 = build_test_runtime();
        let runtime2 = build_test_runtime();

        // Both should be running
        assert!(runtime1.is_running());
        assert!(runtime2.is_running());

        // Shutdown in reverse order
        runtime2.shutdown().await;
        runtime1.shutdown().await;
    }

    #[tokio::test]
    async fn test_dds_client_is_connected() {
        let runtime = build_test_runtime();

        // Client should be connected initially
        let client = runtime.dds_client();
        assert!(client.is_connected());

        runtime.shutdown().await;
    }

    #[test]
    fn test_runtime_config_default() {
        let config = RuntimeConfig::default();
        assert_eq!(config.daemon_config.channel_capacity, 1000);
    }

    #[test]
    fn test_runtime_config_builder() {
        let config = RuntimeConfig::default().with_channel_capacity(500);
        assert_eq!(config.daemon_config.channel_capacity, 500);
    }
}
