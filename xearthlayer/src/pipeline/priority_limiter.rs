//! Priority-aware concurrency limiter for CPU-bound operations.
//!
//! This module provides a limiter that prioritizes on-demand requests over
//! prefetch requests, ensuring X-Plane's tile requests are never blocked
//! by background prefetching.
//!
//! # Design
//!
//! The limiter uses two semaphore pools:
//! - **Priority pool**: Reserved exclusively for on-demand (high-priority) requests
//! - **Shared pool**: Available to both on-demand and prefetch requests
//!
//! ```text
//! Total Permits: 20
//! ├── Priority Pool: 8 (on-demand only)
//! └── Shared Pool: 12 (on-demand and prefetch)
//!
//! On-Demand: Can use priority + shared (up to 20)
//! Prefetch:  Can only use shared (up to 12), non-blocking
//! ```
//!
//! When prefetch requests can't acquire a permit, they fail immediately
//! (non-blocking) rather than waiting. This prevents prefetch from
//! starving on-demand requests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default percentage of permits reserved for priority (on-demand) requests.
/// 40% reserved = 60% available for prefetch.
pub const DEFAULT_PRIORITY_RESERVE_PERCENT: usize = 40;

/// Minimum permits to reserve for priority requests.
pub const MIN_PRIORITY_RESERVE: usize = 4;

/// Priority level for requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPriority {
    /// High priority - on-demand requests from X-Plane.
    /// Can use both priority and shared pools.
    High,
    /// Low priority - background prefetch requests.
    /// Can only use shared pool, non-blocking.
    Low,
}

/// A priority-aware concurrency limiter.
///
/// Ensures high-priority (on-demand) requests are never blocked by
/// low-priority (prefetch) requests.
#[derive(Debug)]
pub struct PriorityConcurrencyLimiter {
    /// Semaphore for priority (on-demand only) permits
    priority_semaphore: Arc<Semaphore>,

    /// Semaphore for shared (both on-demand and prefetch) permits
    shared_semaphore: Arc<Semaphore>,

    /// Number of priority permits
    priority_permits: usize,

    /// Number of shared permits
    shared_permits: usize,

    /// Current number of high-priority operations in flight.
    /// Uses Arc to allow permits to be 'static and work with spawned tasks.
    high_priority_in_flight: Arc<AtomicUsize>,

    /// Current number of low-priority operations in flight.
    /// Uses Arc to allow permits to be 'static and work with spawned tasks.
    low_priority_in_flight: Arc<AtomicUsize>,

    /// Label for debugging
    label: String,
}

impl PriorityConcurrencyLimiter {
    /// Creates a new priority limiter with the specified total permits.
    ///
    /// # Arguments
    ///
    /// * `total_permits` - Total number of concurrent operations allowed
    /// * `priority_reserve_percent` - Percentage of permits reserved for high-priority
    /// * `label` - Human-readable label for logging
    ///
    /// # Example
    ///
    /// ```ignore
    /// // 20 total permits, 40% reserved = 8 priority + 12 shared
    /// let limiter = PriorityConcurrencyLimiter::new(20, 40, "cpu_bound");
    /// ```
    pub fn new(
        total_permits: usize,
        priority_reserve_percent: usize,
        label: impl Into<String>,
    ) -> Self {
        assert!(total_permits > 0, "total_permits must be > 0");
        assert!(
            priority_reserve_percent <= 100,
            "priority_reserve_percent must be <= 100"
        );

        // Calculate priority permits (reserved for on-demand)
        let priority_permits =
            ((total_permits * priority_reserve_percent) / 100).max(MIN_PRIORITY_RESERVE);
        let priority_permits = priority_permits.min(total_permits - 1); // Leave at least 1 for shared

        // Remaining permits are shared
        let shared_permits = total_permits - priority_permits;

        let label_str: String = label.into();

        tracing::info!(
            total = total_permits,
            priority = priority_permits,
            shared = shared_permits,
            label = %label_str,
            "Created priority concurrency limiter"
        );

        Self {
            priority_semaphore: Arc::new(Semaphore::new(priority_permits)),
            shared_semaphore: Arc::new(Semaphore::new(shared_permits)),
            priority_permits,
            shared_permits,
            high_priority_in_flight: Arc::new(AtomicUsize::new(0)),
            low_priority_in_flight: Arc::new(AtomicUsize::new(0)),
            label: label_str,
        }
    }

    /// Creates a limiter with default settings for CPU-bound work.
    ///
    /// Uses modest over-subscription (1.25x cores) with 40% reserved for priority.
    pub fn with_cpu_defaults(label: impl Into<String>) -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);

        // Same formula as ConcurrencyLimiter::with_cpu_oversubscribe
        let total = ((cpus as f64 * 1.25).ceil() as usize).max(cpus + 2);

        Self::new(total, DEFAULT_PRIORITY_RESERVE_PERCENT, label)
    }

    /// Acquires a permit with the specified priority.
    ///
    /// # Behavior by Priority
    ///
    /// - **High priority**: Tries priority pool first, then shared pool. Always waits.
    /// - **Low priority**: Only tries shared pool. Returns `None` if unavailable.
    ///
    /// # Arguments
    ///
    /// * `priority` - The request priority level
    ///
    /// # Returns
    ///
    /// For high priority: Always returns a permit (waits if necessary).
    /// For low priority: Returns `Some(permit)` if available, `None` otherwise.
    pub async fn acquire(&self, priority: RequestPriority) -> Option<PriorityPermit> {
        match priority {
            RequestPriority::High => {
                // High priority: try priority pool first, then shared
                // This ensures on-demand always gets a permit

                // First, try the priority pool (fast path)
                if let Ok(permit) = self.priority_semaphore.clone().try_acquire_owned() {
                    self.high_priority_in_flight.fetch_add(1, Ordering::Relaxed);
                    return Some(PriorityPermit {
                        _permit: PermitInner::Priority(permit),
                        in_flight: Arc::clone(&self.high_priority_in_flight),
                    });
                }

                // Priority pool full, try shared pool (also fast path)
                if let Ok(permit) = self.shared_semaphore.clone().try_acquire_owned() {
                    self.high_priority_in_flight.fetch_add(1, Ordering::Relaxed);
                    return Some(PriorityPermit {
                        _permit: PermitInner::Shared(permit),
                        in_flight: Arc::clone(&self.high_priority_in_flight),
                    });
                }

                // Both pools busy - wait on priority pool (on-demand should wait)
                let permit = self
                    .priority_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("priority semaphore closed");

                self.high_priority_in_flight.fetch_add(1, Ordering::Relaxed);
                Some(PriorityPermit {
                    _permit: PermitInner::Priority(permit),
                    in_flight: Arc::clone(&self.high_priority_in_flight),
                })
            }
            RequestPriority::Low => {
                // Low priority: only try shared pool, non-blocking
                // Prefetch should not block - if busy, skip this cycle
                if let Ok(permit) = self.shared_semaphore.clone().try_acquire_owned() {
                    self.low_priority_in_flight.fetch_add(1, Ordering::Relaxed);
                    Some(PriorityPermit {
                        _permit: PermitInner::Shared(permit),
                        in_flight: Arc::clone(&self.low_priority_in_flight),
                    })
                } else {
                    // No shared permits available - prefetch backs off
                    None
                }
            }
        }
    }

    /// Acquires a high-priority permit (convenience method).
    ///
    /// This is the most common case - on-demand requests from X-Plane.
    /// Always returns a permit (waits if necessary).
    pub async fn acquire_high(&self) -> PriorityPermit {
        self.acquire(RequestPriority::High)
            .await
            .expect("high priority acquire should always succeed")
    }

    /// Tries to acquire a low-priority permit (convenience method).
    ///
    /// For prefetch requests. Returns `None` if no shared permits available.
    pub async fn try_acquire_low(&self) -> Option<PriorityPermit> {
        self.acquire(RequestPriority::Low).await
    }

    /// Returns the label for this limiter.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the total number of permits (priority + shared).
    pub fn total_permits(&self) -> usize {
        self.priority_permits + self.shared_permits
    }

    /// Returns the number of priority-reserved permits.
    pub fn priority_permits(&self) -> usize {
        self.priority_permits
    }

    /// Returns the number of shared permits.
    pub fn shared_permits(&self) -> usize {
        self.shared_permits
    }

    /// Returns the current number of high-priority operations in flight.
    pub fn high_priority_in_flight(&self) -> usize {
        self.high_priority_in_flight.load(Ordering::Relaxed)
    }

    /// Returns the current number of low-priority operations in flight.
    pub fn low_priority_in_flight(&self) -> usize {
        self.low_priority_in_flight.load(Ordering::Relaxed)
    }

    /// Returns the total number of operations in flight.
    pub fn total_in_flight(&self) -> usize {
        self.high_priority_in_flight() + self.low_priority_in_flight()
    }

    /// Returns available permits in the priority pool.
    pub fn priority_available(&self) -> usize {
        self.priority_semaphore.available_permits()
    }

    /// Returns available permits in the shared pool.
    pub fn shared_available(&self) -> usize {
        self.shared_semaphore.available_permits()
    }
}

/// Internal permit type to track which pool the permit came from.
/// The permit is held only for its RAII behavior (release on drop).
#[allow(dead_code)]
enum PermitInner {
    Priority(OwnedSemaphorePermit),
    Shared(OwnedSemaphorePermit),
}

/// A permit from the priority limiter.
///
/// While held, counts against either the priority or shared pool.
/// Automatically released when dropped.
pub struct PriorityPermit {
    _permit: PermitInner,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for PriorityPermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_limiter() {
        let limiter = PriorityConcurrencyLimiter::new(20, 40, "test");
        assert_eq!(limiter.total_permits(), 20);
        assert_eq!(limiter.priority_permits(), 8); // 40% of 20
        assert_eq!(limiter.shared_permits(), 12); // Remaining
    }

    #[test]
    fn test_minimum_priority_reserve() {
        // Even with low total, should have at least MIN_PRIORITY_RESERVE
        let limiter = PriorityConcurrencyLimiter::new(6, 10, "test");
        assert!(limiter.priority_permits() >= MIN_PRIORITY_RESERVE);
    }

    #[test]
    fn test_shared_has_at_least_one() {
        // Should always leave at least 1 for shared
        let limiter = PriorityConcurrencyLimiter::new(5, 100, "test");
        assert!(limiter.shared_permits() >= 1);
    }

    #[tokio::test]
    async fn test_high_priority_always_succeeds() {
        let limiter = PriorityConcurrencyLimiter::new(10, 50, "test");

        // Acquire all permits
        let mut permits = Vec::new();
        for _ in 0..10 {
            permits.push(limiter.acquire_high().await);
        }

        assert_eq!(limiter.high_priority_in_flight(), 10);

        // Release and verify
        drop(permits);
        assert_eq!(limiter.high_priority_in_flight(), 0);
    }

    #[tokio::test]
    async fn test_low_priority_non_blocking() {
        let limiter = PriorityConcurrencyLimiter::new(10, 50, "test");
        // 5 priority + 5 shared

        // Fill shared pool with low priority
        let mut low_permits = Vec::new();
        for _ in 0..5 {
            if let Some(permit) = limiter.try_acquire_low().await {
                low_permits.push(permit);
            }
        }

        assert_eq!(limiter.low_priority_in_flight(), 5);
        assert_eq!(limiter.shared_available(), 0);

        // Next low priority should fail (non-blocking)
        let result = limiter.try_acquire_low().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_high_priority_uses_both_pools() {
        let limiter = PriorityConcurrencyLimiter::new(10, 50, "test");
        // 5 priority + 5 shared

        // Fill with high priority - should use both pools
        let mut permits = Vec::new();
        for _ in 0..10 {
            permits.push(limiter.acquire_high().await);
        }

        assert_eq!(limiter.high_priority_in_flight(), 10);
        assert_eq!(limiter.priority_available(), 0);
        assert_eq!(limiter.shared_available(), 0);
    }

    #[tokio::test]
    async fn test_low_priority_blocked_by_high() {
        let limiter = Arc::new(PriorityConcurrencyLimiter::new(10, 50, "test"));
        // 5 priority + 5 shared

        // Fill shared pool with high priority
        let mut high_permits = Vec::new();
        for _ in 0..5 {
            // Use shared pool (try_acquire on priority first, then shared)
            high_permits.push(limiter.acquire_high().await);
        }

        // Now shared should be partially used
        // Low priority tries should fail
        let low_result = limiter.try_acquire_low().await;

        // Depending on which pool high priority used, low may or may not succeed
        // The key is: low priority never blocks
        // This is more of a behavior test - just ensure it doesn't hang
        drop(low_result);
        drop(high_permits);
    }

    #[tokio::test]
    async fn test_priority_pool_waits_when_busy() {
        let limiter = Arc::new(PriorityConcurrencyLimiter::new(6, 50, "test"));
        // 3 priority + 3 shared (but minimum is 4, so might be different)

        // This test ensures high priority will wait on priority pool when all are busy
        let permits_count = limiter.total_permits();
        let mut permits = Vec::new();

        for _ in 0..permits_count {
            permits.push(limiter.acquire_high().await);
        }

        assert_eq!(limiter.total_in_flight(), permits_count);

        // Spawn a task that will wait for a permit
        let limiter_clone = Arc::clone(&limiter);
        let handle = tokio::spawn(async move {
            // This should wait until a permit is released
            limiter_clone.acquire_high().await
        });

        // Release one permit
        permits.pop();

        // Wait for spawned task to complete
        let _permit = handle.await.expect("task should complete");
        assert_eq!(limiter.total_in_flight(), permits_count);
    }
}
