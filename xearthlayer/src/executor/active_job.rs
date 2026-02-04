//! Active job state management.
//!
//! This module contains [`ActiveJob`] - the internal state for a job that
//! is currently being executed by the executor.

use super::context::{SharedTaskOutputs, SpawnedChildJob};
use super::handle::{JobStatus, Signal};
use super::job::{Job, JobId, JobResult};
use super::policy::{ErrorPolicy, Priority};
use super::submitter::SubmittedJob;
use super::task::{Task, TaskResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit};
use tokio_util::sync::CancellationToken;

// =============================================================================
// Active Job State
// =============================================================================

/// State of an actively executing job.
///
/// This tracks all runtime state for a job being processed by the executor,
/// including task progress, child jobs, and error handling state.
pub(crate) struct ActiveJob {
    /// The job being executed.
    pub job: Box<dyn Job>,

    /// Job priority (stored for potential priority boosting).
    #[allow(dead_code)]
    pub priority: Priority,

    /// Channel to update job status.
    pub status_tx: watch::Sender<JobStatus>,

    /// Channel to receive signals.
    pub signal_rx: mpsc::Receiver<Signal>,

    /// Current job status.
    pub status: JobStatus,

    /// When the job started executing.
    pub started_at: Instant,

    /// Cancellation token for this job's tasks.
    pub cancellation: CancellationToken,

    /// Channel for child jobs spawned by tasks.
    pub child_job_tx: mpsc::UnboundedSender<SpawnedChildJob>,

    /// Receiver for child jobs (polled by executor).
    pub child_job_rx: mpsc::UnboundedReceiver<SpawnedChildJob>,

    /// Tasks waiting to be enqueued (from create_tasks).
    pub pending_tasks: Vec<Box<dyn Task>>,

    /// Number of tasks currently executing.
    pub tasks_in_flight: usize,

    /// Number of tasks queued but not yet dispatched.
    /// This prevents the race condition where job completion check sees
    /// both pending_tasks empty and tasks_in_flight == 0 before a
    /// sequential task is picked up from the queue.
    pub queued_tasks: usize,

    /// Names of tasks that succeeded.
    pub succeeded_tasks: Vec<String>,

    /// Names of tasks that failed.
    pub failed_tasks: Vec<String>,

    /// Names of tasks that were cancelled.
    pub cancelled_tasks: Vec<String>,

    /// Outputs from completed tasks (shared with task contexts).
    pub task_outputs: SharedTaskOutputs,

    /// Child jobs that have been spawned.
    pub child_job_ids: Vec<JobId>,

    /// Child jobs that succeeded.
    pub succeeded_children: Vec<JobId>,

    /// Child jobs that failed.
    pub failed_children: Vec<JobId>,

    /// Parent job ID if this is a child job.
    pub parent_job_id: Option<JobId>,

    /// Result holder shared with the JobHandle for returning job output.
    pub result_holder: std::sync::Arc<tokio::sync::Mutex<Option<JobResult>>>,

    /// Concurrency group permit (RAII - auto-releases when job completes).
    /// Jobs with a concurrency group hold this permit for their entire duration,
    /// limiting how many jobs in that group can run simultaneously.
    #[allow(dead_code)]
    pub concurrency_permit: Option<OwnedSemaphorePermit>,
}

impl ActiveJob {
    /// Creates a new active job from a submitted job.
    pub fn new(submitted: SubmittedJob) -> Self {
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
            parent_job_id: None,
            result_holder,
            concurrency_permit: None,
        }
    }

    /// Creates a new active job with a parent relationship.
    pub fn with_parent(submitted: SubmittedJob, parent_id: JobId) -> Self {
        let mut job = Self::new(submitted);
        job.parent_job_id = Some(parent_id);
        job
    }

    /// Updates the job status and notifies waiters.
    pub fn update_status(&mut self, status: JobStatus) {
        self.status = status;
        let _ = self.status_tx.send(status);
    }

    /// Returns true if the job is currently paused.
    pub fn is_paused(&self) -> bool {
        matches!(self.status, JobStatus::Paused)
    }

    /// Returns true if the job should stop processing new tasks.
    pub fn should_stop(&self) -> bool {
        matches!(
            self.status,
            JobStatus::Stopped | JobStatus::Cancelled | JobStatus::Failed
        )
    }

    /// Returns true if the job has pending work (tasks or children).
    pub fn has_pending_work(&self) -> bool {
        !self.pending_tasks.is_empty()
            || self.tasks_in_flight > 0
            || self.queued_tasks > 0
            || !self.child_job_ids.is_empty()
    }

    /// Returns true if all child jobs have completed.
    pub fn all_children_complete(&self) -> bool {
        self.child_job_ids.is_empty()
    }

    /// Builds the job result from current state.
    pub fn build_result(&self) -> JobResult {
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

    /// Checks if the error policy requires early termination.
    ///
    /// Returns `Some(JobStatus::Failed)` if the job should fail early,
    /// `None` if execution should continue.
    pub fn check_error_policy(&self) -> Option<JobStatus> {
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

    /// Computes the final job status based on results and error policy.
    pub fn compute_final_status(&self) -> JobStatus {
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
///
/// This is sent from the task execution future back to the executor
/// when a task finishes (success, failure, or cancellation).
pub(crate) struct TaskCompletion {
    /// ID of the job this task belongs to.
    pub job_id: JobId,

    /// Name of the completed task.
    pub task_name: String,

    /// The task's result.
    pub result: TaskResult,

    /// How long the task took to execute.
    pub duration: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a mock SubmittedJob for testing
    fn create_test_submitted_job() -> SubmittedJob {
        struct TestJob;
        impl Job for TestJob {
            fn id(&self) -> JobId {
                JobId::new("test-job")
            }
            fn name(&self) -> &str {
                "TestJob"
            }
            fn create_tasks(&self) -> Vec<Box<dyn Task>> {
                vec![]
            }
        }

        let (status_tx, _status_rx) = watch::channel(JobStatus::Pending);
        let (_signal_tx, signal_rx) = mpsc::channel(16);
        let result_holder = Arc::new(tokio::sync::Mutex::new(None));

        SubmittedJob {
            job: Box::new(TestJob),
            job_id: JobId::new("test-job"),
            name: "TestJob".to_string(),
            priority: Priority::PREFETCH,
            status_tx,
            signal_rx,
            result_holder,
        }
    }

    #[test]
    fn test_active_job_new() {
        let submitted = create_test_submitted_job();
        let active = ActiveJob::new(submitted);

        assert_eq!(active.status, JobStatus::Pending);
        assert!(active.pending_tasks.is_empty());
        assert_eq!(active.tasks_in_flight, 0);
        assert!(active.parent_job_id.is_none());
    }

    #[test]
    fn test_active_job_with_parent() {
        let submitted = create_test_submitted_job();
        let parent_id = JobId::new("parent-job");
        let active = ActiveJob::with_parent(submitted, parent_id.clone());

        assert_eq!(active.parent_job_id, Some(parent_id));
    }

    #[test]
    fn test_active_job_status_updates() {
        let submitted = create_test_submitted_job();
        let mut active = ActiveJob::new(submitted);

        assert!(!active.is_paused());
        assert!(!active.should_stop());

        active.update_status(JobStatus::Paused);
        assert!(active.is_paused());
        assert!(!active.should_stop());

        active.update_status(JobStatus::Stopped);
        assert!(!active.is_paused());
        assert!(active.should_stop());
    }

    #[test]
    fn test_active_job_has_pending_work() {
        let submitted = create_test_submitted_job();
        let mut active = ActiveJob::new(submitted);

        // No work initially
        assert!(!active.has_pending_work());

        // Tasks in flight
        active.tasks_in_flight = 1;
        assert!(active.has_pending_work());
        active.tasks_in_flight = 0;

        // Queued tasks
        active.queued_tasks = 1;
        assert!(active.has_pending_work());
        active.queued_tasks = 0;

        // Child jobs
        active.child_job_ids.push(JobId::new("child"));
        assert!(active.has_pending_work());
    }

    #[test]
    fn test_active_job_build_result() {
        let submitted = create_test_submitted_job();
        let mut active = ActiveJob::new(submitted);

        active.succeeded_tasks.push("task1".to_string());
        active.failed_tasks.push("task2".to_string());

        let result = active.build_result();
        assert_eq!(result.succeeded_tasks, vec!["task1"]);
        assert_eq!(result.failed_tasks, vec!["task2"]);
    }
}
