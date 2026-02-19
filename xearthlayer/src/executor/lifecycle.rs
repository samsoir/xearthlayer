//! Job and task lifecycle management.
//!
//! This module handles the lifecycle of jobs and tasks:
//! - Job submission and starting
//! - Task completion handling
//! - Job completion and cleanup
//! - Waiting queue management for concurrency limits

use super::active_job::{ActiveJob, TaskCompletion};
use super::config::DEFAULT_SIGNAL_CHANNEL_CAPACITY;
use super::core::JobExecutor;
use super::handle::JobStatus;
use super::job::JobId;
use super::queue::QueuedTask;
use super::submitter::{SubmittedJob, WaitingJob};
use super::task::TaskResult;
use super::telemetry::TelemetryEvent;
use std::time::Duration;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit};
use tracing::{debug, error, info, warn};

impl JobExecutor {
    /// Handles a newly submitted job.
    ///
    /// Acquires a concurrency permit if needed, or queues the job for later.
    pub(crate) async fn handle_job_submission(&mut self, submitted: SubmittedJob) {
        let job_id = submitted.job_id.clone();
        let name = submitted.name.clone();
        let priority = submitted.priority;

        info!(
            job_id = %job_id,
            job_name = %name,
            priority = ?priority,
            "Job submitted"
        );

        self.telemetry.emit(TelemetryEvent::JobSubmitted {
            job_id: job_id.clone(),
            name: name.clone(),
            priority,
        });

        // Try to acquire concurrency permit (non-blocking to avoid deadlock)
        let concurrency_permit = if let Some(group) = submitted.job.concurrency_group() {
            match self.config.job_concurrency_limits.try_acquire(group) {
                Some(permit) => {
                    debug!(job_id = %job_id, group = %group, "Acquired concurrency permit");
                    Some(permit)
                }
                None => {
                    let seq = self.waiting_sequence;
                    self.waiting_sequence += 1;
                    debug!(
                        job_id = %job_id,
                        group = %group,
                        priority = ?priority,
                        sequence = seq,
                        waiting_count = self.waiting_for_permit.len(),
                        "No concurrency permit available, queueing job"
                    );
                    self.waiting_for_permit.push(WaitingJob {
                        submitted,
                        sequence: seq,
                    });
                    return;
                }
            }
        } else {
            None
        };

        self.start_job_with_permit(submitted, concurrency_permit)
            .await;
    }

    /// Handles a completed task.
    pub(crate) async fn handle_task_completion(&mut self, completion: TaskCompletion) {
        self.dispatched_count = self.dispatched_count.saturating_sub(1);
        let result_kind = completion.result.kind();

        self.log_task_result(&completion);

        self.telemetry.emit(TelemetryEvent::TaskCompleted {
            job_id: completion.job_id.clone(),
            task_name: completion.task_name.clone(),
            result: result_kind,
            duration: completion.duration,
        });

        // Extract job_id before consuming completion.result
        let job_id = completion.job_id.clone();
        let task_name = completion.task_name.clone();

        let next_task_info = {
            let mut jobs = self.active_jobs.lock().await;
            let job = match jobs.get_mut(&job_id) {
                Some(j) => j,
                None => return,
            };

            job.tasks_in_flight -= 1;

            // Process result - consumes completion.result for SuccessWithOutput
            let should_enqueue_next = match completion.result {
                TaskResult::Success => {
                    job.succeeded_tasks.push(task_name);
                    true
                }
                TaskResult::SuccessWithOutput(output) => {
                    if let Ok(mut outputs) = job.task_outputs.write() {
                        outputs.insert(task_name.clone(), output);
                    }
                    job.succeeded_tasks.push(task_name);
                    true
                }
                TaskResult::Failed(_) => {
                    job.failed_tasks.push(task_name);
                    if let Some(status) = job.check_error_policy() {
                        job.update_status(status);
                        job.cancellation.cancel();
                    }
                    false
                }
                TaskResult::Retry(error) => {
                    job.failed_tasks.push(task_name.clone());
                    self.telemetry.emit(TelemetryEvent::TaskRetrying {
                        job_id: job_id.clone(),
                        task_name,
                        attempt: 1,
                        delay: Duration::from_millis(100),
                    });
                    debug!(error = %error, "Task marked for retry (treating as failure)");
                    false
                }
                TaskResult::Cancelled => {
                    job.cancelled_tasks.push(task_name);
                    false
                }
            };

            if should_enqueue_next && !job.pending_tasks.is_empty() {
                job.queued_tasks += 1;
                let next_task = job.pending_tasks.remove(0);
                let priority = job.priority;
                Some((next_task, priority, job_id.clone()))
            } else {
                None
            }
        };

        if let Some((task, priority, job_id)) = next_task_info {
            self.enqueue_next_task(task, job_id, priority).await;
        }

        self.work_notify.notify_one();
    }

    /// Logs the task result at appropriate level.
    fn log_task_result(&self, completion: &TaskCompletion) {
        match &completion.result {
            TaskResult::Success | TaskResult::SuccessWithOutput(_) => {
                debug!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    duration_ms = completion.duration.as_millis(),
                    "Task completed successfully"
                );
            }
            TaskResult::Failed(err) => {
                error!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    error = %err,
                    duration_ms = completion.duration.as_millis(),
                    "Task failed"
                );
            }
            TaskResult::Retry(err) => {
                warn!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    error = %err,
                    duration_ms = completion.duration.as_millis(),
                    "Task needs retry"
                );
            }
            TaskResult::Cancelled => {
                warn!(
                    job_id = %completion.job_id,
                    task_name = %completion.task_name,
                    duration_ms = completion.duration.as_millis(),
                    "Task cancelled"
                );
            }
        }
    }

    /// Enqueues the next sequential task.
    async fn enqueue_next_task(
        &self,
        task: Box<dyn super::task::Task>,
        job_id: JobId,
        priority: super::policy::Priority,
    ) {
        let task_name = task.name().to_string();
        let queued = QueuedTask::new(task, job_id.clone(), priority);

        debug!(job_id = %job_id, task_name = %task_name, "Enqueuing next sequential task");

        self.telemetry.emit(TelemetryEvent::TaskEnqueued {
            job_id,
            task_name,
            priority,
            queue_depth: 0,
        });

        let mut queue = self.task_queue.lock().await;
        queue.push(queued);
    }

    /// Completes jobs that have finished all work.
    pub(crate) async fn complete_finished_jobs(&mut self) {
        let completed_jobs = self.find_completed_jobs().await;

        for job_id in completed_jobs {
            self.complete_job(job_id).await;
        }

        self.start_waiting_jobs().await;
    }

    /// Finds all jobs that are ready to complete.
    async fn find_completed_jobs(&self) -> Vec<JobId> {
        let jobs = self.active_jobs.lock().await;
        jobs.iter()
            .filter(|(_, job)| {
                let is_complete = (!job.has_pending_work() && job.all_children_complete())
                    || job.status == JobStatus::Stopped
                    || job.status == JobStatus::Cancelled;
                is_complete && !job.status.is_terminal()
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Completes a single job.
    async fn complete_job(&mut self, job_id: JobId) {
        let (final_status, result, parent_id) = {
            let mut jobs = self.active_jobs.lock().await;
            let Some(job) = jobs.get_mut(&job_id) else {
                return;
            };

            let status = if job.status.is_terminal() {
                job.status
            } else {
                job.compute_final_status()
            };

            let result = job.build_result();

            {
                let mut holder = job.result_holder.lock().await;
                *holder = Some(result.clone());
            }

            job.update_status(status);
            (status, result, job.parent_job_id.clone())
        };

        self.log_job_completion(&job_id, final_status, &result);

        self.telemetry.emit(TelemetryEvent::JobCompleted {
            job_id: job_id.clone(),
            status: final_status,
            duration: result.duration,
            tasks_succeeded: result.succeeded_tasks.len(),
            tasks_failed: result.failed_tasks.len(),
            children_succeeded: result.succeeded_children.len(),
            children_failed: result.failed_children.len(),
        });

        if let Some(parent_id) = parent_id {
            self.update_parent_job(&parent_id, &job_id, final_status)
                .await;
        }

        let mut jobs = self.active_jobs.lock().await;
        jobs.remove(&job_id);
    }

    /// Logs job completion at appropriate level.
    fn log_job_completion(
        &self,
        job_id: &JobId,
        status: JobStatus,
        result: &super::job::JobResult,
    ) {
        match status {
            JobStatus::Succeeded => {
                info!(
                    job_id = %job_id,
                    duration_ms = result.duration.as_millis(),
                    tasks_succeeded = result.succeeded_tasks.len(),
                    "Job completed successfully"
                );
            }
            JobStatus::Failed => {
                error!(
                    job_id = %job_id,
                    duration_ms = result.duration.as_millis(),
                    tasks_succeeded = result.succeeded_tasks.len(),
                    tasks_failed = result.failed_tasks.len(),
                    "Job failed"
                );
            }
            _ => {
                warn!(
                    job_id = %job_id,
                    status = ?status,
                    duration_ms = result.duration.as_millis(),
                    "Job ended"
                );
            }
        }
    }

    /// Updates a parent job when a child completes.
    async fn update_parent_job(&self, parent_id: &JobId, child_id: &JobId, status: JobStatus) {
        let mut jobs = self.active_jobs.lock().await;
        if let Some(parent) = jobs.get_mut(parent_id) {
            parent.child_job_ids.retain(|id| id != child_id);
            if status == JobStatus::Succeeded {
                parent.succeeded_children.push(child_id.clone());
            } else {
                parent.failed_children.push(child_id.clone());
            }
        }
    }

    /// Starts jobs that were waiting for concurrency permits.
    ///
    /// Iterates the priority-ordered heap (highest priority first) and tries
    /// each job's concurrency group. Jobs whose group is full are set aside
    /// and re-added after the pass. This ensures:
    /// 1. ON_DEMAND jobs are tried before PREFETCH (priority ordering)
    /// 2. A full group doesn't block jobs in OTHER groups (skip-and-continue)
    pub(crate) async fn start_waiting_jobs(&mut self) {
        let mut blocked = Vec::new();

        while let Some(waiting) = self.waiting_for_permit.pop() {
            let group_name = waiting.submitted.job.concurrency_group().map(String::from);

            let Some(group) = group_name else {
                // No concurrency group — start immediately
                self.start_job_with_permit(waiting.submitted, None).await;
                continue;
            };

            match self.config.job_concurrency_limits.try_acquire(&group) {
                Some(permit) => {
                    let job_id = waiting.submitted.job_id.clone();
                    debug!(
                        job_id = %job_id,
                        group = %group,
                        remaining_waiting = self.waiting_for_permit.len() + blocked.len(),
                        "Starting waiting job (acquired permit)"
                    );
                    self.start_job_with_permit(waiting.submitted, Some(permit))
                        .await;
                }
                None => {
                    // Group is full — set aside and try the next job
                    blocked.push(waiting);
                }
            }
        }

        // Re-add blocked jobs back to the heap
        for waiting in blocked {
            self.waiting_for_permit.push(waiting);
        }
    }

    /// Starts a job with an optional concurrency permit.
    pub(crate) async fn start_job_with_permit(
        &mut self,
        submitted: SubmittedJob,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let job_id = submitted.job_id.clone();

        let mut active = ActiveJob::new(submitted);
        active.concurrency_permit = permit;
        active.pending_tasks = active.job.create_tasks();
        active.update_status(JobStatus::Running);

        self.telemetry.emit(TelemetryEvent::JobStarted {
            job_id: job_id.clone(),
        });

        let total_tasks = active.pending_tasks.len();
        info!(job_id = %job_id, task_count = total_tasks, "Job started");

        if !active.pending_tasks.is_empty() {
            let first_task = active.pending_tasks.remove(0);
            let task_name = first_task.name().to_string();
            let priority = active.priority;
            let queued = QueuedTask::new(first_task, job_id.clone(), priority);

            self.telemetry.emit(TelemetryEvent::TaskEnqueued {
                job_id: job_id.clone(),
                task_name,
                priority,
                queue_depth: 0,
            });

            let mut queue = self.task_queue.lock().await;
            queue.push(queued);
        }

        let mut jobs = self.active_jobs.lock().await;
        jobs.insert(job_id, active);

        self.work_notify.notify_one();
    }

    /// Collects child jobs spawned by tasks.
    pub(crate) async fn collect_child_jobs(&mut self) {
        let new_children = self.gather_spawned_children().await;

        for (child_job, parent_id) in new_children {
            self.start_child_job(child_job, parent_id).await;
        }
    }

    /// Gathers all spawned child jobs from active jobs.
    async fn gather_spawned_children(&self) -> Vec<(Box<dyn super::job::Job>, JobId)> {
        let mut new_children = Vec::new();
        let mut jobs = self.active_jobs.lock().await;

        for (parent_id, job) in jobs.iter_mut() {
            while let Ok(spawned) = job.child_job_rx.try_recv() {
                let child_id = spawned.job.id();
                job.child_job_ids.push(child_id.clone());

                self.telemetry.emit(TelemetryEvent::ChildJobSpawned {
                    parent_job_id: parent_id.clone(),
                    child_job_id: child_id,
                    task_name: spawned.spawning_task,
                });

                new_children.push((spawned.job, parent_id.clone()));
            }
        }

        new_children
    }

    /// Starts a child job with parent relationship.
    async fn start_child_job(&mut self, child_job: Box<dyn super::job::Job>, parent_id: JobId) {
        let child_id = child_job.id();
        let priority = child_job.priority();
        let name = child_job.name().to_string();

        let (status_tx, _status_rx) = watch::channel(JobStatus::Pending);
        let (_signal_tx, signal_rx) = mpsc::channel(DEFAULT_SIGNAL_CHANNEL_CAPACITY);
        let result_holder = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let submitted = SubmittedJob {
            job: child_job,
            job_id: child_id.clone(),
            name,
            priority,
            status_tx,
            signal_rx,
            result_holder,
        };

        let mut active = ActiveJob::with_parent(submitted, parent_id);
        active.pending_tasks = active.job.create_tasks();
        active.update_status(JobStatus::Running);

        self.telemetry.emit(TelemetryEvent::JobStarted {
            job_id: child_id.clone(),
        });

        {
            let mut queue = self.task_queue.lock().await;
            for task in active.pending_tasks.drain(..) {
                let queued = QueuedTask::new(task, child_id.clone(), priority);
                queue.push(queued);
            }
        }

        let mut jobs = self.active_jobs.lock().await;
        jobs.insert(child_id, active);
    }

    /// Shuts down the executor.
    pub(crate) async fn shutdown(&mut self) {
        let mut jobs = self.active_jobs.lock().await;
        for (job_id, job) in jobs.iter_mut() {
            job.cancellation.cancel();
            job.update_status(JobStatus::Cancelled);

            self.telemetry.emit(TelemetryEvent::JobCompleted {
                job_id: job_id.clone(),
                status: JobStatus::Cancelled,
                duration: job.started_at.elapsed(),
                tasks_succeeded: job.succeeded_tasks.len(),
                tasks_failed: job.failed_tasks.len(),
                children_succeeded: job.succeeded_children.len(),
                children_failed: job.failed_children.len(),
            });
        }
        jobs.clear();

        let mut queue = self.task_queue.lock().await;
        queue.clear();
    }
}
