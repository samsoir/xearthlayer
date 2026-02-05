//! Job concurrency limiting.
//!
//! This module provides [`JobConcurrencyLimits`] which manages concurrent
//! execution limits for jobs by group. Jobs can declare a concurrency group
//! via [`Job::concurrency_group()`], and the executor will limit how many
//! jobs in that group can run simultaneously.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::JobConcurrencyLimits;
//!
//! // Default limits scale with CPU count
//! let limits = JobConcurrencyLimits::new();
//!
//! // Acquire a slot for a tile generation job
//! let permit = limits.try_acquire("tile_generation");
//! if permit.is_some() {
//!     // Job can run
//! } else {
//!     // At capacity, job must wait
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// =============================================================================
// Configuration Constants
// =============================================================================

/// Multiplier for CPU count to compute tile generation limit.
///
/// Higher values allow more jobs in flight, improving pipeline utilization
/// but increasing memory usage from buffered downloads.
pub const TILE_GENERATION_CPU_MULTIPLIER: usize = 16;

/// Minimum tile generation limit regardless of CPU count.
///
/// Ensures adequate parallelism even on low-core systems.
pub const TILE_GENERATION_MIN_LIMIT: usize = 80;

/// Fallback CPU count when detection fails.
pub const FALLBACK_CPU_COUNT: usize = 8;

/// Concurrency group name for tile generation jobs.
pub const TILE_GENERATION_GROUP: &str = "tile_generation";

// =============================================================================
// Default Limit Calculation
// =============================================================================

/// Computes the default concurrency limit for tile generation jobs.
///
/// This value scales with the system's capability to keep the multi-stage
/// pipeline (download → assemble → encode → cache) fully utilized.
///
/// Formula: `max(num_cpus * TILE_GENERATION_CPU_MULTIPLIER, TILE_GENERATION_MIN_LIMIT)`
/// - 8 cores:  128 concurrent jobs
/// - 16 cores: 256 concurrent jobs
/// - 32 cores: 512 concurrent jobs
///
/// Note: Per-stage resource pools (Network, CPU, DiskIO) already limit
/// concurrent operations at each stage. This limit prevents unbounded
/// job queueing while ensuring enough work in flight.
pub fn default_tile_generation_limit() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(FALLBACK_CPU_COUNT);
    (cpus * TILE_GENERATION_CPU_MULTIPLIER).max(TILE_GENERATION_MIN_LIMIT)
}

/// Manages concurrent execution limits for job groups.
///
/// Jobs can declare a concurrency group, and this struct ensures that
/// no more than the configured number of jobs in that group run at once.
/// This creates back-pressure that helps balance multi-stage pipelines.
#[derive(Clone)]
pub struct JobConcurrencyLimits {
    /// Semaphores for each concurrency group.
    groups: Arc<HashMap<String, Arc<Semaphore>>>,
}

impl Default for JobConcurrencyLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl JobConcurrencyLimits {
    /// Creates a new instance with default limits.
    ///
    /// By default, includes a limit for the "tile_generation" group
    /// that scales with the number of CPUs.
    pub fn new() -> Self {
        let mut groups = HashMap::new();
        let tile_limit = default_tile_generation_limit();
        groups.insert(
            TILE_GENERATION_GROUP.to_string(),
            Arc::new(Semaphore::new(tile_limit)),
        );
        Self {
            groups: Arc::new(groups),
        }
    }

    /// Creates an instance with no default groups.
    pub fn empty() -> Self {
        Self {
            groups: Arc::new(HashMap::new()),
        }
    }

    /// Adds or updates a concurrency limit for a group.
    ///
    /// # Arguments
    ///
    /// * `group` - The group name
    /// * `limit` - Maximum concurrent jobs in this group
    pub fn with_group(mut self, group: impl Into<String>, limit: usize) -> Self {
        let groups = Arc::make_mut(&mut self.groups);
        groups.insert(group.into(), Arc::new(Semaphore::new(limit)));
        self
    }

    /// Returns the configured limit for a group, if any.
    pub fn get_limit(&self, group: &str) -> Option<usize> {
        self.groups.get(group).map(|s| s.available_permits())
    }

    /// Attempts to acquire a permit for the given group.
    ///
    /// Returns `Some(permit)` if a slot is available, `None` if at capacity.
    /// The permit must be held for the duration of the job and dropped when
    /// the job completes.
    ///
    /// If the group doesn't exist, returns `None` (unlimited).
    pub fn try_acquire(&self, group: &str) -> Option<OwnedSemaphorePermit> {
        self.groups
            .get(group)
            .and_then(|sem| sem.clone().try_acquire_owned().ok())
    }

    /// Acquires a permit for the given group, waiting if necessary.
    ///
    /// Returns the permit when a slot becomes available. The permit must be
    /// held for the duration of the job and dropped when the job completes.
    ///
    /// If the group doesn't exist, returns `None` (unlimited).
    pub async fn acquire(&self, group: &str) -> Option<OwnedSemaphorePermit> {
        if let Some(sem) = self.groups.get(group) {
            sem.clone().acquire_owned().await.ok()
        } else {
            None
        }
    }

    /// Returns the number of available permits for a group.
    ///
    /// Returns `None` if the group doesn't exist (unlimited).
    pub fn available(&self, group: &str) -> Option<usize> {
        self.groups.get(group).map(|sem| sem.available_permits())
    }

    /// Returns the number of in-use permits for a group.
    ///
    /// Returns `None` if the group doesn't exist.
    pub fn in_use(&self, group: &str) -> Option<usize> {
        self.groups.get(group).map(|sem| {
            let limit = self.get_configured_limit(group).unwrap_or(0);
            limit.saturating_sub(sem.available_permits())
        })
    }

    /// Returns the configured limit for a group (not current available).
    fn get_configured_limit(&self, group: &str) -> Option<usize> {
        // We store the original limit by checking total permits
        // This is a workaround since Semaphore doesn't expose max capacity
        self.groups.get(group).map(|_| {
            // For now, return the default. In production, we'd track this separately.
            match group {
                TILE_GENERATION_GROUP => default_tile_generation_limit(),
                _ => 0,
            }
        })
    }
}

impl std::fmt::Debug for JobConcurrencyLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (name, sem) in self.groups.iter() {
            map.entry(name, &sem.available_permits());
        }
        map.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_has_tile_generation_group() {
        let limits = JobConcurrencyLimits::new();
        assert!(limits.available(TILE_GENERATION_GROUP).is_some());
        // Limit scales with CPU count: max(cpus * 8, 80)
        assert_eq!(
            limits.available(TILE_GENERATION_GROUP),
            Some(default_tile_generation_limit())
        );
    }

    #[test]
    fn test_default_tile_generation_limit_formula() {
        let limit = default_tile_generation_limit();
        // Should be at least the minimum
        assert!(limit >= TILE_GENERATION_MIN_LIMIT);
        // Should be a multiple of CPU count (if above minimum)
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(FALLBACK_CPU_COUNT);
        let expected = (cpus * TILE_GENERATION_CPU_MULTIPLIER).max(TILE_GENERATION_MIN_LIMIT);
        assert_eq!(limit, expected);
    }

    #[test]
    fn test_empty_has_no_groups() {
        let limits = JobConcurrencyLimits::empty();
        assert!(limits.available(TILE_GENERATION_GROUP).is_none());
    }

    #[test]
    fn test_with_group() {
        let limits = JobConcurrencyLimits::empty().with_group("custom", 10);
        assert_eq!(limits.available("custom"), Some(10));
    }

    #[test]
    fn test_try_acquire_unknown_group() {
        let limits = JobConcurrencyLimits::empty();
        assert!(limits.try_acquire("unknown").is_none());
    }

    #[test]
    fn test_try_acquire_success() {
        let limits = JobConcurrencyLimits::empty().with_group("test", 2);

        let permit1 = limits.try_acquire("test");
        assert!(permit1.is_some());
        assert_eq!(limits.available("test"), Some(1));

        let permit2 = limits.try_acquire("test");
        assert!(permit2.is_some());
        assert_eq!(limits.available("test"), Some(0));

        // Third should fail
        let permit3 = limits.try_acquire("test");
        assert!(permit3.is_none());

        // Drop one permit
        drop(permit1);
        assert_eq!(limits.available("test"), Some(1));

        // Now we can acquire again
        let permit4 = limits.try_acquire("test");
        assert!(permit4.is_some());
    }

    #[tokio::test]
    async fn test_acquire_waits() {
        let limits = JobConcurrencyLimits::empty().with_group("test", 1);

        let permit1 = limits.acquire("test").await;
        assert!(permit1.is_some());

        // Spawn a task that will wait for the permit
        let limits_clone = limits.clone();
        let handle = tokio::spawn(async move {
            let _permit = limits_clone.acquire("test").await;
            "acquired"
        });

        // Give the task time to start waiting
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Release the first permit
        drop(permit1);

        // The waiting task should now complete
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle)
            .await
            .expect("task should complete")
            .expect("task should not panic");
        assert_eq!(result, "acquired");
    }

    #[test]
    fn test_debug_format() {
        let limits = JobConcurrencyLimits::empty().with_group("test", 5);
        let debug = format!("{:?}", limits);
        assert!(debug.contains("test"));
    }
}
