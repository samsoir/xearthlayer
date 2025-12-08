//! Pipeline context containing shared resources.
//!
//! The `PipelineContext` provides access to all shared resources needed by
//! pipeline stages, including HTTP clients, caches, encoders, and configuration.

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
    pub max_concurrent_downloads: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            max_retries: 3,
            dds_format: DdsFormat::BC1,
            mipmap_count: 5,
            max_concurrent_downloads: 256,
        }
    }
}

/// Shared context for pipeline stages.
///
/// This struct holds references to all shared resources that pipeline stages
/// need. It's designed to be cheaply cloneable (via Arc) for passing to
/// spawned tasks.
#[derive(Clone)]
pub struct PipelineContext<P, E, M, D>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
{
    /// Provider for downloading chunks
    pub provider: Arc<P>,

    /// Texture encoder
    pub encoder: Arc<E>,

    /// Memory cache (tile-level)
    pub memory_cache: Arc<M>,

    /// Disk cache (chunk-level)
    pub disk_cache: Arc<D>,

    /// Pipeline configuration
    pub config: PipelineConfig,
}

impl<P, E, M, D> PipelineContext<P, E, M, D>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
{
    /// Creates a new pipeline context.
    pub fn new(
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            provider,
            encoder,
            memory_cache,
            disk_cache,
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
