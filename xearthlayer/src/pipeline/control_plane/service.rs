//! Pipeline Control Plane service.
//!
//! The control plane is the **single authority** for all compute resources,
//! managing both DDS handler tasks and pipeline workers uniformly.
//!
//! # Core Responsibilities
//!
//! - **Unified work submission**: All work goes through `submit()`
//! - **Job-level concurrency limiting** (H-005)
//! - **Job tracking with stage progression**
//! - **Health monitoring with stall detection** (H-003)
//! - **Active recovery of stuck jobs** (H-006)
//!
//! # Work Submission
//!
//! The `submit()` method is the primary entry point for executing work:
//!
//! ```ignore
//! let result = control_plane.submit(
//!     job_id,
//!     tile,
//!     is_prefetch,
//!     cancellation_token,
//!     |observer| async move {
//!         // Work function receives observer for stage updates
//!         process_tile(observer, ...).await
//!     }
//! ).await;
//! ```

use super::health::{ControlPlaneHealth, HealthStatus};
use super::job_registry::{JobEntry, JobRegistry, JobStage};
use super::StageObserver;
use crate::config::num_cpus;
use crate::coord::TileCoord;
use crate::pipeline::{JobError, JobId, RequestCoalescer};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Configuration for the control plane.
#[derive(Debug, Clone)]
pub struct ControlPlaneConfig {
    /// Maximum concurrent jobs allowed.
    /// Default: `num_cpus × 2`
    pub max_concurrent_jobs: usize,

    /// Time threshold for considering a job stalled.
    /// Default: 60 seconds
    pub stall_threshold: Duration,

    /// Interval for health check monitoring.
    /// Default: 5 seconds
    pub health_check_interval: Duration,

    /// Timeout for semaphore acquisition.
    /// Default: 30 seconds
    pub semaphore_timeout: Duration,
}

impl Default for ControlPlaneConfig {
    fn default() -> Self {
        let cpus = num_cpus();
        Self {
            max_concurrent_jobs: cpus * 2,
            stall_threshold: Duration::from_secs(60),
            health_check_interval: Duration::from_secs(5),
            semaphore_timeout: Duration::from_secs(30),
        }
    }
}

impl ControlPlaneConfig {
    /// Creates a new config with specified max concurrent jobs.
    pub fn with_max_concurrent_jobs(mut self, max: usize) -> Self {
        self.max_concurrent_jobs = max;
        self
    }

    /// Creates a new config with specified stall threshold.
    pub fn with_stall_threshold(mut self, threshold: Duration) -> Self {
        self.stall_threshold = threshold;
        self
    }

    /// Creates a new config with specified health check interval.
    pub fn with_health_check_interval(mut self, interval: Duration) -> Self {
        self.health_check_interval = interval;
        self
    }

    /// Creates a new config with specified semaphore timeout.
    pub fn with_semaphore_timeout(mut self, timeout: Duration) -> Self {
        self.semaphore_timeout = timeout;
        self
    }
}

/// Error when acquiring a job slot.
#[derive(Debug, Clone)]
pub enum JobSlotError {
    /// No slots available (for non-blocking prefetch requests)
    NoCapacity,
    /// Timeout waiting for a slot
    Timeout,
    /// Control plane is shutting down
    Shutdown,
}

impl std::fmt::Display for JobSlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCapacity => write!(f, "No job slots available"),
            Self::Timeout => write!(f, "Timeout waiting for job slot"),
            Self::Shutdown => write!(f, "Control plane shutting down"),
        }
    }
}

impl std::error::Error for JobSlotError {}

/// Result of submitting work to the control plane.
#[derive(Debug)]
pub enum SubmitResult<T> {
    /// Work completed successfully with the given result.
    Completed(T),
    /// Work was cancelled (e.g., FUSE timeout).
    Cancelled,
    /// Work was recovered by the health monitor (stall detected).
    Recovered,
    /// Work failed with an error.
    Failed(JobError),
    /// No capacity available for prefetch work.
    NoCapacity,
    /// Control plane is shutting down.
    Shutdown,
}

impl<T> SubmitResult<T> {
    /// Returns true if the work completed successfully.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }

    /// Returns the result if completed, None otherwise.
    pub fn into_completed(self) -> Option<T> {
        match self {
            Self::Completed(t) => Some(t),
            _ => None,
        }
    }
}

/// Observer implementation for the control plane.
///
/// This struct implements `StageObserver` and forwards stage changes
/// to the job registry for tracking.
pub struct ControlPlaneObserver {
    registry: Arc<JobRegistry>,
}

impl ControlPlaneObserver {
    /// Creates a new observer for the given registry.
    pub fn new(registry: Arc<JobRegistry>) -> Self {
        Self { registry }
    }
}

impl StageObserver for ControlPlaneObserver {
    fn on_stage_change(&self, job_id: JobId, stage: JobStage) {
        self.registry.update_stage(job_id, stage);
    }
}

/// Pipeline Control Plane.
///
/// Provides centralized management of the DDS generation pipeline including
/// job-level concurrency limiting, health monitoring, and stall recovery.
///
/// The control plane is always enabled and has minimal overhead on the hot path
/// (lock-free atomic operations).
pub struct PipelineControlPlane {
    /// Configuration
    config: ControlPlaneConfig,

    /// Job registry for tracking active jobs
    registry: Arc<JobRegistry>,

    /// Job-level semaphore for concurrency limiting (H-005)
    job_limiter: Arc<Semaphore>,

    /// Health metrics
    health: Arc<ControlPlaneHealth>,

    /// Request coalescer for recovery integration
    coalescer: Arc<RequestCoalescer>,

    /// Shutdown signal
    shutdown: CancellationToken,
}

impl PipelineControlPlane {
    /// Creates a new control plane with the given configuration.
    pub fn new(config: ControlPlaneConfig, coalescer: Arc<RequestCoalescer>) -> Self {
        let job_limiter = Arc::new(Semaphore::new(config.max_concurrent_jobs));

        info!(
            max_concurrent_jobs = config.max_concurrent_jobs,
            stall_threshold_secs = config.stall_threshold.as_secs(),
            health_check_interval_secs = config.health_check_interval.as_secs(),
            "Pipeline control plane initialized"
        );

        Self {
            config,
            registry: Arc::new(JobRegistry::new()),
            job_limiter,
            health: Arc::new(ControlPlaneHealth::new()),
            coalescer,
            shutdown: CancellationToken::new(),
        }
    }

    /// Creates a control plane with default configuration.
    pub fn with_defaults(coalescer: Arc<RequestCoalescer>) -> Self {
        Self::new(ControlPlaneConfig::default(), coalescer)
    }

    /// Acquires a job slot for processing.
    ///
    /// For on-demand requests, blocks until a slot is available (with timeout).
    /// For prefetch requests, returns immediately if no slots are available.
    ///
    /// Returns an owned permit that releases the slot when dropped.
    pub async fn acquire_job_slot(
        &self,
        is_prefetch: bool,
    ) -> Result<OwnedSemaphorePermit, JobSlotError> {
        if self.shutdown.is_cancelled() {
            return Err(JobSlotError::Shutdown);
        }

        if is_prefetch {
            // Prefetch: non-blocking, skip if no capacity
            match self.job_limiter.clone().try_acquire_owned() {
                Ok(permit) => Ok(permit),
                Err(_) => {
                    self.health.record_rejected_capacity();
                    debug!("Prefetch job rejected - no capacity");
                    Err(JobSlotError::NoCapacity)
                }
            }
        } else {
            // On-demand: block with timeout
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    Err(JobSlotError::Shutdown)
                }
                result = tokio::time::timeout(
                    self.config.semaphore_timeout,
                    self.job_limiter.clone().acquire_owned()
                ) => {
                    match result {
                        Ok(Ok(permit)) => Ok(permit),
                        Ok(Err(_)) => {
                            // Semaphore closed - shouldn't happen
                            Err(JobSlotError::Shutdown)
                        }
                        Err(_) => {
                            self.health.record_semaphore_timeout();
                            warn!(
                                timeout_secs = self.config.semaphore_timeout.as_secs(),
                                "Timeout waiting for job slot"
                            );
                            Err(JobSlotError::Timeout)
                        }
                    }
                }
            }
        }
    }

    /// Registers a job for tracking.
    ///
    /// Call this after acquiring a job slot.
    pub fn register_job(
        &self,
        job_id: JobId,
        tile: TileCoord,
        cancellation_token: CancellationToken,
        is_prefetch: bool,
    ) -> Arc<JobEntry> {
        self.health.job_started();
        self.registry
            .register(job_id, tile, cancellation_token, is_prefetch)
    }

    /// Updates the stage of a job.
    ///
    /// Call this as the job progresses through pipeline stages.
    pub fn update_stage(&self, job_id: JobId, stage: JobStage) {
        self.registry.update_stage(job_id, stage);
    }

    /// Marks a job as completed.
    ///
    /// Call this when a job finishes successfully.
    pub fn complete_job(&self, job_id: JobId) {
        self.registry.complete(job_id);
        self.health.job_finished();
        self.health.record_job_success();
    }

    /// Marks a job as failed.
    ///
    /// Call this when a job fails (but not due to stall recovery).
    pub fn fail_job(&self, job_id: JobId) {
        self.registry.mark_failed(job_id);
        self.health.job_finished();
    }

    /// Returns the current health status.
    pub fn health_status(&self) -> HealthStatus {
        self.health.status()
    }

    /// Returns a reference to the health metrics.
    pub fn health(&self) -> &Arc<ControlPlaneHealth> {
        &self.health
    }

    /// Returns a reference to the job registry.
    pub fn registry(&self) -> &Arc<JobRegistry> {
        &self.registry
    }

    /// Returns a reference to the request coalescer.
    ///
    /// The coalescer is used for:
    /// - Preventing duplicate work for the same tile
    /// - Notifying waiters when a tile completes or is cancelled
    pub fn coalescer(&self) -> &Arc<RequestCoalescer> {
        &self.coalescer
    }

    /// Returns the configuration.
    pub fn config(&self) -> &ControlPlaneConfig {
        &self.config
    }

    /// Starts the background health monitor.
    ///
    /// The monitor periodically checks for stalled jobs and initiates recovery.
    /// Returns a join handle that completes when the monitor is stopped.
    ///
    /// **Note**: This method calls `tokio::spawn` internally, so it must be called
    /// from within a Tokio runtime context. If calling from outside a runtime
    /// (e.g., during service construction), use `run_health_monitor_loop` with
    /// `runtime_handle.spawn()` instead.
    pub fn start_health_monitor(self: Arc<Self>) -> JoinHandle<()> {
        let control_plane = self;

        tokio::spawn(async move {
            control_plane.run_health_monitor_loop().await;
        })
    }

    /// Runs the health monitor loop.
    ///
    /// This is the core loop logic that can be spawned externally using a runtime handle
    /// when not in an async context.
    pub async fn run_health_monitor_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.config.health_check_interval);

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    info!("Health monitor shutting down");
                    break;
                }
                _ = interval.tick() => {
                    self.run_health_check().await;
                }
            }
        }
    }

    /// Runs a single health check iteration.
    async fn run_health_check(&self) {
        // Find stalled jobs
        let stalled = self.registry.find_stalled(self.config.stall_threshold);

        if stalled.is_empty() {
            // All good - reset failure counter if previously failing
            self.health.reset_failures();
            return;
        }

        // Record health check failure
        let failures = self.health.record_health_check_failure();

        warn!(
            stalled_count = stalled.len(),
            consecutive_failures = failures,
            "Detected stalled jobs"
        );

        // Attempt recovery for each stalled job
        for entry in stalled {
            self.recover_job(&entry).await;
        }
    }

    /// Attempts to recover a stalled job.
    async fn recover_job(&self, entry: &JobEntry) {
        let job_id = entry.job_id;
        let tile = entry.tile;
        let stage = entry.stage();
        let elapsed = entry.elapsed();

        warn!(
            job_id = %job_id,
            tile = ?tile,
            stage = %stage,
            elapsed_secs = elapsed.as_secs(),
            is_prefetch = entry.is_prefetch,
            "Recovering stalled job"
        );

        // 1. Cancel the job's cancellation token
        entry.cancel();

        // 2. Mark as recovered in registry
        self.registry.mark_recovered(job_id);
        self.health.record_recovery();
        self.health.job_finished();

        // 3. Cancel in coalescer - this notifies any waiters
        // Waiters will receive a channel error and return placeholder
        self.coalescer.cancel(tile);

        info!(
            job_id = %job_id,
            tile = ?tile,
            "Job recovery complete - waiters notified"
        );
    }

    /// Initiates graceful shutdown of the control plane.
    pub fn shutdown(&self) {
        info!("Control plane shutdown initiated");
        self.shutdown.cancel();
    }

    /// Returns true if the control plane is shutting down.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Returns the number of available job slots.
    pub fn available_slots(&self) -> usize {
        self.job_limiter.available_permits()
    }

    /// Returns the maximum concurrent jobs configured.
    pub fn max_concurrent_jobs(&self) -> usize {
        self.config.max_concurrent_jobs
    }

    /// Creates a stage observer for this control plane.
    ///
    /// The observer forwards stage changes to the job registry.
    pub fn create_observer(&self) -> Arc<ControlPlaneObserver> {
        Arc::new(ControlPlaneObserver::new(Arc::clone(&self.registry)))
    }

    /// Submit work to be executed under control plane management.
    ///
    /// This is the primary entry point for executing work. The control plane:
    /// 1. Acquires a job slot (blocking for on-demand, non-blocking for prefetch)
    /// 2. Registers the job for tracking
    /// 3. Creates a stage observer for the work function
    /// 4. Executes the work function
    /// 5. Handles completion, failure, or cancellation
    ///
    /// # Arguments
    ///
    /// * `job_id` - Unique identifier for this job
    /// * `tile` - The tile coordinates being processed
    /// * `is_prefetch` - If true, fails immediately if no capacity
    /// * `cancellation_token` - Token to signal cancellation
    /// * `work` - The work function that receives a stage observer
    ///
    /// # Returns
    ///
    /// A `SubmitResult` indicating the outcome of the work.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = control_plane.submit(
    ///     job_id,
    ///     tile,
    ///     false, // on-demand
    ///     cancellation_token,
    ///     |observer| async move {
    ///         observer.on_stage_change(job_id, JobStage::Downloading);
    ///         // ... do work ...
    ///         Ok(dds_data)
    ///     }
    /// ).await;
    /// ```
    pub async fn submit<F, Fut, T>(
        &self,
        job_id: JobId,
        tile: TileCoord,
        is_prefetch: bool,
        cancellation_token: CancellationToken,
        work: F,
    ) -> SubmitResult<T>
    where
        F: FnOnce(Arc<dyn StageObserver>) -> Fut,
        Fut: Future<Output = Result<T, JobError>>,
    {
        debug!(
            job_id = %job_id,
            tile = ?tile,
            is_prefetch = is_prefetch,
            "Control plane submit() called"
        );

        // Step 1: Acquire job slot
        let permit = match self.acquire_job_slot(is_prefetch).await {
            Ok(permit) => permit,
            Err(JobSlotError::NoCapacity) => {
                debug!(job_id = %job_id, tile = ?tile, "Prefetch rejected - no capacity");
                return SubmitResult::NoCapacity;
            }
            Err(JobSlotError::Timeout) => {
                warn!(job_id = %job_id, tile = ?tile, "Job slot acquisition timed out");
                return SubmitResult::Failed(JobError::Timeout(self.config.semaphore_timeout));
            }
            Err(JobSlotError::Shutdown) => {
                debug!(job_id = %job_id, tile = ?tile, "Control plane shutting down");
                return SubmitResult::Shutdown;
            }
        };

        // Step 2: Register job for tracking
        let _entry = self.register_job(job_id, tile, cancellation_token.clone(), is_prefetch);
        debug!(
            job_id = %job_id,
            jobs_in_progress = self.health.jobs_in_progress(),
            "Job registered with control plane"
        );

        // Step 3: Create observer for stage updates
        let observer: Arc<dyn StageObserver> = self.create_observer();

        // Step 4: Execute work
        let result = work(observer).await;

        // Step 5: Handle result
        // Drop permit to release job slot
        drop(permit);

        // Check if we were cancelled during execution
        if cancellation_token.is_cancelled() {
            // Check if this was a recovery (job would be marked as recovered)
            if self.registry.get(job_id).is_none() {
                // Job was removed by recovery
                return SubmitResult::Recovered;
            }
            self.fail_job(job_id);
            return SubmitResult::Cancelled;
        }

        match result {
            Ok(value) => {
                self.complete_job(job_id);
                SubmitResult::Completed(value)
            }
            Err(e) => {
                if matches!(e, JobError::Cancelled) {
                    self.fail_job(job_id);
                    SubmitResult::Cancelled
                } else {
                    self.fail_job(job_id);
                    SubmitResult::Failed(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile(row: u32, col: u32) -> TileCoord {
        TileCoord { row, col, zoom: 14 }
    }

    #[tokio::test]
    async fn test_control_plane_creation() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);

        assert_eq!(control_plane.health_status(), HealthStatus::Healthy);
        assert!(control_plane.available_slots() > 0);
    }

    #[tokio::test]
    async fn test_job_slot_acquisition_on_demand() {
        let config = ControlPlaneConfig::default().with_max_concurrent_jobs(2);
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::new(config, coalescer);

        // Acquire two slots
        let permit1 = control_plane.acquire_job_slot(false).await.unwrap();
        let permit2 = control_plane.acquire_job_slot(false).await.unwrap();

        assert_eq!(control_plane.available_slots(), 0);

        // Drop one permit
        drop(permit1);
        assert_eq!(control_plane.available_slots(), 1);

        // Drop the other
        drop(permit2);
        assert_eq!(control_plane.available_slots(), 2);
    }

    #[tokio::test]
    async fn test_job_slot_prefetch_no_capacity() {
        let config = ControlPlaneConfig::default().with_max_concurrent_jobs(1);
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::new(config, coalescer);

        // Take the only slot
        let _permit = control_plane.acquire_job_slot(false).await.unwrap();

        // Prefetch should fail immediately
        let result = control_plane.acquire_job_slot(true).await;
        assert!(matches!(result, Err(JobSlotError::NoCapacity)));

        // Health should record the rejection
        assert_eq!(control_plane.health.jobs_rejected_capacity(), 1);
    }

    #[tokio::test]
    async fn test_job_lifecycle() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        // Register job
        let entry = control_plane.register_job(job_id, tile, token, false);
        assert_eq!(entry.stage(), JobStage::Queued);
        assert_eq!(control_plane.health.jobs_in_progress(), 1);

        // Update stage
        control_plane.update_stage(job_id, JobStage::Downloading);
        let entry = control_plane.registry.get(job_id).unwrap();
        assert_eq!(entry.stage(), JobStage::Downloading);

        // Complete job
        control_plane.complete_job(job_id);
        assert_eq!(control_plane.health.jobs_in_progress(), 0);
        assert!(control_plane.registry.get(job_id).is_none());
    }

    #[tokio::test]
    async fn test_job_failure() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        control_plane.register_job(job_id, tile, token, false);
        control_plane.fail_job(job_id);

        assert_eq!(control_plane.health.jobs_in_progress(), 0);
        let stats = control_plane.registry.stats();
        assert_eq!(stats.failed_jobs, 1);
    }

    #[tokio::test]
    async fn test_stall_detection() {
        let config = ControlPlaneConfig::default()
            .with_max_concurrent_jobs(10)
            .with_stall_threshold(Duration::ZERO); // Immediate stall for testing

        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::new(config, coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        control_plane.register_job(job_id, tile, token.clone(), false);

        // Run health check - should detect stall
        control_plane.run_health_check().await;

        // Job should be cancelled and recovered
        assert!(token.is_cancelled());
        assert!(control_plane.registry.get(job_id).is_none());
        assert_eq!(control_plane.health.jobs_recovered(), 1);
    }

    #[tokio::test]
    async fn test_shutdown() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);

        assert!(!control_plane.is_shutting_down());

        control_plane.shutdown();

        assert!(control_plane.is_shutting_down());

        // Acquiring slots should fail after shutdown
        let result = control_plane.acquire_job_slot(false).await;
        assert!(matches!(result, Err(JobSlotError::Shutdown)));
    }

    #[tokio::test]
    async fn test_health_monitor_lifecycle() {
        let config =
            ControlPlaneConfig::default().with_health_check_interval(Duration::from_millis(10));

        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = Arc::new(PipelineControlPlane::new(config, coalescer));

        // Start health monitor
        let handle = Arc::clone(&control_plane).start_health_monitor();

        // Let it run a few cycles
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown
        control_plane.shutdown();

        // Monitor should complete
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_config_builder() {
        let config = ControlPlaneConfig::default()
            .with_max_concurrent_jobs(32)
            .with_stall_threshold(Duration::from_secs(120))
            .with_health_check_interval(Duration::from_secs(10))
            .with_semaphore_timeout(Duration::from_secs(60));

        assert_eq!(config.max_concurrent_jobs, 32);
        assert_eq!(config.stall_threshold, Duration::from_secs(120));
        assert_eq!(config.health_check_interval, Duration::from_secs(10));
        assert_eq!(config.semaphore_timeout, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_on_demand_timeout() {
        let config = ControlPlaneConfig::default()
            .with_max_concurrent_jobs(1)
            .with_semaphore_timeout(Duration::from_millis(50));

        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::new(config, coalescer);

        // Take the only slot
        let _permit = control_plane.acquire_job_slot(false).await.unwrap();

        // On-demand request should timeout
        let result = control_plane.acquire_job_slot(false).await;
        assert!(matches!(result, Err(JobSlotError::Timeout)));

        // Health should record the timeout
        assert_eq!(control_plane.health.semaphore_timeouts(), 1);
    }

    #[tokio::test]
    async fn test_submit_success() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        let result = control_plane
            .submit(job_id, tile, false, token, |observer| async move {
                // Simulate stage progression
                observer.on_stage_change(job_id, JobStage::Downloading);
                observer.on_stage_change(job_id, JobStage::Assembling);
                observer.on_stage_change(job_id, JobStage::Encoding);
                Ok::<_, JobError>(42u32)
            })
            .await;

        assert!(matches!(result, SubmitResult::Completed(42)));
        assert_eq!(control_plane.health.jobs_in_progress(), 0);
    }

    #[tokio::test]
    async fn test_submit_failure() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        let result = control_plane
            .submit(job_id, tile, false, token, |_observer| async move {
                Err::<u32, _>(JobError::AssemblyFailed("test".into()))
            })
            .await;

        assert!(matches!(result, SubmitResult::Failed(_)));
        let stats = control_plane.registry.stats();
        assert_eq!(stats.failed_jobs, 1);
    }

    #[tokio::test]
    async fn test_submit_prefetch_no_capacity() {
        let config = ControlPlaneConfig::default().with_max_concurrent_jobs(1);
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::new(config, coalescer);

        // Take the only slot
        let _permit = control_plane.acquire_job_slot(false).await.unwrap();

        // Submit prefetch - should fail with NoCapacity
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        let result = control_plane
            .submit(job_id, tile, true, token, |_observer| async move {
                Ok::<_, JobError>(42u32)
            })
            .await;

        assert!(matches!(result, SubmitResult::NoCapacity));
    }

    #[tokio::test]
    async fn test_submit_shutdown() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);

        control_plane.shutdown();

        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        let result = control_plane
            .submit(job_id, tile, false, token, |_observer| async move {
                Ok::<_, JobError>(42u32)
            })
            .await;

        assert!(matches!(result, SubmitResult::Shutdown));
    }

    #[tokio::test]
    async fn test_submit_cancelled() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();
        let token_clone = token.clone();

        let result = control_plane
            .submit(job_id, tile, false, token, |_observer| async move {
                // Cancel during work
                token_clone.cancel();
                Err::<u32, _>(JobError::Cancelled)
            })
            .await;

        assert!(matches!(result, SubmitResult::Cancelled));
    }

    #[tokio::test]
    async fn test_stage_observer() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let control_plane = PipelineControlPlane::with_defaults(coalescer);
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        // Register job manually to track stages
        let entry = control_plane.register_job(job_id, tile, token.clone(), false);
        assert_eq!(entry.stage(), JobStage::Queued);

        // Create observer and update stages
        let observer = control_plane.create_observer();
        observer.on_stage_change(job_id, JobStage::Downloading);

        // Check stage was updated
        let entry = control_plane.registry.get(job_id).unwrap();
        assert_eq!(entry.stage(), JobStage::Downloading);

        observer.on_stage_change(job_id, JobStage::Encoding);
        let entry = control_plane.registry.get(job_id).unwrap();
        assert_eq!(entry.stage(), JobStage::Encoding);
    }
}
