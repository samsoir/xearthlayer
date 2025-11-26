//! Service configuration types.

use crate::config::{DownloadConfig, TextureConfig};
use std::path::PathBuf;

/// Configuration for the XEarthLayer service.
///
/// Combines all configuration needed to create and run the service.
///
/// # Example
///
/// ```
/// use xearthlayer::service::ServiceConfig;
/// use xearthlayer::config::{TextureConfig, DownloadConfig};
/// use xearthlayer::dds::DdsFormat;
///
/// let config = ServiceConfig::builder()
///     .texture(TextureConfig::new(DdsFormat::BC1).with_mipmap_count(5))
///     .download(DownloadConfig::default())
///     .cache_enabled(true)
///     .build();
///
/// assert!(config.cache_enabled());
/// ```
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Texture encoding configuration
    texture: TextureConfig,
    /// Download/orchestrator configuration
    download: DownloadConfig,
    /// Whether caching is enabled
    cache_enabled: bool,
    /// FUSE mountpoint (optional, only needed for serve command)
    mountpoint: Option<String>,
    /// Cache directory path
    cache_directory: Option<PathBuf>,
    /// Memory cache size in bytes
    cache_memory_size: Option<usize>,
    /// Disk cache size in bytes
    cache_disk_size: Option<usize>,
    /// Number of threads for parallel tile generation
    generation_threads: Option<usize>,
    /// Timeout in seconds for generating a single tile
    generation_timeout: Option<u64>,
}

impl ServiceConfig {
    /// Create a new configuration builder.
    pub fn builder() -> ServiceConfigBuilder {
        ServiceConfigBuilder::default()
    }

    /// Get the texture configuration.
    pub fn texture(&self) -> &TextureConfig {
        &self.texture
    }

    /// Get the download configuration.
    pub fn download(&self) -> &DownloadConfig {
        &self.download
    }

    /// Check if caching is enabled.
    pub fn cache_enabled(&self) -> bool {
        self.cache_enabled
    }

    /// Get the FUSE mountpoint, if configured.
    pub fn mountpoint(&self) -> Option<&str> {
        self.mountpoint.as_deref()
    }

    /// Get the cache directory, if configured.
    pub fn cache_directory(&self) -> Option<&PathBuf> {
        self.cache_directory.as_ref()
    }

    /// Get the memory cache size in bytes, if configured.
    pub fn cache_memory_size(&self) -> Option<usize> {
        self.cache_memory_size
    }

    /// Get the disk cache size in bytes, if configured.
    pub fn cache_disk_size(&self) -> Option<usize> {
        self.cache_disk_size
    }

    /// Get the number of threads for parallel tile generation, if configured.
    pub fn generation_threads(&self) -> Option<usize> {
        self.generation_threads
    }

    /// Get the timeout in seconds for generating a single tile, if configured.
    pub fn generation_timeout(&self) -> Option<u64> {
        self.generation_timeout
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            texture: TextureConfig::default(),
            download: DownloadConfig::default(),
            cache_enabled: true,
            mountpoint: None,
            cache_directory: None,
            cache_memory_size: None,
            cache_disk_size: None,
            generation_threads: None,
            generation_timeout: None,
        }
    }
}

/// Builder for ServiceConfig.
///
/// Provides a fluent API for constructing service configuration.
#[derive(Debug, Clone, Default)]
pub struct ServiceConfigBuilder {
    texture: Option<TextureConfig>,
    download: Option<DownloadConfig>,
    cache_enabled: Option<bool>,
    mountpoint: Option<String>,
    cache_directory: Option<PathBuf>,
    cache_memory_size: Option<usize>,
    cache_disk_size: Option<usize>,
    generation_threads: Option<usize>,
    generation_timeout: Option<u64>,
}

impl ServiceConfigBuilder {
    /// Set the texture encoding configuration.
    pub fn texture(mut self, config: TextureConfig) -> Self {
        self.texture = Some(config);
        self
    }

    /// Set the download/orchestrator configuration.
    pub fn download(mut self, config: DownloadConfig) -> Self {
        self.download = Some(config);
        self
    }

    /// Enable or disable caching.
    pub fn cache_enabled(mut self, enabled: bool) -> Self {
        self.cache_enabled = Some(enabled);
        self
    }

    /// Set the FUSE mountpoint (required for serve command).
    pub fn mountpoint(mut self, path: impl Into<String>) -> Self {
        self.mountpoint = Some(path.into());
        self
    }

    /// Set the cache directory.
    pub fn cache_directory(mut self, path: PathBuf) -> Self {
        self.cache_directory = Some(path);
        self
    }

    /// Set the memory cache size in bytes.
    pub fn cache_memory_size(mut self, size: usize) -> Self {
        self.cache_memory_size = Some(size);
        self
    }

    /// Set the disk cache size in bytes.
    pub fn cache_disk_size(mut self, size: usize) -> Self {
        self.cache_disk_size = Some(size);
        self
    }

    /// Set the number of threads for parallel tile generation.
    pub fn generation_threads(mut self, threads: usize) -> Self {
        self.generation_threads = Some(threads);
        self
    }

    /// Set the timeout in seconds for generating a single tile.
    pub fn generation_timeout(mut self, timeout: u64) -> Self {
        self.generation_timeout = Some(timeout);
        self
    }

    /// Build the configuration with defaults for unset values.
    pub fn build(self) -> ServiceConfig {
        ServiceConfig {
            texture: self.texture.unwrap_or_default(),
            download: self.download.unwrap_or_default(),
            cache_enabled: self.cache_enabled.unwrap_or(true),
            mountpoint: self.mountpoint,
            cache_directory: self.cache_directory,
            cache_memory_size: self.cache_memory_size,
            cache_disk_size: self.cache_disk_size,
            generation_threads: self.generation_threads,
            generation_timeout: self.generation_timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsFormat;

    #[test]
    fn test_default_config() {
        let config = ServiceConfig::default();
        assert!(config.cache_enabled());
        assert!(config.mountpoint().is_none());
    }

    #[test]
    fn test_builder_defaults() {
        let config = ServiceConfig::builder().build();
        assert!(config.cache_enabled());
        assert!(config.mountpoint().is_none());
    }

    #[test]
    fn test_builder_with_texture() {
        let texture = TextureConfig::new(DdsFormat::BC3).with_mipmap_count(3);
        let config = ServiceConfig::builder().texture(texture).build();

        assert_eq!(config.texture().format(), DdsFormat::BC3);
        assert_eq!(config.texture().mipmap_count(), 3);
    }

    #[test]
    fn test_builder_with_download() {
        let download = DownloadConfig::new()
            .with_timeout_secs(60)
            .with_max_retries(5);
        let config = ServiceConfig::builder().download(download).build();

        assert_eq!(config.download().timeout_secs(), 60);
        assert_eq!(config.download().max_retries(), 5);
    }

    #[test]
    fn test_builder_cache_disabled() {
        let config = ServiceConfig::builder().cache_enabled(false).build();
        assert!(!config.cache_enabled());
    }

    #[test]
    fn test_builder_with_mountpoint() {
        let config = ServiceConfig::builder()
            .mountpoint("/mnt/xearthlayer")
            .build();

        assert_eq!(config.mountpoint(), Some("/mnt/xearthlayer"));
    }

    #[test]
    fn test_builder_full_chain() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::new(DdsFormat::BC1).with_mipmap_count(5))
            .download(DownloadConfig::new().with_timeout_secs(45))
            .cache_enabled(true)
            .mountpoint("/mnt/test")
            .build();

        assert_eq!(config.texture().format(), DdsFormat::BC1);
        assert_eq!(config.texture().mipmap_count(), 5);
        assert_eq!(config.download().timeout_secs(), 45);
        assert!(config.cache_enabled());
        assert_eq!(config.mountpoint(), Some("/mnt/test"));
    }

    #[test]
    fn test_config_clone() {
        let config = ServiceConfig::builder().mountpoint("/mnt/test").build();
        let cloned = config.clone();
        assert_eq!(config.mountpoint(), cloned.mountpoint());
    }

    #[test]
    fn test_config_debug() {
        let config = ServiceConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("ServiceConfig"));
    }
}
