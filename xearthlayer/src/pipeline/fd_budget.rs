//! File descriptor budget system.
//!
//! Limits XEarthLayer's file descriptor usage to prevent exhaustion.
//! By default, XEL uses at most 50% of the available user FD limit.
//!
//! File descriptors are consumed by:
//! - HTTP connections (each active download)
//! - FUSE operations (each mounted filesystem)
//! - Cache file handles
//! - Internal async runtime resources
//!
//! This module provides:
//! - Detection of the system FD limit (via `/proc` or resource limits)
//! - A budget limiter that blocks/rejects when approaching the limit
//! - Integration with the control plane for admission control

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default percentage of user FD limit that XEL should use (50%).
pub const DEFAULT_FD_BUDGET_PERCENT: u64 = 50;

/// Minimum FD budget to allow (prevents edge cases with very low limits).
pub const MIN_FD_BUDGET: u64 = 100;

/// Fallback FD limit if detection fails.
const FALLBACK_FD_LIMIT: u64 = 1024;

/// File descriptor budget controller.
///
/// Tracks FD usage and enforces a budget limit. This is separate from
/// semaphore-based concurrency limiters because FD exhaustion affects
/// the entire process, not just pipeline stages.
#[derive(Debug)]
pub struct FdBudget {
    /// Maximum FDs this budget allows (the budget itself).
    budget: u64,
    /// Current estimated FD usage (tracked via acquire/release).
    current: AtomicU64,
    /// System FD limit for informational purposes.
    system_limit: u64,
}

impl FdBudget {
    /// Create a new FD budget with the given percentage of system limit.
    ///
    /// # Arguments
    ///
    /// * `budget_percent` - Percentage of user FD limit to use (1-100)
    ///
    /// # Panics
    ///
    /// Panics if `budget_percent` is 0 or greater than 100.
    pub fn new(budget_percent: u64) -> Self {
        assert!(
            budget_percent > 0 && budget_percent <= 100,
            "FD budget percent must be between 1 and 100"
        );

        let system_limit = detect_fd_limit();
        let budget = (system_limit * budget_percent / 100).max(MIN_FD_BUDGET);

        tracing::info!(
            system_limit = system_limit,
            budget = budget,
            budget_percent = budget_percent,
            "FD budget initialized"
        );

        Self {
            budget,
            current: AtomicU64::new(0),
            system_limit,
        }
    }

    /// Create a new FD budget with the default 50% limit.
    pub fn default_budget() -> Self {
        Self::new(DEFAULT_FD_BUDGET_PERCENT)
    }

    /// Get the budget limit.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Get the system FD limit.
    pub fn system_limit(&self) -> u64 {
        self.system_limit
    }

    /// Get current tracked FD usage.
    pub fn current_usage(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// Get remaining FDs available in the budget.
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.current_usage())
    }

    /// Check if the budget has capacity for `count` more FDs.
    pub fn has_capacity(&self, count: u64) -> bool {
        self.current_usage() + count <= self.budget
    }

    /// Get the current usage as a percentage of budget.
    pub fn usage_percent(&self) -> f64 {
        if self.budget == 0 {
            return 100.0;
        }
        (self.current_usage() as f64 / self.budget as f64) * 100.0
    }

    /// Try to acquire `count` FDs from the budget.
    ///
    /// Returns `true` if the FDs were acquired, `false` if the budget
    /// would be exceeded.
    ///
    /// # Thread Safety
    ///
    /// Uses compare-and-swap to ensure atomic acquisition. Multiple
    /// concurrent callers may race, but the budget will never be exceeded.
    pub fn try_acquire(&self, count: u64) -> bool {
        loop {
            let current = self.current.load(Ordering::Acquire);
            let new = current + count;

            if new > self.budget {
                return false;
            }

            if self
                .current
                .compare_exchange_weak(current, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            // CAS failed, retry
        }
    }

    /// Release `count` FDs back to the budget.
    ///
    /// # Safety
    ///
    /// Callers must ensure they previously acquired these FDs.
    pub fn release(&self, count: u64) {
        loop {
            let current = self.current.load(Ordering::Acquire);
            // Saturating sub to handle edge cases
            let new = current.saturating_sub(count);

            if self
                .current
                .compare_exchange_weak(current, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            // CAS failed, retry
        }
    }
}

impl Default for FdBudget {
    fn default() -> Self {
        Self::default_budget()
    }
}

/// Shared FD budget that can be cloned cheaply.
pub type SharedFdBudget = Arc<FdBudget>;

/// Detect the user's file descriptor limit.
///
/// Attempts detection in this order:
/// 1. Read `/proc/self/limits` for the soft limit
/// 2. Use `libc::getrlimit` as fallback
/// 3. Return a safe fallback value
fn detect_fd_limit() -> u64 {
    // Try reading from /proc first (Linux-specific but fast)
    if let Ok(limit) = detect_fd_limit_proc() {
        return limit;
    }

    // Fall back to getrlimit
    if let Ok(limit) = detect_fd_limit_rlimit() {
        return limit;
    }

    tracing::warn!(
        fallback = FALLBACK_FD_LIMIT,
        "Could not detect FD limit, using fallback"
    );
    FALLBACK_FD_LIMIT
}

/// Detect FD limit from /proc/self/limits.
fn detect_fd_limit_proc() -> Result<u64, std::io::Error> {
    let contents = std::fs::read_to_string("/proc/self/limits")?;

    for line in contents.lines() {
        if line.starts_with("Max open files") {
            // Format: "Max open files            1024                 1048576              files"
            //                                   soft                  hard
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                // parts[3] is the soft limit
                if let Ok(soft_limit) = parts[3].parse::<u64>() {
                    return Ok(soft_limit);
                }
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Max open files line not found",
    ))
}

/// Detect FD limit using getrlimit.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // rlim_cur type varies by platform
fn detect_fd_limit_rlimit() -> Result<u64, std::io::Error> {
    use std::mem::MaybeUninit;

    let mut rlim = MaybeUninit::<libc::rlimit>::uninit();

    // SAFETY: We pass a valid pointer to uninitialized memory
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) };

    if result == 0 {
        // SAFETY: getrlimit succeeded, rlim is now initialized
        let rlim = unsafe { rlim.assume_init() };
        Ok(rlim.rlim_cur as u64)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn detect_fd_limit_rlimit() -> Result<u64, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "getrlimit not available on this platform",
    ))
}

/// RAII guard for FD budget reservation.
///
/// Automatically releases the reserved FDs when dropped.
#[derive(Debug)]
pub struct FdReservation {
    budget: SharedFdBudget,
    count: u64,
}

impl FdReservation {
    /// Create a new FD reservation.
    ///
    /// Returns `None` if the budget doesn't have capacity.
    pub fn try_new(budget: SharedFdBudget, count: u64) -> Option<Self> {
        if budget.try_acquire(count) {
            Some(Self { budget, count })
        } else {
            None
        }
    }

    /// Get the number of FDs reserved.
    pub fn count(&self) -> u64 {
        self.count
    }
}

impl Drop for FdReservation {
    fn drop(&mut self) {
        self.budget.release(self.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fd_budget_creation() {
        let budget = FdBudget::new(50);
        assert!(budget.budget() >= MIN_FD_BUDGET);
        assert!(budget.budget() <= budget.system_limit());
    }

    #[test]
    fn test_fd_budget_acquire_release() {
        let budget = FdBudget::new(100);
        let max = budget.budget();

        // Acquire some FDs
        assert!(budget.try_acquire(10));
        assert_eq!(budget.current_usage(), 10);
        assert_eq!(budget.remaining(), max - 10);

        // Release them
        budget.release(10);
        assert_eq!(budget.current_usage(), 0);
        assert_eq!(budget.remaining(), max);
    }

    #[test]
    fn test_fd_budget_exceeds() {
        let budget = FdBudget::new(100);
        let max = budget.budget();

        // Try to acquire more than available
        assert!(!budget.try_acquire(max + 1));
        assert_eq!(budget.current_usage(), 0);

        // Acquire exactly max
        assert!(budget.try_acquire(max));
        assert_eq!(budget.current_usage(), max);

        // Can't acquire any more
        assert!(!budget.try_acquire(1));

        // Release and try again
        budget.release(max);
        assert!(budget.try_acquire(1));
    }

    #[test]
    fn test_fd_budget_has_capacity() {
        let budget = FdBudget::new(100);
        let max = budget.budget();

        assert!(budget.has_capacity(1));
        assert!(budget.has_capacity(max));
        assert!(!budget.has_capacity(max + 1));

        budget.try_acquire(max / 2);
        assert!(budget.has_capacity(max / 2));
        assert!(!budget.has_capacity(max / 2 + 1));
    }

    #[test]
    fn test_fd_reservation_raii() {
        let budget = Arc::new(FdBudget::new(100));

        {
            let reservation = FdReservation::try_new(Arc::clone(&budget), 50);
            assert!(reservation.is_some());
            assert_eq!(budget.current_usage(), 50);
        }
        // Reservation dropped, FDs released
        assert_eq!(budget.current_usage(), 0);
    }

    #[test]
    fn test_fd_reservation_fails_when_exceeded() {
        let budget = Arc::new(FdBudget::new(100));
        let max = budget.budget();

        // Acquire almost all
        let _r1 = FdReservation::try_new(Arc::clone(&budget), max - 1);

        // Can't acquire more than remaining
        let r2 = FdReservation::try_new(Arc::clone(&budget), 2);
        assert!(r2.is_none());
    }

    #[test]
    fn test_detect_fd_limit() {
        let limit = detect_fd_limit();
        assert!(limit >= MIN_FD_BUDGET, "FD limit should be reasonable");
    }

    #[test]
    fn test_usage_percent() {
        let budget = FdBudget::new(100);
        let max = budget.budget();

        assert_eq!(budget.usage_percent(), 0.0);

        budget.try_acquire(max / 2);
        assert!((budget.usage_percent() - 50.0).abs() < 0.1);

        budget.try_acquire(max / 2);
        assert!((budget.usage_percent() - 100.0).abs() < 0.1);
    }

    #[test]
    #[should_panic(expected = "FD budget percent must be between 1 and 100")]
    fn test_invalid_budget_percent_zero() {
        FdBudget::new(0);
    }

    #[test]
    #[should_panic(expected = "FD budget percent must be between 1 and 100")]
    fn test_invalid_budget_percent_over_100() {
        FdBudget::new(101);
    }
}
