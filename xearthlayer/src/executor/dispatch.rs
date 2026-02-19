//! Task dispatching.
//!
//! This module handles dispatching tasks from the queue to execution,
//! respecting resource pool limits and job state.

use super::active_job::TaskCompletion;
use super::context::TaskContext;
use super::core::JobExecutor;
use super::resource_pool::ResourcePermit;
use super::telemetry::TelemetryEvent;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

impl JobExecutor {
    /// Dispatches pending tasks for execution.
    ///
    /// This method:
    /// 1. Checks resource availability for each queued task
    /// 2. Verifies job is still active and not paused
    /// 3. Spawns task execution with proper context
    /// 4. Respects max concurrent task limits
    pub(crate) async fn dispatch_tasks(&mut self) {
        if self.dispatched_count >= self.config.max_concurrent_tasks {
            return;
        }

        loop {
            let Some(dispatchable) = self.find_dispatchable_task().await else {
                return;
            };

            let Some(ctx) = self.prepare_task_context(&dispatchable).await else {
                continue;
            };

            self.spawn_task(dispatchable, ctx).await;

            if self.dispatched_count >= self.config.max_concurrent_tasks {
                break;
            }
        }
    }

    /// Finds a task that can be dispatched (has available resources).
    ///
    /// The acquired resource permit is stored in the returned `DispatchableTask`
    /// and moved into the spawned future, ensuring exactly one permit per task.
    async fn find_dispatchable_task(&self) -> Option<DispatchableTask> {
        let mut queue = self.task_queue.lock().await;
        let mut temp_queue = Vec::new();
        let mut found = None;

        while let Some(queued) = queue.pop() {
            if let Some(permit) = self
                .resource_pools
                .try_acquire_for_priority(queued.resource_type, queued.priority)
            {
                found = Some(DispatchableTask {
                    task: queued.task,
                    job_id: queued.job_id,
                    resource_type: queued.resource_type,
                    permit,
                });
                break;
            }
            temp_queue.push(queued);
        }

        for task in temp_queue {
            queue.push(task);
        }

        found
    }

    /// Prepares the task context, checking job state.
    /// Returns None if the task should be re-queued (job paused/stopped).
    async fn prepare_task_context(&self, task: &DispatchableTask) -> Option<TaskContext> {
        let jobs = self.active_jobs.lock().await;
        let job = jobs.get(&task.job_id)?;

        if job.is_paused() || job.should_stop() {
            // We can't re-queue here because we don't own the task anymore
            // The caller will handle this by continuing the loop
            debug!(job_id = %task.job_id, "Job paused or stopped, skipping dispatch");
            return None;
        }

        let mut ctx = TaskContext::with_child_sender(
            task.job_id.clone(),
            job.cancellation.clone(),
            job.child_job_tx.clone(),
            Arc::clone(&job.task_outputs),
        );

        if let Some(ref metrics) = self.metrics_client {
            ctx = ctx.with_metrics(metrics.clone());
        }

        Some(ctx)
    }

    /// Spawns a task for execution.
    ///
    /// The resource permit from `DispatchableTask` is moved into the spawned
    /// future and held via RAII until the task completes. This ensures exactly
    /// one permit acquisition per dispatched task (no double acquisition).
    async fn spawn_task(&mut self, task: DispatchableTask, ctx: TaskContext) {
        // Update job state
        {
            let mut jobs = self.active_jobs.lock().await;
            if let Some(job) = jobs.get_mut(&task.job_id) {
                job.tasks_in_flight += 1;
                if job.queued_tasks > 0 {
                    job.queued_tasks -= 1;
                }
            }
        }

        self.dispatched_count += 1;
        let task_name = task.task.name().to_string();

        debug!(
            job_id = %task.job_id,
            task_name = %task_name,
            resource_type = ?task.resource_type,
            "Task started"
        );

        self.telemetry.emit(TelemetryEvent::TaskStarted {
            job_id: task.job_id.clone(),
            task_name: task_name.clone(),
            resource_type: task.resource_type,
        });

        let completion_tx = self.completion_tx.clone();
        let work_notify = Arc::clone(&self.work_notify);
        let job_id = task.job_id;
        let task_box = task.task;
        // Move the already-acquired permit into the future (RAII release on drop)
        let _permit = task.permit;

        tokio::spawn(async move {
            let start = Instant::now();

            let mut ctx = ctx;
            let result = task_box.execute(&mut ctx).await;

            // Permit is dropped here when the future completes
            drop(_permit);

            let _ = completion_tx.send(TaskCompletion {
                job_id,
                task_name,
                result,
                duration: start.elapsed(),
            });

            work_notify.notify_one();
        });
    }
}

/// A task that is ready to be dispatched.
///
/// Carries the already-acquired resource permit so that `spawn_task()`
/// doesn't need to acquire a second one (fixing the double permit bug).
struct DispatchableTask {
    task: Box<dyn super::task::Task>,
    job_id: super::job::JobId,
    resource_type: super::resource_pool::ResourceType,
    /// The resource permit acquired during `find_dispatchable_task()`.
    permit: ResourcePermit,
}
