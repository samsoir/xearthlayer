//! XEarthLayer service facade implementation.

use super::config::ServiceConfig;
use super::error::ServiceError;
use crate::cache::{Cache, CacheConfig, CacheSystem, NoOpCache};
use crate::coord::to_tile_coords;
use crate::fuse::XEarthLayerFS;
use crate::orchestrator::TileOrchestrator;
use crate::provider::{ProviderConfig, ProviderFactory, ReqwestClient};
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use crate::tile::{DefaultTileGenerator, TileGenerator, TileRequest};
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

        // Create tile generator
        let generator: Arc<dyn TileGenerator> =
            Arc::new(DefaultTileGenerator::new(orchestrator, encoder));

        // Create cache system based on configuration
        let cache: Arc<dyn Cache> = if config.cache_enabled() {
            let cache_config = CacheConfig::new(&provider_name);
            match CacheSystem::new(cache_config) {
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
    /// * `zoom` - Zoom level
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
        // Validate zoom level
        if !(1..=19).contains(&zoom) {
            return Err(ServiceError::InvalidZoom {
                zoom,
                min: 1,
                max: 19,
            });
        }

        // Check zoom against provider's maximum (accounting for chunk zoom offset)
        if zoom + 4 > self.max_zoom {
            return Err(ServiceError::InvalidZoom {
                zoom,
                min: 1,
                max: self.max_zoom.saturating_sub(4),
            });
        }

        // Convert coordinates to tile
        let tile =
            to_tile_coords(lat, lon, zoom).map_err(|e| ServiceError::InvalidCoordinates {
                lat,
                lon,
                reason: e.to_string(),
            })?;

        // Create tile request
        let request = TileRequest::new(tile.row as i32, tile.col as i32, zoom);

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
        );

        // Mount filesystem
        fuser::mount2(fs, mountpoint, &[]).map_err(ServiceError::from)
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
