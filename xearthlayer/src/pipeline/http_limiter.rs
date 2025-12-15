//! Global HTTP concurrency limiter.
//!
//! This module provides a semaphore-based limiter that constrains the total
//! number of concurrent HTTP requests across all pipeline jobs. This prevents
//! overwhelming the network stack when multiple tiles are being processed
//! simultaneously.
//!
//! # Why This Is Needed
//!
//! Each tile requires 256 chunk downloads. With multiple tiles in flight
//! (e.g., during X-Plane scene loading), the system could attempt thousands
//! of concurrent HTTP connections, causing:
//!
//! - Network stack exhaustion
//! - Connection timeouts and failures
//! - Provider rate limiting
//! - File descriptor exhaustion
//!
//! The `HttpConcurrencyLimiter` constrains total concurrent requests to a
//! manageable level while still allowing high throughput.
//!
//! # Usage
//!
//! ```ignore
//! use std::sync::Arc;
//! use xearthlayer::pipeline::HttpConcurrencyLimiter;
//!
//! // Create limiter with 128 max concurrent requests
//! let limiter = Arc::new(HttpConcurrencyLimiter::new(128));
//!
//! // In download tasks, acquire a permit before making HTTP request
//! async fn download_chunk(limiter: Arc<HttpConcurrencyLimiter>) {
//!     let _permit = limiter.acquire().await;
//!     // HTTP request happens here...
//!     // permit is released when _permit goes out of scope
//! }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Global limiter for HTTP request concurrency.
///
/// Wraps a Tokio semaphore to limit the total number of concurrent HTTP
/// requests across all pipeline jobs. This prevents network stack exhaustion
/// under heavy load.
#[derive(Debug)]
pub struct HttpConcurrencyLimiter {
    /// Semaphore controlling concurrent requests
    semaphore: Arc<Semaphore>,

    /// Maximum permits (for stats/debugging)
    max_permits: usize,

    /// Current number of in-flight requests (for metrics)
    in_flight: AtomicUsize,

    /// Peak concurrent requests observed (for tuning)
    peak_in_flight: AtomicUsize,
}

impl HttpConcurrencyLimiter {
    /// Creates a new limiter with the specified maximum concurrent requests.
    ///
    /// # Arguments
    ///
    /// * `max_concurrent` - Maximum number of concurrent HTTP requests allowed
    ///
    /// # Panics
    ///
    /// Panics if `max_concurrent` is 0.
    pub fn new(max_concurrent: usize) -> Self {
        assert!(max_concurrent > 0, "max_concurrent must be > 0");

        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_permits: max_concurrent,
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    /// Creates a limiter with CPU-based default concurrency.
    ///
    /// Uses the formula: `min(num_cpus * 16, 256)`
    pub fn with_default_concurrency() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);

        let max_concurrent = (cpus * 16).min(256);
        Self::new(max_concurrent)
    }

    /// Acquires a permit for an HTTP request.
    ///
    /// This will wait until a permit is available if the maximum concurrent
    /// requests limit has been reached.
    ///
    /// The permit is automatically released when dropped.
    pub async fn acquire(&self) -> HttpPermit<'_> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed unexpectedly");

        // Track in-flight count
        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;

        // Update peak if this is a new high
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

        HttpPermit {
            _permit: permit,
            in_flight: &self.in_flight,
        }
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self) -> Option<HttpPermit<'_>> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;

        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;

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

        Some(HttpPermit {
            _permit: permit,
            in_flight: &self.in_flight,
        })
    }

    /// Returns the maximum number of concurrent requests allowed.
    pub fn max_concurrent(&self) -> usize {
        self.max_permits
    }

    /// Returns the current number of in-flight HTTP requests.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Returns the peak number of concurrent requests observed.
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

/// A permit for making an HTTP request.
///
/// While this permit is held, it counts against the global concurrency limit.
/// The permit is automatically released when dropped.
pub struct HttpPermit<'a> {
    _permit: OwnedSemaphorePermit,
    in_flight: &'a AtomicUsize,
}

impl Drop for HttpPermit<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_limiter() {
        let limiter = HttpConcurrencyLimiter::new(128);
        assert_eq!(limiter.max_concurrent(), 128);
        assert_eq!(limiter.in_flight(), 0);
        assert_eq!(limiter.available_permits(), 128);
    }

    #[test]
    fn test_default_concurrency() {
        let limiter = HttpConcurrencyLimiter::with_default_concurrency();
        // Should be between 64 (4 CPUs * 16) and 256 (cap)
        assert!(limiter.max_concurrent() >= 64);
        assert!(limiter.max_concurrent() <= 256);
    }

    #[test]
    #[should_panic(expected = "max_concurrent must be > 0")]
    fn test_zero_concurrency_panics() {
        HttpConcurrencyLimiter::new(0);
    }

    #[tokio::test]
    async fn test_acquire_releases_on_drop() {
        let limiter = HttpConcurrencyLimiter::new(2);

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
        let limiter = HttpConcurrencyLimiter::new(1);

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
        let limiter = HttpConcurrencyLimiter::new(10);

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
        let limiter = Arc::new(HttpConcurrencyLimiter::new(5));
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
