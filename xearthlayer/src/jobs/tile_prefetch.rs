//! Tile prefetch job implementation.
//!
//! [`TilePrefetchJob`] is a hierarchical job that prefetches all DDS tiles
//! within a geographic area. It spawns many `DdsGenerateJob` children via
//! the [`GenerateTileListTask`].
//!
//! # Hierarchical Structure
//!
//! ```text
//! TilePrefetchJob
//!     └── GenerateTileListTask
//!             └── Spawns N×N DdsGenerateJob children
//!                     └── Each child runs 4 tasks:
//!                         ├── DownloadChunks
//!                         ├── AssembleImage
//!                         ├── EncodeDds
//!                         └── CacheWrite
//! ```
//!
//! # Error Policy
//!
//! Uses `PartialSuccess` with a configurable threshold (default 80%). This is
//! appropriate for prefetching where some tile failures are acceptable - the
//! goal is to warm the cache, not guarantee 100% success.

use crate::executor::{ErrorPolicy, Job, JobId, JobResult, JobStatus, Priority, Task};
use crate::jobs::DdsJobFactory;
use crate::tasks::GenerateTileListTask;
use std::sync::Arc;

/// Default success threshold for prefetch jobs (80%).
pub const DEFAULT_SUCCESS_THRESHOLD: f64 = 0.8;

/// Job for prefetching all DDS tiles within a geographic area.
///
/// This job creates a single `GenerateTileListTask` that converts the
/// lat/lon center and radius into tile coordinates, then spawns child
/// `DdsGenerateJob` for each tile.
///
/// # Completion Criteria
///
/// The job succeeds if at least `success_threshold` fraction of child jobs
/// succeed. For example, with the default 80% threshold:
/// - 100/121 children succeed → 82.6% → Job succeeds
/// - 90/121 children succeed → 74.4% → Job fails
///
/// # Type Parameters
///
/// * `F` - Factory type implementing `DdsJobFactory` for creating child jobs
pub struct TilePrefetchJob<F>
where
    F: DdsJobFactory,
{
    /// Unique job identifier
    id: JobId,

    /// Center latitude in degrees
    lat: f64,

    /// Center longitude in degrees
    lon: f64,

    /// Target zoom level for tiles
    zoom: u8,

    /// Number of tiles in each direction from center
    radius_tiles: u32,

    /// Factory for creating child DdsGenerateJob instances
    factory: Arc<F>,

    /// Minimum success ratio for job to be considered successful (0.0 - 1.0)
    success_threshold: f64,
}

impl<F> TilePrefetchJob<F>
where
    F: DdsJobFactory,
{
    /// Creates a new tile prefetch job.
    ///
    /// # Arguments
    ///
    /// * `lat` - Center latitude in degrees (-85.05 to 85.05)
    /// * `lon` - Center longitude in degrees (-180.0 to 180.0)
    /// * `zoom` - Target zoom level (0-18)
    /// * `radius_tiles` - Number of tiles in each direction from center
    /// * `factory` - Factory for creating child DdsGenerateJob instances
    ///
    /// # Example
    ///
    /// ```ignore
    /// let job = TilePrefetchJob::new(
    ///     47.6,   // Seattle latitude
    ///     -122.3, // Seattle longitude
    ///     14,     // Zoom level
    ///     5,      // 5 tiles in each direction = 11×11 = 121 tiles
    ///     factory,
    /// );
    /// ```
    pub fn new(lat: f64, lon: f64, zoom: u8, radius_tiles: u32, factory: Arc<F>) -> Self {
        let id = JobId::new(format!(
            "prefetch-{:.2}_{:.2}_ZL{}_r{}",
            lat, lon, zoom, radius_tiles
        ));
        Self {
            id,
            lat,
            lon,
            zoom,
            radius_tiles,
            factory,
            success_threshold: DEFAULT_SUCCESS_THRESHOLD,
        }
    }

    /// Sets a custom success threshold (builder pattern).
    ///
    /// # Arguments
    ///
    /// * `threshold` - Minimum success ratio (0.0 - 1.0)
    ///
    /// # Panics
    ///
    /// Panics if threshold is not in range 0.0..=1.0
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&threshold),
            "Threshold must be between 0.0 and 1.0, got {}",
            threshold
        );
        self.success_threshold = threshold;
        self
    }

    /// Returns the expected number of tiles that will be prefetched.
    pub fn expected_tile_count(&self) -> u32 {
        let side = 2 * self.radius_tiles + 1;
        side * side
    }

    /// Returns the center coordinates (lat, lon).
    pub fn center(&self) -> (f64, f64) {
        (self.lat, self.lon)
    }

    /// Returns the zoom level.
    pub fn zoom(&self) -> u8 {
        self.zoom
    }

    /// Returns the radius in tiles.
    pub fn radius_tiles(&self) -> u32 {
        self.radius_tiles
    }
}

impl<F> Job for TilePrefetchJob<F>
where
    F: DdsJobFactory,
{
    fn id(&self) -> JobId {
        self.id.clone()
    }

    fn name(&self) -> &str {
        "TilePrefetch"
    }

    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::PartialSuccess {
            threshold: self.success_threshold,
        }
    }

    fn priority(&self) -> Priority {
        // Prefetch jobs run at lower priority than on-demand requests
        Priority::PREFETCH
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        vec![Box::new(GenerateTileListTask::new(
            self.lat,
            self.lon,
            self.zoom,
            self.radius_tiles,
            Arc::clone(&self.factory),
            Priority::PREFETCH,
        ))]
    }

    fn on_complete(&self, result: &JobResult) -> JobStatus {
        // Calculate success ratio from children
        let total_children = result.succeeded_children.len() + result.failed_children.len();

        if total_children == 0 {
            // No children spawned - could be due to task failure or cancellation
            if result.failed_tasks.is_empty() {
                // No failures at all - edge case, treat as success
                return JobStatus::Succeeded;
            } else {
                // Task failed before spawning children
                return JobStatus::Failed;
            }
        }

        let success_ratio = result.succeeded_children.len() as f64 / total_children as f64;

        if success_ratio >= self.success_threshold {
            JobStatus::Succeeded
        } else {
            JobStatus::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;

    /// Mock factory for testing
    struct MockFactory;

    struct MockJob {
        id: JobId,
    }

    impl Job for MockJob {
        fn id(&self) -> JobId {
            self.id.clone()
        }
        fn name(&self) -> &str {
            "MockDdsGenerate"
        }
        fn error_policy(&self) -> ErrorPolicy {
            ErrorPolicy::FailFast
        }
        fn priority(&self) -> Priority {
            Priority::PREFETCH
        }
        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            vec![]
        }
        fn on_complete(&self, _: &JobResult) -> JobStatus {
            JobStatus::Succeeded
        }
    }

    impl DdsJobFactory for MockFactory {
        fn create_job(&self, tile: TileCoord, _priority: Priority) -> Box<dyn Job> {
            Box::new(MockJob {
                id: JobId::new(format!("mock-{}_{}", tile.row, tile.col)),
            })
        }
    }

    #[test]
    fn test_job_id_format() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(47.60, -122.33, 14, 5, factory);

        let id = job.id();
        assert!(id.as_str().contains("prefetch-"));
        assert!(id.as_str().contains("47.60"));
        assert!(id.as_str().contains("-122.33"));
        assert!(id.as_str().contains("ZL14"));
        assert!(id.as_str().contains("r5"));
    }

    #[test]
    fn test_job_name() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);
        assert_eq!(job.name(), "TilePrefetch");
    }

    #[test]
    fn test_job_priority() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);
        assert_eq!(job.priority(), Priority::PREFETCH);
    }

    #[test]
    fn test_error_policy_default_threshold() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        match job.error_policy() {
            ErrorPolicy::PartialSuccess { threshold } => {
                assert!((threshold - 0.8).abs() < f64::EPSILON);
            }
            _ => panic!("Expected PartialSuccess error policy"),
        }
    }

    #[test]
    fn test_custom_threshold() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(0.5);

        match job.error_policy() {
            ErrorPolicy::PartialSuccess { threshold } => {
                assert!((threshold - 0.5).abs() < f64::EPSILON);
            }
            _ => panic!("Expected PartialSuccess error policy"),
        }
    }

    #[test]
    #[should_panic(expected = "Threshold must be between 0.0 and 1.0")]
    fn test_invalid_threshold_too_high() {
        let factory = Arc::new(MockFactory);
        TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(1.5);
    }

    #[test]
    #[should_panic(expected = "Threshold must be between 0.0 and 1.0")]
    fn test_invalid_threshold_negative() {
        let factory = Arc::new(MockFactory);
        TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(-0.1);
    }

    #[test]
    fn test_expected_tile_count() {
        let factory = Arc::new(MockFactory);

        let job = TilePrefetchJob::new(0.0, 0.0, 10, 0, Arc::clone(&factory));
        assert_eq!(job.expected_tile_count(), 1);

        let job = TilePrefetchJob::new(0.0, 0.0, 10, 1, Arc::clone(&factory));
        assert_eq!(job.expected_tile_count(), 9);

        let job = TilePrefetchJob::new(0.0, 0.0, 10, 5, Arc::clone(&factory));
        assert_eq!(job.expected_tile_count(), 121);
    }

    #[test]
    fn test_accessors() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(47.6, -122.3, 14, 5, factory);

        assert_eq!(job.center(), (47.6, -122.3));
        assert_eq!(job.zoom(), 14);
        assert_eq!(job.radius_tiles(), 5);
    }

    /// Helper to create a JobResult for testing
    fn make_result(
        failed_tasks: Vec<String>,
        succeeded_children: Vec<JobId>,
        failed_children: Vec<JobId>,
    ) -> JobResult {
        JobResult {
            succeeded_tasks: vec![],
            failed_tasks,
            cancelled_tasks: vec![],
            succeeded_children,
            failed_children,
            cancelled_children: vec![],
            duration: std::time::Duration::from_secs(0),
            output_data: None,
        }
    }

    #[test]
    fn test_on_complete_success_above_threshold() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(0.8);

        // 90 succeeded, 10 failed = 90% success
        let result = make_result(
            vec![],
            (0..90)
                .map(|i| JobId::new(format!("child-{}", i)))
                .collect(),
            (0..10)
                .map(|i| JobId::new(format!("failed-{}", i)))
                .collect(),
        );

        assert_eq!(job.on_complete(&result), JobStatus::Succeeded);
    }

    #[test]
    fn test_on_complete_failure_below_threshold() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(0.8);

        // 70 succeeded, 30 failed = 70% success (below 80%)
        let result = make_result(
            vec![],
            (0..70)
                .map(|i| JobId::new(format!("child-{}", i)))
                .collect(),
            (0..30)
                .map(|i| JobId::new(format!("failed-{}", i)))
                .collect(),
        );

        assert_eq!(job.on_complete(&result), JobStatus::Failed);
    }

    #[test]
    fn test_on_complete_no_children_no_failures() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        let result = make_result(vec![], vec![], vec![]);

        // No children and no failures = success (edge case)
        assert_eq!(job.on_complete(&result), JobStatus::Succeeded);
    }

    #[test]
    fn test_on_complete_no_children_with_task_failure() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        let result = make_result(vec!["GenerateTileList".to_string()], vec![], vec![]);

        // Task failed before spawning children
        assert_eq!(job.on_complete(&result), JobStatus::Failed);
    }

    #[test]
    fn test_on_complete_exact_threshold() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory).with_threshold(0.8);

        // Exactly 80% = should succeed
        let result = make_result(
            vec![],
            (0..80)
                .map(|i| JobId::new(format!("child-{}", i)))
                .collect(),
            (0..20)
                .map(|i| JobId::new(format!("failed-{}", i)))
                .collect(),
        );

        assert_eq!(job.on_complete(&result), JobStatus::Succeeded);
    }

    #[test]
    fn test_on_complete_all_success() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        let result = make_result(
            vec![],
            (0..121)
                .map(|i| JobId::new(format!("child-{}", i)))
                .collect(),
            vec![],
        );

        assert_eq!(job.on_complete(&result), JobStatus::Succeeded);
    }

    #[test]
    fn test_on_complete_all_failure() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        let result = make_result(
            vec![],
            vec![],
            (0..121)
                .map(|i| JobId::new(format!("failed-{}", i)))
                .collect(),
        );

        assert_eq!(job.on_complete(&result), JobStatus::Failed);
    }

    #[test]
    fn test_create_tasks() {
        let factory = Arc::new(MockFactory);
        let job = TilePrefetchJob::new(0.0, 0.0, 10, 2, factory);

        let tasks = job.create_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name(), "GenerateTileList");
    }
}
