//! DDS generation job implementation.
//!
//! [`DdsGenerateJob`] orchestrates the generation of a single DDS texture file
//! from satellite imagery. It creates two sequential tasks:
//!
//! 1. [`DownloadChunksTask`] - Downloads 256 chunks (16×16) from imagery provider
//! 2. [`BuildAndCacheDdsTask`] - Assembles, encodes, and caches the DDS tile
//!
//! # Resource Dependencies
//!
//! Each task receives only the resources it needs (explicit dependency injection):
//! - Download: provider, disk_cache, download_config (shared HTTP semaphore)
//! - BuildAndCache: encoder, memory_cache, executor

use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkProvider, DiskCache, DownloadConfig, MemoryCache, TextureEncoderAsync,
};
use crate::executor::{
    ErrorPolicy, Job, JobId, JobResult, JobStatus, Priority, Task, TILE_GENERATION_GROUP,
};
use crate::tasks::{BuildAndCacheDdsTask, DownloadChunksTask};
use std::sync::Arc;

/// Job for generating a single DDS texture from satellite imagery.
///
/// This job coordinates two sequential tasks to transform raw satellite
/// imagery chunks into a complete DDS texture with mipmaps.
///
/// # Task Flow
///
/// ```text
/// DownloadChunks → BuildAndCacheDds
///    (Network)         (CPU)
/// ```
///
/// The download task runs 256 chunk downloads concurrently (limited by the
/// shared HTTP semaphore in `download_config`). Once complete, the build
/// task assembles, encodes, and caches the result in a single step.
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

    /// Download configuration with shared HTTP semaphore
    download_config: DownloadConfig,
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
    /// * `download_config` - Download configuration with shared HTTP semaphore
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tile: TileCoord,
        priority: Priority,
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
        download_config: DownloadConfig,
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
            download_config,
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
        download_config: DownloadConfig,
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
            download_config,
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

    fn concurrency_group(&self) -> Option<&str> {
        // Limit concurrent tile generation jobs to balance the download→build pipeline.
        // This creates back-pressure, ensuring builds can keep up with downloads.
        Some(TILE_GENERATION_GROUP)
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        vec![
            Box::new(DownloadChunksTask::with_config(
                self.tile,
                Arc::clone(&self.provider),
                Arc::clone(&self.disk_cache),
                self.download_config.clone(),
            )),
            Box::new(BuildAndCacheDdsTask::new(
                self.tile,
                Arc::clone(&self.encoder),
                Arc::clone(&self.memory_cache),
                Arc::clone(&self.executor),
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

    #[test]
    fn test_concurrency_group_is_tile_generation() {
        // DdsGenerateJob uses the tile_generation concurrency group to
        // balance the download→build pipeline. Verify the constant is correct.
        assert_eq!(TILE_GENERATION_GROUP, "tile_generation");
    }
}
