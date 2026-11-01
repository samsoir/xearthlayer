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
    ErrorPolicy, ExecutorConfig, Job, JobConcurrencyLimits, JobExecutor, JobId, JobResult,
    JobStatus, Priority, ResourceType, Task, TaskContext, TaskError, TaskResult,
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
// Concurrency permit release on early termination
//
// A job that ends early (Kill, Stop, or a FailFast failure) must give its
// concurrency group permit back, or every early end shrinks the group for the
// rest of the session. A group of one makes a leak observable: the second job
// can only start once the first has released the permit. See #296.
// =============================================================================

const SINGLE_PERMIT_GROUP: &str = "single-permit";

/// Starts an executor whose only concurrency group admits one job at a time.
fn start_executor_with_single_permit_group() -> (
    xearthlayer::executor::JobSubmitter,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let config = ExecutorConfig {
        job_concurrency_limits: JobConcurrencyLimits::empty().with_group(SINGLE_PERMIT_GROUP, 1),
        ..ExecutorConfig::default()
    };
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let executor_handle = tokio::spawn(async move {
        executor.run(shutdown_clone).await;
    });
    (submitter, shutdown, executor_handle)
}

/// Submits a job to the single-permit group and asserts that it runs, which it
/// can only do if the group's permit has been released.
async fn assert_group_admits_another_job(submitter: &xearthlayer::executor::JobSubmitter) {
    let mut follower = submitter.submit(InGroup(EmptyJob {
        id: "follower".to_string(),
    }));

    let finished = tokio::time::timeout(Duration::from_secs(2), follower.wait()).await;

    assert!(
        finished.is_ok(),
        "the follower never started: the first job kept the group permit"
    );
    assert_eq!(follower.status(), JobStatus::Succeeded);
}

#[tokio::test]
async fn test_killed_job_releases_its_concurrency_permit() {
    let (submitter, shutdown, executor_handle) = start_executor_with_single_permit_group();

    let handle = submitter.submit(InGroup(TestJobWithCounter {
        id: "killed".to_string(),
        counter: Arc::new(AtomicUsize::new(0)),
        task_count: 5,
        delay_ms: 200,
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.kill();

    assert_group_admits_another_job(&submitter).await;

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_stopped_job_releases_its_concurrency_permit() {
    let (submitter, shutdown, executor_handle) = start_executor_with_single_permit_group();

    let handle = submitter.submit(InGroup(TestJobWithCounter {
        id: "stopped".to_string(),
        counter: Arc::new(AtomicUsize::new(0)),
        task_count: 5,
        delay_ms: 200,
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.stop();

    assert_group_admits_another_job(&submitter).await;

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_fail_fast_job_releases_its_concurrency_permit() {
    let (submitter, shutdown, executor_handle) = start_executor_with_single_permit_group();

    let mut handle = submitter.submit(InGroup(FailingJobAfterN {
        id: "fail-fast".to_string(),
        counter: Arc::new(AtomicUsize::new(0)),
        fail_after: 1,
        total_tasks: 5,
    }));
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.wait()).await;
    assert_eq!(handle.status(), JobStatus::Failed);

    assert_group_admits_another_job(&submitter).await;

    shutdown.cancel();
    let _ = executor_handle.await;
}

// =============================================================================
// Admission gate
//
// While the gate reads true no queued job is admitted; a job already admitted
// runs every one of its tasks; clearing the gate admits again. The prefetch
// lane uses this to stand aside while FUSE demand is outstanding (#296).
// =============================================================================

fn start_gated_executor(
    paused: bool,
) -> (
    xearthlayer::executor::JobSubmitter,
    tokio::sync::watch::Sender<bool>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(paused);
    let config = ExecutorConfig::default().with_admission_gate(gate_rx);
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move { executor.run(shutdown_clone).await });
    (submitter, gate_tx, shutdown, handle)
}

#[tokio::test]
async fn test_paused_gate_admits_nothing_until_cleared() {
    let (submitter, gate, shutdown, executor_handle) = start_gated_executor(true);

    let mut handle = submitter.submit(EmptyJob {
        id: "gated".to_string(),
    });
    let admitted = tokio::time::timeout(Duration::from_millis(200), handle.wait()).await;
    assert!(
        admitted.is_err(),
        "a job must not run while the gate is paused"
    );

    gate.send_replace(false);
    let finished = tokio::time::timeout(Duration::from_secs(2), handle.wait()).await;
    assert!(
        finished.is_ok(),
        "clearing the gate must admit the queued job"
    );
    assert_eq!(handle.status(), JobStatus::Succeeded);

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_pausing_lets_an_admitted_job_finish_every_task() {
    let (submitter, gate, shutdown, executor_handle) = start_gated_executor(false);
    let counter = Arc::new(AtomicUsize::new(0));

    let mut handle = submitter.submit(TestJobWithCounter {
        id: "admitted".to_string(),
        counter: counter.clone(),
        task_count: 3,
        delay_ms: 100,
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.send_replace(true);

    let finished = tokio::time::timeout(Duration::from_secs(3), handle.wait()).await;
    assert!(
        finished.is_ok(),
        "an admitted job must not be parked by a pause"
    );
    assert_eq!(handle.status(), JobStatus::Succeeded);
    assert_eq!(counter.load(Ordering::SeqCst), 3, "every task must run");

    shutdown.cancel();
    let _ = executor_handle.await;
}

#[tokio::test]
async fn test_paused_gate_holds_grouped_jobs_too() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(true);
    let config = ExecutorConfig {
        job_concurrency_limits: JobConcurrencyLimits::empty().with_group(SINGLE_PERMIT_GROUP, 1),
        ..ExecutorConfig::default()
    }
    .with_admission_gate(gate_rx);
    let (executor, submitter) = JobExecutor::new(config);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let executor_handle = tokio::spawn(async move { executor.run(shutdown_clone).await });

    let mut handle = submitter.submit(InGroup(EmptyJob {
        id: "grouped".to_string(),
    }));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), handle.wait())
            .await
            .is_err()
    );
    gate_tx.send_replace(false);
    assert!(tokio::time::timeout(Duration::from_secs(2), handle.wait())
        .await
        .is_ok());

    shutdown.cancel();
    let _ = executor_handle.await;
}

// =============================================================================
// Test Job Implementations
// =============================================================================

/// Places any job in [`SINGLE_PERMIT_GROUP`], delegating everything else.
struct InGroup<J: Job>(J);

impl<J: Job> Job for InGroup<J> {
    fn id(&self) -> JobId {
        self.0.id()
    }

    fn name(&self) -> &str {
        self.0.name()
    }

    fn error_policy(&self) -> ErrorPolicy {
        self.0.error_policy()
    }

    fn priority(&self) -> Priority {
        self.0.priority()
    }

    fn concurrency_group(&self) -> Option<&str> {
        Some(SINGLE_PERMIT_GROUP)
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        self.0.create_tasks()
    }

    fn on_complete(&self, result: &JobResult) -> JobStatus {
        self.0.on_complete(result)
    }
}

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
