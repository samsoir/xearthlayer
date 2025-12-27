//! Configuration types for XEarthLayer components.
//!
//! This module provides structured configuration objects that group related
//! parameters together, following SOLID principles by:
//!
//! - **SRP**: Each config struct handles one concern
//! - **OCP**: New config types can be added without modifying existing ones
//! - **DIP**: Components depend on config traits/structs, not raw parameters
//!
//! # Configuration File
//!
//! XEarthLayer uses a configuration file at `~/.xearthlayer/config.ini`.
//! Use [`ConfigFile::load()`] to load settings or [`ConfigFile::ensure_exists()`]
//! to create a default config file.
//!
//! # Example
//!
//! ```
//! use xearthlayer::config::{TextureConfig, DownloadConfig, ConfigFile};
//! use xearthlayer::dds::DdsFormat;
//!
//! // Load configuration from file (or use defaults)
//! let config = ConfigFile::load().unwrap_or_default();
//!
//! // Create texture configuration
//! let texture_config = TextureConfig::new(DdsFormat::BC1)
//!     .with_mipmap_count(5);
//!
//! // Create download configuration
//! let download_config = DownloadConfig::default();
//! ```

mod download;
mod file;
mod keys;
mod size;
mod texture;
mod xplane;

pub use download::DownloadConfig;
pub use file::{
    config_directory,
    config_file_path,
    default_cpu_concurrent,
    default_http_concurrent,
    default_max_concurrent_jobs,
    default_prefetch_in_flight,
    num_cpus,
    CacheSettings,
    ConfigFile,
    ConfigFileError,
    ControlPlaneSettings,
    DownloadSettings,
    GenerationSettings,
    LoggingSettings,
    PackagesSettings,
    PipelineSettings,
    PrefetchSettings,
    ProviderSettings,
    TextureSettings,
    XPlaneSettings,
    // Pipeline defaults
    DEFAULT_COALESCE_CHANNEL_CAPACITY,
    // Control plane defaults
    DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
    DEFAULT_CONTROL_PLANE_JOB_SCALING_FACTOR,
    DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
    DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
    // Cache defaults
    DEFAULT_DISK_CACHE_SIZE,
    // Download defaults
    DEFAULT_DOWNLOAD_TIMEOUT_SECS,
    // Generation defaults
    DEFAULT_GENERATION_TIMEOUT_SECS,
    DEFAULT_MAX_CONCURRENT_DOWNLOADS,
    DEFAULT_MAX_RETRIES,
    DEFAULT_MEMORY_CACHE_SIZE,
    // Texture defaults
    DEFAULT_MIPMAP_COUNT,
    // DownloadConfig defaults
    DEFAULT_PARALLEL_DOWNLOADS,
    // Prefetch defaults
    DEFAULT_PREFETCH_BATCH_SIZE,
    DEFAULT_PREFETCH_CONE_ANGLE,
    DEFAULT_PREFETCH_CONE_DISTANCE_NM,
    DEFAULT_PREFETCH_CYCLE_INTERVAL_MS,
    DEFAULT_PREFETCH_ENABLE_ZL12,
    DEFAULT_PREFETCH_INNER_RADIUS_NM,
    DEFAULT_PREFETCH_MAX_IN_FLIGHT,
    DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE,
    DEFAULT_PREFETCH_OUTER_RADIUS_NM,
    DEFAULT_PREFETCH_RADIAL_RADIUS,
    DEFAULT_PREFETCH_RADIAL_RADIUS_NM,
    DEFAULT_PREFETCH_UDP_PORT,
    DEFAULT_PREFETCH_ZL12_INNER_RADIUS_NM,
    DEFAULT_PREFETCH_ZL12_OUTER_RADIUS_NM,
    DEFAULT_REQUEST_TIMEOUT_SECS,
    DEFAULT_RETRY_BASE_DELAY_MS,
};
pub use keys::{ConfigKey, ConfigKeyError};
pub use size::{format_size, parse_size, Size, SizeParseError};
pub use texture::TextureConfig;
pub use xplane::{
    derive_mountpoint, detect_custom_scenery, detect_scenery_dir, detect_xplane_install,
    detect_xplane_installs, SceneryDetectionResult, XPlanePathError,
};
