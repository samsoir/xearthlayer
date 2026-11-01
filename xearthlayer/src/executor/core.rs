//! Job executor core - main struct and run loop.
//!
//! This module contains the [`JobExecutor`] struct and its main event loop.
//! Handler methods are implemented in separate modules:
//! - `dispatch`: Task dispatching
//! - `lifecycle`: Job/task lifecycle management
//! - `signals`: Signal processing

use super::active_job::{ActiveJob, TaskCompletion};
use super::config::ExecutorConfig;
use super::job::JobId;
use super::queue::PriorityQueue;
use super::resource_pool::ResourcePools;
use super::submitter::{JobSubmitter, SubmittedJob, WaitingJob};
use super::telemetry::{NullTelemetrySink, TelemetrySink};
use super::watchdog::{StallWatchdog, STALL_DETECTION_THRESHOLD_MS, STALL_WATCHDOG_INTERVAL_SECS};
use crate::metrics::MetricsClient;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Number of loop iterations between yield points (scheduler fairness).
pub(crate) const YIELD_EVERY_N_ITERATIONS: u64 = 50;

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
    pub(crate) resource_pools: Arc<ResourcePools>,

    /// Priority queue for pending tasks.
    pub(crate) task_queue: Arc<Mutex<PriorityQueue>>,

    /// Active jobs by ID.
    pub(crate) active_jobs: Arc<Mutex<HashMap<JobId, ActiveJob>>>,

    /// Receiver for submitted jobs.
    pub(crate) job_receiver: mpsc::Receiver<SubmittedJob>,

    /// Sender for task completions.
    pub(crate) completion_tx: mpsc::UnboundedSender<TaskCompletion>,

    /// Receiver for task completions.
    pub(crate) completion_rx: mpsc::UnboundedReceiver<TaskCompletion>,

    /// Telemetry sink for emitting events.
    pub(crate) telemetry: Arc<dyn TelemetrySink>,

    /// Notifier for when work might be available.
    pub(crate) work_notify: Arc<Notify>,

    /// Configuration.
    pub(crate) config: ExecutorConfig,

    /// Number of currently dispatched tasks.
    pub(crate) dispatched_count: usize,

    /// Metrics client for emitting metrics events.
    pub(crate) metrics_client: Option<MetricsClient>,

    /// Last activity timestamp (milliseconds since UNIX epoch).
    pub(crate) last_activity_ms: Arc<AtomicU64>,

    /// Count of pending work (active jobs + queued tasks).
    pub(crate) pending_work_count: Arc<AtomicU64>,

    /// Jobs admitted and jobs held in `waiting_for_permit`. Kept apart from
    /// [`Self::pending_work_count`] because the stall watchdog must not read
    /// a long admission pause as a stall, while a lane's gauges must count
    /// exactly those held jobs.
    pub(crate) job_counts: JobCounts,

    /// Loop iteration counter for periodic yield.
    pub(crate) loop_count: u64,

    /// Jobs waiting for concurrency group permits.
    ///
    /// Uses a priority-ordered heap so ON_DEMAND jobs are tried before PREFETCH,
    /// preventing priority inversion at the job level.
    pub(crate) waiting_for_permit: BinaryHeap<WaitingJob>,

    /// Monotonic sequence counter for FIFO ordering within same priority.
    pub(crate) waiting_sequence: u64,
}

impl JobExecutor {
    /// Creates a new job executor.
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
    pub fn with_telemetry_and_metrics(
        config: ExecutorConfig,
        telemetry: Arc<dyn TelemetrySink>,
        metrics_client: Option<MetricsClient>,
    ) -> (Self, JobSubmitter) {
        let (job_tx, job_rx) = mpsc::channel(config.job_channel_capacity);
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

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
            job_counts: JobCounts::default(),
            loop_count: 0,
            waiting_for_permit: BinaryHeap::new(),
            waiting_sequence: 0,
        };

        let submitter = JobSubmitter::new(job_tx);
        (executor, submitter)
    }

    /// Runs the executor until shutdown is signalled.
    pub async fn run(mut self, shutdown: CancellationToken) {
        self.spawn_watchdog(shutdown.clone());

        info!(
            "Executor started (stall_threshold={}s, check_interval={}s)",
            STALL_DETECTION_THRESHOLD_MS / 1000,
            STALL_WATCHDOG_INTERVAL_SECS
        );

        loop {
            self.update_activity_timestamp();

            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    self.shutdown().await;
                    break;
                }

                Some(submitted) = self.job_receiver.recv() => {
                    self.handle_job_submission(submitted).await;
                }

                Some(completion) = self.completion_rx.recv() => {
                    self.handle_task_completion(completion).await;
                }

                _ = self.work_notify.notified() => {
                    self.dispatch_tasks().await;
                }

                Ok(()) = gate_changed(&mut self.config.admission_gate) => {
                    // The loop tail runs complete_finished_jobs, which calls
                    // start_waiting_jobs: a resume is admitted there.
                }
            }

            self.process_signals().await;
            self.collect_child_jobs().await;
            self.dispatch_tasks().await;
            self.complete_finished_jobs().await;
            self.update_pending_work_count().await;
            self.maybe_yield().await;
        }
    }

    /// Spawns the stall detection watchdog.
    fn spawn_watchdog(&self, shutdown: CancellationToken) {
        let watchdog = StallWatchdog::new(
            Arc::clone(&self.last_activity_ms),
            Arc::clone(&self.pending_work_count),
        );
        tokio::spawn(watchdog.run(shutdown));
    }

    /// Updates the activity timestamp.
    fn update_activity_timestamp(&self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_activity_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Yields periodically for scheduler fairness.
    async fn maybe_yield(&mut self) {
        self.loop_count += 1;
        if self.loop_count.is_multiple_of(YIELD_EVERY_N_ITERATIONS) {
            tokio::task::yield_now().await;
        }
    }

    /// Updates the pending work count for stall detection.
    pub(crate) async fn update_pending_work_count(&self) {
        let active_jobs = self.active_jobs.lock().await.len();
        let queued_tasks = self.task_queue.lock().await.len();
        let total = (active_jobs + queued_tasks) as u64;
        self.pending_work_count.store(total, Ordering::Relaxed);
        self.job_counts
            .store(active_jobs, self.waiting_for_permit.len());
    }
}

/// How many jobs an executor is running and how many it is holding back,
/// refreshed every executor loop. Jobs, not tasks: an admitted job whose
/// next task waits for a pool permit is running, because a lane busy with
/// work it has admitted is productive, not saturated.
#[derive(Clone, Debug, Default)]
pub struct JobCounts {
    running: Arc<AtomicUsize>,
    waiting: Arc<AtomicUsize>,
}

impl JobCounts {
    /// Jobs admitted and not yet finished.
    pub fn running(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    /// Jobs held before admission, by a full concurrency group or a paused
    /// admission gate.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    pub(crate) fn store(&self, running: usize, waiting: usize) {
        self.running.store(running, Ordering::Relaxed);
        self.waiting.store(waiting, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for JobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobExecutor")
            .field("dispatched_count", &self.dispatched_count)
            .field("loop_count", &self.loop_count)
            .field("waiting_for_permit", &self.waiting_for_permit.len())
            .finish_non_exhaustive()
    }
}

/// Resolves when the admission gate changes; never resolves without a gate.
async fn gate_changed(
    gate: &mut Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    match gate {
        Some(rx) => rx.changed().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_executor_creation() {
        let config = ExecutorConfig::default();
        let (executor, _submitter) = JobExecutor::new(config);

        assert_eq!(executor.dispatched_count, 0);
        assert_eq!(executor.loop_count, 0);
        assert!(executor.waiting_for_permit.is_empty());
    }

    struct HeldJob;

    impl crate::executor::Job for HeldJob {
        fn id(&self) -> crate::executor::JobId {
            crate::executor::JobId::new("held")
        }
        fn name(&self) -> &str {
            "HeldJob"
        }
        fn error_policy(&self) -> crate::executor::ErrorPolicy {
            crate::executor::ErrorPolicy::FailFast
        }
        fn priority(&self) -> crate::executor::Priority {
            crate::executor::Priority::PREFETCH
        }
        fn create_tasks(&self) -> Vec<Box<dyn crate::executor::Task>> {
            Vec::new()
        }
        fn on_complete(&self, _result: &crate::executor::JobResult) -> crate::executor::JobStatus {
            crate::executor::JobStatus::Succeeded
        }
    }

    #[tokio::test]
    async fn job_counts_see_jobs_held_by_the_gate_but_the_watchdog_figure_does_not() {
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        let config = ExecutorConfig::default().with_admission_gate(gate_rx);
        let (executor, submitter) = JobExecutor::new(config);
        let counts = executor.job_counts.clone();
        let pending = Arc::clone(&executor.pending_work_count);
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(executor.run(shutdown.clone()));

        let _a = submitter.submit(HeldJob);
        let _b = submitter.submit(HeldJob);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(counts.waiting(), 2, "held jobs are waiting");
        assert_eq!(counts.running(), 0, "held jobs are not running");
        assert_eq!(
            pending.load(Ordering::Relaxed),
            0,
            "watchdog figure unchanged"
        );

        gate_tx.send_replace(false);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(counts.waiting(), 0);

        shutdown.cancel();
        let _ = run.await;
    }

    /// A job whose one task holds its permit until released.
    struct RunningJob(Arc<Notify>);

    struct RunningTask(Arc<Notify>);

    impl crate::executor::Task for RunningTask {
        fn name(&self) -> &str {
            "RunningTask"
        }
        fn resource_type(&self) -> crate::executor::ResourceType {
            crate::executor::ResourceType::CPU
        }
        fn execute<'a>(
            &'a self,
            _ctx: &'a mut crate::executor::TaskContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::executor::TaskResult> + Send + 'a>,
        > {
            Box::pin(async move {
                self.0.notified().await;
                crate::executor::TaskResult::Success
            })
        }
    }

    impl crate::executor::Job for RunningJob {
        fn id(&self) -> crate::executor::JobId {
            crate::executor::JobId::new("running")
        }
        fn name(&self) -> &str {
            "RunningJob"
        }
        fn error_policy(&self) -> crate::executor::ErrorPolicy {
            crate::executor::ErrorPolicy::FailFast
        }
        fn priority(&self) -> crate::executor::Priority {
            crate::executor::Priority::PREFETCH
        }
        fn create_tasks(&self) -> Vec<Box<dyn crate::executor::Task>> {
            vec![Box::new(RunningTask(Arc::clone(&self.0)))]
        }
        fn on_complete(&self, _result: &crate::executor::JobResult) -> crate::executor::JobStatus {
            crate::executor::JobStatus::Succeeded
        }
    }

    #[tokio::test]
    async fn admitted_jobs_count_as_running_until_they_finish() {
        let (executor, submitter) = JobExecutor::new(ExecutorConfig::default());
        let counts = executor.job_counts.clone();
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(executor.run(shutdown.clone()));

        let release = Arc::new(Notify::new());
        let _job = submitter.submit(RunningJob(Arc::clone(&release)));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!((counts.running(), counts.waiting()), (1, 0));

        release.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(counts.running(), 0);

        shutdown.cancel();
        let _ = run.await;
    }

    #[tokio::test]
    async fn test_executor_with_telemetry() {
        let config = ExecutorConfig::default();
        let telemetry = Arc::new(NullTelemetrySink);
        let (executor, _submitter) = JobExecutor::with_telemetry(config, telemetry);

        assert_eq!(executor.dispatched_count, 0);
    }
}
