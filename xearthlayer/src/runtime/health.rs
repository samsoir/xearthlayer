//! Runtime health monitoring.
//!
//! Provides health status tracking for the XEarthLayer runtime, including
//! job processing metrics and status indicators for the dashboard UI.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Health status of the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Runtime is healthy and processing jobs normally.
    Healthy,
    /// Runtime is under high load but still processing.
    Degraded,
    /// Runtime is recovering from an error state.
    Recovering,
    /// Runtime has critical issues.
    Critical,
}

impl HealthStatus {
    /// Returns a string representation of the status.
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthStatus::Healthy => "healthy",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Recovering => "recovering",
            HealthStatus::Critical => "critical",
        }
    }
}

/// A point-in-time snapshot of runtime health.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    /// Current health status.
    pub status: HealthStatus,
    /// Number of jobs currently in progress.
    pub jobs_in_progress: usize,
    /// Peak concurrent jobs seen.
    pub peak_concurrent_jobs: usize,
    /// Total jobs submitted since startup.
    pub total_jobs_submitted: u64,
    /// Total jobs completed since startup.
    pub total_jobs_completed: u64,
    /// Time since last successful job completion.
    pub time_since_last_success: Option<Duration>,
    /// Number of jobs that recovered after initial failure.
    pub jobs_recovered: u64,
    /// Number of jobs rejected due to capacity limits.
    pub jobs_rejected_capacity: u64,
    /// Number of semaphore timeout events.
    pub semaphore_timeouts: u64,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        Self {
            status: HealthStatus::Healthy,
            jobs_in_progress: 0,
            peak_concurrent_jobs: 0,
            total_jobs_submitted: 0,
            total_jobs_completed: 0,
            time_since_last_success: None,
            jobs_recovered: 0,
            jobs_rejected_capacity: 0,
            semaphore_timeouts: 0,
        }
    }
}

/// Runtime health monitor with atomic counters.
///
/// Thread-safe health tracking for the executor daemon.
pub struct RuntimeHealth {
    /// Jobs currently in progress.
    jobs_in_progress: AtomicUsize,
    /// Peak concurrent jobs.
    peak_concurrent_jobs: AtomicUsize,
    /// Total jobs submitted.
    total_jobs_submitted: AtomicU64,
    /// Total jobs completed.
    total_jobs_completed: AtomicU64,
    /// Jobs that recovered.
    jobs_recovered: AtomicU64,
    /// Jobs rejected due to capacity.
    jobs_rejected_capacity: AtomicU64,
    /// Semaphore timeouts.
    semaphore_timeouts: AtomicU64,
    /// Startup time.
    start_time: Instant,
    /// Last success time (stored as duration since start_time in micros).
    /// Using micros for better precision in fast tests.
    last_success_micros: AtomicU64,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeHealth {
    /// Create a new runtime health monitor.
    pub fn new() -> Self {
        Self {
            jobs_in_progress: AtomicUsize::new(0),
            peak_concurrent_jobs: AtomicUsize::new(0),
            total_jobs_submitted: AtomicU64::new(0),
            total_jobs_completed: AtomicU64::new(0),
            jobs_recovered: AtomicU64::new(0),
            jobs_rejected_capacity: AtomicU64::new(0),
            semaphore_timeouts: AtomicU64::new(0),
            start_time: Instant::now(),
            last_success_micros: AtomicU64::new(0),
        }
    }

    /// Record a job starting.
    pub fn job_started(&self) {
        self.total_jobs_submitted.fetch_add(1, Ordering::Relaxed);
        let current = self.jobs_in_progress.fetch_add(1, Ordering::Relaxed) + 1;
        // Update peak if needed
        let mut peak = self.peak_concurrent_jobs.load(Ordering::Relaxed);
        while current > peak {
            match self.peak_concurrent_jobs.compare_exchange_weak(
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

    /// Record a job completing successfully.
    pub fn job_completed(&self) {
        self.jobs_in_progress.fetch_sub(1, Ordering::Relaxed);
        self.total_jobs_completed.fetch_add(1, Ordering::Relaxed);
        // Use micros for better precision in fast tests
        // Add 1 to distinguish from "never set" (0)
        let elapsed = self.start_time.elapsed().as_micros() as u64 + 1;
        self.last_success_micros.store(elapsed, Ordering::Relaxed);
    }

    /// Record a job failing.
    pub fn job_failed(&self) {
        self.jobs_in_progress.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a job recovery.
    pub fn job_recovered(&self) {
        self.jobs_recovered.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a job rejected due to capacity.
    pub fn job_rejected_capacity(&self) {
        self.jobs_rejected_capacity.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a semaphore timeout.
    pub fn semaphore_timeout(&self) {
        self.semaphore_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a snapshot of current health.
    pub fn snapshot(&self) -> HealthSnapshot {
        let jobs_in_progress = self.jobs_in_progress.load(Ordering::Relaxed);
        let peak_concurrent_jobs = self.peak_concurrent_jobs.load(Ordering::Relaxed);
        let total_jobs_submitted = self.total_jobs_submitted.load(Ordering::Relaxed);
        let total_jobs_completed = self.total_jobs_completed.load(Ordering::Relaxed);
        let jobs_recovered = self.jobs_recovered.load(Ordering::Relaxed);
        let jobs_rejected_capacity = self.jobs_rejected_capacity.load(Ordering::Relaxed);
        let semaphore_timeouts = self.semaphore_timeouts.load(Ordering::Relaxed);

        // Calculate time since last success
        let last_success_micros = self.last_success_micros.load(Ordering::Relaxed);
        let time_since_last_success = if last_success_micros > 0 {
            // Subtract 1 to account for the offset we added
            let now_micros = self.start_time.elapsed().as_micros() as u64;
            let last_micros = last_success_micros.saturating_sub(1);
            Some(Duration::from_micros(
                now_micros.saturating_sub(last_micros),
            ))
        } else {
            None
        };

        // Determine status based on metrics
        let status = if jobs_rejected_capacity > 0 || semaphore_timeouts > 10 {
            HealthStatus::Critical
        } else if jobs_recovered > 0 || semaphore_timeouts > 0 {
            HealthStatus::Recovering
        } else if jobs_in_progress > peak_concurrent_jobs / 2 && peak_concurrent_jobs > 10 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        };

        HealthSnapshot {
            status,
            jobs_in_progress,
            peak_concurrent_jobs,
            total_jobs_submitted,
            total_jobs_completed,
            time_since_last_success,
            jobs_recovered,
            jobs_rejected_capacity,
            semaphore_timeouts,
        }
    }
}

/// Shared runtime health monitor.
pub type SharedRuntimeHealth = Arc<RuntimeHealth>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_as_str() {
        assert_eq!(HealthStatus::Healthy.as_str(), "healthy");
        assert_eq!(HealthStatus::Degraded.as_str(), "degraded");
        assert_eq!(HealthStatus::Recovering.as_str(), "recovering");
        assert_eq!(HealthStatus::Critical.as_str(), "critical");
    }

    #[test]
    fn test_default_snapshot() {
        let snapshot = HealthSnapshot::default();
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert_eq!(snapshot.jobs_in_progress, 0);
    }

    #[test]
    fn test_runtime_health_job_lifecycle() {
        let health = RuntimeHealth::new();

        // Start a job
        health.job_started();
        let snapshot = health.snapshot();
        assert_eq!(snapshot.jobs_in_progress, 1);
        assert_eq!(snapshot.peak_concurrent_jobs, 1);

        // Complete a job
        health.job_completed();
        let snapshot = health.snapshot();
        assert_eq!(snapshot.jobs_in_progress, 0);
        assert_eq!(snapshot.total_jobs_completed, 1);
        assert!(snapshot.time_since_last_success.is_some());
    }

    #[test]
    fn test_peak_tracking() {
        let health = RuntimeHealth::new();

        health.job_started();
        health.job_started();
        health.job_started();
        assert_eq!(health.snapshot().peak_concurrent_jobs, 3);

        health.job_completed();
        health.job_completed();
        // Peak should remain at 3
        assert_eq!(health.snapshot().peak_concurrent_jobs, 3);
    }

    #[test]
    fn test_health_status_critical() {
        let health = RuntimeHealth::new();
        health.job_rejected_capacity();

        let snapshot = health.snapshot();
        assert_eq!(snapshot.status, HealthStatus::Critical);
    }
}
