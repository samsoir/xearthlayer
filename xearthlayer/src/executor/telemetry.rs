//! Telemetry for job execution observability.
//!
//! Jobs and tasks emit telemetry events via a sink abstraction. The executor
//! doesn't know how events are consumed—this follows the "emit, don't present"
//! pattern.
//!
//! # Pattern: Emit, Don't Present
//!
//! Jobs and tasks focus on emitting structured events. Consumers of these events
//! (UI, logging, metrics) decide how to present or aggregate them. This separation
//! keeps the executor simple and allows flexible observability.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{TelemetryEvent, TelemetrySink};
//!
//! struct LoggingSink;
//!
//! impl TelemetrySink for LoggingSink {
//!     fn emit(&self, event: TelemetryEvent) {
//!         tracing::info!(?event, "Job event");
//!     }
//! }
//! ```

use super::handle::{JobStatus, Signal};
use super::job::JobId;
use super::policy::Priority;
use super::resource_pool::ResourceType;
use super::task::TaskResultKind;
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// Telemetry Events
// =============================================================================

/// Events emitted during job/task execution.
///
/// These events provide observability into the executor's behavior without
/// coupling the executor to any specific presentation or storage mechanism.
#[derive(Clone, Debug)]
pub enum TelemetryEvent {
    // -------------------------------------------------------------------------
    // Job Lifecycle Events
    // -------------------------------------------------------------------------
    /// A job was submitted to the executor.
    JobSubmitted {
        job_id: JobId,
        name: String,
        priority: Priority,
    },

    /// A job started executing (dependencies satisfied).
    JobStarted { job_id: JobId },

    /// A job completed execution.
    JobCompleted {
        job_id: JobId,
        status: JobStatus,
        duration: Duration,
        tasks_succeeded: usize,
        tasks_failed: usize,
        children_succeeded: usize,
        children_failed: usize,
    },

    /// A job received a signal.
    JobSignalled { job_id: JobId, signal: Signal },

    // -------------------------------------------------------------------------
    // Task Lifecycle Events
    // -------------------------------------------------------------------------
    /// A task started executing.
    TaskStarted {
        job_id: JobId,
        task_name: String,
        resource_type: ResourceType,
    },

    /// A task completed execution.
    TaskCompleted {
        job_id: JobId,
        task_name: String,
        result: TaskResultKind,
        duration: Duration,
    },

    /// A task is being retried.
    TaskRetrying {
        job_id: JobId,
        task_name: String,
        attempt: u32,
        delay: Duration,
    },

    // -------------------------------------------------------------------------
    // Child Job Events
    // -------------------------------------------------------------------------
    /// A child job was spawned by a task.
    ChildJobSpawned {
        parent_job_id: JobId,
        child_job_id: JobId,
        task_name: String,
    },

    // -------------------------------------------------------------------------
    // Resource Pool Events
    // -------------------------------------------------------------------------
    /// A resource pool has no available permits.
    ResourcePoolExhausted {
        resource_type: ResourceType,
        waiting_tasks: usize,
    },

    /// A resource pool has permits available again.
    ResourcePoolAvailable {
        resource_type: ResourceType,
        available_permits: usize,
    },

    // -------------------------------------------------------------------------
    // Queue Events
    // -------------------------------------------------------------------------
    /// A task was enqueued for execution.
    TaskEnqueued {
        job_id: JobId,
        task_name: String,
        priority: Priority,
        queue_depth: usize,
    },

    /// A task was dequeued for execution.
    TaskDequeued {
        job_id: JobId,
        task_name: String,
        wait_time: Duration,
    },
}

impl TelemetryEvent {
    /// Returns the job ID associated with this event, if any.
    pub fn job_id(&self) -> Option<&JobId> {
        match self {
            Self::JobSubmitted { job_id, .. }
            | Self::JobStarted { job_id }
            | Self::JobCompleted { job_id, .. }
            | Self::JobSignalled { job_id, .. }
            | Self::TaskStarted { job_id, .. }
            | Self::TaskCompleted { job_id, .. }
            | Self::TaskRetrying { job_id, .. }
            | Self::ChildJobSpawned {
                parent_job_id: job_id,
                ..
            }
            | Self::TaskEnqueued { job_id, .. }
            | Self::TaskDequeued { job_id, .. } => Some(job_id),
            Self::ResourcePoolExhausted { .. } | Self::ResourcePoolAvailable { .. } => None,
        }
    }

    /// Returns a short name for this event type.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::JobSubmitted { .. } => "job_submitted",
            Self::JobStarted { .. } => "job_started",
            Self::JobCompleted { .. } => "job_completed",
            Self::JobSignalled { .. } => "job_signalled",
            Self::TaskStarted { .. } => "task_started",
            Self::TaskCompleted { .. } => "task_completed",
            Self::TaskRetrying { .. } => "task_retrying",
            Self::ChildJobSpawned { .. } => "child_job_spawned",
            Self::ResourcePoolExhausted { .. } => "resource_pool_exhausted",
            Self::ResourcePoolAvailable { .. } => "resource_pool_available",
            Self::TaskEnqueued { .. } => "task_enqueued",
            Self::TaskDequeued { .. } => "task_dequeued",
        }
    }
}

// =============================================================================
// Telemetry Sink Trait
// =============================================================================

/// Sink for telemetry events.
///
/// Implement this trait to receive telemetry events from the executor.
/// Common implementations include logging, metrics collection, and UI updates.
///
/// # Thread Safety
///
/// Implementations must be thread-safe (`Send + Sync`) as events may be
/// emitted from multiple tasks concurrently.
pub trait TelemetrySink: Send + Sync {
    /// Called when a telemetry event occurs.
    ///
    /// This method should be fast and non-blocking. For expensive operations
    /// (e.g., network calls), consider buffering events or using async channels.
    fn emit(&self, event: TelemetryEvent);
}

// =============================================================================
// Built-in Sink Implementations
// =============================================================================

/// No-op sink for when telemetry is disabled.
///
/// This is useful for testing or when telemetry overhead is not desired.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullTelemetrySink;

impl TelemetrySink for NullTelemetrySink {
    fn emit(&self, _event: TelemetryEvent) {
        // Intentionally empty
    }
}

/// Sink that logs events using the `tracing` crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingTelemetrySink;

impl TelemetrySink for TracingTelemetrySink {
    fn emit(&self, event: TelemetryEvent) {
        match &event {
            TelemetryEvent::JobSubmitted {
                job_id,
                name,
                priority,
            } => {
                tracing::debug!(
                    job_id = %job_id,
                    name = %name,
                    priority = %priority,
                    "Job submitted"
                );
            }
            TelemetryEvent::JobStarted { job_id } => {
                tracing::debug!(job_id = %job_id, "Job started");
            }
            TelemetryEvent::JobCompleted {
                job_id,
                status,
                duration,
                tasks_succeeded,
                tasks_failed,
                ..
            } => {
                tracing::debug!(
                    job_id = %job_id,
                    status = %status,
                    duration_ms = duration.as_millis(),
                    tasks_succeeded = tasks_succeeded,
                    tasks_failed = tasks_failed,
                    "Job completed"
                );
            }
            TelemetryEvent::JobSignalled { job_id, signal } => {
                tracing::debug!(
                    job_id = %job_id,
                    signal = %signal,
                    "Job signalled"
                );
            }
            TelemetryEvent::TaskStarted {
                job_id,
                task_name,
                resource_type,
            } => {
                tracing::debug!(
                    job_id = %job_id,
                    task = %task_name,
                    resource = %resource_type,
                    "Task started"
                );
            }
            TelemetryEvent::TaskCompleted {
                job_id,
                task_name,
                result,
                duration,
            } => {
                tracing::debug!(
                    job_id = %job_id,
                    task = %task_name,
                    result = ?result,
                    duration_ms = duration.as_millis(),
                    "Task completed"
                );
            }
            TelemetryEvent::TaskRetrying {
                job_id,
                task_name,
                attempt,
                delay,
            } => {
                tracing::warn!(
                    job_id = %job_id,
                    task = %task_name,
                    attempt = attempt,
                    delay_ms = delay.as_millis(),
                    "Task retrying"
                );
            }
            TelemetryEvent::ChildJobSpawned {
                parent_job_id,
                child_job_id,
                task_name,
            } => {
                tracing::debug!(
                    parent_job_id = %parent_job_id,
                    child_job_id = %child_job_id,
                    task = %task_name,
                    "Child job spawned"
                );
            }
            TelemetryEvent::ResourcePoolExhausted {
                resource_type,
                waiting_tasks,
            } => {
                tracing::warn!(
                    resource = %resource_type,
                    waiting = waiting_tasks,
                    "Resource pool exhausted"
                );
            }
            TelemetryEvent::ResourcePoolAvailable {
                resource_type,
                available_permits,
            } => {
                tracing::trace!(
                    resource = %resource_type,
                    available = available_permits,
                    "Resource pool available"
                );
            }
            TelemetryEvent::TaskEnqueued {
                job_id,
                task_name,
                priority,
                queue_depth,
            } => {
                tracing::trace!(
                    job_id = %job_id,
                    task = %task_name,
                    priority = %priority,
                    queue_depth = queue_depth,
                    "Task enqueued"
                );
            }
            TelemetryEvent::TaskDequeued {
                job_id,
                task_name,
                wait_time,
            } => {
                tracing::trace!(
                    job_id = %job_id,
                    task = %task_name,
                    wait_time_ms = wait_time.as_millis(),
                    "Task dequeued"
                );
            }
        }
    }
}

/// Sink that forwards events to multiple sinks.
pub struct MultiplexTelemetrySink {
    sinks: Vec<Arc<dyn TelemetrySink>>,
}

impl MultiplexTelemetrySink {
    /// Creates a new multiplex sink with the given sinks.
    pub fn new(sinks: Vec<Arc<dyn TelemetrySink>>) -> Self {
        Self { sinks }
    }

    /// Adds a sink to the multiplex.
    pub fn add_sink(&mut self, sink: Arc<dyn TelemetrySink>) {
        self.sinks.push(sink);
    }
}

impl TelemetrySink for MultiplexTelemetrySink {
    fn emit(&self, event: TelemetryEvent) {
        for sink in &self.sinks {
            sink.emit(event.clone());
        }
    }
}

impl std::fmt::Debug for MultiplexTelemetrySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexTelemetrySink")
            .field("sink_count", &self.sinks.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_null_sink() {
        let sink = NullTelemetrySink;
        // Should not panic
        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("test"),
        });
    }

    #[test]
    fn test_tracing_sink() {
        let sink = TracingTelemetrySink;
        // Should not panic (logging may or may not be configured)
        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("test"),
        });
    }

    #[test]
    fn test_event_job_id() {
        let job_id = JobId::new("test-job");

        let event = TelemetryEvent::JobStarted {
            job_id: job_id.clone(),
        };
        assert_eq!(event.job_id(), Some(&job_id));

        let event = TelemetryEvent::ResourcePoolExhausted {
            resource_type: ResourceType::CPU,
            waiting_tasks: 5,
        };
        assert_eq!(event.job_id(), None);
    }

    #[test]
    fn test_event_type_names() {
        assert_eq!(
            TelemetryEvent::JobStarted {
                job_id: JobId::new("x")
            }
            .event_type(),
            "job_started"
        );
        assert_eq!(
            TelemetryEvent::TaskCompleted {
                job_id: JobId::new("x"),
                task_name: "y".to_string(),
                result: TaskResultKind::Success,
                duration: Duration::ZERO,
            }
            .event_type(),
            "task_completed"
        );
    }

    #[test]
    fn test_multiplex_sink() {
        struct CountingSink(AtomicUsize);

        impl TelemetrySink for CountingSink {
            fn emit(&self, _event: TelemetryEvent) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let sink1 = Arc::new(CountingSink(AtomicUsize::new(0)));
        let sink2 = Arc::new(CountingSink(AtomicUsize::new(0)));

        let multiplex = MultiplexTelemetrySink::new(vec![
            Arc::clone(&sink1) as Arc<dyn TelemetrySink>,
            Arc::clone(&sink2) as Arc<dyn TelemetrySink>,
        ]);

        multiplex.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("test"),
        });

        assert_eq!(sink1.0.load(Ordering::Relaxed), 1);
        assert_eq!(sink2.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_event_debug() {
        let event = TelemetryEvent::JobSubmitted {
            job_id: JobId::new("test"),
            name: "TestJob".to_string(),
            priority: Priority::ON_DEMAND,
        };
        let debug = format!("{:?}", event);
        assert!(debug.contains("JobSubmitted"));
        assert!(debug.contains("test"));
    }
}
