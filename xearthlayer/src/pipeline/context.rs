//! Pipeline context containing shared resources.
//!
//! The `PipelineContext` provides access to all shared resources needed by
//! pipeline stages, including HTTP clients, caches, encoders, and configuration.

use crate::config::{
    default_cpu_concurrent, default_http_concurrent, default_prefetch_in_flight,
    DEFAULT_COALESCE_CHANNEL_CAPACITY, DEFAULT_MAX_CONCURRENT_DOWNLOADS, DEFAULT_MAX_RETRIES,
    DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_RETRY_BASE_DELAY_MS,
};
use crate::dds::DdsFormat;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for the pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// HTTP request timeout for chunk downloads
    pub request_timeout: Duration,

    /// Maximum retry attempts per chunk
    pub max_retries: u32,

    /// DDS compression format
    pub dds_format: DdsFormat,

    /// Number of mipmap levels to generate
    pub mipmap_count: usize,

    /// Maximum concurrent downloads per job (up to 256)
    /// This limits concurrency within a single tile's download stage.
    pub max_concurrent_downloads: usize,

    /// Global maximum concurrent HTTP requests across all jobs.
    /// This prevents overwhelming the network stack when multiple tiles
    /// are being processed simultaneously.
    ///
    /// Recommended: `num_cpus * 16` capped at 256.
    /// - 4-core system: 64-128 concurrent requests
    /// - 8-core system: 128-256 concurrent requests
    /// - 12+ core system: 256 concurrent requests (cap)
    pub max_global_http_requests: usize,

    /// Maximum concurrent prefetch jobs in flight.
    /// This prevents prefetch from overwhelming the system when using
    /// large radial prefetch configurations.
    ///
    /// Default: `max(num_cpus / 4, 2)` - leaves 75% of resources for on-demand.
    /// Each prefetch job processes one tile through the full pipeline.
    pub max_prefetch_in_flight: usize,

    /// Base delay in milliseconds for exponential backoff between retries.
    /// Actual delay = base_delay * 2^attempt (e.g., 100ms, 200ms, 400ms, 800ms)
    ///
    /// Default: 100
    pub retry_base_delay_ms: u64,

    /// Broadcast channel capacity for request coalescing.
    /// Higher values allow more concurrent waiters for the same tile.
    ///
    /// Default: 16
    pub coalesce_channel_capacity: usize,

    /// Maximum concurrent CPU-bound operations (assemble + encode stages).
    /// Shared limiter prevents blocking thread pool exhaustion.
    ///
    /// Default: `num_cpus * 1.25`, minimum `num_cpus + 2`
    pub max_cpu_concurrent: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
            dds_format: DdsFormat::BC1,
            mipmap_count: 5,
            max_concurrent_downloads: DEFAULT_MAX_CONCURRENT_DOWNLOADS,
            max_global_http_requests: default_http_concurrent(),
            max_prefetch_in_flight: default_prefetch_in_flight(),
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            coalesce_channel_capacity: DEFAULT_COALESCE_CHANNEL_CAPACITY,
            max_cpu_concurrent: default_cpu_concurrent(),
        }
    }
}

// Note: Default calculation functions are now in crate::config module.
// Use default_http_concurrent(), default_prefetch_in_flight(), default_cpu_concurrent() from there.

use crate::pipeline::executor::BlockingExecutor;

/// Shared context for pipeline stages.
///
/// This struct holds references to all shared resources that pipeline stages
/// need. It's designed to be cheaply cloneable (via Arc) for passing to
/// spawned tasks.
#[derive(Clone)]
pub struct PipelineContext<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    /// Provider for downloading chunks
    pub provider: Arc<P>,

    /// Texture encoder
    pub encoder: Arc<E>,

    /// Memory cache (tile-level)
    pub memory_cache: Arc<M>,

    /// Disk cache (chunk-level)
    pub disk_cache: Arc<D>,

    /// Executor for blocking operations
    pub executor: Arc<X>,

    /// Pipeline configuration
    pub config: PipelineConfig,
}

impl<P, E, M, D, X> PipelineContext<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    /// Creates a new pipeline context.
    pub fn new(
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
        }
    }
}

/// Trait for async chunk providers.
///
/// This is the async equivalent of the existing `Provider` trait.
/// Implementations download individual 256x256 chunks from satellite imagery services.
pub trait ChunkProvider: Send + Sync + 'static {
    /// Downloads a chunk asynchronously.
    ///
    /// # Arguments
    ///
    /// * `row` - Global row coordinate at chunk resolution
    /// * `col` - Global column coordinate at chunk resolution
    /// * `zoom` - Zoom level (chunk zoom, typically tile_zoom + 4)
    ///
    /// # Returns
    ///
    /// Raw image data (JPEG) on success.
    fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ChunkDownloadError>> + Send;

    /// Returns the provider name for logging.
    fn name(&self) -> &str;
}

/// Errors from chunk downloads.
#[derive(Debug, Clone)]
pub struct ChunkDownloadError {
    pub message: String,
    pub is_retryable: bool,
}

impl ChunkDownloadError {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_retryable: true,
        }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_retryable: false,
        }
    }
}

impl std::fmt::Display for ChunkDownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ChunkDownloadError {}

/// Trait for async texture encoding.
///
/// Wraps the existing `TextureEncoder` for use with `spawn_blocking`.
pub trait TextureEncoderAsync: Send + Sync + 'static {
    /// Encodes an RGBA image to DDS format.
    ///
    /// This is expected to be CPU-intensive and should be called via
    /// `spawn_blocking`.
    fn encode(&self, image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError>;

    /// Returns the expected DDS file size for given dimensions.
    fn expected_size(&self, width: u32, height: u32) -> usize;
}

/// Errors from texture encoding.
#[derive(Debug, Clone)]
pub struct TextureEncodeError {
    pub message: String,
}

impl TextureEncodeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TextureEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TextureEncodeError {}

/// Trait for memory cache (tile-level).
///
/// Memory cache stores complete DDS tiles for fast repeated access.
/// Operations are synchronous since memory access is fast.
pub trait MemoryCache: Send + Sync + 'static {
    /// Gets a tile from the cache.
    fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>>;

    /// Stores a tile in the cache.
    fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>);

    /// Returns current cache size in bytes.
    fn size_bytes(&self) -> usize;

    /// Returns number of cached tiles.
    fn entry_count(&self) -> usize;
}

/// Trait for disk cache (chunk-level).
///
/// Disk cache stores individual JPEG chunks for persistence across sessions.
/// Operations are async since they involve disk I/O.
pub trait DiskCache: Send + Sync + 'static {
    /// Gets a chunk from the cache.
    fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;

    /// Stores a chunk in the cache.
    fn put(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_config_default() {
        let config = PipelineConfig::default();

        assert_eq!(config.request_timeout, Duration::from_secs(10));
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.dds_format, DdsFormat::BC1);
        assert_eq!(config.mipmap_count, 5);
        assert_eq!(config.max_concurrent_downloads, 256);
        // Global HTTP should be CPU-based, between 64 and 256
        assert!(config.max_global_http_requests >= 64);
        assert!(config.max_global_http_requests <= 256);
    }

    #[test]
    fn test_default_global_http_requests() {
        let default = default_http_concurrent();
        // Should be num_cpus * 16, capped at 256
        assert!(default >= 64); // At least 4 CPUs * 16
        assert!(default <= 256); // Capped at 256
    }

    #[test]
    fn test_chunk_download_error_retryable() {
        let err = ChunkDownloadError::retryable("timeout");
        assert!(err.is_retryable);
        assert_eq!(err.message, "timeout");
    }

    #[test]
    fn test_chunk_download_error_permanent() {
        let err = ChunkDownloadError::permanent("404 not found");
        assert!(!err.is_retryable);
        assert_eq!(err.message, "404 not found");
    }

    #[test]
    fn test_texture_encode_error() {
        let err = TextureEncodeError::new("invalid dimensions");
        assert_eq!(format!("{}", err), "invalid dimensions");
    }
}
