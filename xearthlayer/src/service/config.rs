//! Service configuration types.

use crate::config::{ControlPlaneSettings, DownloadConfig, PipelineSettings, TextureConfig};
use std::path::PathBuf;

/// Default disk GC interval in seconds (60 seconds).
pub const DEFAULT_GC_INTERVAL_SECS: u64 = 60;

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
    /// Disk GC interval in seconds
    disk_gc_interval_secs: u64,
    /// Number of threads for parallel tile generation
    generation_threads: Option<usize>,
    /// Timeout in seconds for generating a single tile
    generation_timeout: Option<u64>,
    /// Quiet mode - disables periodic stats logging (for TUI mode)
    quiet_mode: bool,
    /// Pipeline configuration for concurrency and retry behavior
    pipeline: PipelineSettings,
    /// Control plane configuration for job management and health monitoring
    control_plane: ControlPlaneSettings,
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

    /// Get the disk GC interval in seconds.
    pub fn disk_gc_interval_secs(&self) -> u64 {
        self.disk_gc_interval_secs
    }

    /// Get the number of threads for parallel tile generation, if configured.
    pub fn generation_threads(&self) -> Option<usize> {
        self.generation_threads
    }

    /// Get the timeout in seconds for generating a single tile, if configured.
    pub fn generation_timeout(&self) -> Option<u64> {
        self.generation_timeout
    }

    /// Check if quiet mode is enabled (disables periodic stats logging).
    pub fn quiet_mode(&self) -> bool {
        self.quiet_mode
    }

    /// Get the pipeline configuration for concurrency and retry behavior.
    pub fn pipeline(&self) -> &PipelineSettings {
        &self.pipeline
    }

    /// Get the control plane configuration for job management and health monitoring.
    pub fn control_plane(&self) -> &ControlPlaneSettings {
        &self.control_plane
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        use crate::config::{
            default_cpu_concurrent, default_http_concurrent, default_max_concurrent_jobs,
            default_prefetch_in_flight, DEFAULT_COALESCE_CHANNEL_CAPACITY,
            DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
            DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS, DEFAULT_MAX_RETRIES,
            DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_RETRY_BASE_DELAY_MS,
        };
        Self {
            texture: TextureConfig::default(),
            download: DownloadConfig::default(),
            cache_enabled: true,
            mountpoint: None,
            cache_directory: None,
            cache_memory_size: None,
            cache_disk_size: None,
            disk_gc_interval_secs: DEFAULT_GC_INTERVAL_SECS,
            generation_threads: None,
            generation_timeout: None,
            quiet_mode: false,
            pipeline: PipelineSettings {
                max_http_concurrent: default_http_concurrent(),
                max_cpu_concurrent: default_cpu_concurrent(),
                max_prefetch_in_flight: default_prefetch_in_flight(),
                request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
                max_retries: DEFAULT_MAX_RETRIES,
                retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
                coalesce_channel_capacity: DEFAULT_COALESCE_CHANNEL_CAPACITY,
            },
            control_plane: ControlPlaneSettings {
                max_concurrent_jobs: default_max_concurrent_jobs(),
                stall_threshold_secs: DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
                health_check_interval_secs: DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
                semaphore_timeout_secs: DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            },
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
    disk_gc_interval_secs: Option<u64>,
    generation_threads: Option<usize>,
    generation_timeout: Option<u64>,
    quiet_mode: Option<bool>,
    pipeline: Option<PipelineSettings>,
    control_plane: Option<ControlPlaneSettings>,
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

    /// Set the disk GC interval in seconds.
    pub fn disk_gc_interval(mut self, secs: u64) -> Self {
        self.disk_gc_interval_secs = Some(secs);
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

    /// Enable quiet mode (disables periodic stats logging).
    pub fn quiet_mode(mut self, quiet: bool) -> Self {
        self.quiet_mode = Some(quiet);
        self
    }

    /// Set the pipeline configuration for concurrency and retry behavior.
    pub fn pipeline(mut self, settings: PipelineSettings) -> Self {
        self.pipeline = Some(settings);
        self
    }

    /// Set the control plane configuration for job management and health monitoring.
    pub fn control_plane(mut self, settings: ControlPlaneSettings) -> Self {
        self.control_plane = Some(settings);
        self
    }

    /// Build the configuration with defaults for unset values.
    pub fn build(self) -> ServiceConfig {
        use crate::config::{
            default_cpu_concurrent, default_http_concurrent, default_max_concurrent_jobs,
            default_prefetch_in_flight, DEFAULT_COALESCE_CHANNEL_CAPACITY,
            DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
            DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS, DEFAULT_MAX_RETRIES,
            DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_RETRY_BASE_DELAY_MS,
        };
        ServiceConfig {
            texture: self.texture.unwrap_or_default(),
            download: self.download.unwrap_or_default(),
            cache_enabled: self.cache_enabled.unwrap_or(true),
            mountpoint: self.mountpoint,
            cache_directory: self.cache_directory,
            cache_memory_size: self.cache_memory_size,
            cache_disk_size: self.cache_disk_size,
            disk_gc_interval_secs: self
                .disk_gc_interval_secs
                .unwrap_or(DEFAULT_GC_INTERVAL_SECS),
            generation_threads: self.generation_threads,
            generation_timeout: self.generation_timeout,
            quiet_mode: self.quiet_mode.unwrap_or(false),
            pipeline: self.pipeline.unwrap_or(PipelineSettings {
                max_http_concurrent: default_http_concurrent(),
                max_cpu_concurrent: default_cpu_concurrent(),
                max_prefetch_in_flight: default_prefetch_in_flight(),
                request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
                max_retries: DEFAULT_MAX_RETRIES,
                retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
                coalesce_channel_capacity: DEFAULT_COALESCE_CHANNEL_CAPACITY,
            }),
            control_plane: self.control_plane.unwrap_or(ControlPlaneSettings {
                max_concurrent_jobs: default_max_concurrent_jobs(),
                stall_threshold_secs: DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
                health_check_interval_secs: DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
                semaphore_timeout_secs: DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            }),
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
