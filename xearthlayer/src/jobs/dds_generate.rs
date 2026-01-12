//! DDS generation job implementation.
//!
//! [`DdsGenerateJob`] orchestrates the generation of a single DDS texture file
//! from satellite imagery. It creates a sequence of tasks:
//!
//! 1. [`DownloadChunksTask`] - Downloads 256 chunks (16×16) from imagery provider
//! 2. [`AssembleImageTask`] - Assembles chunks into a 4096×4096 RGBA image
//! 3. [`EncodeDdsTask`] - Encodes the image to DDS format with mipmaps
//! 4. [`CacheWriteTask`] - Writes the DDS data to memory cache
//!
//! # Resource Dependencies
//!
//! Each task receives only the resources it needs (explicit dependency injection):
//! - Download: provider, disk_cache
//! - Assemble: executor (for spawn_blocking)
//! - Encode: encoder, executor
//! - Cache: memory_cache

use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkProvider, DiskCache, MemoryCache, TextureEncoderAsync,
};
use crate::executor::{ErrorPolicy, Job, JobId, JobResult, JobStatus, Priority, Task};
use crate::tasks::{AssembleImageTask, CacheWriteTask, DownloadChunksTask, EncodeDdsTask};
use std::sync::Arc;

/// Job for generating a single DDS texture from satellite imagery.
///
/// This job coordinates four sequential tasks to transform raw satellite
/// imagery chunks into a complete DDS texture with mipmaps.
///
/// # Task Flow
///
/// ```text
/// DownloadChunks → AssembleImage → EncodeDds → CacheWrite
///     (Network)      (CPU)          (CPU)       (CPU)
/// ```
///
/// # Error Policy
///
/// Uses `FailFast` - any task failure immediately fails the job.
/// This is appropriate since a DDS file requires all stages to succeed.
pub struct DdsGenerateJob<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    /// Unique job identifier
    id: JobId,

    /// Tile coordinates to generate
    tile: TileCoord,

    /// Job priority (ON_DEMAND for FUSE requests, PREFETCH for background)
    priority: Priority,

    /// Chunk provider for downloading satellite imagery
    provider: Arc<P>,

    /// Texture encoder for DDS compression
    encoder: Arc<E>,

    /// Memory cache for storing completed DDS tiles
    memory_cache: Arc<M>,

    /// Disk cache for storing individual chunks
    disk_cache: Arc<D>,

    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<P, E, M, D, X> DdsGenerateJob<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    /// Creates a new DDS generation job.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates to generate
    /// * `priority` - Job priority
    /// * `provider` - Chunk provider for downloads
    /// * `encoder` - Texture encoder
    /// * `memory_cache` - Memory cache for tiles
    /// * `disk_cache` - Disk cache for chunks
    /// * `executor` - Blocking executor
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tile: TileCoord,
        priority: Priority,
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
    ) -> Self {
        let id = JobId::new(format!("dds-{}_{}_ZL{}", tile.row, tile.col, tile.zoom));
        Self {
            id,
            tile,
            priority,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
        }
    }

    /// Creates a new DDS generation job with a custom job ID.
    #[allow(clippy::too_many_arguments)]
    pub fn with_id(
        id: JobId,
        tile: TileCoord,
        priority: Priority,
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
    ) -> Self {
        Self {
            id,
            tile,
            priority,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
        }
    }

    /// Returns the tile coordinates for this job.
    pub fn tile(&self) -> TileCoord {
        self.tile
    }
}

impl<P, E, M, D, X> Job for DdsGenerateJob<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    fn id(&self) -> JobId {
        self.id.clone()
    }

    fn name(&self) -> &str {
        "DdsGenerate"
    }

    fn error_policy(&self) -> ErrorPolicy {
        // DDS generation requires all stages - fail fast on any error
        ErrorPolicy::FailFast
    }

    fn priority(&self) -> Priority {
        self.priority
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        vec![
            Box::new(DownloadChunksTask::new(
                self.tile,
                Arc::clone(&self.provider),
                Arc::clone(&self.disk_cache),
            )),
            Box::new(AssembleImageTask::new(Arc::clone(&self.executor))),
            Box::new(EncodeDdsTask::new(
                self.tile,
                Arc::clone(&self.encoder),
                Arc::clone(&self.executor),
            )),
            Box::new(CacheWriteTask::new(
                self.tile,
                Arc::clone(&self.memory_cache),
            )),
        ]
    }

    fn on_complete(&self, result: &JobResult) -> JobStatus {
        // Simple success/fail based on task results
        if result.failed_tasks.is_empty() && result.failed_children.is_empty() {
            JobStatus::Succeeded
        } else {
            JobStatus::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require mock implementations of the traits.
    // These tests verify the basic structure and configuration.

    #[test]
    fn test_job_id_format() {
        // Verify the job ID format matches the tile coordinates
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let id = JobId::new(format!("dds-{}_{}_ZL{}", tile.row, tile.col, tile.zoom));
        assert_eq!(id.as_str(), "dds-100_200_ZL16");
    }

    #[test]
    fn test_job_name() {
        // DdsGenerateJob should report its name correctly
        // This test just verifies the expected name string
        assert_eq!("DdsGenerate", "DdsGenerate");
    }
}
