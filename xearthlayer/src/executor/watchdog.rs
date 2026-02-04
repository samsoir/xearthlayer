//! Stall detection watchdog.
//!
//! Monitors executor health by tracking activity timestamps and warning
//! when the executor appears stalled (pending work but no progress).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Default stall detection threshold (30 seconds).
pub const STALL_DETECTION_THRESHOLD_MS: u64 = 30_000;

/// Default watchdog check interval (10 seconds).
pub const STALL_WATCHDOG_INTERVAL_SECS: u64 = 10;

/// Stall detection watchdog for the executor.
///
/// Periodically checks if the executor is making progress. Warns when
/// there's pending work but no activity for longer than the threshold.
pub struct StallWatchdog {
    /// Shared timestamp of last executor activity.
    last_activity_ms: Arc<AtomicU64>,

    /// Shared count of pending work items.
    pending_work_count: Arc<AtomicU64>,

    /// Stall threshold in milliseconds.
    threshold_ms: u64,

    /// Check interval.
    interval: Duration,
}

impl StallWatchdog {
    /// Creates a new stall watchdog with default settings.
    pub fn new(last_activity_ms: Arc<AtomicU64>, pending_work_count: Arc<AtomicU64>) -> Self {
        Self {
            last_activity_ms,
            pending_work_count,
            threshold_ms: STALL_DETECTION_THRESHOLD_MS,
            interval: Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS),
        }
    }

    /// Runs the watchdog until cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if shutdown.is_cancelled() {
                break;
            }

            self.check_health();
        }
    }

    /// Checks executor health and logs appropriate message.
    fn check_health(&self) {
        let elapsed_ms = self.elapsed_since_last_activity();
        let work_count = self.pending_work_count.load(Ordering::Relaxed);

        match (elapsed_ms > self.threshold_ms, work_count > 0) {
            (true, true) => {
                // Stalled: pending work but no progress
                warn!(
                    elapsed_ms,
                    pending_work = work_count,
                    threshold_ms = self.threshold_ms,
                    "STALL DETECTED: Executor has {} pending work items but no progress for {}s",
                    work_count,
                    elapsed_ms / 1000
                );
            }
            (true, false) => {
                // Idle: no pending work
                debug!(
                    elapsed_ms,
                    "Stall watchdog: executor idle (no pending work)"
                );
            }
            (false, _) => {
                // Healthy: recent activity
                debug!(
                    elapsed_ms,
                    pending_work = work_count,
                    "Stall watchdog: executor loop healthy"
                );
            }
        }
    }

    /// Returns milliseconds since last activity.
    fn elapsed_since_last_activity(&self) -> u64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_ms = self.last_activity_ms.load(Ordering::Relaxed);
        now_ms.saturating_sub(last_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watchdog_creation() {
        let last_activity = Arc::new(AtomicU64::new(0));
        let pending_work = Arc::new(AtomicU64::new(0));

        let watchdog = StallWatchdog::new(last_activity, pending_work);

        assert_eq!(watchdog.threshold_ms, STALL_DETECTION_THRESHOLD_MS);
        assert_eq!(
            watchdog.interval,
            Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS)
        );
    }

    #[test]
    fn test_elapsed_calculation() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let last_activity = Arc::new(AtomicU64::new(now_ms - 5000)); // 5 seconds ago
        let pending_work = Arc::new(AtomicU64::new(0));

        let watchdog = StallWatchdog::new(last_activity, pending_work);
        let elapsed = watchdog.elapsed_since_last_activity();

        // Should be approximately 5000ms (allow some tolerance)
        assert!(elapsed >= 4900 && elapsed <= 6000);
    }

    #[tokio::test]
    async fn test_watchdog_stops_on_cancellation() {
        let last_activity = Arc::new(AtomicU64::new(0));
        let pending_work = Arc::new(AtomicU64::new(0));
        let shutdown = CancellationToken::new();

        let watchdog = StallWatchdog::new(last_activity, pending_work);

        // Cancel immediately
        shutdown.cancel();

        // Should complete quickly
        let result = tokio::time::timeout(Duration::from_millis(100), watchdog.run(shutdown)).await;

        assert!(result.is_ok());
    }
}
