//! Cache GC batch deletion task.
//!
//! [`CacheGcBatchTask`] deletes a batch of cache files as part of garbage
//! collection. Each batch is small (default 25 files) to allow yielding
//! to higher-priority work.
//!
//! # Resource Type
//!
//! Uses `ResourceType::DiskIO` since this task performs file deletions.
//!
//! # Cancellation
//!
//! The task checks for cancellation between each file deletion, allowing
//! GC to be interrupted when higher-priority work arrives.
//!
//! # Output
//!
//! Returns a [`TaskOutput`] with:
//! - "deleted_count": Number of files deleted
//! - "freed_bytes": Total bytes freed

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::cache::LruIndex;
use crate::executor::{ResourceType, Task, TaskContext, TaskOutput, TaskResult};

/// Output key for the count of deleted files.
pub const OUTPUT_KEY_DELETED_COUNT: &str = "deleted_count";

/// Output key for the bytes freed.
pub const OUTPUT_KEY_FREED_BYTES: &str = "freed_bytes";

/// Task that deletes a batch of cache files.
///
/// This task is designed for incremental garbage collection:
/// - Processes a small batch of files (default 25)
/// - Yields between deletions to allow higher-priority work
/// - Respects cancellation for graceful shutdown
///
/// # Output Keys
///
/// - `deleted_count`: `u64` - Number of files successfully deleted
/// - `freed_bytes`: `u64` - Total bytes freed
pub struct CacheGcBatchTask {
    /// Cache keys to delete in this batch.
    keys: Vec<String>,

    /// LRU index to update after deletions.
    lru_index: Arc<LruIndex>,

    /// Cache directory path (for computing file paths).
    cache_path: PathBuf,
}

impl CacheGcBatchTask {
    /// Creates a new cache GC batch task.
    ///
    /// # Arguments
    ///
    /// * `keys` - Cache keys to delete
    /// * `lru_index` - LRU index to update after deletions
    /// * `cache_path` - Base cache directory
    pub fn new(keys: Vec<String>, lru_index: Arc<LruIndex>, cache_path: PathBuf) -> Self {
        Self {
            keys,
            lru_index,
            cache_path,
        }
    }

    /// Returns the number of files in this batch.
    pub fn batch_size(&self) -> usize {
        self.keys.len()
    }
}

impl Task for CacheGcBatchTask {
    fn name(&self) -> &str {
        "CacheGcBatch"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::DiskIO
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            let mut deleted_count = 0u64;
            let mut freed_bytes = 0u64;

            for key in &self.keys {
                // Check for cancellation between files
                if ctx.is_cancelled() {
                    tracing::debug!(
                        deleted = deleted_count,
                        remaining = self.keys.len() as u64 - deleted_count,
                        "GC batch cancelled"
                    );
                    break;
                }

                // Compute file path from key
                let path = self.cache_path.join(LruIndex::key_to_filename(key));

                // Get file size before deletion (for tracking freed bytes)
                let file_size = match tokio::fs::metadata(&path).await {
                    Ok(meta) => meta.len(),
                    Err(_) => {
                        // File doesn't exist or can't access - remove from index anyway
                        self.lru_index.remove(key);
                        continue;
                    }
                };

                // Delete the file
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {
                        self.lru_index.remove(key);
                        deleted_count += 1;
                        freed_bytes += file_size;

                        tracing::trace!(
                            key = %key,
                            size = file_size,
                            "GC deleted cache file"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // File already gone - just remove from index
                        self.lru_index.remove(key);
                    }
                    Err(e) => {
                        tracing::warn!(
                            key = %key,
                            error = %e,
                            "Failed to delete cache file during GC"
                        );
                        // Continue with next file - partial cleanup is better than none
                    }
                }

                // Yield to allow other tasks to run
                tokio::task::yield_now().await;
            }

            tracing::debug!(
                deleted = deleted_count,
                freed_mb = freed_bytes / 1_000_000,
                "GC batch complete"
            );

            // Return stats as output
            let mut output = TaskOutput::new();
            output.set(OUTPUT_KEY_DELETED_COUNT, deleted_count);
            output.set(OUTPUT_KEY_FREED_BYTES, freed_bytes);

            TaskResult::SuccessWithOutput(output)
        })
    }
}

/// Retrieves the deleted count from task output.
pub fn get_deleted_count_from_output(output: &TaskOutput) -> Option<u64> {
    output.get::<u64>(OUTPUT_KEY_DELETED_COUNT).copied()
}

/// Retrieves the freed bytes from task output.
pub fn get_freed_bytes_from_output(output: &TaskOutput) -> Option<u64> {
    output.get::<u64>(OUTPUT_KEY_FREED_BYTES).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::JobId;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

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
    fn gc_batch_task_name() {
        let (temp_dir, index) = create_test_setup();
        let task = CacheGcBatchTask::new(vec![], index, temp_dir.path().to_path_buf());

        assert_eq!(task.name(), "CacheGcBatch");
    }

    #[test]
    fn gc_batch_task_resource_type() {
        let (temp_dir, index) = create_test_setup();
        let task = CacheGcBatchTask::new(vec![], index, temp_dir.path().to_path_buf());

        assert_eq!(task.resource_type(), ResourceType::DiskIO);
    }

    #[test]
    fn gc_batch_task_batch_size() {
        let (temp_dir, index) = create_test_setup();
        let keys = vec![
            "key:1".to_string(),
            "key:2".to_string(),
            "key:3".to_string(),
        ];
        let task = CacheGcBatchTask::new(keys, index, temp_dir.path().to_path_buf());

        assert_eq!(task.batch_size(), 3);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Deletion tests
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_batch_task_deletes_files() {
        let (temp_dir, index) = create_test_setup();

        // Create cache files
        write_cache_file(&temp_dir, "tile:15:100:200", 1000);
        write_cache_file(&temp_dir, "tile:15:100:201", 2000);

        // Record in index
        index.record("tile:15:100:200", 1000);
        index.record("tile:15:100:201", 2000);

        assert_eq!(index.entry_count(), 2);
        assert_eq!(index.total_size(), 3000);

        // Create and execute task
        let keys = vec!["tile:15:100:200".to_string(), "tile:15:100:201".to_string()];
        let task = CacheGcBatchTask::new(keys, index.clone(), temp_dir.path().to_path_buf());

        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        let result = task.execute(&mut ctx).await;

        // Verify success
        assert!(result.is_success());

        // Verify files deleted
        assert!(!temp_dir.path().join("tile_15_100_200.cache").exists());
        assert!(!temp_dir.path().join("tile_15_100_201.cache").exists());

        // Verify index updated
        assert_eq!(index.entry_count(), 0);
        assert_eq!(index.total_size(), 0);
    }

    #[tokio::test]
    async fn gc_batch_task_handles_missing_files() {
        let (temp_dir, index) = create_test_setup();

        // Record in index but DON'T create the file
        index.record("tile:15:100:200", 1000);

        let keys = vec!["tile:15:100:200".to_string()];
        let task = CacheGcBatchTask::new(keys, index.clone(), temp_dir.path().to_path_buf());

        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        let result = task.execute(&mut ctx).await;

        // Should still succeed (missing file is not an error)
        assert!(result.is_success());

        // Index should be cleaned up
        assert_eq!(index.entry_count(), 0);
    }

    #[tokio::test]
    async fn gc_batch_task_returns_stats() {
        let (temp_dir, index) = create_test_setup();

        // Create cache files
        write_cache_file(&temp_dir, "tile:15:100:200", 1000);
        write_cache_file(&temp_dir, "tile:15:100:201", 2000);

        index.record("tile:15:100:200", 1000);
        index.record("tile:15:100:201", 2000);

        let keys = vec!["tile:15:100:200".to_string(), "tile:15:100:201".to_string()];
        let task = CacheGcBatchTask::new(keys, index.clone(), temp_dir.path().to_path_buf());

        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        let result = task.execute(&mut ctx).await;

        // Extract output
        if let TaskResult::SuccessWithOutput(output) = result {
            let deleted = get_deleted_count_from_output(&output);
            let freed = get_freed_bytes_from_output(&output);

            assert_eq!(deleted, Some(2));
            assert_eq!(freed, Some(3000));
        } else {
            panic!("Expected SuccessWithOutput");
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Cancellation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_batch_task_respects_cancellation() {
        let (temp_dir, index) = create_test_setup();

        // Create 5 cache files
        for i in 0..5 {
            let key = format!("tile:15:100:{}", i);
            write_cache_file(&temp_dir, &key, 1000);
            index.record(&key, 1000);
        }

        let keys: Vec<String> = (0..5).map(|i| format!("tile:15:100:{}", i)).collect();
        let task = CacheGcBatchTask::new(keys, index.clone(), temp_dir.path().to_path_buf());

        // Create a pre-cancelled token
        let token = CancellationToken::new();
        token.cancel();

        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        let result = task.execute(&mut ctx).await;

        // Should still return success (partial completion is fine)
        assert!(result.is_success());

        // But should have deleted 0 files (cancelled before first deletion)
        if let TaskResult::SuccessWithOutput(output) = result {
            let deleted = get_deleted_count_from_output(&output);
            assert_eq!(deleted, Some(0));
        }

        // Files should still exist
        assert!(temp_dir.path().join("tile_15_100_0.cache").exists());
    }

    #[tokio::test]
    async fn gc_batch_task_updates_lru_index() {
        let (temp_dir, index) = create_test_setup();

        // Create and record a file
        write_cache_file(&temp_dir, "tile:15:100:200", 5000);
        index.record("tile:15:100:200", 5000);

        assert!(index.contains("tile:15:100:200"));
        assert_eq!(index.total_size(), 5000);

        // Delete it
        let keys = vec!["tile:15:100:200".to_string()];
        let task = CacheGcBatchTask::new(keys, index.clone(), temp_dir.path().to_path_buf());

        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        task.execute(&mut ctx).await;

        // Index should be updated
        assert!(!index.contains("tile:15:100:200"));
        assert_eq!(index.total_size(), 0);
        assert_eq!(index.entry_count(), 0);
    }

    #[tokio::test]
    async fn gc_batch_task_empty_batch() {
        let (temp_dir, index) = create_test_setup();

        let task = CacheGcBatchTask::new(vec![], index, temp_dir.path().to_path_buf());

        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-gc"), token);

        let result = task.execute(&mut ctx).await;

        assert!(result.is_success());

        if let TaskResult::SuccessWithOutput(output) = result {
            assert_eq!(get_deleted_count_from_output(&output), Some(0));
            assert_eq!(get_freed_bytes_from_output(&output), Some(0));
        }
    }
}
