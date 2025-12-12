//! XEarthLayer service facade implementation.

use super::config::ServiceConfig;
use super::error::ServiceError;
use super::network_logger::NetworkStatsLogger;
use crate::cache::{Cache, CacheConfig, CacheSystem, MemoryCache, NoOpCache};
use crate::coord::to_tile_coords;
use crate::fuse::{AsyncPassthroughFS, DdsHandler, XEarthLayerFS};
use crate::log::Logger;
use crate::log_info;
use crate::orchestrator::{NetworkStats, TileOrchestrator};
use crate::pipeline::adapters::{
    AsyncProviderAdapter, DiskCacheAdapter, MemoryCacheAdapter, NullDiskCache, ProviderAdapter,
    TextureEncoderAdapter,
};
use crate::pipeline::{create_dds_handler, PipelineConfig, TokioExecutor};
use crate::provider::{
    AsyncProviderFactory, AsyncProviderType, AsyncReqwestClient, Provider, ProviderConfig,
    ProviderFactory, ReqwestClient,
};
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use crate::tile::{
    DefaultTileGenerator, ParallelConfig, ParallelTileGenerator, TileGenerator, TileRequest,
};
use fuser::BackgroundSession;
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
    /// Memory cache for async pipeline (tile-level)
    memory_cache: Option<MemoryCache>,
    /// Cache directory for disk cache
    cache_dir: Option<PathBuf>,
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
    fn build(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
        owned_runtime: Option<Runtime>,
    ) -> Result<Self, ServiceError> {
        // Create sync HTTP client for legacy pipeline (TileOrchestrator)
        let http_client =
            ReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

        // Create sync provider using factory (for legacy tile generator)
        let factory = ProviderFactory::new(http_client);
        let (provider, provider_name, max_zoom) = factory.create(&provider_config)?;

        // Create async HTTP client for async pipeline (non-blocking I/O)
        let async_http_client =
            AsyncReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

        // Create async provider - this eliminates spawn_blocking for HTTP calls
        let async_factory = AsyncProviderFactory::new(async_http_client);
        let async_provider = runtime_handle
            .block_on(async_factory.create(&provider_config))
            .map(|(provider, _, _)| Arc::new(provider))
            .ok();

        // Create texture encoder from config
        let dds_encoder = Arc::new(
            DdsTextureEncoder::new(config.texture().format())
                .with_mipmap_count(config.texture().mipmap_count()),
        );
        let encoder: Arc<dyn TextureEncoder> = Arc::clone(&dds_encoder) as Arc<dyn TextureEncoder>;

        // Create network stats tracker
        let network_stats = Arc::new(NetworkStats::new());

        // Create orchestrator with download config and network stats
        let orchestrator = TileOrchestrator::with_config(Arc::clone(&provider), *config.download())
            .with_network_stats(network_stats.clone());

        // Create base tile generator
        let base_generator: Arc<dyn TileGenerator> = Arc::new(DefaultTileGenerator::new(
            orchestrator,
            Arc::clone(&encoder),
            logger.clone(),
        ));

        // Wrap with parallel generator for concurrent tile requests
        let parallel_config = ParallelConfig::default()
            .with_threads(config.generation_threads().unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            }))
            .with_timeout_secs(config.generation_timeout().unwrap_or(10))
            .with_dds_format(config.texture().format())
            .with_mipmap_count(config.texture().mipmap_count());

        let generator: Arc<dyn TileGenerator> = Arc::new(ParallelTileGenerator::new(
            base_generator,
            parallel_config.clone(),
            logger.clone(),
        ));

        log_info!(
            logger,
            "Tile generation: {} threads, {}s timeout",
            parallel_config.threads,
            parallel_config.timeout_secs
        );

        // Create cache system based on configuration
        let (cache, memory_cache, cache_dir): (
            Arc<dyn Cache>,
            Option<MemoryCache>,
            Option<PathBuf>,
        ) = if config.cache_enabled() {
            let mut cache_config = CacheConfig::new(&provider_name);

            // Track the cache directory for async pipeline
            let mut dir_for_pipeline = cache_config.disk.cache_dir.clone();
            let mut mem_size = cache_config.memory.max_size_bytes;

            // Apply cache settings from config if provided
            if let Some(dir) = config.cache_directory() {
                cache_config = cache_config.with_cache_dir(dir.clone());
                dir_for_pipeline = dir.clone();
            }
            if let Some(size) = config.cache_memory_size() {
                cache_config = cache_config.with_memory_size(size);
                mem_size = size;
            }
            if let Some(size) = config.cache_disk_size() {
                cache_config = cache_config.with_disk_size(size);
            }

            // Create separate memory cache for async pipeline
            let mem_cache = MemoryCache::new(mem_size);

            match CacheSystem::new(cache_config, logger.clone()) {
                Ok(cache) => (Arc::new(cache), Some(mem_cache), Some(dir_for_pipeline)),
                Err(e) => return Err(ServiceError::CacheError(e.to_string())),
            }
        } else {
            (Arc::new(NoOpCache::new(&provider_name)), None, None)
        };

        // Start network stats logger (uses same interval as cache stats: 60s)
        let network_stats_logger = Some(NetworkStatsLogger::start(
            network_stats,
            logger.clone(),
            60, // Log every 60 seconds
        ));

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

    /// Start the FUSE filesystem server.
    ///
    /// Mounts a virtual filesystem at the configured mountpoint that serves
    /// DDS textures on-demand.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No mountpoint is configured
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    ///
    /// # Note
    ///
    /// This method blocks until the filesystem is unmounted (e.g., via Ctrl+C
    /// or `fusermount -u`).
    pub fn serve(&self) -> Result<(), ServiceError> {
        let mountpoint = self
            .config
            .mountpoint()
            .ok_or_else(|| ServiceError::ConfigError("No mountpoint configured".to_string()))?;

        // Check if mountpoint exists
        if !std::path::Path::new(mountpoint).exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Mountpoint does not exist: {}", mountpoint),
            )));
        }

        // Create FUSE filesystem
        let fs = XEarthLayerFS::new(
            self.generator.clone(),
            self.cache.clone(),
            self.config.texture().format(),
            self.logger.clone(),
        );

        // Mount filesystem
        fuser::mount2(fs, mountpoint, &[]).map_err(ServiceError::from)
    }

    /// Start the FUSE filesystem server in the background.
    ///
    /// Returns a `BackgroundSession` that can be used to manage the mount's lifecycle.
    /// When the session is dropped, the filesystem is automatically unmounted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No mountpoint is configured
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    pub fn serve_background(&self) -> Result<BackgroundSession, ServiceError> {
        let mountpoint = self
            .config
            .mountpoint()
            .ok_or_else(|| ServiceError::ConfigError("No mountpoint configured".to_string()))?;

        // Check if mountpoint exists
        if !std::path::Path::new(mountpoint).exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Mountpoint does not exist: {}", mountpoint),
            )));
        }

        // Create FUSE filesystem
        let fs = XEarthLayerFS::new(
            self.generator.clone(),
            self.cache.clone(),
            self.config.texture().format(),
            self.logger.clone(),
        );

        // Mount filesystem in background
        fuser::spawn_mount2(fs, mountpoint, &[]).map_err(ServiceError::from)
    }

    /// Create the DDS handler for the async pipeline.
    ///
    /// This wires up all the pipeline components and returns a handler
    /// that can be used with `AsyncPassthroughFS`.
    ///
    /// When an async provider is available, uses `AsyncProviderAdapter` which
    /// avoids `spawn_blocking` for HTTP calls, preventing thread pool exhaustion.
    fn create_dds_handler(&self) -> DdsHandler {
        let format = self.config.texture().format();

        // Create texture encoder adapter - create a new encoder for the pipeline
        // (the adapter takes ownership, so we create a fresh encoder with the same config)
        let pipeline_encoder =
            DdsTextureEncoder::new(format).with_mipmap_count(self.config.texture().mipmap_count());
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(pipeline_encoder));

        // Create memory cache adapter
        let memory_cache_adapter = Arc::new(match &self.memory_cache {
            Some(cache) => MemoryCacheAdapter::new(
                MemoryCache::new(cache.max_size_bytes()),
                &self.provider_name,
                format,
            ),
            None => MemoryCacheAdapter::new(
                MemoryCache::new(0), // Minimal cache when disabled
                &self.provider_name,
                format,
            ),
        });

        // Create executor
        let executor = Arc::new(TokioExecutor::new());

        // Create pipeline config
        let pipeline_config = PipelineConfig {
            request_timeout: Duration::from_secs(self.config.download().timeout_secs()),
            max_retries: self.config.download().max_retries(),
            dds_format: format,
            mipmap_count: self.config.texture().mipmap_count(),
            max_concurrent_downloads: 256,
        };

        // Create the handler - use async provider if available (preferred),
        // otherwise fall back to sync provider with spawn_blocking (legacy)
        match (&self.async_provider, &self.cache_dir) {
            // Async provider with disk cache (PREFERRED - non-blocking I/O)
            (Some(async_prov), Some(dir)) => {
                let provider_adapter =
                    Arc::new(AsyncProviderAdapter::from_arc(Arc::clone(async_prov)));
                let disk_cache = Arc::new(DiskCacheAdapter::new(dir.clone(), &self.provider_name));
                tracing::info!("Using async provider for DDS pipeline (non-blocking I/O)");
                create_dds_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    self.runtime_handle.clone(),
                )
            }
            // Async provider without disk cache (PREFERRED - non-blocking I/O)
            (Some(async_prov), None) => {
                let provider_adapter =
                    Arc::new(AsyncProviderAdapter::from_arc(Arc::clone(async_prov)));
                let disk_cache = Arc::new(NullDiskCache);
                tracing::info!("Using async provider for DDS pipeline (non-blocking I/O)");
                create_dds_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    self.runtime_handle.clone(),
                )
            }
            // No async provider - use sync provider with spawn_blocking (LEGACY)
            (None, Some(dir)) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(Arc::clone(&self.provider)));
                let disk_cache = Arc::new(DiskCacheAdapter::new(dir.clone(), &self.provider_name));
                tracing::warn!("Using sync provider with spawn_blocking (may exhaust thread pool under high load)");
                create_dds_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    self.runtime_handle.clone(),
                )
            }
            (None, None) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(Arc::clone(&self.provider)));
                let disk_cache = Arc::new(NullDiskCache);
                tracing::warn!("Using sync provider with spawn_blocking (may exhaust thread pool under high load)");
                create_dds_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    self.runtime_handle.clone(),
                )
            }
        }
    }

    /// Calculate the expected DDS file size based on encoder configuration.
    fn expected_dds_size(&self) -> usize {
        self.dds_encoder.expected_size(4096, 4096)
    }

    /// Start the passthrough FUSE filesystem server.
    ///
    /// Overlays an existing scenery pack directory, passing through real files
    /// and generating DDS textures on-demand via the async pipeline.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source directory doesn't exist
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    ///
    /// # Note
    ///
    /// This method blocks until the filesystem is unmounted (e.g., via Ctrl+C
    /// or `fusermount -u`).
    pub fn serve_passthrough(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<(), ServiceError> {
        // Check if source directory exists
        let source_path = PathBuf::from(source_dir);
        if !source_path.exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Source directory does not exist: {}", source_dir),
            )));
        }

        // Check if mountpoint exists
        if !std::path::Path::new(mountpoint).exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Mountpoint does not exist: {}", mountpoint),
            )));
        }

        // Create the DDS handler for async pipeline
        let dds_handler = self.create_dds_handler();

        // Create async passthrough FUSE filesystem
        let fs = AsyncPassthroughFS::new(
            source_path,
            dds_handler,
            self.runtime_handle.clone(),
            self.expected_dds_size(),
        )
        .with_timeout(Duration::from_secs(
            self.config.generation_timeout().unwrap_or(30),
        ));

        log_info!(
            self.logger,
            "Starting async passthrough filesystem: {} -> {}",
            source_dir,
            mountpoint
        );

        // Mount filesystem
        fuser::mount2(fs, mountpoint, &[]).map_err(ServiceError::from)
    }

    /// Start the passthrough FUSE filesystem server in the background.
    ///
    /// Returns a `BackgroundSession` that can be used to manage the mount's lifecycle.
    /// When the session is dropped, the filesystem is automatically unmounted.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source directory doesn't exist
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    pub fn serve_passthrough_background(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<BackgroundSession, ServiceError> {
        // Check if source directory exists
        let source_path = PathBuf::from(source_dir);
        if !source_path.exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Source directory does not exist: {}", source_dir),
            )));
        }

        // Check if mountpoint exists
        if !std::path::Path::new(mountpoint).exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Mountpoint does not exist: {}", mountpoint),
            )));
        }

        // Create the DDS handler for async pipeline
        let dds_handler = self.create_dds_handler();

        // Create async passthrough FUSE filesystem
        let fs = AsyncPassthroughFS::new(
            source_path,
            dds_handler,
            self.runtime_handle.clone(),
            self.expected_dds_size(),
        )
        .with_timeout(Duration::from_secs(
            self.config.generation_timeout().unwrap_or(30),
        ));

        log_info!(
            self.logger,
            "Starting async passthrough filesystem (background): {} -> {}",
            source_dir,
            mountpoint
        );

        // Mount filesystem in background
        fuser::spawn_mount2(fs, mountpoint, &[]).map_err(ServiceError::from)
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
