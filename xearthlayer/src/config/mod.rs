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
//! use xearthlayer::config::{TextureConfig, ConfigFile};
//! use xearthlayer::dds::DdsFormat;
//!
//! // Load configuration from file (or use defaults)
//! let config = ConfigFile::load().unwrap_or_default();
//!
//! // Create texture configuration
//! let texture_config = TextureConfig::new(DdsFormat::BC1)
//!     .with_mipmap_count(5);
//! ```

pub(crate) mod defaults;
mod file;
pub mod help;
mod keys;
mod parser;
mod settings;
mod size;
mod texture;
mod upgrade;
mod writer;
pub use file::{
    config_directory,
    config_file_path,
    default_max_concurrent_jobs,
    num_cpus,
    CacheSettings,
    ConfigFile,
    ConfigFileError,
    ControlPlaneSettings,
    ExecutorSettings,
    GenerationSettings,
    LoggingSettings,
    NightSettings,
    PackagesSettings,
    PatchesSettings,
    PrefetchSettings,
    PrewarmSettings,
    ProviderSettings,
    TextureSettings,
    TuiSettings,
    XPlaneSettings,
    // Package manager defaults
    ALPHA_STAGING_LIBRARY_URL,
    // Pipeline defaults
    // Control plane defaults
    DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
    DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
    DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
    // Cache defaults
    DEFAULT_DDS_DISK_RATIO,
    DEFAULT_DISK_CACHE_SIZE,
    // Download defaults
    // Generation defaults
    DEFAULT_GENERATION_TIMEOUT_SECS,
    DEFAULT_LIBRARY_URL,
    DEFAULT_MAX_RETRIES,
    DEFAULT_MEMORY_CACHE_SIZE,
    // DownloadConfig defaults
    DEFAULT_PARALLEL_DOWNLOADS,
    DEFAULT_PREFETCH_CAPACITY_SHARE,
    DEFAULT_PREFETCH_CYCLE_INTERVAL_MS,
    DEFAULT_REQUEST_TIMEOUT_SECS,
    DEFAULT_WEB_API_PORT,
    LEGACY_LIBRARY_URL,
};
pub use keys::{ConfigKey, ConfigKeyError, SENSITIVE_VALUE_MASK};
pub use size::{format_size, parse_size, Size, SizeParseError, GB, KB, MB};
pub use texture::TextureConfig;
pub use upgrade::{
    analyze_config, deprecated_value, folded_value, upgrade_config, ConfigUpgradeAnalysis,
    DeprecatedValue, FoldedValue, UpgradeResult, DEPRECATED_KEYS, DEPRECATED_VALUES, FOLDED_VALUES,
};
// Re-export X-Plane utilities for backwards compatibility
// Prefer using xearthlayer::xplane module directly for new code
pub use crate::xplane::{
    derive_mountpoint, detect_custom_scenery, detect_scenery_dir, detect_xplane_install,
    detect_xplane_installs, SceneryDetectionResult, XPlanePathError,
};
