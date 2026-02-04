//! Job trait and related types.
//!
//! A job is a named unit of work composed of tasks, with error handling policies
//! and completion criteria. Jobs can depend on other jobs and can spawn child jobs.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{Job, JobId, ErrorPolicy, Priority};
//!
//! struct MyJob {
//!     id: JobId,
//!     name: String,
//! }
//!
//! impl Job for MyJob {
//!     fn id(&self) -> JobId { self.id.clone() }
//!     fn name(&self) -> &str { &self.name }
//!     fn error_policy(&self) -> ErrorPolicy { ErrorPolicy::FailFast }
//!     fn priority(&self) -> Priority { Priority::PREFETCH }
//!     fn create_tasks(&self) -> Vec<Box<dyn Task>> { vec![] }
//! }
//! ```

use super::handle::JobStatus;
use super::policy::{ErrorPolicy, Priority};
use super::task::Task;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for generating unique job IDs.
static JOB_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique identifier for a job.
///
/// Job IDs are strings that uniquely identify a job instance. They can be
/// generated automatically or constructed from meaningful data (like file paths).
///
/// # Example
///
/// ```ignore
/// use xearthlayer::executor::JobId;
///
/// // Auto-generated unique ID
/// let id = JobId::auto();
///
/// // ID from meaningful data
/// let id = JobId::new("dds-60_-146_14");
/// ```
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct JobId(String);

impl JobId {
    /// Creates a new job ID with the given string value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Creates a unique auto-generated job ID.
    ///
    /// The ID format is `job-{counter}` where counter is a monotonically
    /// increasing number. This is suitable for jobs that don't need
    /// meaningful IDs.
    pub fn auto() -> Self {
        let counter = JOB_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("job-{}", counter))
    }

    /// Returns the string value of this job ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JobId({})", self.0)
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for JobId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for JobId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// A unit of work composed of tasks with error handling and completion criteria.
///
/// Jobs are the primary unit of work scheduling. Each job:
/// - Has a unique identifier and human-readable name
/// - Contains one or more tasks that are executed sequentially or in parallel
/// - Can declare dependencies on other jobs
/// - Has an error policy that determines how failures are handled
/// - Has a priority that affects scheduling order
///
/// # Lifecycle
///
/// 1. Job is submitted to the executor via `JobExecutor::submit()`
/// 2. Executor waits for dependencies (if any)
/// 3. Executor calls `create_tasks()` to get the task list
/// 4. Tasks are executed according to their resource requirements
/// 5. When all tasks complete, `on_complete()` determines final status
///
/// # Child Jobs
///
/// Tasks can spawn child jobs via `TaskContext::spawn_child_job()`. The parent
/// job will not complete until all child jobs complete. This enables hierarchical
/// work structures like "prefetch tile → generate DDS files".
pub trait Job: Send + Sync + 'static {
    /// Returns the unique identifier for this job instance.
    fn id(&self) -> JobId;

    /// Returns a human-readable name for logging/display.
    ///
    /// This should be a short, descriptive name like "TilePrefetch" or "DdsGenerate".
    fn name(&self) -> &str;

    /// Returns the error handling policy for this job.
    ///
    /// The default is `ErrorPolicy::FailFast`, which stops the job immediately
    /// on any task failure.
    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    /// Returns IDs of jobs that must complete before this job can start.
    ///
    /// The executor will wait for all dependencies to complete successfully
    /// before starting this job. If any dependency fails, this job will be
    /// cancelled.
    ///
    /// The default is no dependencies.
    fn dependencies(&self) -> Vec<JobId> {
        vec![]
    }

    /// Returns the priority for this job's tasks.
    ///
    /// Higher priority jobs have their tasks scheduled before lower priority jobs.
    /// The default is `Priority::PREFETCH`.
    fn priority(&self) -> Priority {
        Priority::PREFETCH
    }

    /// Returns the concurrency group for this job, if any.
    ///
    /// Jobs in the same concurrency group share a limited number of execution
    /// slots. This creates back-pressure to prevent resource exhaustion when
    /// many jobs of the same type are submitted.
    ///
    /// For example, `DdsGenerateJob` uses group `"tile_generation"` with a
    /// default limit of 40 concurrent jobs, ensuring that tiles flow through
    /// the download→build pipeline smoothly instead of all downloads running
    /// before any builds.
    ///
    /// The default is `None`, meaning no concurrency limit beyond the task
    /// resource pools.
    fn concurrency_group(&self) -> Option<&str> {
        None
    }

    /// Creates the tasks for this job.
    ///
    /// This is called when the job is ready to execute (dependencies satisfied).
    /// Tasks are returned as trait objects and will be executed in order,
    /// respecting their resource requirements.
    ///
    /// # Returns
    ///
    /// A vector of tasks to execute. Tasks may be executed concurrently if they
    /// use different resource pools.
    fn create_tasks(&self) -> Vec<Box<dyn Task>>;

    /// Called when all tasks and child jobs complete.
    ///
    /// This method allows the job to inspect results and determine the final
    /// status. The default implementation returns `Succeeded` if there are no
    /// failures, `Failed` otherwise.
    ///
    /// # Arguments
    ///
    /// * `result` - The execution results including task outcomes and child job statuses
    fn on_complete(&self, result: &JobResult) -> JobStatus {
        let no_failed_tasks = result.failed_tasks.is_empty();
        let no_failed_children = result.failed_children.is_empty();

        if no_failed_tasks && no_failed_children {
            JobStatus::Succeeded
        } else {
            JobStatus::Failed
        }
    }
}

/// Results of job execution.
///
/// This struct contains the outcomes of all tasks and child jobs, allowing
/// the job's `on_complete` method to determine the final status.
#[derive(Debug, Default, Clone)]
pub struct JobResult {
    /// Names of tasks that completed successfully.
    pub succeeded_tasks: Vec<String>,

    /// Names of tasks that failed.
    pub failed_tasks: Vec<String>,

    /// Names of tasks that were cancelled.
    pub cancelled_tasks: Vec<String>,

    /// IDs of child jobs that completed successfully.
    pub succeeded_children: Vec<JobId>,

    /// IDs of child jobs that failed.
    pub failed_children: Vec<JobId>,

    /// IDs of child jobs that were cancelled.
    pub cancelled_children: Vec<JobId>,

    /// Total execution duration.
    pub duration: std::time::Duration,

    /// Output data from the job (e.g., generated DDS tile data).
    ///
    /// This allows jobs to return data directly to the caller without
    /// relying on cache reads, avoiding race conditions with eventual
    /// consistency caches like moka.
    pub output_data: Option<Vec<u8>>,
}

impl JobResult {
    /// Creates a new empty job result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the total number of tasks.
    pub fn total_tasks(&self) -> usize {
        self.succeeded_tasks.len() + self.failed_tasks.len() + self.cancelled_tasks.len()
    }

    /// Returns the success ratio for tasks (0.0 - 1.0).
    pub fn task_success_ratio(&self) -> f64 {
        let total = self.total_tasks();
        if total == 0 {
            1.0 // No tasks = 100% success
        } else {
            self.succeeded_tasks.len() as f64 / total as f64
        }
    }

    /// Returns the total number of child jobs.
    pub fn total_children(&self) -> usize {
        self.succeeded_children.len() + self.failed_children.len() + self.cancelled_children.len()
    }

    /// Returns the success ratio for child jobs (0.0 - 1.0).
    pub fn child_success_ratio(&self) -> f64 {
        let total = self.total_children();
        if total == 0 {
            1.0 // No children = 100% success
        } else {
            self.succeeded_children.len() as f64 / total as f64
        }
    }

    /// Checks if the result meets a partial success threshold.
    ///
    /// This considers both tasks and children when calculating success ratio.
    pub fn meets_threshold(&self, threshold: f64) -> bool {
        let total = self.total_tasks() + self.total_children();
        if total == 0 {
            return true; // No work = success
        }

        let succeeded = self.succeeded_tasks.len() + self.succeeded_children.len();
        let ratio = succeeded as f64 / total as f64;
        ratio >= threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_id_new() {
        let id = JobId::new("test-job");
        assert_eq!(id.as_str(), "test-job");
    }

    #[test]
    fn test_job_id_auto() {
        let id1 = JobId::auto();
        let id2 = JobId::auto();
        assert_ne!(id1, id2);
        assert!(id1.as_str().starts_with("job-"));
    }

    #[test]
    fn test_job_id_equality() {
        let id1 = JobId::new("test");
        let id2 = JobId::new("test");
        let id3 = JobId::new("other");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_job_id_display() {
        let id = JobId::new("my-job-123");
        assert_eq!(format!("{}", id), "my-job-123");
    }

    #[test]
    fn test_job_id_from_string() {
        let id: JobId = String::from("from-string").into();
        assert_eq!(id.as_str(), "from-string");
    }

    #[test]
    fn test_job_id_from_str() {
        let id: JobId = "from-str".into();
        assert_eq!(id.as_str(), "from-str");
    }

    #[test]
    fn test_job_result_empty() {
        let result = JobResult::new();
        assert_eq!(result.total_tasks(), 0);
        assert_eq!(result.total_children(), 0);
        assert_eq!(result.task_success_ratio(), 1.0);
        assert_eq!(result.child_success_ratio(), 1.0);
        assert!(result.meets_threshold(0.8));
    }

    #[test]
    fn test_job_result_task_success_ratio() {
        let result = JobResult {
            succeeded_tasks: vec!["a".to_string(), "b".to_string()],
            failed_tasks: vec!["c".to_string()],
            cancelled_tasks: vec![],
            ..Default::default()
        };

        assert_eq!(result.total_tasks(), 3);
        assert!((result.task_success_ratio() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_job_result_meets_threshold() {
        let result = JobResult {
            succeeded_tasks: vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
            failed_tasks: vec!["e".to_string()],
            ..Default::default()
        };

        // 4/5 = 0.8, should meet 0.8 threshold
        assert!(result.meets_threshold(0.8));
        // Should not meet 0.9 threshold
        assert!(!result.meets_threshold(0.9));
    }

    #[test]
    fn test_job_result_with_children() {
        let result = JobResult {
            succeeded_children: vec![JobId::new("child1"), JobId::new("child2")],
            failed_children: vec![JobId::new("child3")],
            ..Default::default()
        };

        assert_eq!(result.total_children(), 3);
        assert!((result.child_success_ratio() - 0.666).abs() < 0.01);
    }
}
