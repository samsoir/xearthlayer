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

mod defaults;
mod download;
mod file;
mod keys;
mod parser;
mod settings;
mod size;
mod storage;
mod texture;
mod upgrade;
mod writer;
pub use download::DownloadConfig;
pub use file::{
    config_directory,
    config_file_path,
    default_cpu_concurrent,
    // Executor defaults
    default_executor_cpu_concurrent,
    default_http_concurrent,
    default_max_concurrent_jobs,
    default_prefetch_in_flight,
    num_cpus,
    CacheSettings,
    ConfigFile,
    ConfigFileError,
    ControlPlaneSettings,
    DownloadSettings,
    ExecutorSettings,
    GenerationSettings,
    LoggingSettings,
    OnlineNetworkSettings,
    PackagesSettings,
    PatchesSettings,
    PipelineSettings,
    PrefetchSettings,
    PrewarmSettings,
    ProviderSettings,
    TextureSettings,
    XPlaneSettings,
    // Prefetch defaults
    DEFAULT_CIRCUIT_BREAKER_HALF_OPEN_SECS,
    DEFAULT_CIRCUIT_BREAKER_OPEN_MS,
    DEFAULT_CIRCUIT_BREAKER_THRESHOLD,
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
    DEFAULT_EXECUTOR_DISK_IO_CONCURRENT,
    DEFAULT_EXECUTOR_JOB_CHANNEL_CAPACITY,
    DEFAULT_EXECUTOR_MAX_CONCURRENT_TASKS,
    DEFAULT_EXECUTOR_NETWORK_CONCURRENT,
    DEFAULT_EXECUTOR_REQUEST_CHANNEL_CAPACITY,
    // Generation defaults
    DEFAULT_GENERATION_TIMEOUT_SECS,
    // Package manager defaults
    DEFAULT_LIBRARY_URL,
    DEFAULT_MAX_CONCURRENT_DOWNLOADS,
    DEFAULT_MAX_RETRIES,
    DEFAULT_MEMORY_CACHE_SIZE,
    // Texture defaults
    DEFAULT_MIPMAP_COUNT,
    // DownloadConfig defaults
    DEFAULT_PARALLEL_DOWNLOADS,
    DEFAULT_PREFETCH_CYCLE_INTERVAL_MS,
    DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE,
    DEFAULT_PREFETCH_UDP_PORT,
    DEFAULT_REQUEST_TIMEOUT_SECS,
    DEFAULT_RETRY_BASE_DELAY_MS,
};
pub use keys::{ConfigKey, ConfigKeyError};
pub use size::{format_size, parse_size, Size, SizeParseError};
pub use storage::{
    DiskIoProfile, DEFAULT_CPU_FALLBACK, HDD_BLOCKING_CEILING, HDD_BLOCKING_SCALING_FACTOR,
    HDD_IO_CEILING, HDD_IO_SCALING_FACTOR, NVME_BLOCKING_CEILING, NVME_BLOCKING_SCALING_FACTOR,
    NVME_IO_CEILING, NVME_IO_SCALING_FACTOR, SSD_BLOCKING_CEILING, SSD_BLOCKING_SCALING_FACTOR,
    SSD_IO_CEILING, SSD_IO_SCALING_FACTOR,
};
pub use texture::TextureConfig;
pub use upgrade::{
    analyze_config, upgrade_config, ConfigUpgradeAnalysis, UpgradeResult, DEPRECATED_KEYS,
};
// Re-export X-Plane utilities for backwards compatibility
// Prefer using xearthlayer::xplane module directly for new code
pub use crate::xplane::{
    derive_mountpoint, detect_custom_scenery, detect_scenery_dir, detect_xplane_install,
    detect_xplane_installs, SceneryDetectionResult, XPlanePathError,
};
