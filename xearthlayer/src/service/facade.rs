//! XEarthLayer service facade implementation.

use super::config::ServiceConfig;
use super::error::ServiceError;
use crate::cache::{Cache, CacheConfig, CacheSystem, NoOpCache};
use crate::coord::to_tile_coords;
use crate::fuse::{PassthroughFS, XEarthLayerFS};
use crate::log::Logger;
use crate::log_info;
use crate::orchestrator::TileOrchestrator;
use crate::provider::{ProviderConfig, ProviderFactory, ReqwestClient};
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use crate::tile::{
    DefaultTileGenerator, ParallelConfig, ParallelTileGenerator, TileGenerator, TileRequest,
};
use fuser::BackgroundSession;
use std::path::PathBuf;
use std::sync::Arc;

/// High-level facade for XEarthLayer operations.
///
/// Encapsulates all component creation and wiring, providing a simplified
/// API for common operations like serving tiles via FUSE or downloading
/// individual tiles.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::service::{XEarthLayerService, ServiceConfig};
/// use xearthlayer::provider::ProviderConfig;
///
/// // Create service with default configuration
/// let config = ServiceConfig::default();
/// let service = XEarthLayerService::new(config, ProviderConfig::bing())?;
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
}

impl XEarthLayerService {
    /// Create a new XEarthLayer service from configuration.
    ///
    /// This constructor wires together all the necessary components:
    /// - HTTP client
    /// - Provider (Bing, Google, etc.)
    /// - Texture encoder
    /// - Tile orchestrator
    /// - Tile generator
    /// - Cache system
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize (e.g., HTTP client
    /// creation fails, Google session creation fails, cache directory creation fails).
    pub fn new(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
    ) -> Result<Self, ServiceError> {
        // Create HTTP client
        let http_client =
            ReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

        // Create provider using factory
        let factory = ProviderFactory::new(http_client);
        let (provider, provider_name, max_zoom) = factory.create(&provider_config)?;

        // Create texture encoder from config
        let encoder: Arc<dyn TextureEncoder> = Arc::new(
            DdsTextureEncoder::new(config.texture().format())
                .with_mipmap_count(config.texture().mipmap_count()),
        );

        // Create orchestrator with download config
        let orchestrator = TileOrchestrator::with_config(provider, *config.download());

        // Create base tile generator
        let base_generator: Arc<dyn TileGenerator> = Arc::new(DefaultTileGenerator::new(
            orchestrator,
            encoder,
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
        let cache: Arc<dyn Cache> = if config.cache_enabled() {
            let mut cache_config = CacheConfig::new(&provider_name);

            // Apply cache settings from config if provided
            if let Some(dir) = config.cache_directory() {
                cache_config = cache_config.with_cache_dir(dir.clone());
            }
            if let Some(size) = config.cache_memory_size() {
                cache_config = cache_config.with_memory_size(size);
            }
            if let Some(size) = config.cache_disk_size() {
                cache_config = cache_config.with_disk_size(size);
            }

            match CacheSystem::new(cache_config, logger.clone()) {
                Ok(cache) => Arc::new(cache),
                Err(e) => return Err(ServiceError::CacheError(e.to_string())),
            }
        } else {
            Arc::new(NoOpCache::new(&provider_name))
        };

        Ok(Self {
            config,
            provider_name,
            max_zoom,
            generator,
            cache,
            logger,
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

    /// Start the passthrough FUSE filesystem server.
    ///
    /// Overlays an existing scenery pack directory, passing through real files
    /// and generating DDS textures on-demand for virtual files.
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

        // Create passthrough FUSE filesystem
        let fs = PassthroughFS::new(
            source_path,
            self.generator.clone(),
            self.cache.clone(),
            self.config.texture().format(),
            self.logger.clone(),
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

        // Create passthrough FUSE filesystem
        let fs = PassthroughFS::new(
            source_path,
            self.generator.clone(),
            self.cache.clone(),
            self.config.texture().format(),
            self.logger.clone(),
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
}
