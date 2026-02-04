//! Integration tests for the Job Executor framework.
//!
//! These tests verify the complete executor workflow including:
//! - Job submission and execution
//! - Task completion handling
//! - Signal processing (pause, resume, stop, kill)
//! - Job concurrency limits
//! - Sequential task execution within jobs
//! - Error handling and job failure

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use xearthlayer::executor::{
    ErrorPolicy, ExecutorConfig, Job, JobExecutor, JobId, JobResult, JobStatus, Priority,
    ResourceType, Task, TaskContext, TaskError, TaskResult,
};

// =============================================================================
// Test Helpers
// =============================================================================

/// A simple task that increments a counter.
struct CountingTask {
    name: String,
    counter: Arc<AtomicUsize>,
    delay_ms: u64,
}

impl Task for CountingTask {
    fn name(&self) -> &str {
        &self.name
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.counter.fetch_add(1, Ordering::SeqCst);
            TaskResult::Success
        })
    }
}

/// A task that always fails.
struct FailingTask {
    name: String,
}

impl Task for FailingTask {
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
        Box::pin(async { TaskResult::Failed(TaskError::new("Task failed")) })
    }
}

// =============================================================================
// Integration Tests
// =============================================================================

#[tokio::test]
async fn test_executor_runs_single_task_job() {
    let counter = Arc::new(AtomicUsize::new(0));
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    let counter_clone = counter.clone();
    let job = TestJobWithCounter {
        id: "single-task".to_string(),
        counter: counter_clone,
        task_count: 1,
        delay_ms: 0,
    };

    let mut handle = submitter.submit(job);

    tokio::select! {
        _ = handle.wait() => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => {
            panic!("Job timed out");
        }
    }

    assert_eq!(handle.status(), JobStatus::Succeeded);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_runs_sequential_tasks() {
    let counter = Arc::new(AtomicUsize::new(0));
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    // Job with 3 sequential tasks
    let counter_clone = counter.clone();
    let job = TestJobWithCounter {
        id: "sequential-tasks".to_string(),
        counter: counter_clone,
        task_count: 3,
        delay_ms: 10,
    };

    let mut handle = submitter.submit(job);

    tokio::select! {
        _ = handle.wait() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("Job timed out");
        }
    }

    assert_eq!(handle.status(), JobStatus::Succeeded);
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_handles_job_with_zero_tasks() {
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    // Job with 0 tasks - should complete immediately
    let job = EmptyJob {
        id: "empty-job".to_string(),
    };

    let mut handle = submitter.submit(job);

    tokio::select! {
        _ = handle.wait() => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => {
            panic!("Job timed out");
        }
    }

    assert_eq!(handle.status(), JobStatus::Succeeded);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_kill_signal_cancels_job() {
    let counter = Arc::new(AtomicUsize::new(0));
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    // Job with slow task
    let counter_clone = counter.clone();
    let job = TestJobWithCounter {
        id: "killable-job".to_string(),
        counter: counter_clone,
        task_count: 5,
        delay_ms: 500, // 500ms per task
    };

    let handle = submitter.submit(job);

    // Give it time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send kill signal
    handle.kill();

    // Wait for cancellation with timeout
    let cancelled = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handle.status().is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    assert!(cancelled.is_ok(), "Job should be cancelled within timeout");
    assert_eq!(handle.status(), JobStatus::Cancelled);

    // Not all tasks should have completed
    assert!(counter.load(Ordering::SeqCst) < 5);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_multiple_concurrent_jobs() {
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    let counter1 = Arc::new(AtomicUsize::new(0));
    let counter2 = Arc::new(AtomicUsize::new(0));
    let counter3 = Arc::new(AtomicUsize::new(0));

    // Submit 3 jobs concurrently
    let job1 = TestJobWithCounter {
        id: "job1".to_string(),
        counter: counter1.clone(),
        task_count: 2,
        delay_ms: 10,
    };
    let job2 = TestJobWithCounter {
        id: "job2".to_string(),
        counter: counter2.clone(),
        task_count: 2,
        delay_ms: 10,
    };
    let job3 = TestJobWithCounter {
        id: "job3".to_string(),
        counter: counter3.clone(),
        task_count: 2,
        delay_ms: 10,
    };

    let mut handle1 = submitter.submit(job1);
    let mut handle2 = submitter.submit(job2);
    let mut handle3 = submitter.submit(job3);

    // Wait for all to complete
    tokio::select! {
        _ = async {
            handle1.wait().await;
            handle2.wait().await;
            handle3.wait().await;
        } => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("Jobs timed out");
        }
    }

    assert_eq!(handle1.status(), JobStatus::Succeeded);
    assert_eq!(handle2.status(), JobStatus::Succeeded);
    assert_eq!(handle3.status(), JobStatus::Succeeded);
    assert_eq!(counter1.load(Ordering::SeqCst), 2);
    assert_eq!(counter2.load(Ordering::SeqCst), 2);
    assert_eq!(counter3.load(Ordering::SeqCst), 2);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_fail_fast_policy_stops_on_failure() {
    let counter = Arc::new(AtomicUsize::new(0));
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    // Job where second task fails
    let counter_clone = counter.clone();
    let job = FailingJobAfterN {
        id: "fail-fast-job".to_string(),
        counter: counter_clone,
        fail_after: 1, // Fail on second task
        total_tasks: 5,
    };

    let mut handle = submitter.submit(job);

    tokio::select! {
        _ = handle.wait() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("Job timed out");
        }
    }

    assert_eq!(handle.status(), JobStatus::Failed);
    // Only first task should have succeeded
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_executor_graceful_shutdown() {
    let counter = Arc::new(AtomicUsize::new(0));
    let config = ExecutorConfig::default();
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });

    // Submit a job
    let counter_clone = counter.clone();
    let job = TestJobWithCounter {
        id: "shutdown-test".to_string(),
        counter: counter_clone,
        task_count: 10,
        delay_ms: 100,
    };

    let _handle = submitter.submit(job);

    // Give it time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Request shutdown
    shutdown.cancel();

    // Executor should complete gracefully
    let result = tokio::time::timeout(Duration::from_secs(2), executor_handle).await;
    assert!(result.is_ok(), "Executor should shut down gracefully");
}

// =============================================================================
// Test Job Implementations
// =============================================================================

/// Job that creates tasks which increment a shared counter.
struct TestJobWithCounter {
    id: String,
    counter: Arc<AtomicUsize>,
    task_count: usize,
    delay_ms: u64,
}

impl Job for TestJobWithCounter {
    fn id(&self) -> JobId {
        JobId::new(&self.id)
    }

    fn name(&self) -> &str {
        "TestJobWithCounter"
    }

    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    fn priority(&self) -> Priority {
        Priority::PREFETCH
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        (0..self.task_count)
            .map(|i| {
                Box::new(CountingTask {
                    name: format!("task-{}", i),
                    counter: self.counter.clone(),
                    delay_ms: self.delay_ms,
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

/// Job with zero tasks.
struct EmptyJob {
    id: String,
}

impl Job for EmptyJob {
    fn id(&self) -> JobId {
        JobId::new(&self.id)
    }

    fn name(&self) -> &str {
        "EmptyJob"
    }

    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    fn priority(&self) -> Priority {
        Priority::PREFETCH
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        Vec::new()
    }

    fn on_complete(&self, _result: &JobResult) -> JobStatus {
        JobStatus::Succeeded
    }
}

/// Job that fails after N successful tasks.
struct FailingJobAfterN {
    id: String,
    counter: Arc<AtomicUsize>,
    fail_after: usize,
    total_tasks: usize,
}

impl Job for FailingJobAfterN {
    fn id(&self) -> JobId {
        JobId::new(&self.id)
    }

    fn name(&self) -> &str {
        "FailingJobAfterN"
    }

    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    fn priority(&self) -> Priority {
        Priority::PREFETCH
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        (0..self.total_tasks)
            .map(|i| {
                if i < self.fail_after {
                    Box::new(CountingTask {
                        name: format!("task-{}", i),
                        counter: self.counter.clone(),
                        delay_ms: 0,
                    }) as Box<dyn Task>
                } else if i == self.fail_after {
                    Box::new(FailingTask {
                        name: format!("failing-task-{}", i),
                    }) as Box<dyn Task>
                } else {
                    Box::new(CountingTask {
                        name: format!("task-{}", i),
                        counter: self.counter.clone(),
                        delay_ms: 0,
                    }) as Box<dyn Task>
                }
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
