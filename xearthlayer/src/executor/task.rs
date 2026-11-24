//! Task trait and related types.
//!
//! A task is a single async operation within a job. Tasks declare their
//! resource requirements and can spawn child jobs.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{Task, TaskResult, ResourceType};
//!
//! struct DownloadTask {
//!     url: String,
//! }
//!
//! impl Task for DownloadTask {
//!     fn name(&self) -> &str { "Download" }
//!     fn resource_type(&self) -> ResourceType { ResourceType::Network }
//!
//!     async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
//!         // Download logic here...
//!         TaskResult::Success
//!     }
//! }
//! ```

use super::context::TaskContext;
use super::policy::RetryPolicy;
use super::resource_pool::ResourceType;
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// A single async operation within a job.
///
/// Tasks are the atomic units of work in the executor. Each task:
/// - Has a name for logging and tracking
/// - Declares what resource type it needs (network, disk, CPU)
/// - Has a retry policy for handling transient failures
/// - Executes asynchronously with access to shared resources
///
/// # Resource Types
///
/// Tasks must declare their resource type so the scheduler can properly
/// manage concurrency. The resource pools have different capacities:
/// - `Network`: High capacity (~256) for HTTP requests
/// - `DiskIO`: Medium capacity (~64) for file operations
/// - `CPU`: Low capacity (~num_cpus) for compute-bound work
///
/// # Child Jobs
///
/// Tasks can spawn child jobs via `TaskContext::spawn_child_job()`. The
/// parent job will not complete until all child jobs complete.
pub trait Task: Send + Sync + 'static {
    /// Returns a human-readable name for logging/display.
    ///
    /// This should be a short, descriptive name like "DownloadChunks" or "BuildAndCacheDds".
    fn name(&self) -> &str;

    /// Returns the resource type required by this task.
    ///
    /// The scheduler acquires a permit from this pool before executing the task.
    /// This prevents resource exhaustion by limiting concurrent operations of
    /// each type.
    fn resource_type(&self) -> ResourceType;

    /// Returns the retry policy for this task.
    ///
    /// The default is no retries. Override this to enable automatic retries
    /// for transient failures.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::None
    }

    /// Executes the task.
    ///
    /// This method is called when the task is ready to execute (resource permit
    /// acquired, no cancellation). The task should check `ctx.is_cancelled()`
    /// periodically for long-running operations.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Context for spawning child jobs, accessing resources, and cancellation
    ///
    /// # Returns
    ///
    /// A `TaskResult` indicating success, failure, retry request, or cancellation.
    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>>;
}

/// Result of task execution.
///
/// This enum represents all possible outcomes of a task:
/// - Success (with or without output data)
/// - Failure (with error details)
/// - Retry request (for transient failures)
/// - Cancellation (task was cancelled before completion)
#[derive(Debug)]
pub enum TaskResult {
    /// Task completed successfully with no output.
    Success,

    /// Task completed successfully with output data.
    ///
    /// The output can be retrieved by subsequent tasks in the same job
    /// via `TaskContext::get_output()`.
    SuccessWithOutput(TaskOutput),

    /// Task failed with an error.
    ///
    /// This is a permanent failure - the task will not be retried (unless
    /// the retry policy allows it and the task returns `Retry` instead).
    Failed(TaskError),

    /// Task requests a retry.
    ///
    /// The scheduler will retry the task according to its retry policy.
    /// If no more retries are available, this becomes a `Failed` result.
    Retry(TaskError),

    /// Task was cancelled.
    ///
    /// This typically happens when the job is cancelled or the system is
    /// shutting down.
    Cancelled,
}

impl TaskResult {
    /// Returns true if the task succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success | Self::SuccessWithOutput(_))
    }

    /// Returns true if the task failed.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// Returns true if the task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Returns true if the task requested a retry.
    pub fn is_retry(&self) -> bool {
        matches!(self, Self::Retry(_))
    }

    /// Converts the result to a simplified kind for telemetry.
    pub fn kind(&self) -> TaskResultKind {
        match self {
            Self::Success | Self::SuccessWithOutput(_) => TaskResultKind::Success,
            Self::Failed(_) | Self::Retry(_) => TaskResultKind::Failed,
            Self::Cancelled => TaskResultKind::Cancelled,
        }
    }
}

/// Simplified result kind for telemetry (no error details).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskResultKind {
    /// Task succeeded.
    Success,
    /// Task failed (includes retry exhausted).
    Failed,
    /// Task was cancelled.
    Cancelled,
}

/// Error type for task failures.
#[derive(Debug)]
pub struct TaskError {
    /// Human-readable error message.
    message: String,
    /// Whether this error is transient (retryable).
    transient: bool,
    /// Optional source error.
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl TaskError {
    /// Creates a new task error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
            source: None,
        }
    }

    /// Creates a new transient (retryable) error.
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
            source: None,
        }
    }

    /// Creates an error for missing input from a previous task.
    pub fn missing_input(key: &str) -> Self {
        Self::new(format!("Missing required input: {}", key))
    }

    /// Attaches a source error.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Returns true if this error is transient (retryable).
    pub fn is_transient(&self) -> bool {
        self.transient
    }

    /// Returns the error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TaskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref() as &_)
    }
}

// Note: We don't provide a blanket From<E> implementation to avoid conflicts.
// Use TaskError::new(err.to_string()).with_source(err) explicitly instead.

/// Output data from a task.
///
/// Tasks can produce output that is passed to subsequent tasks in the same job.
/// The output is stored as a map of string keys to type-erased values.
///
/// # Example
///
/// ```ignore
/// let mut output = TaskOutput::new();
/// output.set("chunks", downloaded_chunks);
/// output.set("count", chunk_count);
///
/// // Later, in another task:
/// let chunks: &Vec<Bytes> = ctx.get_output("DownloadChunks", "chunks")?;
/// ```
#[derive(Default)]
pub struct TaskOutput {
    data: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl TaskOutput {
    /// Creates a new empty output.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a value in the output.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to store the value under
    /// * `value` - The value to store (must be Send + Sync + 'static)
    pub fn set<T: Any + Send + Sync>(&mut self, key: &str, value: T) {
        self.data.insert(key.to_string(), Box::new(value));
    }

    /// Retrieves a value from the output.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to retrieve
    ///
    /// # Returns
    ///
    /// The value if it exists and has the correct type, `None` otherwise.
    pub fn get<T: Any + Send + Sync>(&self, key: &str) -> Option<&T> {
        self.data.get(key).and_then(|v| v.downcast_ref())
    }

    /// Returns true if the output contains a value for the given key.
    pub fn contains(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// Returns the number of values in the output.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true if the output is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl fmt::Debug for TaskOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskOutput")
            .field("keys", &self.data.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_result_is_success() {
        assert!(TaskResult::Success.is_success());
        assert!(TaskResult::SuccessWithOutput(TaskOutput::new()).is_success());
        assert!(!TaskResult::Failed(TaskError::new("err")).is_success());
        assert!(!TaskResult::Cancelled.is_success());
    }

    #[test]
    fn test_task_result_is_failed() {
        assert!(!TaskResult::Success.is_failed());
        assert!(TaskResult::Failed(TaskError::new("err")).is_failed());
        assert!(!TaskResult::Cancelled.is_failed());
    }

    #[test]
    fn test_task_result_kind() {
        assert_eq!(TaskResult::Success.kind(), TaskResultKind::Success);
        assert_eq!(
            TaskResult::SuccessWithOutput(TaskOutput::new()).kind(),
            TaskResultKind::Success
        );
        assert_eq!(
            TaskResult::Failed(TaskError::new("err")).kind(),
            TaskResultKind::Failed
        );
        assert_eq!(
            TaskResult::Retry(TaskError::transient("retry")).kind(),
            TaskResultKind::Failed
        );
        assert_eq!(TaskResult::Cancelled.kind(), TaskResultKind::Cancelled);
    }

    #[test]
    fn test_task_error_new() {
        let err = TaskError::new("something went wrong");
        assert_eq!(err.message(), "something went wrong");
        assert!(!err.is_transient());
    }

    #[test]
    fn test_task_error_transient() {
        let err = TaskError::transient("network timeout");
        assert_eq!(err.message(), "network timeout");
        assert!(err.is_transient());
    }

    #[test]
    fn test_task_error_missing_input() {
        let err = TaskError::missing_input("chunks");
        assert!(err.message().contains("chunks"));
        assert!(err.message().contains("Missing"));
    }

    #[test]
    fn test_task_error_display() {
        let err = TaskError::new("test error");
        assert_eq!(format!("{}", err), "test error");
    }

    #[test]
    fn test_task_output_basic() {
        let mut output = TaskOutput::new();
        assert!(output.is_empty());
        assert_eq!(output.len(), 0);

        output.set("count", 42i32);
        assert!(!output.is_empty());
        assert_eq!(output.len(), 1);
        assert!(output.contains("count"));
        assert!(!output.contains("other"));

        assert_eq!(output.get::<i32>("count"), Some(&42));
        assert_eq!(output.get::<String>("count"), None); // Wrong type
        assert_eq!(output.get::<i32>("other"), None); // Wrong key
    }

    #[test]
    fn test_task_output_multiple_types() {
        let mut output = TaskOutput::new();
        output.set("number", 123i32);
        output.set("text", "hello".to_string());
        output.set("flag", true);

        assert_eq!(output.get::<i32>("number"), Some(&123));
        assert_eq!(output.get::<String>("text"), Some(&"hello".to_string()));
        assert_eq!(output.get::<bool>("flag"), Some(&true));
    }

    #[test]
    fn test_task_output_debug() {
        let mut output = TaskOutput::new();
        output.set("a", 1);
        output.set("b", 2);

        let debug = format!("{:?}", output);
        assert!(debug.contains("TaskOutput"));
        assert!(debug.contains("keys"));
    }
}
