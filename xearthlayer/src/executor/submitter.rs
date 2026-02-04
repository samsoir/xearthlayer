//! Job submission interface.
//!
//! This module contains [`JobSubmitter`] - the public interface for submitting
//! jobs to the executor.

use super::config::DEFAULT_SIGNAL_CHANNEL_CAPACITY;
use super::handle::{JobHandle, JobStatus, Signal};
use super::job::{Job, JobId, JobResult};
use super::policy::Priority;
use tokio::sync::{mpsc, watch};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_submitter_try_submit_closed_channel() {
        let (tx, rx) = mpsc::channel(1);
        let submitter = JobSubmitter::new(tx);

        // Drop the receiver to close the channel
        drop(rx);

        // Create a simple test job
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

        // Should return None when channel is closed
        let result = submitter.try_submit(TestJob);
        assert!(result.is_none());
    }
}
