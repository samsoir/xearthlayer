//! Cache garbage collection job.
//!
//! [`CacheGcJob`] orchestrates garbage collection for the disk cache by:
//!
//! 1. Querying the LRU index for eviction candidates
//! 2. Splitting candidates into batches
//! 3. Creating [`CacheGcBatchTask`] for each batch
//!
//! # Priority
//!
//! Uses `Priority::HOUSEKEEPING` (-50), the lowest priority, ensuring
//! GC yields to on-demand tile generation and prefetch operations.
//!
//! # Error Policy
//!
//! Uses `ErrorPolicy::ContinueOnError` since partial cleanup is better
//! than no cleanup. Individual file deletion failures don't fail the job.
//!
//! # Batching
//!
//! Files are deleted in small batches (default 25) to allow higher-priority
//! work to preempt the GC. Each batch is a separate task with its own
//! cancellation check.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::LruIndex;
use crate::executor::{ErrorPolicy, Job, JobId, JobResult, JobStatus, Priority, Task};
use crate::tasks::CacheGcBatchTask;

/// Default batch size for GC operations.
pub const DEFAULT_BATCH_SIZE: usize = 25;

/// Default minimum age for eviction candidates (1 minute).
pub const DEFAULT_MIN_AGE_SECS: u64 = 60;

/// Job for garbage collecting disk cache entries.
///
/// This job queries the LRU index for eviction candidates (oldest entries)
/// and creates batched deletion tasks. Using batches allows the executor
/// to preempt GC for higher-priority work.
///
/// # Example
///
/// ```ignore
/// let gc_job = CacheGcJob::new(lru_index, cache_path, target_size);
/// executor.submit(gc_job).await;
/// ```
pub struct CacheGcJob {
    /// Unique job ID.
    id: JobId,

    /// LRU index for querying and updating cache state.
    lru_index: Arc<LruIndex>,

    /// Base cache directory path.
    cache_path: PathBuf,

    /// Target size to reduce cache to (bytes).
    target_size_bytes: u64,

    /// Number of files per deletion batch.
    batch_size: usize,

    /// Minimum age for eviction candidates.
    min_age: Duration,
}

impl CacheGcJob {
    /// Creates a new cache GC job.
    ///
    /// # Arguments
    ///
    /// * `lru_index` - LRU index for cache entries
    /// * `cache_path` - Base cache directory
    /// * `target_size_bytes` - Target size to reduce cache to
    pub fn new(lru_index: Arc<LruIndex>, cache_path: PathBuf, target_size_bytes: u64) -> Self {
        Self {
            id: JobId::auto(),
            lru_index,
            cache_path,
            target_size_bytes,
            batch_size: DEFAULT_BATCH_SIZE,
            min_age: Duration::from_secs(DEFAULT_MIN_AGE_SECS),
        }
    }

    /// Creates a GC job with a custom job ID.
    pub fn with_id(
        id: JobId,
        lru_index: Arc<LruIndex>,
        cache_path: PathBuf,
        target_size_bytes: u64,
    ) -> Self {
        Self {
            id,
            lru_index,
            cache_path,
            target_size_bytes,
            batch_size: DEFAULT_BATCH_SIZE,
            min_age: Duration::from_secs(DEFAULT_MIN_AGE_SECS),
        }
    }

    /// Sets the batch size for deletion tasks.
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Sets the minimum age for eviction candidates.
    pub fn with_min_age(mut self, min_age: Duration) -> Self {
        self.min_age = min_age;
        self
    }

    /// Returns the target size in bytes.
    pub fn target_size_bytes(&self) -> u64 {
        self.target_size_bytes
    }

    /// Returns the current cache size in bytes.
    pub fn current_size_bytes(&self) -> u64 {
        self.lru_index.total_size()
    }

    /// Calculates how many bytes need to be freed.
    pub fn bytes_to_free(&self) -> u64 {
        self.lru_index
            .total_size()
            .saturating_sub(self.target_size_bytes)
    }
}

impl Job for CacheGcJob {
    fn id(&self) -> JobId {
        self.id.clone()
    }

    fn name(&self) -> &str {
        "CacheGc"
    }

    fn priority(&self) -> Priority {
        Priority::HOUSEKEEPING
    }

    fn error_policy(&self) -> ErrorPolicy {
        // Partial cleanup is better than no cleanup
        ErrorPolicy::ContinueOnError
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        let current_size = self.lru_index.total_size();

        // Skip if already under target
        if current_size <= self.target_size_bytes {
            tracing::debug!(
                current_mb = current_size / 1_000_000,
                target_mb = self.target_size_bytes / 1_000_000,
                "Cache already under target, skipping GC"
            );
            return vec![];
        }

        let bytes_to_free = current_size.saturating_sub(self.target_size_bytes);

        // Estimate how many files we need (assume average 20KB per file, typical DDS tile)
        // We request 2x the estimate to ensure we have enough candidates
        let estimated_files = (bytes_to_free / 20_000).max(100) as usize;
        let max_candidates = estimated_files * 2;

        // Get eviction candidates from LRU index
        let candidates = self
            .lru_index
            .get_eviction_candidates(self.min_age, max_candidates);

        if candidates.is_empty() {
            tracing::debug!(
                current_mb = current_size / 1_000_000,
                min_age_secs = self.min_age.as_secs(),
                "No eviction candidates found (all files too recent)"
            );
            return vec![];
        }

        tracing::info!(
            candidates = candidates.len(),
            bytes_to_free_mb = bytes_to_free / 1_000_000,
            batch_size = self.batch_size,
            "Creating GC tasks"
        );

        // Log sample of candidate sizes to diagnose selection issues
        if !candidates.is_empty() {
            let sample_size = candidates.len().min(5);
            let sample_sizes: Vec<_> = candidates[..sample_size]
                .iter()
                .map(|c| c.metadata.size_bytes)
                .collect();
            let zero_count = candidates
                .iter()
                .filter(|c| c.metadata.size_bytes == 0)
                .count();
            tracing::debug!(
                sample_sizes = ?sample_sizes,
                zero_size_count = zero_count,
                total_candidates = candidates.len(),
                "GC candidate size diagnostics"
            );
        }

        // Collect enough bytes to free
        let mut selected_keys = Vec::new();
        let mut selected_bytes = 0u64;

        for candidate in candidates {
            selected_keys.push(candidate.key);
            selected_bytes += candidate.metadata.size_bytes;

            // Stop once we have enough to free
            if selected_bytes >= bytes_to_free {
                break;
            }
        }

        // Split into batches
        let batches: Vec<Vec<String>> = selected_keys
            .chunks(self.batch_size)
            .map(|chunk| chunk.to_vec())
            .collect();

        tracing::debug!(
            batches = batches.len(),
            files_selected = selected_keys.len(),
            selected_bytes_mb = selected_bytes / 1_000_000,
            bytes_to_free_mb = bytes_to_free / 1_000_000,
            "GC batches created"
        );

        // Create tasks for each batch
        batches
            .into_iter()
            .map(|keys| {
                Box::new(CacheGcBatchTask::new(
                    keys,
                    Arc::clone(&self.lru_index),
                    self.cache_path.clone(),
                )) as Box<dyn Task>
            })
            .collect()
    }

    fn on_complete(&self, result: &JobResult) -> JobStatus {
        // GC jobs always "succeed" even with partial failures
        // Log the outcome for visibility
        let succeeded = result.succeeded_tasks.len();
        let failed = result.failed_tasks.len();

        if failed > 0 {
            tracing::warn!(
                succeeded_batches = succeeded,
                failed_batches = failed,
                "GC completed with some batch failures"
            );
        } else if succeeded > 0 {
            tracing::info!(
                batches = succeeded,
                final_size_mb = self.lru_index.total_size() / 1_000_000,
                "GC completed successfully"
            );
        }

        // Always report success - partial cleanup is fine
        JobStatus::Succeeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ─────────────────────────────────────────────────────────────────────────
    // Helper functions
    // ─────────────────────────────────────────────────────────────────────────

    fn create_test_setup() -> (TempDir, Arc<LruIndex>) {
        let temp_dir = TempDir::new().unwrap();
        let index = Arc::new(LruIndex::new(temp_dir.path().to_path_buf()));
        (temp_dir, index)
    }

    fn write_cache_file(dir: &TempDir, key: &str, size: usize) {
        let filename = LruIndex::key_to_filename(key);
        let path = dir.path().join(&filename);
        std::fs::write(&path, vec![0u8; size]).unwrap();
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Basic tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_job_name() {
        let (temp_dir, index) = create_test_setup();
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 1000);

        assert_eq!(job.name(), "CacheGc");
    }

    #[test]
    fn gc_job_priority_is_housekeeping() {
        let (temp_dir, index) = create_test_setup();
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 1000);

        assert_eq!(job.priority(), Priority::HOUSEKEEPING);
        assert!(job.priority().value() < Priority::PREFETCH.value());
        assert!(job.priority().value() < Priority::ON_DEMAND.value());
    }

    #[test]
    fn gc_job_error_policy_is_continue() {
        let (temp_dir, index) = create_test_setup();
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 1000);

        assert_eq!(job.error_policy(), ErrorPolicy::ContinueOnError);
    }

    #[test]
    fn gc_job_bytes_to_free() {
        let (temp_dir, index) = create_test_setup();

        // Add 10KB to index
        index.record("tile:15:100:200", 5000);
        index.record("tile:15:100:201", 5000);

        // Target is 3KB - need to free 7KB
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 3000);

        assert_eq!(job.bytes_to_free(), 7000);
        assert_eq!(job.target_size_bytes(), 3000);
        assert_eq!(job.current_size_bytes(), 10000);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Task creation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_job_creates_no_tasks_when_under_target() {
        let (temp_dir, index) = create_test_setup();

        // Add 1KB to index
        index.record("tile:15:100:200", 1000);

        // Target is 10KB - no need to GC
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 10000);
        let tasks = job.create_tasks();

        assert!(tasks.is_empty());
    }

    #[test]
    fn gc_job_creates_batched_tasks() {
        let (temp_dir, index) = create_test_setup();

        // Add entries with old access times
        // We need to add files that are old enough to be eviction candidates
        for i in 0..10 {
            let key = format!("tile:15:100:{}", i);
            write_cache_file(&temp_dir, &key, 1000);
            index.record(&key, 1000);
        }

        // Sleep to ensure entries are "old enough"
        std::thread::sleep(Duration::from_millis(100));

        // Target is 2KB with 10KB in cache - need to free 8KB
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 2000)
            .with_min_age(Duration::from_millis(50))
            .with_batch_size(3);

        let tasks = job.create_tasks();

        // Should have created tasks (batches of 3)
        assert!(!tasks.is_empty());
        // All tasks should be CacheGcBatch
        for task in &tasks {
            assert_eq!(task.name(), "CacheGcBatch");
        }
    }

    #[test]
    fn gc_job_respects_batch_size() {
        let (temp_dir, index) = create_test_setup();

        // Add 10 entries
        for i in 0..10 {
            let key = format!("tile:15:100:{}", i);
            index.record(&key, 1000);
        }

        std::thread::sleep(Duration::from_millis(100));

        // Batch size of 2 with 10 entries over target = ~5 batches
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 0)
            .with_min_age(Duration::from_millis(50))
            .with_batch_size(2);

        let tasks = job.create_tasks();

        // Should have multiple batches
        assert!(tasks.len() > 1);
    }

    #[test]
    fn gc_job_with_custom_id() {
        let (temp_dir, index) = create_test_setup();
        let custom_id = JobId::new("custom-gc-001");

        let job = CacheGcJob::with_id(
            custom_id.clone(),
            index,
            temp_dir.path().to_path_buf(),
            1000,
        );

        assert_eq!(job.id().as_str(), "custom-gc-001");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Completion tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_job_always_succeeds_on_complete() {
        let (temp_dir, index) = create_test_setup();
        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 1000);

        // Even with failed tasks, GC reports success
        let result_with_failures = JobResult {
            succeeded_tasks: vec!["batch1".to_string()],
            failed_tasks: vec!["batch2".to_string()],
            ..Default::default()
        };

        assert_eq!(job.on_complete(&result_with_failures), JobStatus::Succeeded);

        // And obviously with all successes
        let result_success = JobResult {
            succeeded_tasks: vec!["batch1".to_string(), "batch2".to_string()],
            ..Default::default()
        };

        assert_eq!(job.on_complete(&result_success), JobStatus::Succeeded);
    }

    #[test]
    fn gc_job_builder_methods() {
        let (temp_dir, index) = create_test_setup();

        let job = CacheGcJob::new(index, temp_dir.path().to_path_buf(), 5000)
            .with_batch_size(50)
            .with_min_age(Duration::from_secs(120));

        assert_eq!(job.batch_size, 50);
        assert_eq!(job.min_age, Duration::from_secs(120));
    }
}
