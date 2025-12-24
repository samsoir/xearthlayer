//! Pipeline Control Plane for job tracking, health monitoring, and recovery.
//!
//! The control plane is the **single authority** for all compute resources,
//! managing both DDS handler tasks and pipeline workers uniformly.
//!
//! # Responsibilities
//!
//! - **Job-level concurrency limiting** (H-005): Prevents unbounded tile starts
//! - **Job registry**: Tracks all active jobs with stage progression
//! - **Health monitoring** (H-003): Detects stalled jobs
//! - **Active recovery** (H-006): Cancels stuck jobs and notifies waiters
//! - **Unified work submission**: All work goes through `submit()`
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                        DDS Handler                               │
//! │  (Decides what work is needed, submits to control plane)        │
//! └─────────────────────────────────────────────────────────────────┘
//!                               │
//!                               │ control_plane.submit(work)
//!                               ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    Pipeline Control Plane                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Job Registry    │  Health Monitor   │  Pool Manager             │
//! │  ────────────    │  ──────────────   │  ────────────             │
//! │  • DashMap       │  • 5s interval    │  • Job limiter            │
//! │  • Stage track   │  • Stall detect   │  • HTTP limiter           │
//! │  • Timestamps    │  • Recovery       │  • CPU limiter            │
//! └─────────────────────────────────────────────────────────────────┘
//!                               │
//!                               │ executes with StageObserver
//!                               ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    Worker (Processor)                            │
//! │  (Pure work unit - no knowledge of control plane)               │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Stage Observer Pattern
//!
//! Workers report stage transitions via the [`StageObserver`] trait.
//! This allows workers to emit events without knowing who's listening:
//!
//! ```ignore
//! // Worker calls observer without knowing implementation
//! observer.on_stage_change(job_id, JobStage::Downloading);
//!
//! // Control plane implements StageObserver to track progress
//! impl StageObserver for ControlPlaneObserver {
//!     fn on_stage_change(&self, job_id: JobId, stage: JobStage) {
//!         self.registry.update_stage(job_id, stage);
//!     }
//! }
//! ```
//!
//! # Design Decisions
//!
//! - Control plane is **always enabled** (not optional)
//! - All work units are **equal** - DDS handlers and workers treated uniformly
//! - Default `max_concurrent_jobs` = `num_cpus × 2`
//! - Stage tracking via **callbacks** - workers emit, control plane observes
//! - Uses lock-free data structures (DashMap, atomics)
//! - Minimal overhead on hot path

mod health;
mod job_registry;
mod service;

use crate::pipeline::JobId;

pub use health::{ControlPlaneHealth, HealthSnapshot, HealthStatus};
pub use job_registry::{JobEntry, JobRegistry, JobStage, RegistryStats};
pub use service::{
    ControlPlaneConfig, ControlPlaneObserver, JobSlotError, PipelineControlPlane, SubmitResult,
};

/// Trait for observing pipeline stage transitions.
///
/// Workers call methods on this trait to report stage changes without
/// knowing the implementation. This enables clean separation between
/// the work execution (processor) and work management (control plane).
///
/// # Example
///
/// ```ignore
/// async fn process_tile(
///     observer: Option<Arc<dyn StageObserver>>,
///     // ... other params
/// ) {
///     // Report stage change - observer might be control plane or a mock
///     if let Some(obs) = &observer {
///         obs.on_stage_change(job_id, JobStage::Downloading);
///     }
///
///     // Do the actual work...
/// }
/// ```
pub trait StageObserver: Send + Sync {
    /// Called when a job transitions to a new stage.
    fn on_stage_change(&self, job_id: JobId, stage: JobStage);
}
