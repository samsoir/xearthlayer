//! Task execution context.
//!
//! The [`TaskContext`] provides tasks with access to:
//! - Cancellation checking via `CancellationToken`
//! - Child job spawning for hierarchical work
//! - Output from previous tasks in the same job
//! - The parent job's ID for logging and telemetry
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{Task, TaskContext, TaskResult};
//!
//! impl Task for MyTask {
//!     async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
//!         // Check for cancellation periodically
//!         if ctx.is_cancelled() {
//!             return TaskResult::Cancelled;
//!         }
//!
//!         // Get output from a previous task
//!         let chunks: &Vec<Bytes> = ctx.get_output("DownloadChunks", "data")?;
//!
//!         // Spawn a child job for additional work
//!         ctx.spawn_child_job(ChildJob::new(chunk_id));
//!
//!         TaskResult::Success
//!     }
//! }
//! ```

use super::job::{Job, JobId};
use super::task::TaskOutput;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// Shared reference to task outputs, allowing multiple readers.
pub type SharedTaskOutputs = Arc<RwLock<HashMap<String, TaskOutput>>>;

// =============================================================================
// Task Context
// =============================================================================

/// Execution context passed to tasks during execution.
///
/// This struct provides tasks with:
/// - Cancellation checking for graceful shutdown
/// - Access to outputs from previous tasks
/// - Ability to spawn child jobs
/// - Reference to the parent job ID
///
/// The context is created by the executor and passed to each task's `execute`
/// method. Tasks should check `is_cancelled()` periodically for long-running
/// operations.
pub struct TaskContext {
    /// The parent job's ID.
    job_id: JobId,

    /// Cancellation token for cooperative cancellation.
    cancellation: CancellationToken,

    /// Shared reference to outputs from completed tasks, keyed by task name.
    task_outputs: SharedTaskOutputs,

    /// Channel for spawning child jobs.
    child_job_sender: Option<ChildJobSender>,

    /// Count of child jobs spawned by this task.
    spawned_children: usize,
}

/// Sender for spawning child jobs.
///
/// This is a wrapper around a channel sender that accepts boxed jobs.
/// It's optional because not all tasks need to spawn children.
pub struct ChildJobSender {
    sender: tokio::sync::mpsc::UnboundedSender<SpawnedChildJob>,
}

/// A child job spawned by a task.
pub struct SpawnedChildJob {
    /// The child job to execute.
    pub job: Box<dyn Job>,
    /// The name of the task that spawned this child.
    pub spawning_task: String,
}

impl TaskContext {
    /// Creates a new task context.
    ///
    /// This is typically called by the executor when preparing to run a task.
    ///
    /// # Arguments
    ///
    /// * `job_id` - The parent job's ID
    /// * `cancellation` - Token for cooperative cancellation
    pub fn new(job_id: JobId, cancellation: CancellationToken) -> Self {
        Self {
            job_id,
            cancellation,
            task_outputs: Arc::new(RwLock::new(HashMap::new())),
            child_job_sender: None,
            spawned_children: 0,
        }
    }

    /// Creates a task context with child job spawning capability.
    ///
    /// # Arguments
    ///
    /// * `job_id` - The parent job's ID
    /// * `cancellation` - Token for cooperative cancellation
    /// * `child_sender` - Channel for spawning child jobs
    /// * `task_outputs` - Shared reference to outputs from previously completed tasks
    pub fn with_child_sender(
        job_id: JobId,
        cancellation: CancellationToken,
        child_sender: tokio::sync::mpsc::UnboundedSender<SpawnedChildJob>,
        task_outputs: SharedTaskOutputs,
    ) -> Self {
        Self {
            job_id,
            cancellation,
            task_outputs,
            child_job_sender: Some(ChildJobSender {
                sender: child_sender,
            }),
            spawned_children: 0,
        }
    }

    /// Returns the parent job's ID.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Returns true if cancellation has been requested.
    ///
    /// Tasks should check this periodically during long-running operations
    /// and return `TaskResult::Cancelled` if true.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Returns the cancellation token.
    ///
    /// This can be used with `tokio::select!` to race operations against
    /// cancellation.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Adds output from a completed task.
    ///
    /// This is called by the executor after a task completes successfully
    /// with `TaskResult::SuccessWithOutput`.
    #[allow(dead_code)] // Infrastructure for future executor use
    pub(crate) fn add_task_output(&self, task_name: String, output: TaskOutput) {
        if let Ok(mut outputs) = self.task_outputs.write() {
            outputs.insert(task_name, output);
        }
    }

    /// Gets a cloned value from a previous task's output.
    ///
    /// Since task outputs are behind a RwLock, this method clones the value
    /// to return an owned copy.
    ///
    /// # Arguments
    ///
    /// * `task_name` - Name of the task that produced the output
    /// * `key` - Key within that task's output
    ///
    /// # Returns
    ///
    /// A cloned copy of the value if it exists and has the correct type, `None` otherwise.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let chunks: Option<ChunkResults> = ctx.get_output("DownloadChunks", "chunks");
    /// ```
    pub fn get_output<T: std::any::Any + Send + Sync + Clone>(
        &self,
        task_name: &str,
        key: &str,
    ) -> Option<T> {
        let outputs = self.task_outputs.read().ok()?;
        let output = outputs.get(task_name)?;
        output.get::<T>(key).cloned()
    }

    /// Returns true if output from a task exists.
    pub fn has_output(&self, task_name: &str) -> bool {
        self.task_outputs
            .read()
            .map(|outputs| outputs.contains_key(task_name))
            .unwrap_or(false)
    }

    /// Spawns a child job.
    ///
    /// The child job will be executed by the executor after the current task
    /// completes. The parent job will not complete until all child jobs complete.
    ///
    /// # Arguments
    ///
    /// * `job` - The child job to spawn
    /// * `task_name` - Name of the spawning task (for telemetry)
    ///
    /// # Returns
    ///
    /// `true` if the job was successfully queued, `false` if child spawning
    /// is not available (context was created without a child sender).
    pub fn spawn_child_job(&mut self, job: impl Job + 'static, task_name: &str) -> bool {
        if let Some(ref sender) = self.child_job_sender {
            let spawned = SpawnedChildJob {
                job: Box::new(job),
                spawning_task: task_name.to_string(),
            };
            if sender.sender.send(spawned).is_ok() {
                self.spawned_children += 1;
                return true;
            }
        }
        false
    }

    /// Spawns a child job from a boxed trait object.
    ///
    /// This is a variant of [`spawn_child_job`] that accepts an already-boxed
    /// job. Use this when working with factory patterns that return `Box<dyn Job>`.
    ///
    /// # Arguments
    ///
    /// * `job` - The boxed child job to spawn
    /// * `task_name` - Name of the spawning task (for telemetry)
    ///
    /// # Returns
    ///
    /// `true` if the job was successfully queued, `false` if child spawning
    /// is not available (context was created without a child sender).
    pub fn spawn_child_job_boxed(&mut self, job: Box<dyn Job>, task_name: &str) -> bool {
        if let Some(ref sender) = self.child_job_sender {
            let spawned = SpawnedChildJob {
                job,
                spawning_task: task_name.to_string(),
            };
            if sender.sender.send(spawned).is_ok() {
                self.spawned_children += 1;
                return true;
            }
        }
        false
    }

    /// Returns the number of child jobs spawned by this context.
    pub fn spawned_children_count(&self) -> usize {
        self.spawned_children
    }

    /// Returns true if this context can spawn child jobs.
    pub fn can_spawn_children(&self) -> bool {
        self.child_job_sender.is_some()
    }
}

impl std::fmt::Debug for TaskContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output_keys: Vec<String> = self
            .task_outputs
            .read()
            .map(|outputs| outputs.keys().cloned().collect())
            .unwrap_or_default();

        f.debug_struct("TaskContext")
            .field("job_id", &self.job_id)
            .field("is_cancelled", &self.is_cancelled())
            .field("task_outputs", &output_keys)
            .field("can_spawn_children", &self.can_spawn_children())
            .field("spawned_children", &self.spawned_children)
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::handle::JobStatus;
    use crate::executor::policy::{ErrorPolicy, Priority};
    use crate::executor::task::Task;

    /// A minimal test job for spawning.
    struct TestChildJob {
        id: String,
    }

    impl Job for TestChildJob {
        fn id(&self) -> JobId {
            JobId::new(&self.id)
        }

        fn name(&self) -> &str {
            "TestChildJob"
        }

        fn error_policy(&self) -> ErrorPolicy {
            ErrorPolicy::FailFast
        }

        fn priority(&self) -> Priority {
            Priority::PREFETCH
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            vec![]
        }

        fn on_complete(&self, result: &super::super::job::JobResult) -> JobStatus {
            if result.failed_tasks.is_empty() {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
    }

    #[test]
    fn test_context_creation() {
        let token = CancellationToken::new();
        let ctx = TaskContext::new(JobId::new("test-job"), token);

        assert_eq!(ctx.job_id().as_str(), "test-job");
        assert!(!ctx.is_cancelled());
        assert!(!ctx.can_spawn_children());
        assert_eq!(ctx.spawned_children_count(), 0);
    }

    #[test]
    fn test_context_cancellation() {
        let token = CancellationToken::new();
        let ctx = TaskContext::new(JobId::new("test"), token.clone());

        assert!(!ctx.is_cancelled());
        token.cancel();
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_context_task_outputs() {
        let token = CancellationToken::new();
        let ctx = TaskContext::new(JobId::new("test"), token);

        // Initially no outputs
        assert!(!ctx.has_output("task1"));
        assert!(ctx.get_output::<i32>("task1", "value").is_none());

        // Add an output
        let mut output = TaskOutput::new();
        output.set("value", 42i32);
        output.set("name", "test".to_string());
        ctx.add_task_output("task1".to_string(), output);

        // Now we can retrieve it (returns owned clone)
        assert!(ctx.has_output("task1"));
        assert_eq!(ctx.get_output::<i32>("task1", "value"), Some(42));
        assert_eq!(
            ctx.get_output::<String>("task1", "name"),
            Some("test".to_string())
        );

        // Wrong type returns None
        assert!(ctx.get_output::<String>("task1", "value").is_none());

        // Wrong key returns None
        assert!(ctx.get_output::<i32>("task1", "missing").is_none());

        // Wrong task returns None
        assert!(ctx.get_output::<i32>("task2", "value").is_none());
    }

    #[tokio::test]
    async fn test_context_spawn_children() {
        let token = CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outputs = Arc::new(RwLock::new(HashMap::new()));
        let mut ctx = TaskContext::with_child_sender(JobId::new("parent"), token, tx, outputs);

        assert!(ctx.can_spawn_children());
        assert_eq!(ctx.spawned_children_count(), 0);

        // Spawn a child
        let child = TestChildJob {
            id: "child-1".to_string(),
        };
        assert!(ctx.spawn_child_job(child, "SpawningTask"));
        assert_eq!(ctx.spawned_children_count(), 1);

        // Verify it was sent
        let spawned = rx.recv().await.unwrap();
        assert_eq!(spawned.job.id().as_str(), "child-1");
        assert_eq!(spawned.spawning_task, "SpawningTask");
    }

    #[test]
    fn test_context_spawn_without_sender() {
        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test"), token);

        assert!(!ctx.can_spawn_children());

        // Spawning should fail gracefully
        let child = TestChildJob {
            id: "child".to_string(),
        };
        assert!(!ctx.spawn_child_job(child, "task"));
        assert_eq!(ctx.spawned_children_count(), 0);
    }

    #[test]
    fn test_context_debug() {
        let token = CancellationToken::new();
        let ctx = TaskContext::new(JobId::new("debug-test"), token);

        let debug = format!("{:?}", ctx);
        assert!(debug.contains("TaskContext"));
        assert!(debug.contains("debug-test"));
        assert!(debug.contains("is_cancelled"));
    }
}
