//! XEarthLayer service facade implementation.

use super::builder::{self, CacheComponents, GeneratorComponents, ProviderComponents};
use super::config::ServiceConfig;
use super::dds_handler::DdsHandlerBuilder;
use super::error::ServiceError;
use super::fuse_mount::{FuseMountConfig, FuseMountService};
use super::network_logger::NetworkStatsLogger;
use crate::cache::{Cache, MemoryCache};
use crate::coord::to_tile_coords;
use crate::fuse::{DdsHandler, MountHandle, SpawnedMountHandle};
use crate::log::Logger;
use crate::provider::{AsyncProviderType, Provider, ProviderConfig};
use crate::telemetry::{PipelineMetrics, TelemetrySnapshot};
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use crate::tile::{TileGenerator, TileRequest};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};

/// High-level facade for XEarthLayer operations.
///
/// Encapsulates all component creation and wiring, providing a simplified
/// API for common operations like serving tiles via FUSE or downloading
/// individual tiles.
///
/// # Runtime Management
///
/// The service can be created with either a default runtime or an injected handle:
///
/// ```ignore
/// // Option 1: Default runtime (convenience)
/// let service = XEarthLayerService::new(config, provider_config, logger)?;
///
/// // Option 2: Injected runtime handle (for DI/testing)
/// let runtime = Runtime::new()?;
/// let service = XEarthLayerService::with_runtime(
///     config, provider_config, logger, runtime.handle().clone()
/// )?;
/// ```
///
/// # Example
///
/// ```ignore
/// use xearthlayer::service::{XEarthLayerService, ServiceConfig};
/// use xearthlayer::provider::ProviderConfig;
///
/// // Create service with default configuration
/// let config = ServiceConfig::default();
/// let service = XEarthLayerService::new(config, ProviderConfig::bing(), logger)?;
///
/// // Download a tile
/// let data = service.download_tile(37.7749, -122.4194, 15)?;
/// ```
pub struct XEarthLayerService {
    /// Service configuration
    config: ServiceConfig,
    /// Provider name (for cache and logging)
    provider_name: String,
    /// Provider's maximum supported zoom level
    max_zoom: u8,
    /// Tile generator (handles download + encoding)
    generator: Arc<dyn TileGenerator>,
    /// Cache implementation
    cache: Arc<dyn Cache>,
    /// Logger for diagnostic output
    logger: Arc<dyn Logger>,
    /// Network stats logger (keeps logger thread alive)
    #[allow(dead_code)]
    network_stats_logger: Option<NetworkStatsLogger>,
    /// Owned Tokio runtime (when created via `new()`)
    #[allow(dead_code)]
    owned_runtime: Option<Runtime>,
    /// Handle to the Tokio runtime
    runtime_handle: Handle,
    /// Sync provider for legacy pipeline (TileOrchestrator)
    provider: Arc<dyn Provider>,
    /// Async provider for async pipeline (non-blocking I/O)
    async_provider: Option<Arc<AsyncProviderType>>,
    /// Texture encoder for async pipeline (concrete type for adapter compatibility)
    dds_encoder: Arc<DdsTextureEncoder>,
    /// Shared memory cache for async pipeline (tile-level)
    memory_cache: Option<Arc<MemoryCache>>,
    /// Cache directory for disk cache
    cache_dir: Option<PathBuf>,
    /// Pipeline telemetry metrics
    metrics: Arc<PipelineMetrics>,
}

impl XEarthLayerService {
    /// Create a new XEarthLayer service with a default Tokio runtime.
    ///
    /// This is a convenience constructor that creates its own runtime internally.
    /// For advanced use cases or testing, use [`with_runtime`] instead.
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub fn new(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
    ) -> Result<Self, ServiceError> {
        // Create multi-threaded runtime with worker threads
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_cpus)
            .enable_all()
            .thread_name("xearthlayer-tokio")
            .build()
            .map_err(|e| ServiceError::RuntimeError(format!("failed to create runtime: {}", e)))?;

        tracing::info!(
            worker_threads = num_cpus,
            "Created Tokio runtime with worker threads"
        );

        let handle = runtime.handle().clone();

        Self::build(config, provider_config, logger, handle, Some(runtime))
    }

    /// Create a new XEarthLayer service with a provided runtime handle.
    ///
    /// Use this constructor when you want to control the runtime lifecycle
    /// externally, or for testing with injected runtimes.
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    /// * `runtime_handle` - Handle to an existing Tokio runtime
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub fn with_runtime(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
    ) -> Result<Self, ServiceError> {
        Self::build(config, provider_config, logger, runtime_handle, None)
    }

    /// Internal builder that does the actual construction.
    ///
    /// This method delegates to focused builder functions in the `builder` module
    /// for each component, keeping the overall flow clear and each piece testable.
    fn build(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
        owned_runtime: Option<Runtime>,
    ) -> Result<Self, ServiceError> {
        // 1. Create providers (sync for legacy pipeline, async for new pipeline)
        let ProviderComponents {
            sync_provider: provider,
            async_provider,
            name: provider_name,
            max_zoom,
        } = builder::create_providers(&provider_config, &runtime_handle)?;

        // 2. Create texture encoder
        let dds_encoder = builder::create_encoder(&config);
        let encoder: Arc<dyn TextureEncoder> = Arc::clone(&dds_encoder) as Arc<dyn TextureEncoder>;

        // 3. Create tile generator pipeline
        let GeneratorComponents {
            generator,
            network_stats,
        } = builder::create_generator(&config, Arc::clone(&provider), encoder, logger.clone());

        // 4. Create cache system
        let CacheComponents {
            cache,
            memory_cache,
            cache_dir,
        } = builder::create_cache(&config, &provider_name, logger.clone())?;

        // 5. Create network stats logger (if not in quiet mode)
        let network_stats_logger =
            builder::create_network_logger(&config, network_stats, logger.clone());

        // 6. Create pipeline telemetry metrics
        let metrics = Arc::new(PipelineMetrics::new());

        // 7. Initialize disk cache size from existing cache (async background task)
        builder::init_disk_cache_metrics(cache_dir.as_ref(), &metrics, &runtime_handle);

        Ok(Self {
            config,
            provider_name,
            max_zoom,
            generator,
            cache,
            logger,
            network_stats_logger,
            owned_runtime,
            runtime_handle,
            provider,
            async_provider,
            dds_encoder,
            memory_cache,
            cache_dir,
            metrics,
        })
    }

    /// Get the provider name.
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Get the provider's maximum zoom level.
    pub fn max_zoom(&self) -> u8 {
        self.max_zoom
    }

    /// Check if caching is enabled.
    pub fn cache_enabled(&self) -> bool {
        self.config.cache_enabled()
    }

    /// Get the service configuration.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Get the tile generator.
    pub fn generator(&self) -> &Arc<dyn TileGenerator> {
        &self.generator
    }

    /// Get the cache.
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }

    /// Get the runtime handle.
    pub fn runtime_handle(&self) -> &Handle {
        &self.runtime_handle
    }

    /// Get a snapshot of pipeline telemetry metrics.
    ///
    /// Returns a point-in-time copy of all pipeline metrics, including:
    /// - Job counts (submitted, completed, failed, coalesced)
    /// - Download statistics (chunks, bytes, throughput)
    /// - Cache hit rates (memory and disk)
    /// - Cache sizes (memory and disk)
    /// - Timing information
    ///
    /// The snapshot is safe to use for display without blocking the pipeline.
    ///
    /// Note: This automatically updates cache size metrics before taking
    /// the snapshot, ensuring accurate cache utilization data.
    pub fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        // Update cache sizes from the cache system
        self.update_cache_sizes();
        self.metrics.snapshot()
    }

    /// Update cache size metrics from the cache systems.
    ///
    /// This is called automatically by `telemetry_snapshot()`, but can also
    /// be called manually if you need to update metrics without taking a
    /// snapshot.
    fn update_cache_sizes(&self) {
        // Get memory cache size from the shared async pipeline cache
        if let Some(ref mem_cache) = self.memory_cache {
            self.metrics
                .set_memory_cache_size(mem_cache.size_bytes() as u64);
        }

        // Note: Disk cache size (chunk-level) is tracked incrementally in the
        // download stage via add_disk_cache_bytes(). We don't query CacheSystem
        // here because that tracks tile-level cache, not chunk-level.
    }

    /// Get a reference to the pipeline metrics for direct access.
    ///
    /// This allows external code to record metrics or get raw metric values.
    /// For display purposes, prefer `telemetry_snapshot()` which provides
    /// computed rates and formatted values.
    pub fn metrics(&self) -> &Arc<PipelineMetrics> {
        &self.metrics
    }

    /// Download a single tile for the given coordinates.
    ///
    /// Converts lat/lon coordinates to tile coordinates and generates
    /// the DDS texture data.
    ///
    /// # Arguments
    ///
    /// * `lat` - Latitude in decimal degrees
    /// * `lon` - Longitude in decimal degrees
    /// * `zoom` - Zoom level (chunk resolution, like Ortho4XP: 12-19 for Bing, 12-22 for Google)
    ///
    /// # Returns
    ///
    /// DDS texture data as bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Coordinates are invalid
    /// - Zoom level is out of range for the provider
    /// - Tile generation fails
    pub fn download_tile(&self, lat: f64, lon: f64, zoom: u8) -> Result<Vec<u8>, ServiceError> {
        // Zoom level represents chunk resolution (like Ortho4XP).
        // Each tile is composed of 16x16 chunks, so tile zoom = chunk zoom - 4.
        // Valid chunk zoom range: 12 (min usable) to provider's max_zoom.
        let min_chunk_zoom: u8 = 12;

        if zoom < min_chunk_zoom || zoom > self.max_zoom {
            return Err(ServiceError::InvalidZoom {
                zoom,
                min: min_chunk_zoom,
                max: self.max_zoom,
            });
        }

        // Convert chunk zoom to tile zoom for coordinate conversion
        let tile_zoom = zoom - 4;

        // Convert lat/lon to tile coordinates at the tile zoom level
        let tile =
            to_tile_coords(lat, lon, tile_zoom).map_err(|e| ServiceError::InvalidCoordinates {
                lat,
                lon,
                reason: e.to_string(),
            })?;

        // Create tile request with tile row/col coordinates and tile zoom
        // (TileGenerator will add 4 to get chunk zoom for downloads)
        let request = TileRequest::new(tile.row, tile.col, tile_zoom);

        // Generate tile
        self.generator
            .generate(&request)
            .map_err(ServiceError::from)
    }

    /// Create the DDS handler for the async pipeline.
    ///
    /// This wires up all the pipeline components and returns a handler
    /// that can be used with `Fuse3PassthroughFS`.
    ///
    /// Uses `DdsHandlerBuilder` for clean configuration of the pipeline.
    fn create_dds_handler(&self) -> DdsHandler {
        let mut builder = DdsHandlerBuilder::new(&self.provider_name)
            .with_format(self.config.texture().format())
            .with_mipmap_count(self.config.texture().mipmap_count())
            .with_timeout(Duration::from_secs(self.config.download().timeout_secs()))
            .with_max_retries(self.config.download().max_retries())
            .with_metrics(Arc::clone(&self.metrics));

        // Configure provider (prefer async, fallback to sync)
        if let Some(ref async_prov) = self.async_provider {
            builder = builder.with_async_provider(Arc::clone(async_prov));
        } else {
            builder = builder.with_sync_provider(Arc::clone(&self.provider));
        }

        // Configure disk cache if enabled
        if let Some(ref dir) = self.cache_dir {
            builder = builder.with_disk_cache(dir.clone());
        }

        // Configure memory cache if enabled (use the shared instance)
        if let Some(ref cache) = self.memory_cache {
            builder = builder.with_memory_cache(Arc::clone(cache));
        }

        builder.build(self.runtime_handle.clone())
    }

    /// Calculate the expected DDS file size based on encoder configuration.
    fn expected_dds_size(&self) -> usize {
        self.dds_encoder.expected_size(4096, 4096)
    }

    /// Create a mount configuration for FUSE filesystem.
    fn create_mount_config(&self) -> FuseMountConfig {
        FuseMountConfig::new(self.create_dds_handler(), self.expected_dds_size())
            .with_timeout(Duration::from_secs(
                self.config.generation_timeout().unwrap_or(30),
            ))
            .with_logger(Arc::clone(&self.logger))
    }

    /// Start the passthrough FUSE filesystem server using fuse3 (async multi-threaded).
    ///
    /// Uses the fuse3 library which runs all FUSE operations asynchronously on the
    /// Tokio runtime, enabling true parallel I/O processing. This is optimized for
    /// high-concurrency scenarios like X-Plane scene loading.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `MountHandle` that keeps the filesystem mounted. When dropped, the
    /// filesystem is automatically unmounted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source directory doesn't exist
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    pub async fn serve_passthrough_fuse3(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<MountHandle, ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3(&config, source_dir, mountpoint).await
    }

    /// Start the passthrough FUSE filesystem server using fuse3 (synchronous wrapper).
    ///
    /// This is a convenience wrapper around `serve_passthrough_fuse3` that blocks
    /// until the filesystem is unmounted. For async code, use `serve_passthrough_fuse3`
    /// directly.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Note
    ///
    /// This method blocks until the filesystem is unmounted (e.g., via Ctrl+C
    /// or `fusermount -u`).
    pub fn serve_passthrough_fuse3_blocking(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<(), ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3_blocking(
            &config,
            source_dir,
            mountpoint,
            &self.runtime_handle,
        )
    }

    /// Start the passthrough FUSE filesystem server using fuse3 as a background task.
    ///
    /// This spawns the fuse3 mount as a background Tokio task, returning a handle
    /// that can be safely stored and dropped outside of an async context. This is
    /// the recommended method for use with `MountManager`.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `SpawnedMountHandle` that keeps the filesystem mounted. The handle can be
    /// dropped safely from any context (async or sync) - it will use `fusermount -u`
    /// as a fallback for cleanup if needed.
    pub async fn serve_passthrough_fuse3_spawned(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<SpawnedMountHandle, ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3_spawned(&config, source_dir, mountpoint).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsFormat;

    // Note: Most XEarthLayerService tests require network access or mocks.
    // Unit tests here focus on configuration and validation logic.

    #[test]
    fn test_invalid_zoom_too_low() {
        // We can't easily test without a real service, but we can test the error type
        let err = ServiceError::InvalidZoom {
            zoom: 0,
            min: 1,
            max: 19,
        };
        assert!(err.to_string().contains("0"));
    }

    #[test]
    fn test_invalid_zoom_too_high() {
        let err = ServiceError::InvalidZoom {
            zoom: 25,
            min: 1,
            max: 19,
        };
        assert!(err.to_string().contains("25"));
    }

    #[test]
    fn test_invalid_coordinates_error() {
        let err = ServiceError::InvalidCoordinates {
            lat: 91.0,
            lon: 0.0,
            reason: "latitude must be between -90 and 90".to_string(),
        };
        assert!(err.to_string().contains("91"));
    }

    #[test]
    fn test_service_config_default() {
        let config = ServiceConfig::default();
        assert!(config.cache_enabled());
        assert_eq!(config.texture().format(), DdsFormat::BC1);
        assert_eq!(config.texture().mipmap_count(), 5);
    }

    #[test]
    fn test_config_error() {
        let err = ServiceError::ConfigError("No mountpoint".to_string());
        assert!(err.to_string().contains("Configuration error"));
        assert!(err.to_string().contains("No mountpoint"));
    }

    #[test]
    fn test_runtime_error() {
        let err = ServiceError::RuntimeError("failed to spawn".to_string());
        assert!(err.to_string().contains("Runtime error"));
        assert!(err.to_string().contains("failed to spawn"));
    }
}
