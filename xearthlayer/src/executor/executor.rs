//! Job executor - the core scheduling and execution engine.
//!
//! The [`JobExecutor`] is the central orchestrator that:
//! - Accepts job submissions and tracks their lifecycle
//! - Schedules tasks via a priority queue
//! - Dispatches tasks when resources become available
//! - Handles signals (pause, resume, stop, kill)
//! - Manages child job relationships
//! - Emits telemetry events
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                       JobExecutor                            │
//! │  ┌──────────┐  ┌───────────────┐  ┌──────────────────────┐  │
//! │  │ Job      │  │ Priority      │  │ Resource             │  │
//! │  │ Receiver │──│ Queue         │──│ Pools                │  │
//! │  └──────────┘  └───────────────┘  └──────────────────────┘  │
//! │       │               │                    │                 │
//! │       ▼               ▼                    ▼                 │
//! │  ┌──────────┐  ┌───────────────┐  ┌──────────────────────┐  │
//! │  │ Active   │  │ Task          │  │ Telemetry            │  │
//! │  │ Jobs     │  │ Dispatch      │  │ Sink                 │  │
//! │  └──────────┘  └───────────────┘  └──────────────────────┘  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{JobExecutor, ExecutorConfig};
//!
//! let config = ExecutorConfig::default();
//! let (executor, submitter) = JobExecutor::new(config);
//!
//! // Submit a job
//! let handle = submitter.submit(my_job);
//!
//! // Run the executor (in a spawned task)
//! tokio::spawn(async move {
//!     executor.run(shutdown_token).await;
//! });
//!
//! // Wait for job completion
//! let result = handle.wait().await;
//! ```

use super::context::{SharedTaskOutputs, SpawnedChildJob, TaskContext};
use super::handle::{JobHandle, JobStatus, Signal};
use super::job::{Job, JobId, JobResult};
use super::policy::{ErrorPolicy, Priority};
use super::queue::{PriorityQueue, QueuedTask};
use super::resource_pool::{ResourcePoolConfig, ResourcePools};
use super::task::{Task, TaskResult};
use super::telemetry::{NullTelemetrySink, TelemetryEvent, TelemetrySink};
use crate::metrics::MetricsClient;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// =============================================================================
// Configuration Constants
// =============================================================================

/// Default job receiver channel capacity.
pub const DEFAULT_JOB_CHANNEL_CAPACITY: usize = 256;

/// Default signal channel capacity per job.
pub const DEFAULT_SIGNAL_CHANNEL_CAPACITY: usize = 16;

/// Default maximum concurrent tasks (across all resource types).
pub const DEFAULT_MAX_CONCURRENT_TASKS: usize = 128;

/// Number of loop iterations between yield points (scheduler fairness).
const YIELD_EVERY_N_ITERATIONS: u64 = 50;

/// Stall detection threshold in milliseconds (warn if no activity for this long).
const STALL_DETECTION_THRESHOLD_MS: u64 = 30_000; // 30 seconds

/// Stall watchdog check interval in seconds.
const STALL_WATCHDOG_INTERVAL_SECS: u64 = 10;

// =============================================================================
// Executor Configuration
// =============================================================================

/// Configuration for the job executor.
#[derive(Clone, Debug)]
pub struct ExecutorConfig {
    /// Resource pool configuration.
    pub resource_pools: ResourcePoolConfig,

    /// Job receiver channel capacity.
    pub job_channel_capacity: usize,

    /// Maximum concurrent tasks the executor will dispatch.
    pub max_concurrent_tasks: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            resource_pools: ResourcePoolConfig::default(),
            job_channel_capacity: DEFAULT_JOB_CHANNEL_CAPACITY,
            max_concurrent_tasks: DEFAULT_MAX_CONCURRENT_TASKS,
        }
    }
}

impl From<&crate::config::ExecutorSettings> for ExecutorConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            resource_pools: ResourcePoolConfig::from(settings),
            job_channel_capacity: settings.job_channel_capacity,
            max_concurrent_tasks: settings.max_concurrent_tasks,
        }
    }
}

// =============================================================================
// Job Submitter
// =============================================================================

/// Handle for submitting jobs to the executor.
///
/// This is the public interface for job submission. It's cloneable and can
/// be shared across tasks.
#[derive(Clone)]
pub struct JobSubmitter {
    sender: mpsc::Sender<SubmittedJob>,
}

impl JobSubmitter {
    /// Submits a job for execution.
    ///
    /// Returns a handle that can be used to query status, wait for completion,
    /// or send signals to the job.
    ///
    /// # Panics
    ///
    /// Panics if the executor has been dropped (channel closed).
    pub fn submit(&self, job: impl Job + 'static) -> JobHandle {
        self.try_submit(job).expect("Executor channel closed")
    }

    /// Attempts to submit a job for execution.
    ///
    /// Returns `None` if the executor has been dropped.
    pub fn try_submit(&self, job: impl Job + 'static) -> Option<JobHandle> {
        self.try_submit_boxed(Box::new(job))
    }

    /// Attempts to submit a boxed job for execution.
    ///
    /// This is useful when working with factory patterns that return `Box<dyn Job>`.
    /// Returns `None` if the executor has been dropped.
    pub fn try_submit_boxed(&self, job: Box<dyn Job>) -> Option<JobHandle> {
        let job_id = job.id();
        let priority = job.priority();
        let name = job.name().to_string();

        // Create channels for status updates and signalling
        let (status_tx, status_rx) = watch::channel(JobStatus::Pending);
        let (signal_tx, signal_rx) = mpsc::channel(DEFAULT_SIGNAL_CHANNEL_CAPACITY);

        let handle = JobHandle::new(job_id.clone(), status_rx, signal_tx);
        let result_holder = handle.result_holder();

        let submitted = SubmittedJob {
            job,
            job_id,
            name,
            priority,
            status_tx,
            signal_rx,
            result_holder,
        };

        self.sender.try_send(submitted).ok()?;
        Some(handle)
    }
}

/// A job that has been submitted to the executor.
struct SubmittedJob {
    job: Box<dyn Job>,
    job_id: JobId,
    name: String,
    priority: Priority,
    status_tx: watch::Sender<JobStatus>,
    signal_rx: mpsc::Receiver<Signal>,
    result_holder: std::sync::Arc<tokio::sync::Mutex<Option<JobResult>>>,
}

// =============================================================================
// Active Job State
// =============================================================================

/// State of an actively executing job.
struct ActiveJob {
    /// The job being executed.
    job: Box<dyn Job>,

    /// Job priority (stored for potential priority boosting).
    #[allow(dead_code)]
    priority: Priority,

    /// Channel to update job status.
    status_tx: watch::Sender<JobStatus>,

    /// Channel to receive signals.
    signal_rx: mpsc::Receiver<Signal>,

    /// Current job status.
    status: JobStatus,

    /// When the job started executing.
    started_at: Instant,

    /// Cancellation token for this job's tasks.
    cancellation: CancellationToken,

    /// Channel for child jobs spawned by tasks.
    child_job_tx: mpsc::UnboundedSender<SpawnedChildJob>,

    /// Receiver for child jobs (polled by executor).
    child_job_rx: mpsc::UnboundedReceiver<SpawnedChildJob>,

    /// Tasks waiting to be enqueued (from create_tasks).
    pending_tasks: Vec<Box<dyn Task>>,

    /// Number of tasks currently executing.
    tasks_in_flight: usize,

    /// Number of tasks queued but not yet dispatched.
    /// This prevents the race condition where job completion check sees
    /// both pending_tasks empty and tasks_in_flight == 0 before a
    /// sequential task is picked up from the queue.
    queued_tasks: usize,

    /// Names of tasks that succeeded.
    succeeded_tasks: Vec<String>,

    /// Names of tasks that failed.
    failed_tasks: Vec<String>,

    /// Names of tasks that were cancelled.
    cancelled_tasks: Vec<String>,

    /// Outputs from completed tasks (shared with task contexts).
    task_outputs: SharedTaskOutputs,

    /// Child jobs that have been spawned.
    child_job_ids: Vec<JobId>,

    /// Child jobs that succeeded.
    succeeded_children: Vec<JobId>,

    /// Child jobs that failed.
    failed_children: Vec<JobId>,

    /// Retry state for tasks (task_name -> attempt count).
    retry_counts: HashMap<String, u32>,

    /// Parent job ID if this is a child job.
    parent_job_id: Option<JobId>,

    /// Result holder shared with the JobHandle for returning job output.
    result_holder: std::sync::Arc<tokio::sync::Mutex<Option<JobResult>>>,
}

impl ActiveJob {
    fn new(submitted: SubmittedJob) -> Self {
        let (child_job_tx, child_job_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let result_holder = submitted.result_holder;

        Self {
            job: submitted.job,
            priority: submitted.priority,
            status_tx: submitted.status_tx,
            signal_rx: submitted.signal_rx,
            status: JobStatus::Pending,
            started_at: Instant::now(),
            cancellation,
            child_job_tx,
            child_job_rx,
            pending_tasks: Vec::new(),
            tasks_in_flight: 0,
            queued_tasks: 0,
            succeeded_tasks: Vec::new(),
            failed_tasks: Vec::new(),
            cancelled_tasks: Vec::new(),
            task_outputs: Arc::new(std::sync::RwLock::new(HashMap::new())),
            child_job_ids: Vec::new(),
            succeeded_children: Vec::new(),
            failed_children: Vec::new(),
            retry_counts: HashMap::new(),
            parent_job_id: None,
            result_holder,
        }
    }

    fn with_parent(submitted: SubmittedJob, parent_id: JobId) -> Self {
        let mut job = Self::new(submitted);
        job.parent_job_id = Some(parent_id);
        job
    }

    fn update_status(&mut self, status: JobStatus) {
        self.status = status;
        let _ = self.status_tx.send(status);
    }

    fn is_paused(&self) -> bool {
        matches!(self.status, JobStatus::Paused)
    }

    fn should_stop(&self) -> bool {
        matches!(
            self.status,
            JobStatus::Stopped | JobStatus::Cancelled | JobStatus::Failed
        )
    }

    fn has_pending_work(&self) -> bool {
        !self.pending_tasks.is_empty()
            || self.tasks_in_flight > 0
            || self.queued_tasks > 0
            || !self.child_job_ids.is_empty()
    }

    fn all_children_complete(&self) -> bool {
        self.child_job_ids.is_empty()
    }

    fn build_result(&self) -> JobResult {
        // Extract DDS data from task outputs if available
        // This is used by BuildAndCacheDdsTask to return data directly
        let output_data = {
            let outputs = self.task_outputs.read().unwrap();
            outputs
                .get("BuildAndCacheDds")
                .and_then(|output| output.get::<Vec<u8>>("dds_data"))
                .cloned()
        };

        JobResult {
            succeeded_tasks: self.succeeded_tasks.clone(),
            failed_tasks: self.failed_tasks.clone(),
            cancelled_tasks: self.cancelled_tasks.clone(),
            succeeded_children: self.succeeded_children.clone(),
            failed_children: self.failed_children.clone(),
            cancelled_children: Vec::new(), // Not tracked separately
            duration: self.started_at.elapsed(),
            output_data,
        }
    }

    fn check_error_policy(&self) -> Option<JobStatus> {
        match self.job.error_policy() {
            ErrorPolicy::FailFast => {
                if !self.failed_tasks.is_empty() || !self.failed_children.is_empty() {
                    Some(JobStatus::Failed)
                } else {
                    None
                }
            }
            ErrorPolicy::ContinueOnError => None, // Never fail early
            ErrorPolicy::PartialSuccess { threshold } => {
                let total = self.succeeded_tasks.len() + self.failed_tasks.len();
                if total > 0 {
                    let success_ratio = self.succeeded_tasks.len() as f64 / total as f64;
                    if success_ratio < threshold && self.pending_tasks.is_empty() {
                        Some(JobStatus::Failed)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            ErrorPolicy::Custom => None, // Defer to on_complete
        }
    }

    fn compute_final_status(&self) -> JobStatus {
        let result = self.build_result();
        match self.job.error_policy() {
            ErrorPolicy::Custom => self.job.on_complete(&result),
            ErrorPolicy::PartialSuccess { threshold } => {
                let total = result.succeeded_tasks.len() + result.failed_tasks.len();
                if total == 0 {
                    JobStatus::Succeeded
                } else {
                    let success_ratio = result.succeeded_tasks.len() as f64 / total as f64;
                    if success_ratio >= threshold {
                        JobStatus::Succeeded
                    } else {
                        JobStatus::Failed
                    }
                }
            }
            _ => {
                if result.failed_tasks.is_empty() && result.failed_children.is_empty() {
                    JobStatus::Succeeded
                } else {
                    JobStatus::Failed
                }
            }
        }
    }
}

// =============================================================================
// Task Completion
// =============================================================================

/// Result of a completed task execution.
struct TaskCompletion {
    job_id: JobId,
    task_name: String,
    result: TaskResult,
    duration: Duration,
}

// =============================================================================
// Job Executor
// =============================================================================

/// The core job execution engine.
///
/// The executor manages the lifecycle of jobs and tasks:
/// - Receives submitted jobs via channel
/// - Creates tasks from jobs and enqueues them
/// - Dispatches tasks when resources are available
/// - Handles task completion and updates job state
/// - Processes signals (pause, resume, stop, kill)
/// - Manages parent-child job relationships
pub struct JobExecutor {
    /// Resource pools for limiting concurrency.
    resource_pools: Arc<ResourcePools>,

    /// Priority queue for pending tasks.
    task_queue: Arc<Mutex<PriorityQueue>>,

    /// Active jobs by ID.
    active_jobs: Arc<Mutex<HashMap<JobId, ActiveJob>>>,

    /// Receiver for submitted jobs.
    job_receiver: mpsc::Receiver<SubmittedJob>,

    /// Sender for task completions.
    completion_tx: mpsc::UnboundedSender<TaskCompletion>,

    /// Receiver for task completions.
    completion_rx: mpsc::UnboundedReceiver<TaskCompletion>,

    /// Telemetry sink for emitting events.
    telemetry: Arc<dyn TelemetrySink>,

    /// Notifier for when work might be available.
    work_notify: Arc<Notify>,

    /// Configuration.
    config: ExecutorConfig,

    /// Number of currently dispatched tasks.
    dispatched_count: usize,

    /// Metrics client for emitting metrics events.
    metrics_client: Option<MetricsClient>,

    /// Last activity timestamp (milliseconds since UNIX epoch).
    /// Used for stall detection - updated on every loop iteration.
    last_activity_ms: Arc<AtomicU64>,

    /// Count of pending work (active jobs + queued tasks).
    /// Used by stall detector to distinguish idle from stalled.
    /// Only warn about stall if there's pending work that should be processing.
    pending_work_count: Arc<AtomicU64>,

    /// Loop iteration counter for periodic yield.
    loop_count: u64,
}

impl JobExecutor {
    /// Creates a new job executor.
    ///
    /// Returns the executor and a submitter handle for submitting jobs.
    pub fn new(config: ExecutorConfig) -> (Self, JobSubmitter) {
        Self::with_telemetry(config, Arc::new(NullTelemetrySink))
    }

    /// Creates a new job executor with a telemetry sink.
    pub fn with_telemetry(
        config: ExecutorConfig,
        telemetry: Arc<dyn TelemetrySink>,
    ) -> (Self, JobSubmitter) {
        Self::with_telemetry_and_metrics(config, telemetry, None)
    }

    /// Creates a new job executor with telemetry and metrics.
    ///
    /// The `MetricsClient` will be passed to all tasks, enabling per-chunk
    /// metrics emission for downloads, disk cache, and job lifecycle events.
    pub fn with_telemetry_and_metrics(
        config: ExecutorConfig,
        telemetry: Arc<dyn TelemetrySink>,
        metrics_client: Option<MetricsClient>,
    ) -> (Self, JobSubmitter) {
        let (job_tx, job_rx) = mpsc::channel(config.job_channel_capacity);
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        // Initialize last activity to current time
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let executor = Self {
            resource_pools: Arc::new(ResourcePools::new(config.resource_pools.clone())),
            task_queue: Arc::new(Mutex::new(PriorityQueue::new())),
            active_jobs: Arc::new(Mutex::new(HashMap::new())),
            job_receiver: job_rx,
            completion_tx,
            completion_rx,
            telemetry,
            work_notify: Arc::new(Notify::new()),
            config,
            dispatched_count: 0,
            metrics_client,
            last_activity_ms: Arc::new(AtomicU64::new(now_ms)),
            pending_work_count: Arc::new(AtomicU64::new(0)),
            loop_count: 0,
        };

        let submitter = JobSubmitter { sender: job_tx };

        (executor, submitter)
    }

    /// Runs the executor until shutdown is signalled.
    ///
    /// This is the main event loop that:
    /// - Receives new job submissions
    /// - Dispatches tasks to workers
    /// - Handles task completions
    /// - Processes job signals
    ///
    /// Includes defensive measures:
    /// - Stall detection watchdog (logs warning if no activity for 30s)
    /// - Periodic yield points (every 50 iterations) for scheduler fairness
    pub async fn run(mut self, shutdown: CancellationToken) {
        // Spawn stall detection watchdog
        let last_activity = Arc::clone(&self.last_activity_ms);
        let pending_work = Arc::clone(&self.pending_work_count);
        let watchdog_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = watchdog_shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let last_ms = last_activity.load(Ordering::Relaxed);
                        let elapsed_ms = now_ms.saturating_sub(last_ms);
                        let work_count = pending_work.load(Ordering::Relaxed);

                        if elapsed_ms > STALL_DETECTION_THRESHOLD_MS && work_count > 0 {
                            // True stall: pending work but no progress
                            warn!(
                                elapsed_ms = elapsed_ms,
                                pending_work = work_count,
                                threshold_ms = STALL_DETECTION_THRESHOLD_MS,
                                "STALL DETECTED: Executor has {} pending work items but no progress for {}s",
                                work_count,
                                elapsed_ms / 1000
                            );
                        } else if elapsed_ms > STALL_DETECTION_THRESHOLD_MS {
                            // Idle: no pending work, just waiting for requests
                            debug!(
                                elapsed_ms = elapsed_ms,
                                "Stall watchdog: executor idle (no pending work)"
                            );
                        } else {
                            debug!(
                                elapsed_ms = elapsed_ms,
                                pending_work = work_count,
                                "Stall watchdog: executor loop healthy"
                            );
                        }
                    }
                }
            }
        });

        info!(
            "Executor started with stall detection (threshold={}s, check_interval={}s)",
            STALL_DETECTION_THRESHOLD_MS / 1000,
            STALL_WATCHDOG_INTERVAL_SECS
        );

        loop {
            // Update activity timestamp at start of each iteration
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            self.last_activity_ms.store(now_ms, Ordering::Relaxed);

            tokio::select! {
                biased;

                // Check for shutdown first
                _ = shutdown.cancelled() => {
                    self.shutdown().await;
                    break;
                }

                // Receive new job submissions
                Some(submitted) = self.job_receiver.recv() => {
                    self.handle_job_submission(submitted).await;
                }

                // Handle task completions
                Some(completion) = self.completion_rx.recv() => {
                    self.handle_task_completion(completion).await;
                }

                // Dispatch tasks when notified
                _ = self.work_notify.notified() => {
                    self.dispatch_tasks().await;
                }
            }

            // Process signals for all active jobs
            self.process_signals().await;

            // Check for new child jobs
            self.collect_child_jobs().await;

            // Always try to dispatch after any activity
            self.dispatch_tasks().await;

            // Complete jobs that are done
            self.complete_finished_jobs().await;

            // Update pending work count for stall detection
            self.update_pending_work_count().await;

            // Periodic yield for scheduler fairness
            self.loop_count += 1;
            if self.loop_count.is_multiple_of(YIELD_EVERY_N_ITERATIONS) {
                tokio::task::yield_now().await;
            }
        }
    }

    /// Update the pending work count for stall detection.
    ///
    /// Counts active jobs + queued tasks. The watchdog uses this to
    /// distinguish between "idle" (no work) and "stalled" (work pending
    /// but not progressing).
    async fn update_pending_work_count(&self) {
        let active_jobs = self.active_jobs.lock().await.len();
        let queued_tasks = self.task_queue.lock().await.len();
        let total = (active_jobs + queued_tasks) as u64;
        self.pending_work_count.store(total, Ordering::Relaxed);
    }

    async fn handle_job_submission(&mut self, submitted: SubmittedJob) {
        let job_id = submitted.job_id.clone();
        let name = submitted.name.clone();
        let priority = submitted.priority;

        info!(
            job_id = %job_id,
            job_name = %name,
            priority = ?priority,
            "Job submitted"
        );

        // Emit telemetry
        self.telemetry.emit(TelemetryEvent::JobSubmitted {
            job_id: job_id.clone(),
            name: name.clone(),
            priority,
        });

        // Create active job state
        let mut active = ActiveJob::new(submitted);

        // Create tasks from the job
        active.pending_tasks = active.job.create_tasks();

        // Update status to running
        active.update_status(JobStatus::Running);
        self.telemetry.emit(TelemetryEvent::JobStarted {
            job_id: job_id.clone(),
        });

        // Enqueue ONLY the first task - tasks run sequentially within a job
        // to ensure proper data flow between dependent tasks
        let total_tasks = active.pending_tasks.len();

        info!(
            job_id = %job_id,
            task_count = total_tasks,
            "Job started"
        );

        // Handle first task if present. Use is_empty() check instead of drain(..1)
        // to avoid panic when job has 0 tasks (e.g., CacheGcJob with no eviction candidates).
        if !active.pending_tasks.is_empty() {
            let first_task = active.pending_tasks.remove(0);
            let task_name = first_task.name().to_string();
            let queued = QueuedTask::new(first_task, job_id.clone(), priority);

            self.telemetry.emit(TelemetryEvent::TaskEnqueued {
                job_id: job_id.clone(),
                task_name,
                priority,
                queue_depth: 0,
            });

            let mut queue = self.task_queue.lock().await;
            queue.push(queued);
        }

        // Store active job
        let mut jobs = self.active_jobs.lock().await;
        jobs.insert(job_id, active);

        // Notify that work is available
        self.work_notify.notify_one();
    }

    async fn dispatch_tasks(&mut self) {
        // Don't dispatch if we're at capacity
        if self.dispatched_count >= self.config.max_concurrent_tasks {
            return;
        }

        loop {
            // Try to get a task that has resources available
            let (task, job_id, priority, resource_type) = {
                let mut queue = self.task_queue.lock().await;

                // Look for a task we can dispatch
                let mut temp_queue = Vec::new();
                let mut found = None;

                while let Some(queued) = queue.pop() {
                    // Check if resources are available
                    if self
                        .resource_pools
                        .try_acquire(queued.resource_type)
                        .is_some()
                    {
                        found = Some((
                            queued.task,
                            queued.job_id,
                            queued.priority,
                            queued.resource_type,
                        ));
                        break;
                    } else {
                        temp_queue.push(queued);
                    }
                }

                // Put back tasks we couldn't dispatch
                for task in temp_queue {
                    queue.push(task);
                }

                match found {
                    Some(t) => t,
                    None => return, // No dispatchable tasks
                }
            };

            // Check if job is still active and not paused/stopped
            let (can_dispatch, ctx) = {
                let jobs = self.active_jobs.lock().await;
                if let Some(job) = jobs.get(&job_id) {
                    if job.is_paused() || job.should_stop() {
                        // Re-queue the task
                        let mut queue = self.task_queue.lock().await;
                        queue.push(QueuedTask::new(task, job_id, priority));
                        continue;
                    }

                    // Create task context with shared reference to accumulated outputs
                    let mut ctx = TaskContext::with_child_sender(
                        job_id.clone(),
                        job.cancellation.clone(),
                        job.child_job_tx.clone(),
                        Arc::clone(&job.task_outputs),
                    );
                    // Attach metrics client if available
                    if let Some(ref metrics) = self.metrics_client {
                        ctx = ctx.with_metrics(metrics.clone());
                    }
                    (true, ctx)
                } else {
                    // Job was removed, skip this task
                    continue;
                }
            };

            if !can_dispatch {
                continue;
            }

            // Update in-flight count and queued count
            {
                let mut jobs = self.active_jobs.lock().await;
                if let Some(job) = jobs.get_mut(&job_id) {
                    job.tasks_in_flight += 1;
                    // Decrement queued_tasks if this was a sequential task
                    // (queued_tasks is incremented in handle_task_completion before enqueueing)
                    if job.queued_tasks > 0 {
                        job.queued_tasks -= 1;
                    }
                }
            }

            self.dispatched_count += 1;
            let task_name = task.name().to_string();

            info!(
                job_id = %job_id,
                task_name = %task_name,
                resource_type = ?resource_type,
                "Task started"
            );

            // Emit telemetry
            self.telemetry.emit(TelemetryEvent::TaskStarted {
                job_id: job_id.clone(),
                task_name: task_name.clone(),
                resource_type,
            });

            // Spawn task execution
            let completion_tx = self.completion_tx.clone();
            let resource_pools = Arc::clone(&self.resource_pools);
            let work_notify = Arc::clone(&self.work_notify);
            let _telemetry = Arc::clone(&self.telemetry);

            tokio::spawn(async move {
                let start = Instant::now();

                // Acquire resource permit (we know it's available)
                let _permit = resource_pools.acquire(resource_type).await;

                // Execute the task
                let mut ctx = ctx;
                let result = task.execute(&mut ctx).await;

                let duration = start.elapsed();

                // Send completion
                let _ = completion_tx.send(TaskCompletion {
                    job_id,
                    task_name,
                    result,
                    duration,
                });

                // Notify executor
                work_notify.notify_one();
            });

            // Check if we've hit dispatch limit
            if self.dispatched_count >= self.config.max_concurrent_tasks {
                break;
            }
        }
    }

    async fn handle_task_completion(&mut self, completion: TaskCompletion) {
        self.dispatched_count = self.dispatched_count.saturating_sub(1);

        let result_kind = completion.result.kind();

        match &completion.result {
            TaskResult::Success | TaskResult::SuccessWithOutput(_) => {
                info!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    duration_ms = completion.duration.as_millis(),
                    "Task completed successfully"
                );
            }
            TaskResult::Failed(err) => {
                error!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    error = %err,
                    duration_ms = completion.duration.as_millis(),
                    "Task failed"
                );
            }
            TaskResult::Retry(err) => {
                warn!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    error = %err,
                    duration_ms = completion.duration.as_millis(),
                    "Task needs retry"
                );
            }
            TaskResult::Cancelled => {
                warn!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    duration_ms = completion.duration.as_millis(),
                    "Task cancelled"
                );
            }
        }

        // Emit telemetry
        self.telemetry.emit(TelemetryEvent::TaskCompleted {
            job_id: completion.job_id.clone(),
            task_name: completion.task_name.clone(),
            result: result_kind,
            duration: completion.duration,
        });

        let mut jobs = self.active_jobs.lock().await;
        let job = match jobs.get_mut(&completion.job_id) {
            Some(j) => j,
            None => return, // Job was removed
        };

        job.tasks_in_flight -= 1;

        // Track whether to enqueue the next task
        let mut should_enqueue_next = false;

        match completion.result {
            TaskResult::Success => {
                job.succeeded_tasks.push(completion.task_name);
                should_enqueue_next = true;
            }
            TaskResult::SuccessWithOutput(output) => {
                if let Ok(mut outputs) = job.task_outputs.write() {
                    outputs.insert(completion.task_name.clone(), output);
                }
                job.succeeded_tasks.push(completion.task_name);
                should_enqueue_next = true;
            }
            TaskResult::Failed(_error) => {
                // Check if we should retry
                let task_name = &completion.task_name;
                let _attempt = job.retry_counts.get(task_name).copied().unwrap_or(1);
                // Note: we'd need to keep the task to retry, which requires refactoring
                // For now, just record the failure
                job.failed_tasks.push(completion.task_name);

                // Check error policy
                if let Some(status) = job.check_error_policy() {
                    job.update_status(status);
                    job.cancellation.cancel();
                }
                // Don't enqueue next task on failure
            }
            TaskResult::Retry(_error) => {
                // For now, treat retry as failure
                // Full retry implementation would re-enqueue the task
                job.failed_tasks.push(completion.task_name.clone());

                self.telemetry.emit(TelemetryEvent::TaskRetrying {
                    job_id: completion.job_id.clone(),
                    task_name: completion.task_name,
                    attempt: 1,
                    delay: Duration::from_millis(100),
                });
                // Don't enqueue next task on retry/failure
            }
            TaskResult::Cancelled => {
                job.cancelled_tasks.push(completion.task_name);
                // Don't enqueue next task on cancellation
            }
        }

        // Sequential task execution: enqueue the next pending task on success
        // IMPORTANT: Increment queued_tasks BEFORE removing from pending_tasks
        // to prevent the race condition where job completion check sees both
        // pending_tasks empty and tasks_in_flight == 0 before the task is dispatched.
        let next_task_info = if should_enqueue_next && !job.pending_tasks.is_empty() {
            job.queued_tasks += 1; // Reserve the slot before removing from pending
            let next_task = job.pending_tasks.remove(0);
            let priority = job.priority;
            Some((next_task, priority))
        } else {
            None
        };

        // Release jobs lock before acquiring task_queue lock to avoid deadlock
        let job_id_for_enqueue = completion.job_id.clone();
        drop(jobs);

        // Enqueue the next task if available
        if let Some((task, priority)) = next_task_info {
            let task_name = task.name().to_string();
            let queued = QueuedTask::new(task, job_id_for_enqueue.clone(), priority);

            info!(
                job_id = %job_id_for_enqueue,
                task_name = %task_name,
                "Enqueuing next sequential task"
            );

            self.telemetry.emit(TelemetryEvent::TaskEnqueued {
                job_id: job_id_for_enqueue,
                task_name,
                priority,
                queue_depth: 0,
            });

            let mut queue = self.task_queue.lock().await;
            queue.push(queued);
        }

        self.work_notify.notify_one();
    }

    async fn process_signals(&mut self) {
        let mut jobs = self.active_jobs.lock().await;

        for (job_id, job) in jobs.iter_mut() {
            // Drain all pending signals
            while let Ok(signal) = job.signal_rx.try_recv() {
                self.telemetry.emit(TelemetryEvent::JobSignalled {
                    job_id: job_id.clone(),
                    signal,
                });

                match signal {
                    Signal::Pause => {
                        if job.status == JobStatus::Running {
                            job.update_status(JobStatus::Paused);
                        }
                    }
                    Signal::Resume => {
                        if job.status == JobStatus::Paused {
                            job.update_status(JobStatus::Running);
                            self.work_notify.notify_one();
                        }
                    }
                    Signal::Stop => {
                        job.update_status(JobStatus::Stopped);
                        // Don't cancel - let in-flight tasks complete
                    }
                    Signal::Kill => {
                        job.update_status(JobStatus::Cancelled);
                        job.cancellation.cancel();
                    }
                }
            }
        }
    }

    async fn collect_child_jobs(&mut self) {
        let mut new_children = Vec::new();

        {
            let mut jobs = self.active_jobs.lock().await;
            for (parent_id, job) in jobs.iter_mut() {
                while let Ok(spawned) = job.child_job_rx.try_recv() {
                    let child_id = spawned.job.id();
                    job.child_job_ids.push(child_id.clone());

                    self.telemetry.emit(TelemetryEvent::ChildJobSpawned {
                        parent_job_id: parent_id.clone(),
                        child_job_id: child_id.clone(),
                        task_name: spawned.spawning_task,
                    });

                    new_children.push((spawned.job, parent_id.clone()));
                }
            }
        }

        // Submit child jobs
        for (child_job, parent_id) in new_children {
            let child_id = child_job.id();
            let priority = child_job.priority();
            let name = child_job.name().to_string();

            let (status_tx, _status_rx) = watch::channel(JobStatus::Pending);
            let (_signal_tx, signal_rx) = mpsc::channel(DEFAULT_SIGNAL_CHANNEL_CAPACITY);

            // Child jobs don't have external handles, so use a dummy result holder
            let result_holder = std::sync::Arc::new(tokio::sync::Mutex::new(None));

            let submitted = SubmittedJob {
                job: child_job,
                job_id: child_id.clone(),
                name: name.clone(),
                priority,
                status_tx,
                signal_rx,
                result_holder,
            };

            // Create active job with parent reference
            let mut active = ActiveJob::with_parent(submitted, parent_id);
            active.pending_tasks = active.job.create_tasks();
            active.update_status(JobStatus::Running);

            self.telemetry.emit(TelemetryEvent::JobStarted {
                job_id: child_id.clone(),
            });

            // Enqueue child's tasks
            {
                let mut queue = self.task_queue.lock().await;
                for task in active.pending_tasks.drain(..) {
                    let queued = QueuedTask::new(task, child_id.clone(), priority);
                    queue.push(queued);
                }
            }

            let mut jobs = self.active_jobs.lock().await;
            jobs.insert(child_id, active);
        }
    }

    async fn complete_finished_jobs(&mut self) {
        let mut completed_jobs = Vec::new();

        {
            let jobs = self.active_jobs.lock().await;
            for (job_id, job) in jobs.iter() {
                // Job is complete when:
                // - No pending tasks
                // - No tasks in flight
                // - All children complete
                // - OR job was stopped/cancelled
                let is_complete = !job.has_pending_work() && job.all_children_complete()
                    || job.status == JobStatus::Stopped
                    || job.status == JobStatus::Cancelled;

                if is_complete && !job.status.is_terminal() {
                    completed_jobs.push(job_id.clone());
                }
            }
        }

        // Complete each finished job
        for job_id in completed_jobs {
            let (final_status, result, parent_id) = {
                let mut jobs = self.active_jobs.lock().await;
                if let Some(job) = jobs.get_mut(&job_id) {
                    let status = if job.status.is_terminal() {
                        job.status
                    } else {
                        job.compute_final_status()
                    };

                    // Build result BEFORE updating status (handle waits on status change)
                    let result = job.build_result();

                    // Set the result in the holder for the JobHandle to read
                    {
                        let mut holder = job.result_holder.lock().await;
                        *holder = Some(result.clone());
                    }

                    // Now update status - this triggers the handle's wait() to unblock
                    job.update_status(status);
                    (status, result, job.parent_job_id.clone())
                } else {
                    continue;
                }
            };

            // Log job completion
            match final_status {
                JobStatus::Succeeded => {
                    info!(
                        job_id = %job_id,
                        duration_ms = result.duration.as_millis(),
                        tasks_succeeded = result.succeeded_tasks.len(),
                        "Job completed successfully"
                    );
                }
                JobStatus::Failed => {
                    error!(
                        job_id = %job_id,
                        duration_ms = result.duration.as_millis(),
                        tasks_succeeded = result.succeeded_tasks.len(),
                        tasks_failed = result.failed_tasks.len(),
                        "Job failed"
                    );
                }
                _ => {
                    warn!(
                        job_id = %job_id,
                        status = ?final_status,
                        duration_ms = result.duration.as_millis(),
                        "Job ended"
                    );
                }
            }

            // Emit completion telemetry
            self.telemetry.emit(TelemetryEvent::JobCompleted {
                job_id: job_id.clone(),
                status: final_status,
                duration: result.duration,
                tasks_succeeded: result.succeeded_tasks.len(),
                tasks_failed: result.failed_tasks.len(),
                children_succeeded: result.succeeded_children.len(),
                children_failed: result.failed_children.len(),
            });

            // Update parent if this was a child job
            if let Some(parent_id) = parent_id {
                let mut jobs = self.active_jobs.lock().await;
                if let Some(parent) = jobs.get_mut(&parent_id) {
                    // Remove from parent's child list
                    parent.child_job_ids.retain(|id| id != &job_id);

                    // Record child result
                    if final_status == JobStatus::Succeeded {
                        parent.succeeded_children.push(job_id.clone());
                    } else {
                        parent.failed_children.push(job_id.clone());
                    }
                }
            }

            // Remove completed job
            let mut jobs = self.active_jobs.lock().await;
            jobs.remove(&job_id);
        }
    }

    async fn shutdown(&mut self) {
        // Cancel all active jobs
        let mut jobs = self.active_jobs.lock().await;
        for (job_id, job) in jobs.iter_mut() {
            job.cancellation.cancel();
            job.update_status(JobStatus::Cancelled);

            self.telemetry.emit(TelemetryEvent::JobCompleted {
                job_id: job_id.clone(),
                status: JobStatus::Cancelled,
                duration: job.started_at.elapsed(),
                tasks_succeeded: job.succeeded_tasks.len(),
                tasks_failed: job.failed_tasks.len(),
                children_succeeded: job.succeeded_children.len(),
                children_failed: job.failed_children.len(),
            });
        }
        jobs.clear();

        // Clear task queue
        let mut queue = self.task_queue.lock().await;
        queue.clear();
    }
}

impl std::fmt::Debug for JobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobExecutor")
            .field("dispatched_count", &self.dispatched_count)
            .field("config", &self.config)
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ResourceType;

    struct SimpleTask {
        name: String,
    }

    impl Task for SimpleTask {
        fn name(&self) -> &str {
            &self.name
        }

        fn resource_type(&self) -> ResourceType {
            ResourceType::CPU
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a mut TaskContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TaskResult> + Send + 'a>> {
            Box::pin(async { TaskResult::Success })
        }
    }

    struct SimpleJob {
        id: String,
        tasks: Vec<Box<dyn Task>>,
    }

    impl Job for SimpleJob {
        fn id(&self) -> JobId {
            JobId::new(&self.id)
        }

        fn name(&self) -> &str {
            "SimpleJob"
        }

        fn error_policy(&self) -> ErrorPolicy {
            ErrorPolicy::FailFast
        }

        fn priority(&self) -> Priority {
            Priority::PREFETCH
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            self.tasks
                .iter()
                .map(|t| {
                    Box::new(SimpleTask {
                        name: t.name().to_string(),
                    }) as Box<dyn Task>
                })
                .collect()
        }

        fn on_complete(&self, result: &JobResult) -> JobStatus {
            if result.failed_tasks.is_empty() {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
    }

    #[tokio::test]
    async fn test_executor_creation() {
        let config = ExecutorConfig::default();
        let (executor, _submitter) = JobExecutor::new(config);

        assert_eq!(executor.dispatched_count, 0);
    }

    #[tokio::test]
    async fn test_job_submission() {
        let config = ExecutorConfig::default();
        let (_executor, submitter) = JobExecutor::new(config);

        let job = SimpleJob {
            id: "test-job".to_string(),
            tasks: vec![Box::new(SimpleTask {
                name: "task1".to_string(),
            })],
        };

        let handle = submitter.try_submit(job);
        assert!(handle.is_some());
        let handle = handle.unwrap();
        assert_eq!(handle.id().as_str(), "test-job");
    }

    #[tokio::test]
    async fn test_job_execution() {
        let config = ExecutorConfig::default();
        let (executor, submitter) = JobExecutor::new(config);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // Spawn executor
        let executor_handle = tokio::spawn(async move {
            executor.run(shutdown_clone).await;
        });

        // Submit a job
        let job = SimpleJob {
            id: "exec-test".to_string(),
            tasks: vec![Box::new(SimpleTask {
                name: "task1".to_string(),
            })],
        };

        let mut handle = submitter.submit(job);

        // Wait for completion with timeout
        tokio::select! {
            _result = handle.wait() => {
                // Job completed
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                panic!("Job timed out");
            }
        }

        // Verify job succeeded
        assert!(handle.status().is_terminal());

        // Shutdown executor
        shutdown.cancel();
        let _ = executor_handle.await;
    }

    #[tokio::test]
    async fn test_job_signalling() {
        let config = ExecutorConfig::default();
        let (_executor, submitter) = JobExecutor::new(config);

        let job = SimpleJob {
            id: "signal-test".to_string(),
            tasks: vec![],
        };

        let handle = submitter.try_submit(job).unwrap();

        // Initial status should be pending
        assert_eq!(handle.status(), JobStatus::Pending);

        // Send signals (they won't be processed without running executor)
        handle.pause();
        handle.resume();
        handle.stop();
        handle.kill();

        // Signals were sent successfully (no panic)
    }

    #[test]
    fn test_executor_config_default() {
        let config = ExecutorConfig::default();
        assert_eq!(config.job_channel_capacity, DEFAULT_JOB_CHANNEL_CAPACITY);
        assert_eq!(config.max_concurrent_tasks, DEFAULT_MAX_CONCURRENT_TASKS);
    }

    /// Regression test for panic when job has 0 tasks.
    ///
    /// This bug occurred when CacheGcJob returned 0 eviction candidates,
    /// causing `drain(..1)` to panic on an empty vector.
    #[tokio::test]
    async fn test_job_with_zero_tasks_does_not_panic() {
        let config = ExecutorConfig::default();
        let (executor, submitter) = JobExecutor::new(config);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // Spawn executor
        let executor_handle = tokio::spawn(async move {
            executor.run(shutdown_clone).await;
        });

        // Submit a job with ZERO tasks (reproduces GC bug)
        let job = SimpleJob {
            id: "zero-tasks-test".to_string(),
            tasks: vec![], // Empty!
        };

        let mut handle = submitter.submit(job);

        // Wait for completion with timeout - should NOT panic
        tokio::select! {
            _result = handle.wait() => {
                // Job completed (with 0 tasks, should complete immediately)
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                panic!("Job timed out - executor may have panicked");
            }
        }

        // Verify job succeeded (0 tasks = success)
        assert!(handle.status().is_terminal());
        assert_eq!(handle.status(), JobStatus::Succeeded);

        // Shutdown executor
        shutdown.cancel();
        let _ = executor_handle.await;
    }
}
