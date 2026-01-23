//! Job handle for status queries and signalling.
//!
//! The [`JobHandle`] is returned when a job is submitted to the executor.
//! It provides methods to query the job's status, wait for completion, and
//! send control signals (pause, resume, stop, kill).
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{JobExecutor, JobHandle, JobStatus};
//!
//! let handle = executor.submit(my_job);
//!
//! // Check status without waiting
//! if handle.status() == JobStatus::Running {
//!     println!("Job is running");
//! }
//!
//! // Wait for completion
//! let result = handle.wait().await;
//!
//! // Or cancel the job
//! handle.kill();
//! ```

use super::job::{JobId, JobResult};
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};

/// Handle to a submitted job for status queries and signalling.
///
/// This handle is cloneable and can be shared across tasks. All clones
/// refer to the same underlying job.
#[derive(Clone)]
pub struct JobHandle {
    job_id: JobId,
    status_rx: watch::Receiver<JobStatus>,
    signal_tx: mpsc::Sender<Signal>,
    /// Result receiver - set by executor when job completes.
    result: Arc<Mutex<Option<JobResult>>>,
}

impl JobHandle {
    /// Creates a new job handle.
    ///
    /// This is typically called by the executor when a job is submitted.
    pub(crate) fn new(
        job_id: JobId,
        status_rx: watch::Receiver<JobStatus>,
        signal_tx: mpsc::Sender<Signal>,
    ) -> Self {
        Self {
            job_id,
            status_rx,
            signal_tx,
            result: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns a clone of the result holder for the executor to set.
    pub(crate) fn result_holder(&self) -> Arc<Mutex<Option<JobResult>>> {
        Arc::clone(&self.result)
    }

    /// Returns the job's unique identifier.
    pub fn id(&self) -> &JobId {
        &self.job_id
    }

    /// Returns the current job status.
    ///
    /// This is a non-blocking operation that returns the most recent status.
    pub fn status(&self) -> JobStatus {
        *self.status_rx.borrow()
    }

    /// Waits for the job to complete and returns the result.
    ///
    /// This method will block until the job reaches a terminal state
    /// (Succeeded, Failed, Cancelled, or Stopped).
    pub async fn wait(&mut self) -> JobResult {
        loop {
            if self.status().is_terminal() {
                break;
            }
            // Wait for status change
            if self.status_rx.changed().await.is_err() {
                // Channel closed - job is done
                break;
            }
        }
        // Take the result from the holder (set by executor on completion)
        self.result
            .lock()
            .await
            .take()
            .unwrap_or_else(JobResult::new)
    }

    /// Pauses the job: no new tasks start, in-flight tasks continue.
    ///
    /// This is a non-blocking operation. The job status will change to
    /// `Paused` once the signal is processed.
    pub fn pause(&self) {
        let _ = self.signal_tx.try_send(Signal::Pause);
    }

    /// Resumes the job from a paused state.
    ///
    /// This is a non-blocking operation. The job status will change to
    /// `Running` once the signal is processed.
    pub fn resume(&self) {
        let _ = self.signal_tx.try_send(Signal::Resume);
    }

    /// Gracefully stops the job: finish current tasks, don't start new ones.
    ///
    /// This is a non-blocking operation. The job status will change to
    /// `Stopped` once all in-flight tasks complete.
    pub fn stop(&self) {
        let _ = self.signal_tx.try_send(Signal::Stop);
    }

    /// Immediately cancels the job and all in-flight tasks.
    ///
    /// This is a non-blocking operation. The job status will change to
    /// `Cancelled` once the signal is processed.
    pub fn kill(&self) {
        let _ = self.signal_tx.try_send(Signal::Kill);
    }
}

impl std::fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("job_id", &self.job_id)
            .field("status", &self.status())
            .finish()
    }
}

/// Job execution status.
///
/// This enum represents all possible states of a job during its lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JobStatus {
    /// Waiting for dependencies to complete.
    #[default]
    Pending,

    /// Currently executing tasks.
    Running,

    /// Paused (no new tasks dispatched, in-flight tasks continue).
    Paused,

    /// Completed successfully (all tasks succeeded).
    Succeeded,

    /// Completed with failures (at least one task failed).
    Failed,

    /// Cancelled before completion.
    Cancelled,

    /// Stopped gracefully (finished current work, didn't start new tasks).
    Stopped,
}

impl JobStatus {
    /// Returns true if this is a terminal state (job is complete).
    ///
    /// Terminal states are: Succeeded, Failed, Cancelled, Stopped.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Stopped
        )
    }

    /// Returns true if the job is currently paused.
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused)
    }

    /// Returns true if the job is still running (not terminal).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Paused)
    }

    /// Returns true if the job completed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Running => write!(f, "Running"),
            Self::Paused => write!(f, "Paused"),
            Self::Succeeded => write!(f, "Succeeded"),
            Self::Failed => write!(f, "Failed"),
            Self::Cancelled => write!(f, "Cancelled"),
            Self::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Signal that can be sent to a running job.
///
/// Signals provide control over job execution, allowing pause, resume,
/// stop, and cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Pause execution: no new tasks start, in-flight tasks continue.
    ///
    /// The job status changes to `Paused`.
    Pause,

    /// Resume from paused state.
    ///
    /// The job status changes back to `Running`.
    Resume,

    /// Graceful stop: finish current tasks, don't start new ones.
    ///
    /// The job status changes to `Stopped` when all in-flight tasks complete.
    Stop,

    /// Immediate cancellation: cancel all in-flight tasks.
    ///
    /// Tasks receive cancellation via their `CancellationToken`.
    /// The job status changes to `Cancelled`.
    Kill,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pause => write!(f, "Pause"),
            Self::Resume => write!(f, "Resume"),
            Self::Stop => write!(f, "Stop"),
            Self::Kill => write!(f, "Kill"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_status_is_terminal() {
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(!JobStatus::Paused.is_terminal());
        assert!(JobStatus::Succeeded.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Stopped.is_terminal());
    }

    #[test]
    fn test_job_status_is_paused() {
        assert!(JobStatus::Paused.is_paused());
        assert!(!JobStatus::Running.is_paused());
        assert!(!JobStatus::Pending.is_paused());
    }

    #[test]
    fn test_job_status_is_active() {
        assert!(JobStatus::Pending.is_active());
        assert!(JobStatus::Running.is_active());
        assert!(JobStatus::Paused.is_active());
        assert!(!JobStatus::Succeeded.is_active());
        assert!(!JobStatus::Failed.is_active());
        assert!(!JobStatus::Cancelled.is_active());
    }

    #[test]
    fn test_job_status_is_success() {
        assert!(JobStatus::Succeeded.is_success());
        assert!(!JobStatus::Failed.is_success());
        assert!(!JobStatus::Running.is_success());
    }

    #[test]
    fn test_job_status_default() {
        assert_eq!(JobStatus::default(), JobStatus::Pending);
    }

    #[test]
    fn test_job_status_display() {
        assert_eq!(format!("{}", JobStatus::Running), "Running");
        assert_eq!(format!("{}", JobStatus::Paused), "Paused");
        assert_eq!(format!("{}", JobStatus::Succeeded), "Succeeded");
    }

    #[test]
    fn test_signal_display() {
        assert_eq!(format!("{}", Signal::Pause), "Pause");
        assert_eq!(format!("{}", Signal::Resume), "Resume");
        assert_eq!(format!("{}", Signal::Stop), "Stop");
        assert_eq!(format!("{}", Signal::Kill), "Kill");
    }

    #[tokio::test]
    async fn test_job_handle_status() {
        let (status_tx, status_rx) = watch::channel(JobStatus::Running);
        let (signal_tx, _signal_rx) = mpsc::channel(16);
        let handle = JobHandle::new(JobId::new("test"), status_rx, signal_tx);

        assert_eq!(handle.status(), JobStatus::Running);

        status_tx.send(JobStatus::Succeeded).unwrap();
        assert_eq!(handle.status(), JobStatus::Succeeded);
    }

    #[tokio::test]
    async fn test_job_handle_signalling() {
        let (_status_tx, status_rx) = watch::channel(JobStatus::Running);
        let (signal_tx, mut signal_rx) = mpsc::channel(16);
        let handle = JobHandle::new(JobId::new("test"), status_rx, signal_tx);

        handle.pause();
        assert_eq!(signal_rx.recv().await, Some(Signal::Pause));

        handle.resume();
        assert_eq!(signal_rx.recv().await, Some(Signal::Resume));

        handle.stop();
        assert_eq!(signal_rx.recv().await, Some(Signal::Stop));

        handle.kill();
        assert_eq!(signal_rx.recv().await, Some(Signal::Kill));
    }

    #[test]
    fn test_job_handle_clone() {
        let (_status_tx, status_rx) = watch::channel(JobStatus::Running);
        let (signal_tx, _signal_rx) = mpsc::channel(16);
        let handle1 = JobHandle::new(JobId::new("test"), status_rx, signal_tx);
        let handle2 = handle1.clone();

        assert_eq!(handle1.id(), handle2.id());
        assert_eq!(handle1.status(), handle2.status());
    }
}
