//! Health monitoring for the pipeline control plane.
//!
//! Provides atomic health metrics and status tracking for the DDS generation
//! pipeline. All operations are lock-free for minimal impact on the hot path.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Health status of the control plane.
///
/// Status transitions:
/// - Healthy → Degraded (when stalled jobs detected)
/// - Degraded → Recovering (when recovery initiated)
/// - Recovering → Healthy (when recovery complete and no stalls)
/// - Any → Critical (when multiple recovery attempts fail)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HealthStatus {
    /// All systems operating normally
    Healthy = 0,
    /// Stalled jobs detected, monitoring closely
    Degraded = 1,
    /// Actively recovering stalled jobs
    Recovering = 2,
    /// Multiple recovery failures, system unstable
    Critical = 3,
}

impl HealthStatus {
    /// Converts from u8 representation.
    #[inline]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Healthy),
            1 => Some(Self::Degraded),
            2 => Some(Self::Recovering),
            3 => Some(Self::Critical),
            _ => None,
        }
    }

    /// Returns the status name for display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Recovering => "recovering",
            Self::Critical => "critical",
        }
    }

    /// Returns true if the system is in a problematic state.
    #[inline]
    pub fn is_unhealthy(&self) -> bool {
        !matches!(self, Self::Healthy)
    }
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Health metrics for the control plane.
///
/// All metrics use atomics for lock-free updates from multiple tasks.
/// This struct is designed to be shared via Arc across the control plane.
pub struct ControlPlaneHealth {
    /// Current health status
    status: AtomicU8,

    /// Timestamp of last successful job completion (millis since epoch)
    /// Using u64 for atomic operations (Instant is not atomic-compatible)
    last_successful_job_ms: AtomicU64,

    /// Reference instant for timestamp calculations
    start_instant: Instant,

    /// Current number of jobs in progress
    jobs_in_progress: AtomicUsize,

    /// Peak concurrent jobs (high water mark)
    peak_concurrent_jobs: AtomicUsize,

    /// Total jobs submitted (for rate calculations)
    total_jobs_submitted: AtomicU64,

    /// Total jobs completed (for rate calculations)
    total_jobs_completed: AtomicU64,

    /// Total jobs recovered by control plane
    jobs_recovered: AtomicU64,

    /// Jobs rejected due to capacity limits
    jobs_rejected_capacity: AtomicU64,

    /// Semaphore acquisition timeouts
    semaphore_timeouts: AtomicU64,

    /// Number of consecutive health check failures
    consecutive_failures: AtomicU64,
}

impl ControlPlaneHealth {
    /// Creates new health metrics.
    pub fn new() -> Self {
        Self {
            status: AtomicU8::new(HealthStatus::Healthy as u8),
            last_successful_job_ms: AtomicU64::new(0),
            start_instant: Instant::now(),
            jobs_in_progress: AtomicUsize::new(0),
            peak_concurrent_jobs: AtomicUsize::new(0),
            total_jobs_submitted: AtomicU64::new(0),
            total_jobs_completed: AtomicU64::new(0),
            jobs_recovered: AtomicU64::new(0),
            jobs_rejected_capacity: AtomicU64::new(0),
            semaphore_timeouts: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
        }
    }

    /// Returns the current health status.
    #[inline]
    pub fn status(&self) -> HealthStatus {
        HealthStatus::from_u8(self.status.load(Ordering::Acquire)).unwrap_or(HealthStatus::Healthy)
    }

    /// Sets the health status.
    pub fn set_status(&self, status: HealthStatus) {
        let old = self.status.swap(status as u8, Ordering::Release);
        if old != status as u8 {
            tracing::info!(
                old_status = %HealthStatus::from_u8(old).unwrap_or(HealthStatus::Healthy),
                new_status = %status,
                "Control plane health status changed"
            );
        }
    }

    /// Records a successful job completion.
    ///
    /// Updates the last successful timestamp and resets failure counters.
    pub fn record_job_success(&self) {
        // Use 1-indexed milliseconds so 0 means "never recorded"
        let elapsed_ms = self.start_instant.elapsed().as_millis() as u64 + 1;
        self.last_successful_job_ms
            .store(elapsed_ms, Ordering::Release);
        self.consecutive_failures.store(0, Ordering::Release);

        // If we were degraded/recovering, transition back to healthy
        let current = self.status.load(Ordering::Acquire);
        if current != HealthStatus::Healthy as u8 && current != HealthStatus::Critical as u8 {
            self.set_status(HealthStatus::Healthy);
        }
    }

    /// Returns duration since last successful job.
    ///
    /// Returns None if no successful jobs have been recorded.
    pub fn time_since_last_success(&self) -> Option<Duration> {
        let last_ms = self.last_successful_job_ms.load(Ordering::Acquire);
        if last_ms == 0 {
            return None;
        }

        // Subtract 1 to account for 1-indexing (0 = never recorded)
        let last_ms = last_ms - 1;
        let current_ms = self.start_instant.elapsed().as_millis() as u64;
        if current_ms > last_ms {
            Some(Duration::from_millis(current_ms - last_ms))
        } else {
            Some(Duration::ZERO)
        }
    }

    /// Increments jobs in progress, updates peak, and tracks submission.
    pub fn job_started(&self) {
        let current = self.jobs_in_progress.fetch_add(1, Ordering::AcqRel) + 1;
        self.total_jobs_submitted.fetch_add(1, Ordering::Relaxed);

        // Update peak if necessary (CAS loop for correctness)
        loop {
            let peak = self.peak_concurrent_jobs.load(Ordering::Acquire);
            if current <= peak {
                break;
            }
            if self
                .peak_concurrent_jobs
                .compare_exchange_weak(peak, current, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Decrements jobs in progress and tracks completion.
    pub fn job_finished(&self) {
        self.jobs_in_progress.fetch_sub(1, Ordering::AcqRel);
        self.total_jobs_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns current jobs in progress.
    #[inline]
    pub fn jobs_in_progress(&self) -> usize {
        self.jobs_in_progress.load(Ordering::Acquire)
    }

    /// Returns peak concurrent jobs.
    #[inline]
    pub fn peak_concurrent_jobs(&self) -> usize {
        self.peak_concurrent_jobs.load(Ordering::Acquire)
    }

    /// Returns total jobs submitted (for rate calculations).
    #[inline]
    pub fn total_jobs_submitted(&self) -> u64 {
        self.total_jobs_submitted.load(Ordering::Relaxed)
    }

    /// Returns total jobs completed (for rate calculations).
    #[inline]
    pub fn total_jobs_completed(&self) -> u64 {
        self.total_jobs_completed.load(Ordering::Relaxed)
    }

    /// Records a job recovery.
    pub fn record_recovery(&self) {
        self.jobs_recovered.fetch_add(1, Ordering::Relaxed);
        self.set_status(HealthStatus::Recovering);
    }

    /// Returns total jobs recovered.
    #[inline]
    pub fn jobs_recovered(&self) -> u64 {
        self.jobs_recovered.load(Ordering::Relaxed)
    }

    /// Records a job rejected due to capacity.
    pub fn record_rejected_capacity(&self) {
        self.jobs_rejected_capacity.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns jobs rejected due to capacity.
    #[inline]
    pub fn jobs_rejected_capacity(&self) -> u64 {
        self.jobs_rejected_capacity.load(Ordering::Relaxed)
    }

    /// Records a semaphore timeout.
    pub fn record_semaphore_timeout(&self) {
        self.semaphore_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns semaphore timeouts.
    #[inline]
    pub fn semaphore_timeouts(&self) -> u64 {
        self.semaphore_timeouts.load(Ordering::Relaxed)
    }

    /// Records a health check failure.
    ///
    /// Returns the new consecutive failure count.
    pub fn record_health_check_failure(&self) -> u64 {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;

        // Transition to critical after 3 consecutive failures
        if failures >= 3 {
            self.set_status(HealthStatus::Critical);
        } else if failures >= 1 {
            let current = self.status.load(Ordering::Acquire);
            if current == HealthStatus::Healthy as u8 {
                self.set_status(HealthStatus::Degraded);
            }
        }

        failures
    }

    /// Resets consecutive failure count.
    pub fn reset_failures(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
    }

    /// Returns a snapshot of all health metrics.
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            status: self.status(),
            time_since_last_success: self.time_since_last_success(),
            jobs_in_progress: self.jobs_in_progress(),
            peak_concurrent_jobs: self.peak_concurrent_jobs(),
            total_jobs_submitted: self.total_jobs_submitted(),
            total_jobs_completed: self.total_jobs_completed(),
            jobs_recovered: self.jobs_recovered(),
            jobs_rejected_capacity: self.jobs_rejected_capacity(),
            semaphore_timeouts: self.semaphore_timeouts(),
        }
    }
}

impl Default for ControlPlaneHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of health metrics at a point in time.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    /// Current health status
    pub status: HealthStatus,
    /// Time since last successful job
    pub time_since_last_success: Option<Duration>,
    /// Current jobs in progress
    pub jobs_in_progress: usize,
    /// Peak concurrent jobs
    pub peak_concurrent_jobs: usize,
    /// Total jobs submitted (for rate calculations)
    pub total_jobs_submitted: u64,
    /// Total jobs completed (for rate calculations)
    pub total_jobs_completed: u64,
    /// Total jobs recovered
    pub jobs_recovered: u64,
    /// Jobs rejected due to capacity
    pub jobs_rejected_capacity: u64,
    /// Semaphore acquisition timeouts
    pub semaphore_timeouts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_from_u8() {
        assert_eq!(HealthStatus::from_u8(0), Some(HealthStatus::Healthy));
        assert_eq!(HealthStatus::from_u8(1), Some(HealthStatus::Degraded));
        assert_eq!(HealthStatus::from_u8(2), Some(HealthStatus::Recovering));
        assert_eq!(HealthStatus::from_u8(3), Some(HealthStatus::Critical));
        assert_eq!(HealthStatus::from_u8(4), None);
    }

    #[test]
    fn test_health_status_is_unhealthy() {
        assert!(!HealthStatus::Healthy.is_unhealthy());
        assert!(HealthStatus::Degraded.is_unhealthy());
        assert!(HealthStatus::Recovering.is_unhealthy());
        assert!(HealthStatus::Critical.is_unhealthy());
    }

    #[test]
    fn test_health_initial_state() {
        let health = ControlPlaneHealth::new();

        assert_eq!(health.status(), HealthStatus::Healthy);
        assert_eq!(health.jobs_in_progress(), 0);
        assert_eq!(health.peak_concurrent_jobs(), 0);
        assert_eq!(health.jobs_recovered(), 0);
        assert!(health.time_since_last_success().is_none());
    }

    #[test]
    fn test_health_job_lifecycle() {
        let health = ControlPlaneHealth::new();

        health.job_started();
        assert_eq!(health.jobs_in_progress(), 1);
        assert_eq!(health.peak_concurrent_jobs(), 1);

        health.job_started();
        assert_eq!(health.jobs_in_progress(), 2);
        assert_eq!(health.peak_concurrent_jobs(), 2);

        health.job_finished();
        assert_eq!(health.jobs_in_progress(), 1);
        assert_eq!(health.peak_concurrent_jobs(), 2); // Peak unchanged

        health.job_finished();
        assert_eq!(health.jobs_in_progress(), 0);
        assert_eq!(health.peak_concurrent_jobs(), 2); // Peak unchanged
    }

    #[test]
    fn test_health_record_success() {
        let health = ControlPlaneHealth::new();

        // Initially no successful jobs
        assert!(health.time_since_last_success().is_none());

        // Record success
        health.record_job_success();

        // Now we should have a recent success
        let elapsed = health.time_since_last_success();
        assert!(elapsed.is_some());
        assert!(elapsed.unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn test_health_status_transitions() {
        let health = ControlPlaneHealth::new();

        assert_eq!(health.status(), HealthStatus::Healthy);

        // First failure -> degraded
        health.record_health_check_failure();
        assert_eq!(health.status(), HealthStatus::Degraded);

        // Second failure -> still degraded
        health.record_health_check_failure();
        assert_eq!(health.status(), HealthStatus::Degraded);

        // Third failure -> critical
        health.record_health_check_failure();
        assert_eq!(health.status(), HealthStatus::Critical);
    }

    #[test]
    fn test_health_recovery_resets_status() {
        let health = ControlPlaneHealth::new();

        // Go to degraded
        health.record_health_check_failure();
        assert_eq!(health.status(), HealthStatus::Degraded);

        // Success brings us back to healthy
        health.record_job_success();
        assert_eq!(health.status(), HealthStatus::Healthy);
    }

    #[test]
    fn test_health_snapshot() {
        let health = ControlPlaneHealth::new();

        health.job_started();
        health.job_started();
        health.record_recovery();
        health.record_rejected_capacity();
        health.record_semaphore_timeout();

        let snapshot = health.snapshot();

        assert_eq!(snapshot.status, HealthStatus::Recovering);
        assert_eq!(snapshot.jobs_in_progress, 2);
        assert_eq!(snapshot.peak_concurrent_jobs, 2);
        assert_eq!(snapshot.jobs_recovered, 1);
        assert_eq!(snapshot.jobs_rejected_capacity, 1);
        assert_eq!(snapshot.semaphore_timeouts, 1);
    }

    #[test]
    fn test_health_concurrent_peak_update() {
        use std::sync::Arc;
        use std::thread;

        let health = Arc::new(ControlPlaneHealth::new());
        let mut handles = vec![];

        // Spawn multiple threads that increment concurrently
        for _ in 0..10 {
            let h = Arc::clone(&health);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    h.job_started();
                    // Small work
                    std::hint::spin_loop();
                    h.job_finished();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All jobs should be finished
        assert_eq!(health.jobs_in_progress(), 0);
        // Peak should be at least 1 (some concurrency happened)
        assert!(health.peak_concurrent_jobs() >= 1);
    }
}
