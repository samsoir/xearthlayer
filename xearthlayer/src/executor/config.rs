//! Executor configuration.
//!
//! This module contains the [`ExecutorConfig`] struct and related constants
//! for configuring the job executor.

use super::concurrency::JobConcurrencyLimits;
use super::resource_pool::ResourcePoolConfig;

// =============================================================================
// Configuration Constants
// =============================================================================

/// Default job receiver channel capacity.
pub const DEFAULT_JOB_CHANNEL_CAPACITY: usize = 256;

/// Default signal channel capacity per job.
pub const DEFAULT_SIGNAL_CHANNEL_CAPACITY: usize = 16;

/// Default maximum concurrent tasks (across all resource types).
pub const DEFAULT_MAX_CONCURRENT_TASKS: usize = 128;

// =============================================================================
// Executor Configuration
// =============================================================================

/// Configuration for the job executor.
#[derive(Clone, Debug)]
pub struct ExecutorConfig {
    /// Resource pool configuration.
    pub resource_pools: ResourcePoolConfig,

    /// Job receiver channel capacity.
    pub job_channel_capacity: usize,

    /// Maximum concurrent tasks the executor will dispatch.
    pub max_concurrent_tasks: usize,

    /// Job concurrency limits by group.
    ///
    /// Jobs can declare a concurrency group via `Job::concurrency_group()`.
    /// This limits how many jobs in that group can run simultaneously,
    /// creating back-pressure for balanced pipeline flow.
    pub job_concurrency_limits: JobConcurrencyLimits,

    /// Optional admission gate. While it reads `true`, the executor admits no
    /// queued job; jobs already admitted run all their tasks. `None` (the
    /// default) means always open. The prefetch lane is gated by demand (#296).
    /// The gate's sender must outlive the executor: once it drops, the gate
    /// keeps its last value, so a gate dropped while paused never reopens.
    pub admission_gate: Option<tokio::sync::watch::Receiver<bool>>,
}

impl ExecutorConfig {
    /// Gates job admission on `gate`: `true` pauses admission.
    pub fn with_admission_gate(mut self, gate: tokio::sync::watch::Receiver<bool>) -> Self {
        self.admission_gate = Some(gate);
        self
    }
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            resource_pools: ResourcePoolConfig::default(),
            job_channel_capacity: DEFAULT_JOB_CHANNEL_CAPACITY,
            max_concurrent_tasks: DEFAULT_MAX_CONCURRENT_TASKS,
            job_concurrency_limits: JobConcurrencyLimits::new(),
            admission_gate: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_config_default() {
        let config = ExecutorConfig::default();
        assert_eq!(config.job_channel_capacity, DEFAULT_JOB_CHANNEL_CAPACITY);
        assert_eq!(config.max_concurrent_tasks, DEFAULT_MAX_CONCURRENT_TASKS);
    }

    #[test]
    fn test_executor_config_clone() {
        let config = ExecutorConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.job_channel_capacity, config.job_channel_capacity);
    }
}
