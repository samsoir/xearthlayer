//! Generic concurrency limiter for I/O operations.
//!
//! This module provides a configurable semaphore-based limiter that can be used
//! to constrain concurrent operations for any resource type (HTTP, disk I/O, etc.).
//!
//! # Scaling Formula
//!
//! The default concurrency is calculated as:
//! ```text
//! min(num_cpus * scaling_factor, ceiling)
//! ```
//!
//! Default values:
//! - Scaling factor: 16
//! - Ceiling: 256
//!
//! # Usage
//!
//! ```ignore
//! use std::sync::Arc;
//! use xearthlayer::pipeline::ConcurrencyLimiter;
//!
//! // Create limiter with default scaling (num_cpus * 16, max 256)
//! let limiter = Arc::new(ConcurrencyLimiter::with_defaults());
//!
//! // Or with custom scaling
//! let limiter = Arc::new(ConcurrencyLimiter::with_scaling(8, 128));
//!
//! // Acquire permit before I/O operation
//! async fn do_io(limiter: Arc<ConcurrencyLimiter>) {
//!     let _permit = limiter.acquire().await;
//!     // I/O operation happens here...
//!     // permit is released when _permit goes out of scope
//! }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default scaling factor for calculating max concurrency.
/// Formula: `num_cpus * SCALING_FACTOR`
pub const DEFAULT_SCALING_FACTOR: usize = 16;

/// Default ceiling for max concurrency.
/// The calculated concurrency will not exceed this value.
pub const DEFAULT_CEILING: usize = 256;

/// Generic concurrency limiter for I/O operations.
///
/// Wraps a Tokio semaphore to limit the total number of concurrent operations.
/// This prevents resource exhaustion (file descriptors, network connections, etc.)
/// under heavy load.
#[derive(Debug)]
pub struct ConcurrencyLimiter {
    /// Semaphore controlling concurrent operations
    semaphore: Arc<Semaphore>,

    /// Maximum permits (for stats/debugging)
    max_permits: usize,

    /// Current number of in-flight operations (for metrics)
    in_flight: AtomicUsize,

    /// Peak concurrent operations observed (for tuning)
    peak_in_flight: AtomicUsize,

    /// Label for this limiter (e.g., "http", "disk_io")
    label: String,
}

impl ConcurrencyLimiter {
    /// Creates a new limiter with the specified maximum concurrent operations.
    ///
    /// # Arguments
    ///
    /// * `max_concurrent` - Maximum number of concurrent operations allowed
    /// * `label` - Human-readable label for logging/debugging
    ///
    /// # Panics
    ///
    /// Panics if `max_concurrent` is 0.
    pub fn new(max_concurrent: usize, label: impl Into<String>) -> Self {
        assert!(max_concurrent > 0, "max_concurrent must be > 0");

        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_permits: max_concurrent,
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            label: label.into(),
        }
    }

    /// Creates a limiter with default scaling: `min(num_cpus * 16, 256)`.
    ///
    /// # Arguments
    ///
    /// * `label` - Human-readable label for logging/debugging
    pub fn with_defaults(label: impl Into<String>) -> Self {
        Self::with_scaling(DEFAULT_SCALING_FACTOR, DEFAULT_CEILING, label)
    }

    /// Creates a limiter with custom scaling parameters.
    ///
    /// The maximum concurrency is calculated as:
    /// ```text
    /// min(num_cpus * scaling_factor, ceiling)
    /// ```
    ///
    /// # Arguments
    ///
    /// * `scaling_factor` - Multiplier for CPU count
    /// * `ceiling` - Maximum cap for concurrency
    /// * `label` - Human-readable label for logging/debugging
    ///
    /// # Panics
    ///
    /// Panics if the calculated concurrency would be 0.
    pub fn with_scaling(scaling_factor: usize, ceiling: usize, label: impl Into<String>) -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);

        let max_concurrent = (cpus * scaling_factor).min(ceiling).max(1);
        Self::new(max_concurrent, label)
    }

    /// Acquires a permit for an operation.
    ///
    /// This will wait until a permit is available if the maximum concurrent
    /// operations limit has been reached.
    ///
    /// The permit is automatically released when dropped.
    pub async fn acquire(&self) -> ConcurrencyPermit<'_> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed unexpectedly");

        // Track in-flight count
        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;

        // Update peak if this is a new high
        self.update_peak(current);

        ConcurrencyPermit {
            _permit: permit,
            in_flight: &self.in_flight,
        }
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self) -> Option<ConcurrencyPermit<'_>> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;

        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_peak(current);

        Some(ConcurrencyPermit {
            _permit: permit,
            in_flight: &self.in_flight,
        })
    }

    /// Updates the peak counter if current exceeds it.
    fn update_peak(&self, current: usize) {
        let mut peak = self.peak_in_flight.load(Ordering::Relaxed);
        while current > peak {
            match self.peak_in_flight.compare_exchange_weak(
                peak,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }
    }

    /// Returns the label for this limiter.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the maximum number of concurrent operations allowed.
    pub fn max_concurrent(&self) -> usize {
        self.max_permits
    }

    /// Returns the current number of in-flight operations.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Returns the peak number of concurrent operations observed.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    /// Returns the number of available permits.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Resets the peak counter (useful for periodic stats).
    pub fn reset_peak(&self) {
        self.peak_in_flight.store(0, Ordering::Relaxed);
    }
}

/// A permit for performing a concurrent operation.
///
/// While this permit is held, it counts against the limiter's concurrency limit.
/// The permit is automatically released when dropped.
pub struct ConcurrencyPermit<'a> {
    _permit: OwnedSemaphorePermit,
    in_flight: &'a AtomicUsize,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_limiter() {
        let limiter = ConcurrencyLimiter::new(128, "test");
        assert_eq!(limiter.max_concurrent(), 128);
        assert_eq!(limiter.in_flight(), 0);
        assert_eq!(limiter.available_permits(), 128);
        assert_eq!(limiter.label(), "test");
    }

    #[test]
    fn test_with_defaults() {
        let limiter = ConcurrencyLimiter::with_defaults("disk_io");
        // Should be between 64 (4 CPUs * 16) and 256 (cap)
        assert!(limiter.max_concurrent() >= 64);
        assert!(limiter.max_concurrent() <= 256);
        assert_eq!(limiter.label(), "disk_io");
    }

    #[test]
    fn test_with_scaling() {
        let limiter = ConcurrencyLimiter::with_scaling(8, 64, "custom");
        // With 4+ CPUs, should hit ceiling of 64
        // With fewer CPUs, should be cpus * 8
        assert!(limiter.max_concurrent() <= 64);
        assert!(limiter.max_concurrent() >= 8); // At least 1 CPU * 8
    }

    #[test]
    fn test_scaling_ceiling() {
        // Very high scaling factor should be capped at ceiling
        let limiter = ConcurrencyLimiter::with_scaling(1000, 50, "capped");
        assert_eq!(limiter.max_concurrent(), 50);
    }

    #[test]
    #[should_panic(expected = "max_concurrent must be > 0")]
    fn test_zero_concurrency_panics() {
        ConcurrencyLimiter::new(0, "test");
    }

    #[tokio::test]
    async fn test_acquire_releases_on_drop() {
        let limiter = ConcurrencyLimiter::new(2, "test");

        assert_eq!(limiter.available_permits(), 2);
        assert_eq!(limiter.in_flight(), 0);

        {
            let _permit1 = limiter.acquire().await;
            assert_eq!(limiter.available_permits(), 1);
            assert_eq!(limiter.in_flight(), 1);

            {
                let _permit2 = limiter.acquire().await;
                assert_eq!(limiter.available_permits(), 0);
                assert_eq!(limiter.in_flight(), 2);
            }

            // permit2 dropped
            assert_eq!(limiter.available_permits(), 1);
            assert_eq!(limiter.in_flight(), 1);
        }

        // permit1 dropped
        assert_eq!(limiter.available_permits(), 2);
        assert_eq!(limiter.in_flight(), 0);
    }

    #[tokio::test]
    async fn test_try_acquire() {
        let limiter = ConcurrencyLimiter::new(1, "test");

        let permit1 = limiter.try_acquire();
        assert!(permit1.is_some());
        assert_eq!(limiter.in_flight(), 1);

        // Second try should fail (no permits available)
        let permit2 = limiter.try_acquire();
        assert!(permit2.is_none());

        drop(permit1);
        assert_eq!(limiter.in_flight(), 0);

        // Now should succeed
        let permit3 = limiter.try_acquire();
        assert!(permit3.is_some());
    }

    #[tokio::test]
    async fn test_peak_tracking() {
        let limiter = ConcurrencyLimiter::new(10, "test");

        assert_eq!(limiter.peak_in_flight(), 0);

        let _p1 = limiter.acquire().await;
        let _p2 = limiter.acquire().await;
        let _p3 = limiter.acquire().await;

        assert_eq!(limiter.peak_in_flight(), 3);

        drop(_p3);
        drop(_p2);

        // Peak should still be 3 even after dropping
        assert_eq!(limiter.peak_in_flight(), 3);
        assert_eq!(limiter.in_flight(), 1);

        limiter.reset_peak();
        assert_eq!(limiter.peak_in_flight(), 0);
    }

    #[tokio::test]
    async fn test_concurrent_acquire() {
        let limiter = Arc::new(ConcurrencyLimiter::new(5, "test"));
        let mut handles = Vec::new();

        // Spawn 10 tasks that each try to acquire
        for _ in 0..10 {
            let limiter = Arc::clone(&limiter);
            handles.push(tokio::spawn(async move {
                let _permit = limiter.acquire().await;
                // Simulate work
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }));
        }

        // Give tasks time to start
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Should never exceed 5 concurrent
        assert!(limiter.in_flight() <= 5);

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(limiter.in_flight(), 0);
    }
}
