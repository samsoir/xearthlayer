//! Job submission interface.
//!
//! This module contains [`JobSubmitter`] - the public interface for submitting
//! jobs to the executor.

use super::config::DEFAULT_SIGNAL_CHANNEL_CAPACITY;
use super::handle::{JobHandle, JobStatus, Signal};
use super::job::{Job, JobId, JobResult};
use super::policy::Priority;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};

// =============================================================================
// Submission Errors
// =============================================================================

/// Why a job could not be handed to the executor.
///
/// These were collapsed into a single `None` until #272, which meant the
/// daemon logged "executor may be shutdown" for ordinary back pressure. On a
/// slow system that was the loudest line in the log and pointed at exactly the
/// wrong conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// The executor's queue is full. Expected under load: the executor is
    /// healthy and working, and the caller's work is being shed.
    QueueFull,
    /// The executor is gone and will accept nothing further.
    ExecutorGone,
}

impl SubmitError {
    /// Whether this is back pressure rather than a failure.
    ///
    /// Callers use it to decide how loudly to report a rejected submission:
    /// shed work is a debug-level fact about load, a gone executor is a
    /// warning about the process.
    pub fn is_back_pressure(self) -> bool {
        matches!(self, Self::QueueFull)
    }
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "executor queue is full"),
            Self::ExecutorGone => write!(f, "executor is no longer running"),
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
    pub(crate) sender: mpsc::Sender<SubmittedJob>,
}

impl JobSubmitter {
    /// Creates a new job submitter with the given channel sender.
    pub(crate) fn new(sender: mpsc::Sender<SubmittedJob>) -> Self {
        Self { sender }
    }

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
    /// Distinguishes a full queue from a gone executor; see [`SubmitError`].
    pub fn try_submit(&self, job: impl Job + 'static) -> Result<JobHandle, SubmitError> {
        self.try_submit_boxed(Box::new(job))
    }

    /// Attempts to submit a boxed job for execution.
    ///
    /// This is useful when working with factory patterns that return `Box<dyn Job>`.
    /// Distinguishes a full queue from a gone executor; see [`SubmitError`].
    pub fn try_submit_boxed(&self, job: Box<dyn Job>) -> Result<JobHandle, SubmitError> {
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

        match self.sender.try_send(submitted) {
            Ok(()) => Ok(handle),
            Err(TrySendError::Full(_)) => Err(SubmitError::QueueFull),
            Err(TrySendError::Closed(_)) => Err(SubmitError::ExecutorGone),
        }
    }
}

// =============================================================================
// Submitted Job
// =============================================================================

/// A job that has been submitted to the executor.
///
/// This is the internal representation passed through the channel from
/// `JobSubmitter` to the executor.
pub(crate) struct SubmittedJob {
    /// The job to execute.
    pub job: Box<dyn Job>,

    /// Unique job identifier.
    pub job_id: JobId,

    /// Human-readable job name.
    pub name: String,

    /// Job priority.
    pub priority: Priority,

    /// Channel to send status updates.
    pub status_tx: watch::Sender<JobStatus>,

    /// Channel to receive signals.
    pub signal_rx: mpsc::Receiver<Signal>,

    /// Shared result holder for returning job output.
    pub result_holder: std::sync::Arc<tokio::sync::Mutex<Option<JobResult>>>,
}

// =============================================================================
// Waiting Job (priority-ordered queue wrapper)
// =============================================================================

/// A job waiting for a concurrency group permit.
///
/// Wraps a `SubmittedJob` with ordering support for the priority-aware
/// waiting queue. Jobs are ordered by priority (higher first), then by
/// sequence number (FIFO within same priority).
///
/// This replaces the previous `VecDeque<SubmittedJob>` which was FIFO-only,
/// causing ON_DEMAND jobs to queue behind PREFETCH jobs (priority inversion).
pub(crate) struct WaitingJob {
    /// The submitted job awaiting a concurrency permit.
    pub submitted: SubmittedJob,
    /// Sequence number for FIFO ordering within the same priority level.
    pub sequence: u64,
}

impl PartialEq for WaitingJob {
    fn eq(&self, other: &Self) -> bool {
        self.submitted.priority == other.submitted.priority && self.sequence == other.sequence
    }
}

impl Eq for WaitingJob {}

impl PartialOrd for WaitingJob {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WaitingJob {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher priority first, then lower sequence (older) first
        match self.submitted.priority.cmp(&other.submitted.priority) {
            std::cmp::Ordering::Equal => other.sequence.cmp(&self.sequence),
            other_ordering => other_ordering,
        }
    }
}

impl std::fmt::Debug for WaitingJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitingJob")
            .field("job_id", &self.submitted.job_id)
            .field("priority", &self.submitted.priority)
            .field("sequence", &self.sequence)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_waiting_queue_prioritizes_on_demand_over_prefetch() {
        use std::collections::BinaryHeap;

        // Create mock submitted jobs at different priorities
        let prefetch = create_test_submitted_job("prefetch-1", Priority::PREFETCH);
        let on_demand = create_test_submitted_job("on-demand-1", Priority::ON_DEMAND);

        let mut heap = BinaryHeap::new();

        // Submit prefetch first, then on_demand
        heap.push(WaitingJob {
            submitted: prefetch,
            sequence: 0,
        });
        heap.push(WaitingJob {
            submitted: on_demand,
            sequence: 1,
        });

        // ON_DEMAND should come out first despite being submitted second
        let first = heap.pop().unwrap();
        assert_eq!(first.submitted.priority, Priority::ON_DEMAND);

        let second = heap.pop().unwrap();
        assert_eq!(second.submitted.priority, Priority::PREFETCH);
    }

    #[test]
    fn test_waiting_queue_fifo_within_same_priority() {
        use std::collections::BinaryHeap;

        let job1 = create_test_submitted_job("prefetch-1", Priority::PREFETCH);
        let job2 = create_test_submitted_job("prefetch-2", Priority::PREFETCH);

        let mut heap = BinaryHeap::new();
        heap.push(WaitingJob {
            submitted: job1,
            sequence: 0,
        });
        heap.push(WaitingJob {
            submitted: job2,
            sequence: 1,
        });

        // Same priority — FIFO (lower sequence first)
        let first = heap.pop().unwrap();
        assert_eq!(first.submitted.name, "prefetch-1");
        assert_eq!(first.sequence, 0);

        let second = heap.pop().unwrap();
        assert_eq!(second.submitted.name, "prefetch-2");
        assert_eq!(second.sequence, 1);
    }

    /// Helper to create a test SubmittedJob.
    fn create_test_submitted_job(name: &str, priority: Priority) -> SubmittedJob {
        struct TestJob {
            name: String,
        }
        impl Job for TestJob {
            fn id(&self) -> JobId {
                JobId::new(&self.name)
            }
            fn name(&self) -> &str {
                &self.name
            }
            fn create_tasks(&self) -> Vec<Box<dyn super::super::task::Task>> {
                vec![]
            }
            fn priority(&self) -> Priority {
                // Not used directly — priority is on SubmittedJob
                Priority::PREFETCH
            }
        }

        let (status_tx, _status_rx) = watch::channel(JobStatus::Pending);
        let (_signal_tx, signal_rx) = mpsc::channel(16);
        let result_holder = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        SubmittedJob {
            job: Box::new(TestJob {
                name: name.to_string(),
            }),
            job_id: JobId::new(name),
            name: name.to_string(),
            priority,
            status_tx,
            signal_rx,
            result_holder,
        }
    }

    struct TestJob;
    impl Job for TestJob {
        fn id(&self) -> JobId {
            JobId::new("test")
        }
        fn name(&self) -> &str {
            "Test"
        }
        fn create_tasks(&self) -> Vec<Box<dyn super::super::task::Task>> {
            vec![]
        }
    }

    /// #272: a full queue and a gone executor were both reported as `None`, so
    /// the daemon logged "executor may be shutdown" for ordinary back pressure.
    /// The two must be distinguishable at the submission site.
    #[tokio::test]
    async fn try_submit_reports_a_closed_channel_as_executor_gone() {
        let (tx, rx) = mpsc::channel(1);
        let submitter = JobSubmitter::new(tx);

        drop(rx);

        assert_eq!(
            submitter.try_submit(TestJob).err(),
            Some(SubmitError::ExecutorGone)
        );
    }

    #[tokio::test]
    async fn try_submit_reports_a_full_queue_as_queue_full() {
        // Capacity 1, receiver alive but never reading: the second submission
        // is back pressure, not failure.
        let (tx, _rx) = mpsc::channel(1);
        let submitter = JobSubmitter::new(tx);

        assert!(
            submitter.try_submit(TestJob).is_ok(),
            "first fills the queue"
        );

        assert_eq!(
            submitter.try_submit(TestJob).err(),
            Some(SubmitError::QueueFull)
        );
    }

    #[test]
    fn queue_full_is_back_pressure_and_executor_gone_is_not() {
        // The predicate the daemon uses to pick a log level, so the two cases
        // cannot silently collapse back together.
        assert!(SubmitError::QueueFull.is_back_pressure());
        assert!(!SubmitError::ExecutorGone.is_back_pressure());
    }
}
