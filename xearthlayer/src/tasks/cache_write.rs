//! Cache write task implementation.
//!
//! [`CacheWriteTask`] writes the completed DDS tile to the memory cache
//! for fast repeated access.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::CPU` since cache writes are lightweight
//! and don't require network or disk I/O permits.
//!
//! # Input
//!
//! Reads `TaskOutput` key "dds_data" containing `Bytes` from previous task.
//!
//! # Output
//!
//! No output - cache writes are fire-and-forget.

use bytes::Bytes;

use crate::coord::TileCoord;
use crate::executor::{MemoryCache, ResourceType, Task, TaskContext, TaskError, TaskResult};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, Instrument};

/// Task that writes DDS data to the memory cache.
///
/// This task stores the completed DDS tile in the memory cache for fast
/// repeated access. Cache writes never fail the job - caching is an
/// optimization, not a requirement.
///
/// # Inputs
///
/// - Key: "dds_data" (from EncodeDdsTask)
/// - Type: `Vec<u8>`
///
/// # Outputs
///
/// None - cache writes are fire-and-forget.
pub struct CacheWriteTask<M>
where
    M: MemoryCache,
{
    /// Tile coordinates for cache key
    tile: TileCoord,

    /// Memory cache for storing completed DDS tiles
    memory_cache: Arc<M>,
}

impl<M> CacheWriteTask<M>
where
    M: MemoryCache,
{
    /// Creates a new cache write task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates for cache key
    /// * `memory_cache` - Memory cache to write to
    pub fn new(tile: TileCoord, memory_cache: Arc<M>) -> Self {
        Self { tile, memory_cache }
    }
}

impl<M> Task for CacheWriteTask<M>
where
    M: MemoryCache,
{
    fn name(&self) -> &str {
        "CacheWrite"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        let span = tracing::debug_span!(target: "profiling", "task_cache_write");
        Box::pin(
            async move {
                // Check for cancellation before starting
                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let job_id = ctx.job_id();

                // Get DDS data from previous task
                let dds_data: Option<&Bytes> = ctx.get_output("EncodeDds", "dds_data");
                let dds_data = match dds_data {
                    Some(data) => data.clone(),
                    None => {
                        return TaskResult::Failed(TaskError::missing_input("dds_data"));
                    }
                };

                let size = dds_data.len();

                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    size_bytes = size,
                    "Writing to memory cache"
                );

                // Memory cache is async-safe (uses moka internally).
                // Size metrics are reported internally by the provider.
                self.memory_cache
                    .put(self.tile.row, self.tile.col, self.tile.zoom, dds_data)
                    .await;

                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    "Cache write complete"
                );

                TaskResult::Success
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_task_name() {
        assert_eq!("CacheWrite", "CacheWrite");
    }
}
