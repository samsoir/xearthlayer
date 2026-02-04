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
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            resource_pools: ResourcePoolConfig::default(),
            job_channel_capacity: DEFAULT_JOB_CHANNEL_CAPACITY,
            max_concurrent_tasks: DEFAULT_MAX_CONCURRENT_TASKS,
            job_concurrency_limits: JobConcurrencyLimits::new(),
        }
    }
}

impl From<&crate::config::ExecutorSettings> for ExecutorConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            resource_pools: ResourcePoolConfig::from(settings),
            job_channel_capacity: settings.job_channel_capacity,
            max_concurrent_tasks: settings.max_concurrent_tasks,
            job_concurrency_limits: JobConcurrencyLimits::new(),
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
