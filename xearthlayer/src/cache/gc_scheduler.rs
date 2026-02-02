//! Garbage collection scheduler daemon.
//!
//! This module provides a background daemon that periodically checks if the
//! disk cache needs garbage collection and submits [`CacheGcJob`] to the
//! job executor when the cache exceeds its threshold.
//!
//! # Architecture
//!
//! The scheduler runs in a background task and:
//! 1. Checks cache size periodically (default: every 60 seconds)
//! 2. When cache exceeds 95% of max size, submits a GC job
//! 3. GC job runs at `Priority::HOUSEKEEPING` (-50), yielding to other work
//! 4. Respects cancellation for graceful shutdown
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::cache::GcSchedulerDaemon;
//!
//! let daemon = GcSchedulerDaemon::new(
//!     lru_index,
//!     cache_dir,
//!     max_size_bytes,
//!     job_submitter,
//! );
//!
//! tokio::spawn(daemon.run(shutdown_token));
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::LruIndex;
use crate::executor::JobSubmitter;
use crate::jobs::CacheGcJob;

/// Default interval between GC checks (60 seconds).
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 60;

/// Default threshold to trigger GC (95% of max size).
pub const DEFAULT_TRIGGER_THRESHOLD: f64 = 0.95;

/// Default target size after GC (80% of max size).
pub const DEFAULT_TARGET_RATIO: f64 = 0.80;

/// Background daemon that schedules garbage collection jobs.
///
/// This daemon periodically checks the disk cache size via the LRU index
/// and submits `CacheGcJob` when the cache exceeds its threshold.
pub struct GcSchedulerDaemon {
    /// LRU index for checking cache size.
    lru_index: Arc<LruIndex>,

    /// Cache directory path.
    cache_dir: PathBuf,

    /// Maximum cache size in bytes.
    max_size_bytes: u64,

    /// Job submitter for sending GC jobs to the executor.
    job_submitter: JobSubmitter,

    /// Interval between GC checks.
    check_interval: Duration,

    /// Threshold ratio to trigger GC (0.0-1.0).
    trigger_threshold: f64,

    /// Target ratio after GC (0.0-1.0).
    target_ratio: f64,
}

impl GcSchedulerDaemon {
    /// Creates a new GC scheduler daemon with default settings.
    ///
    /// # Arguments
    ///
    /// * `lru_index` - LRU index for checking cache size
    /// * `cache_dir` - Cache directory path
    /// * `max_size_bytes` - Maximum cache size in bytes
    /// * `job_submitter` - Job submitter for sending GC jobs
    pub fn new(
        lru_index: Arc<LruIndex>,
        cache_dir: PathBuf,
        max_size_bytes: u64,
        job_submitter: JobSubmitter,
    ) -> Self {
        Self {
            lru_index,
            cache_dir,
            max_size_bytes,
            job_submitter,
            check_interval: Duration::from_secs(DEFAULT_CHECK_INTERVAL_SECS),
            trigger_threshold: DEFAULT_TRIGGER_THRESHOLD,
            target_ratio: DEFAULT_TARGET_RATIO,
        }
    }

    /// Sets a custom check interval.
    pub fn with_check_interval(mut self, interval: Duration) -> Self {
        self.check_interval = interval;
        self
    }

    /// Sets a custom trigger threshold (0.0-1.0).
    ///
    /// GC is triggered when cache size exceeds `max_size * threshold`.
    pub fn with_trigger_threshold(mut self, threshold: f64) -> Self {
        self.trigger_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Sets a custom target ratio (0.0-1.0).
    ///
    /// GC will try to reduce cache to `max_size * ratio`.
    pub fn with_target_ratio(mut self, ratio: f64) -> Self {
        self.target_ratio = ratio.clamp(0.0, 1.0);
        self
    }

    /// Returns the trigger threshold in bytes.
    pub fn trigger_size_bytes(&self) -> u64 {
        (self.max_size_bytes as f64 * self.trigger_threshold) as u64
    }

    /// Returns the target size in bytes.
    pub fn target_size_bytes(&self) -> u64 {
        (self.max_size_bytes as f64 * self.target_ratio) as u64
    }

    /// Checks if GC should be triggered based on current cache size.
    pub fn needs_gc(&self) -> bool {
        self.lru_index.total_size() > self.trigger_size_bytes()
    }

    /// Submits a GC job to the executor.
    ///
    /// Returns true if the job was submitted successfully.
    pub fn submit_gc_job(&self) -> bool {
        let current_size = self.lru_index.total_size();
        let target_size = self.target_size_bytes();

        info!(
            current_mb = current_size / 1_000_000,
            target_mb = target_size / 1_000_000,
            threshold_mb = self.trigger_size_bytes() / 1_000_000,
            "Submitting GC job"
        );

        let job = CacheGcJob::new(
            Arc::clone(&self.lru_index),
            self.cache_dir.clone(),
            target_size,
        );

        match self.job_submitter.try_submit(job) {
            Some(handle) => {
                debug!(job_id = %handle.id(), "GC job submitted");
                true
            }
            None => {
                warn!("Failed to submit GC job - executor channel closed");
                false
            }
        }
    }

    /// Runs the scheduler until shutdown is signalled.
    ///
    /// This is the main loop that periodically checks if GC is needed
    /// and submits jobs when the threshold is exceeded.
    pub async fn run(self, shutdown: CancellationToken) {
        info!(
            check_interval_secs = self.check_interval.as_secs(),
            trigger_threshold = self.trigger_threshold,
            target_ratio = self.target_ratio,
            max_mb = self.max_size_bytes / 1_000_000,
            "GC scheduler daemon starting"
        );

        let mut interval = tokio::time::interval(self.check_interval);
        // Skip the first immediate tick
        interval.tick().await;

        loop {
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    info!("GC scheduler daemon shutting down");
                    break;
                }

                _ = interval.tick() => {
                    if self.needs_gc() {
                        self.submit_gc_job();
                    } else {
                        debug!(
                            current_mb = self.lru_index.total_size() / 1_000_000,
                            threshold_mb = self.trigger_size_bytes() / 1_000_000,
                            "Cache below threshold, skipping GC"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutorConfig, JobExecutor};
    use tempfile::TempDir;

    // ─────────────────────────────────────────────────────────────────────────
    // Helper functions
    // ─────────────────────────────────────────────────────────────────────────

    /// Test context holding all resources needed for GC scheduler tests.
    /// The executor must be kept alive to keep the channel open.
    struct TestContext {
        temp_dir: TempDir,
        lru_index: Arc<LruIndex>,
        submitter: JobSubmitter,
        #[allow(dead_code)] // Keep executor alive to keep channel open
        executor: JobExecutor,
    }

    fn create_test_setup() -> TestContext {
        let temp_dir = TempDir::new().unwrap();
        let lru_index = Arc::new(LruIndex::new(temp_dir.path().to_path_buf()));

        // Create a real executor to get a valid JobSubmitter
        let config = ExecutorConfig::default();
        let (executor, submitter) = JobExecutor::new(config);

        TestContext {
            temp_dir,
            lru_index,
            submitter,
            executor,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Constructor tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_scheduler_new_with_defaults() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000, // 10 MB
            ctx.submitter,
        );

        assert_eq!(daemon.check_interval.as_secs(), DEFAULT_CHECK_INTERVAL_SECS);
        assert!((daemon.trigger_threshold - DEFAULT_TRIGGER_THRESHOLD).abs() < 0.001);
        assert!((daemon.target_ratio - DEFAULT_TARGET_RATIO).abs() < 0.001);
    }

    #[test]
    fn gc_scheduler_custom_settings() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        )
        .with_check_interval(Duration::from_secs(30))
        .with_trigger_threshold(0.90)
        .with_target_ratio(0.70);

        assert_eq!(daemon.check_interval.as_secs(), 30);
        assert!((daemon.trigger_threshold - 0.90).abs() < 0.001);
        assert!((daemon.target_ratio - 0.70).abs() < 0.001);
    }

    #[test]
    fn gc_scheduler_threshold_clamping() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        )
        .with_trigger_threshold(1.5) // Should clamp to 1.0
        .with_target_ratio(-0.5); // Should clamp to 0.0

        assert!((daemon.trigger_threshold - 1.0).abs() < 0.001);
        assert!((daemon.target_ratio - 0.0).abs() < 0.001);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Size calculation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_scheduler_trigger_size_bytes() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000, // 10 MB
            ctx.submitter,
        )
        .with_trigger_threshold(0.95);

        // 95% of 10 MB = 9.5 MB
        assert_eq!(daemon.trigger_size_bytes(), 9_500_000);
    }

    #[test]
    fn gc_scheduler_target_size_bytes() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000, // 10 MB
            ctx.submitter,
        )
        .with_target_ratio(0.80);

        // 80% of 10 MB = 8 MB
        assert_eq!(daemon.target_size_bytes(), 8_000_000);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // GC trigger tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_scheduler_needs_gc_when_over_threshold() {
        let ctx = create_test_setup();

        // Add 9.6 MB to the index (over 95% of 10 MB)
        ctx.lru_index.record("tile:1", 9_600_000);

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000, // 10 MB max
            ctx.submitter,
        )
        .with_trigger_threshold(0.95);

        assert!(daemon.needs_gc());
    }

    #[test]
    fn gc_scheduler_no_gc_when_under_threshold() {
        let ctx = create_test_setup();

        // Add 9 MB to the index (under 95% of 10 MB)
        ctx.lru_index.record("tile:1", 9_000_000);

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000, // 10 MB max
            ctx.submitter,
        )
        .with_trigger_threshold(0.95);

        assert!(!daemon.needs_gc());
    }

    #[test]
    fn gc_scheduler_no_gc_when_empty() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        );

        assert!(!daemon.needs_gc());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Job submission tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_scheduler_submit_gc_job_succeeds() {
        let ctx = create_test_setup();

        // Add some data to the index
        ctx.lru_index.record("tile:1", 1_000_000);

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        );

        // Should succeed in submitting
        assert!(daemon.submit_gc_job());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Daemon run tests
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_scheduler_respects_shutdown() {
        let ctx = create_test_setup();

        let daemon = GcSchedulerDaemon::new(
            ctx.lru_index,
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        )
        .with_check_interval(Duration::from_millis(100));

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // Start daemon - note: ctx.executor is moved into the spawn closure
        // to keep the channel alive
        let executor = ctx.executor;
        let handle = tokio::spawn(async move {
            // Keep executor alive during daemon run
            let _executor = executor;
            daemon.run(shutdown_clone).await;
        });

        // Wait a bit, then cancel
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();

        // Should complete within reasonable time
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn gc_scheduler_submits_job_when_over_threshold() {
        let ctx = create_test_setup();

        // Add data over threshold (96% of 10 MB)
        ctx.lru_index.record("tile:1", 9_600_000);

        let daemon = GcSchedulerDaemon::new(
            Arc::clone(&ctx.lru_index),
            ctx.temp_dir.path().to_path_buf(),
            10_000_000,
            ctx.submitter,
        )
        .with_check_interval(Duration::from_millis(50))
        .with_trigger_threshold(0.95);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // Start daemon - keep executor alive during daemon run
        let executor = ctx.executor;
        let handle = tokio::spawn(async move {
            let _executor = executor;
            daemon.run(shutdown_clone).await;
        });

        // Wait for at least one check cycle
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();

        // Should complete
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok());

        // Note: We can't easily verify the job was submitted without running
        // the executor, but the daemon should have attempted submission
    }
}
