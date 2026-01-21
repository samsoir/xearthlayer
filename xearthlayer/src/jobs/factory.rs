//! DDS job factory for creating DdsGenerateJob instances.
//!
//! The factory pattern encapsulates the complex dependency wiring needed to create
//! `DdsGenerateJob` instances. This allows tasks that spawn child jobs (like
//! `GenerateTileListTask`) to remain lean and focused on their core logic.
//!
//! # HTTP Concurrency
//!
//! The factory creates a **shared HTTP semaphore** that is passed to all jobs.
//! This ensures that regardless of how many tiles are downloading concurrently,
//! the total HTTP requests to the imagery provider stay within bounds.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::jobs::{DdsJobFactory, DefaultDdsJobFactory};
//!
//! // Create factory once at startup with all dependencies
//! let factory = DefaultDdsJobFactory::new(
//!     provider, encoder, memory_cache, disk_cache, executor
//! );
//!
//! // Use factory to create jobs without knowing dependency details
//! // All jobs share the same HTTP semaphore for concurrency control
//! let job = factory.create_job(tile, Priority::PREFETCH);
//! ```

use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkProvider, DiskCache, DownloadConfig, MemoryCache, TextureEncoderAsync,
};
use crate::executor::{Job, Priority};
use crate::jobs::DdsGenerateJob;
use std::sync::Arc;

/// Factory trait for creating DDS generation jobs.
///
/// This trait abstracts the creation of `DdsGenerateJob` instances, allowing
/// tasks to spawn child jobs without needing to know about the complex
/// dependencies (provider, encoder, caches, executor).
///
/// # Why Factory Pattern?
///
/// 1. **Encapsulation**: Tasks don't need to hold all DDS job dependencies
/// 2. **Testability**: Mock factories can create test jobs
/// 3. **Reusability**: Factory can be shared across multiple parent jobs
/// 4. **Single wiring point**: Dependencies are configured once at startup
pub trait DdsJobFactory: Send + Sync + 'static {
    /// Creates a new DDS generation job for the given tile.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    /// * `priority` - Priority for the job (affects scheduling order)
    ///
    /// # Returns
    ///
    /// A boxed Job that can be submitted to the executor.
    fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job>;
}

/// Default factory implementation that creates `DdsGenerateJob` instances.
///
/// This factory holds all the dependencies needed to create a complete
/// DDS generation job: provider for downloads, encoder for compression,
/// caches for storage, executor for blocking operations, and the shared
/// HTTP semaphore for concurrency control.
///
/// # HTTP Concurrency Control
///
/// The factory creates a single `DownloadConfig` with a shared HTTP semaphore.
/// All jobs created by this factory share this semaphore, ensuring total
/// concurrent HTTP requests stay within bounds regardless of how many tiles
/// are being processed.
///
/// # Type Parameters
///
/// * `P` - Chunk provider for downloading satellite imagery
/// * `E` - Texture encoder for DDS compression
/// * `M` - Memory cache for storing completed tiles
/// * `D` - Disk cache for storing individual chunks
/// * `X` - Blocking executor for CPU-bound operations
pub struct DefaultDdsJobFactory<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
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

    /// Shared download configuration with HTTP semaphore
    download_config: DownloadConfig,
}

impl<P, E, M, D, X> DefaultDdsJobFactory<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    /// Creates a new factory with the given dependencies.
    ///
    /// Uses default `DownloadConfig` which creates a shared HTTP semaphore
    /// with 256 permits for global HTTP concurrency limiting.
    ///
    /// # Arguments
    ///
    /// * `provider` - Chunk provider for downloading satellite imagery
    /// * `encoder` - Texture encoder for DDS compression
    /// * `memory_cache` - Memory cache for storing completed tiles
    /// * `disk_cache` - Disk cache for storing chunks
    /// * `executor` - Executor for blocking operations
    pub fn new(
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
    ) -> Self {
        Self {
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            download_config: DownloadConfig::default(),
        }
    }

    /// Creates a new factory with custom download configuration.
    ///
    /// Use this to configure custom timeout, retries, or HTTP concurrency.
    ///
    /// # Arguments
    ///
    /// * `provider` - Chunk provider for downloading satellite imagery
    /// * `encoder` - Texture encoder for DDS compression
    /// * `memory_cache` - Memory cache for storing completed tiles
    /// * `disk_cache` - Disk cache for storing chunks
    /// * `executor` - Executor for blocking operations
    /// * `download_config` - Custom download configuration
    pub fn with_config(
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
        download_config: DownloadConfig,
    ) -> Self {
        Self {
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            download_config,
        }
    }
}

impl<P, E, M, D, X> DdsJobFactory for DefaultDdsJobFactory<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job> {
        Box::new(DdsGenerateJob::new(
            tile,
            priority,
            Arc::clone(&self.provider),
            Arc::clone(&self.encoder),
            Arc::clone(&self.memory_cache),
            Arc::clone(&self.disk_cache),
            Arc::clone(&self.executor),
            self.download_config.clone(), // Clone shares the Arc<Semaphore>
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{JobResult, JobStatus};

    /// Mock factory for testing that creates minimal test jobs.
    struct MockDdsJobFactory {
        jobs_created: std::sync::atomic::AtomicUsize,
    }

    impl MockDdsJobFactory {
        fn new() -> Self {
            Self {
                jobs_created: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn jobs_created(&self) -> usize {
            self.jobs_created.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Minimal test job that does nothing
    struct MockJob {
        id: crate::executor::JobId,
        priority: Priority,
    }

    impl Job for MockJob {
        fn id(&self) -> crate::executor::JobId {
            self.id.clone()
        }

        fn name(&self) -> &str {
            "MockDdsGenerate"
        }

        fn error_policy(&self) -> crate::executor::ErrorPolicy {
            crate::executor::ErrorPolicy::FailFast
        }

        fn priority(&self) -> Priority {
            self.priority
        }

        fn create_tasks(&self) -> Vec<Box<dyn crate::executor::Task>> {
            vec![] // No tasks - this is just for testing factory pattern
        }

        fn on_complete(&self, result: &JobResult) -> JobStatus {
            if result.failed_tasks.is_empty() {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
    }

    impl DdsJobFactory for MockDdsJobFactory {
        fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job> {
            self.jobs_created
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(MockJob {
                id: crate::executor::JobId::new(format!(
                    "mock-dds-{}_{}_ZL{}",
                    tile.row, tile.col, tile.zoom
                )),
                priority,
            })
        }
    }

    #[test]
    fn test_mock_factory_creates_jobs() {
        let factory = MockDdsJobFactory::new();

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        };

        let job = factory.create_job(tile, Priority::PREFETCH);

        assert_eq!(job.name(), "MockDdsGenerate");
        assert_eq!(job.priority(), Priority::PREFETCH);
        assert_eq!(factory.jobs_created(), 1);
    }

    #[test]
    fn test_mock_factory_creates_multiple_jobs() {
        let factory = MockDdsJobFactory::new();

        for i in 0..10 {
            let tile = TileCoord {
                row: i,
                col: i * 2,
                zoom: 14,
            };
            factory.create_job(tile, Priority::PREFETCH);
        }

        assert_eq!(factory.jobs_created(), 10);
    }

    #[test]
    fn test_job_id_contains_tile_coords() {
        let factory = MockDdsJobFactory::new();

        let tile = TileCoord {
            row: 12345,
            col: 67890,
            zoom: 16,
        };

        let job = factory.create_job(tile, Priority::ON_DEMAND);

        let id = job.id();
        assert!(id.as_str().contains("12345"), "Job ID should contain row");
        assert!(id.as_str().contains("67890"), "Job ID should contain col");
        assert!(id.as_str().contains("16"), "Job ID should contain zoom");
    }

    #[test]
    fn test_job_priority_propagation() {
        let factory = MockDdsJobFactory::new();

        let tile = TileCoord {
            row: 1,
            col: 1,
            zoom: 10,
        };

        let prefetch_job = factory.create_job(tile, Priority::PREFETCH);
        assert_eq!(prefetch_job.priority(), Priority::PREFETCH);

        let on_demand_job = factory.create_job(tile, Priority::ON_DEMAND);
        assert_eq!(on_demand_job.priority(), Priority::ON_DEMAND);

        let housekeeping_job = factory.create_job(tile, Priority::HOUSEKEEPING);
        assert_eq!(housekeeping_job.priority(), Priority::HOUSEKEEPING);
    }
}
