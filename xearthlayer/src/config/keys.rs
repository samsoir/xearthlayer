//! Configuration key access and validation.
//!
//! This module provides a type-safe interface for getting and setting
//! configuration values by key name, with validation via the Specification Pattern.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

use super::file::ConfigFile;
use super::size::{format_size, parse_size};
use crate::dds::DdsFormat;

/// Errors that can occur when getting or setting configuration values.
#[derive(Debug, Error)]
pub enum ConfigKeyError {
    /// Unknown configuration key.
    #[error("Unknown configuration key '{0}'")]
    UnknownKey(String),

    /// Validation failed for the value.
    #[error("Invalid value for {key}: {reason}")]
    ValidationFailed { key: String, reason: String },
}

/// Supported configuration keys.
///
/// Each key maps to a specific field in [`ConfigFile`] and knows how to
/// get and set its value with proper validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKey {
    // Provider settings
    ProviderType,
    ProviderGoogleApiKey,

    // Cache settings
    CacheDirectory,
    CacheMemorySize,
    CacheDiskSize,
    CacheDiskIoProfile,

    // Texture settings
    TextureFormat,

    // Download settings
    DownloadTimeout,

    // Generation settings
    GenerationThreads,
    GenerationTimeout,

    // Pipeline settings
    PipelineMaxHttpConcurrent,
    PipelineMaxCpuConcurrent,
    PipelineMaxPrefetchInFlight,
    PipelineRequestTimeoutSecs,
    PipelineMaxRetries,
    PipelineRetryBaseDelayMs,
    PipelineCoalesceChannelCapacity,

    // X-Plane settings
    XplaneSceneryDir,

    // Packages settings
    PackagesLibraryUrl,
    PackagesInstallLocation,
    PackagesCustomSceneryPath,
    PackagesAutoInstallOverlays,
    PackagesTempDir,

    // Logging settings
    LoggingFile,

    // Prefetch settings
    PrefetchEnabled,
    PrefetchUdpPort,
    PrefetchConeAngle,
    PrefetchConeDistanceNm,
    PrefetchRadialRadiusNm,
    PrefetchBatchSize,
    PrefetchMaxInFlight,
    PrefetchRadialRadius,

    // Control plane settings
    ControlPlaneMaxConcurrentJobs,
    ControlPlaneStallThresholdSecs,
    ControlPlaneHealthCheckIntervalSecs,
    ControlPlaneSemaphoreTimeoutSecs,
}

impl FromStr for ConfigKey {
    type Err = ConfigKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "provider.type" => Ok(ConfigKey::ProviderType),
            "provider.google_api_key" => Ok(ConfigKey::ProviderGoogleApiKey),

            "cache.directory" => Ok(ConfigKey::CacheDirectory),
            "cache.memory_size" => Ok(ConfigKey::CacheMemorySize),
            "cache.disk_size" => Ok(ConfigKey::CacheDiskSize),
            "cache.disk_io_profile" => Ok(ConfigKey::CacheDiskIoProfile),

            "texture.format" => Ok(ConfigKey::TextureFormat),

            "download.timeout" => Ok(ConfigKey::DownloadTimeout),

            "generation.threads" => Ok(ConfigKey::GenerationThreads),
            "generation.timeout" => Ok(ConfigKey::GenerationTimeout),

            "pipeline.max_http_concurrent" => Ok(ConfigKey::PipelineMaxHttpConcurrent),
            "pipeline.max_cpu_concurrent" => Ok(ConfigKey::PipelineMaxCpuConcurrent),
            "pipeline.max_prefetch_in_flight" => Ok(ConfigKey::PipelineMaxPrefetchInFlight),
            "pipeline.request_timeout_secs" => Ok(ConfigKey::PipelineRequestTimeoutSecs),
            "pipeline.max_retries" => Ok(ConfigKey::PipelineMaxRetries),
            "pipeline.retry_base_delay_ms" => Ok(ConfigKey::PipelineRetryBaseDelayMs),
            "pipeline.coalesce_channel_capacity" => Ok(ConfigKey::PipelineCoalesceChannelCapacity),

            "xplane.scenery_dir" => Ok(ConfigKey::XplaneSceneryDir),

            "packages.library_url" => Ok(ConfigKey::PackagesLibraryUrl),
            "packages.install_location" => Ok(ConfigKey::PackagesInstallLocation),
            "packages.custom_scenery_path" => Ok(ConfigKey::PackagesCustomSceneryPath),
            "packages.auto_install_overlays" => Ok(ConfigKey::PackagesAutoInstallOverlays),
            "packages.temp_dir" => Ok(ConfigKey::PackagesTempDir),

            "logging.file" => Ok(ConfigKey::LoggingFile),

            "prefetch.enabled" => Ok(ConfigKey::PrefetchEnabled),
            "prefetch.udp_port" => Ok(ConfigKey::PrefetchUdpPort),
            "prefetch.cone_angle" => Ok(ConfigKey::PrefetchConeAngle),
            "prefetch.cone_distance_nm" => Ok(ConfigKey::PrefetchConeDistanceNm),
            "prefetch.radial_radius_nm" => Ok(ConfigKey::PrefetchRadialRadiusNm),
            "prefetch.batch_size" => Ok(ConfigKey::PrefetchBatchSize),
            "prefetch.max_in_flight" => Ok(ConfigKey::PrefetchMaxInFlight),
            "prefetch.radial_radius" => Ok(ConfigKey::PrefetchRadialRadius),

            "control_plane.max_concurrent_jobs" => Ok(ConfigKey::ControlPlaneMaxConcurrentJobs),
            "control_plane.stall_threshold_secs" => Ok(ConfigKey::ControlPlaneStallThresholdSecs),
            "control_plane.health_check_interval_secs" => {
                Ok(ConfigKey::ControlPlaneHealthCheckIntervalSecs)
            }
            "control_plane.semaphore_timeout_secs" => {
                Ok(ConfigKey::ControlPlaneSemaphoreTimeoutSecs)
            }

            _ => Err(ConfigKeyError::UnknownKey(s.to_string())),
        }
    }
}

impl ConfigKey {
    /// Get the canonical key name (e.g., "packages.library_url").
    pub fn name(&self) -> &'static str {
        match self {
            ConfigKey::ProviderType => "provider.type",
            ConfigKey::ProviderGoogleApiKey => "provider.google_api_key",
            ConfigKey::CacheDirectory => "cache.directory",
            ConfigKey::CacheMemorySize => "cache.memory_size",
            ConfigKey::CacheDiskSize => "cache.disk_size",
            ConfigKey::CacheDiskIoProfile => "cache.disk_io_profile",
            ConfigKey::TextureFormat => "texture.format",
            ConfigKey::DownloadTimeout => "download.timeout",
            ConfigKey::GenerationThreads => "generation.threads",
            ConfigKey::GenerationTimeout => "generation.timeout",
            ConfigKey::PipelineMaxHttpConcurrent => "pipeline.max_http_concurrent",
            ConfigKey::PipelineMaxCpuConcurrent => "pipeline.max_cpu_concurrent",
            ConfigKey::PipelineMaxPrefetchInFlight => "pipeline.max_prefetch_in_flight",
            ConfigKey::PipelineRequestTimeoutSecs => "pipeline.request_timeout_secs",
            ConfigKey::PipelineMaxRetries => "pipeline.max_retries",
            ConfigKey::PipelineRetryBaseDelayMs => "pipeline.retry_base_delay_ms",
            ConfigKey::PipelineCoalesceChannelCapacity => "pipeline.coalesce_channel_capacity",
            ConfigKey::XplaneSceneryDir => "xplane.scenery_dir",
            ConfigKey::PackagesLibraryUrl => "packages.library_url",
            ConfigKey::PackagesInstallLocation => "packages.install_location",
            ConfigKey::PackagesCustomSceneryPath => "packages.custom_scenery_path",
            ConfigKey::PackagesAutoInstallOverlays => "packages.auto_install_overlays",
            ConfigKey::PackagesTempDir => "packages.temp_dir",
            ConfigKey::LoggingFile => "logging.file",
            ConfigKey::PrefetchEnabled => "prefetch.enabled",
            ConfigKey::PrefetchUdpPort => "prefetch.udp_port",
            ConfigKey::PrefetchConeAngle => "prefetch.cone_angle",
            ConfigKey::PrefetchConeDistanceNm => "prefetch.cone_distance_nm",
            ConfigKey::PrefetchRadialRadiusNm => "prefetch.radial_radius_nm",
            ConfigKey::PrefetchBatchSize => "prefetch.batch_size",
            ConfigKey::PrefetchMaxInFlight => "prefetch.max_in_flight",
            ConfigKey::PrefetchRadialRadius => "prefetch.radial_radius",
            ConfigKey::ControlPlaneMaxConcurrentJobs => "control_plane.max_concurrent_jobs",
            ConfigKey::ControlPlaneStallThresholdSecs => "control_plane.stall_threshold_secs",
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => {
                "control_plane.health_check_interval_secs"
            }
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => "control_plane.semaphore_timeout_secs",
        }
    }

    /// Get the section name (e.g., "packages").
    pub fn section(&self) -> &'static str {
        self.name().split('.').next().unwrap_or("")
    }

    /// Get the key name within the section (e.g., "library_url").
    pub fn key_name(&self) -> &'static str {
        self.name().split('.').nth(1).unwrap_or(self.name())
    }

    /// Get the value from a config file as a string.
    pub fn get(&self, config: &ConfigFile) -> String {
        match self {
            ConfigKey::ProviderType => config.provider.provider_type.clone(),
            ConfigKey::ProviderGoogleApiKey => {
                config.provider.google_api_key.clone().unwrap_or_default()
            }
            ConfigKey::CacheDirectory => path_to_display(&config.cache.directory),
            ConfigKey::CacheMemorySize => format_size(config.cache.memory_size),
            ConfigKey::CacheDiskSize => format_size(config.cache.disk_size),
            ConfigKey::CacheDiskIoProfile => config.cache.disk_io_profile.as_str().to_string(),
            ConfigKey::TextureFormat => config.texture.format.to_string().to_lowercase(),
            ConfigKey::DownloadTimeout => config.download.timeout.to_string(),
            ConfigKey::GenerationThreads => config.generation.threads.to_string(),
            ConfigKey::GenerationTimeout => config.generation.timeout.to_string(),
            ConfigKey::PipelineMaxHttpConcurrent => config.pipeline.max_http_concurrent.to_string(),
            ConfigKey::PipelineMaxCpuConcurrent => config.pipeline.max_cpu_concurrent.to_string(),
            ConfigKey::PipelineMaxPrefetchInFlight => {
                config.pipeline.max_prefetch_in_flight.to_string()
            }
            ConfigKey::PipelineRequestTimeoutSecs => {
                config.pipeline.request_timeout_secs.to_string()
            }
            ConfigKey::PipelineMaxRetries => config.pipeline.max_retries.to_string(),
            ConfigKey::PipelineRetryBaseDelayMs => config.pipeline.retry_base_delay_ms.to_string(),
            ConfigKey::PipelineCoalesceChannelCapacity => {
                config.pipeline.coalesce_channel_capacity.to_string()
            }
            ConfigKey::XplaneSceneryDir => config
                .xplane
                .scenery_dir
                .as_ref()
                .map(|p| path_to_display(p))
                .unwrap_or_default(),
            ConfigKey::PackagesLibraryUrl => {
                config.packages.library_url.clone().unwrap_or_default()
            }
            ConfigKey::PackagesInstallLocation => config
                .packages
                .install_location
                .as_ref()
                .map(|p| path_to_display(p))
                .unwrap_or_default(),
            ConfigKey::PackagesCustomSceneryPath => config
                .packages
                .custom_scenery_path
                .as_ref()
                .map(|p| path_to_display(p))
                .unwrap_or_default(),
            ConfigKey::PackagesAutoInstallOverlays => {
                config.packages.auto_install_overlays.to_string()
            }
            ConfigKey::PackagesTempDir => config
                .packages
                .temp_dir
                .as_ref()
                .map(|p| path_to_display(p))
                .unwrap_or_default(),
            ConfigKey::LoggingFile => path_to_display(&config.logging.file),
            ConfigKey::PrefetchEnabled => config.prefetch.enabled.to_string(),
            ConfigKey::PrefetchUdpPort => config.prefetch.udp_port.to_string(),
            ConfigKey::PrefetchConeAngle => config.prefetch.cone_angle.to_string(),
            ConfigKey::PrefetchConeDistanceNm => config.prefetch.cone_distance_nm.to_string(),
            ConfigKey::PrefetchRadialRadiusNm => config.prefetch.radial_radius_nm.to_string(),
            ConfigKey::PrefetchBatchSize => config.prefetch.batch_size.to_string(),
            ConfigKey::PrefetchMaxInFlight => config.prefetch.max_in_flight.to_string(),
            ConfigKey::PrefetchRadialRadius => config.prefetch.radial_radius.to_string(),
            ConfigKey::ControlPlaneMaxConcurrentJobs => {
                config.control_plane.max_concurrent_jobs.to_string()
            }
            ConfigKey::ControlPlaneStallThresholdSecs => {
                config.control_plane.stall_threshold_secs.to_string()
            }
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => {
                config.control_plane.health_check_interval_secs.to_string()
            }
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => {
                config.control_plane.semaphore_timeout_secs.to_string()
            }
        }
    }

    /// Set the value in a config file.
    ///
    /// Validates the value according to the key's specification before setting.
    pub fn set(&self, config: &mut ConfigFile, value: &str) -> Result<(), ConfigKeyError> {
        self.validate(value)?;
        self.set_unchecked(config, value);
        Ok(())
    }

    /// Set the value without validation. Use `set()` for validated setting.
    fn set_unchecked(&self, config: &mut ConfigFile, value: &str) {
        match self {
            ConfigKey::ProviderType => {
                config.provider.provider_type = value.to_lowercase();
            }
            ConfigKey::ProviderGoogleApiKey => {
                config.provider.google_api_key = optional_string(value);
            }
            ConfigKey::CacheDirectory => {
                config.cache.directory = expand_tilde(value);
            }
            ConfigKey::CacheMemorySize => {
                // Validation ensures this won't panic
                config.cache.memory_size = parse_size(value).unwrap();
            }
            ConfigKey::CacheDiskSize => {
                config.cache.disk_size = parse_size(value).unwrap();
            }
            ConfigKey::CacheDiskIoProfile => {
                config.cache.disk_io_profile = value.parse().unwrap();
            }
            ConfigKey::TextureFormat => {
                config.texture.format = match value.to_lowercase().as_str() {
                    "bc1" => DdsFormat::BC1,
                    "bc3" => DdsFormat::BC3,
                    _ => DdsFormat::BC1, // Validation prevents this
                };
            }
            ConfigKey::DownloadTimeout => {
                config.download.timeout = value.parse().unwrap();
            }
            ConfigKey::GenerationThreads => {
                config.generation.threads = value.parse().unwrap();
            }
            ConfigKey::GenerationTimeout => {
                config.generation.timeout = value.parse().unwrap();
            }
            ConfigKey::PipelineMaxHttpConcurrent => {
                config.pipeline.max_http_concurrent = value.parse().unwrap();
            }
            ConfigKey::PipelineMaxCpuConcurrent => {
                config.pipeline.max_cpu_concurrent = value.parse().unwrap();
            }
            ConfigKey::PipelineMaxPrefetchInFlight => {
                config.pipeline.max_prefetch_in_flight = value.parse().unwrap();
            }
            ConfigKey::PipelineRequestTimeoutSecs => {
                config.pipeline.request_timeout_secs = value.parse().unwrap();
            }
            ConfigKey::PipelineMaxRetries => {
                config.pipeline.max_retries = value.parse().unwrap();
            }
            ConfigKey::PipelineRetryBaseDelayMs => {
                config.pipeline.retry_base_delay_ms = value.parse().unwrap();
            }
            ConfigKey::PipelineCoalesceChannelCapacity => {
                config.pipeline.coalesce_channel_capacity = value.parse().unwrap();
            }
            ConfigKey::XplaneSceneryDir => {
                config.xplane.scenery_dir = optional_path(value);
            }
            ConfigKey::PackagesLibraryUrl => {
                config.packages.library_url = optional_string(value);
            }
            ConfigKey::PackagesInstallLocation => {
                config.packages.install_location = optional_path(value);
            }
            ConfigKey::PackagesCustomSceneryPath => {
                config.packages.custom_scenery_path = optional_path(value);
            }
            ConfigKey::PackagesAutoInstallOverlays => {
                let v = value.to_lowercase();
                config.packages.auto_install_overlays =
                    v == "true" || v == "1" || v == "yes" || v == "on";
            }
            ConfigKey::PackagesTempDir => {
                config.packages.temp_dir = optional_path(value);
            }
            ConfigKey::LoggingFile => {
                config.logging.file = expand_tilde(value);
            }
            ConfigKey::PrefetchEnabled => {
                let v = value.to_lowercase();
                config.prefetch.enabled = v == "true" || v == "1" || v == "yes" || v == "on";
            }
            ConfigKey::PrefetchUdpPort => {
                config.prefetch.udp_port = value.parse().unwrap();
            }
            ConfigKey::PrefetchConeAngle => {
                config.prefetch.cone_angle = value.parse().unwrap();
            }
            ConfigKey::PrefetchConeDistanceNm => {
                config.prefetch.cone_distance_nm = value.parse().unwrap();
            }
            ConfigKey::PrefetchRadialRadiusNm => {
                config.prefetch.radial_radius_nm = value.parse().unwrap();
            }
            ConfigKey::PrefetchBatchSize => {
                config.prefetch.batch_size = value.parse().unwrap();
            }
            ConfigKey::PrefetchMaxInFlight => {
                config.prefetch.max_in_flight = value.parse().unwrap();
            }
            ConfigKey::PrefetchRadialRadius => {
                config.prefetch.radial_radius = value.parse().unwrap();
            }
            ConfigKey::ControlPlaneMaxConcurrentJobs => {
                config.control_plane.max_concurrent_jobs = value.parse().unwrap();
            }
            ConfigKey::ControlPlaneStallThresholdSecs => {
                config.control_plane.stall_threshold_secs = value.parse().unwrap();
            }
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => {
                config.control_plane.health_check_interval_secs = value.parse().unwrap();
            }
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => {
                config.control_plane.semaphore_timeout_secs = value.parse().unwrap();
            }
        }
    }

    /// Validate a value according to this key's specification.
    pub fn validate(&self, value: &str) -> Result<(), ConfigKeyError> {
        self.specification()
            .is_satisfied_by(value)
            .map_err(|reason| ConfigKeyError::ValidationFailed {
                key: self.name().to_string(),
                reason,
            })
    }

    /// Get the validation specification for this key.
    fn specification(&self) -> Box<dyn ValueSpecification> {
        match self {
            ConfigKey::ProviderType => Box::new(OneOfSpec::new(&[
                "apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs",
            ])),
            ConfigKey::ProviderGoogleApiKey => Box::new(AnyStringSpec),
            ConfigKey::CacheDirectory => Box::new(PathSpec),
            ConfigKey::CacheMemorySize => Box::new(SizeSpec),
            ConfigKey::CacheDiskSize => Box::new(SizeSpec),
            ConfigKey::CacheDiskIoProfile => {
                Box::new(OneOfSpec::new(&["auto", "hdd", "ssd", "nvme"]))
            }
            ConfigKey::TextureFormat => Box::new(OneOfSpec::new(&["bc1", "bc3"])),
            ConfigKey::DownloadTimeout => Box::new(PositiveIntegerSpec),
            ConfigKey::GenerationThreads => Box::new(PositiveIntegerSpec),
            ConfigKey::GenerationTimeout => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineMaxHttpConcurrent => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineMaxCpuConcurrent => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineMaxPrefetchInFlight => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineRequestTimeoutSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineMaxRetries => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineRetryBaseDelayMs => Box::new(PositiveIntegerSpec),
            ConfigKey::PipelineCoalesceChannelCapacity => Box::new(PositiveIntegerSpec),
            ConfigKey::XplaneSceneryDir => Box::new(OptionalPathSpec),
            ConfigKey::PackagesLibraryUrl => Box::new(OptionalUrlSpec),
            ConfigKey::PackagesInstallLocation => Box::new(OptionalPathSpec),
            ConfigKey::PackagesCustomSceneryPath => Box::new(OptionalPathSpec),
            ConfigKey::PackagesAutoInstallOverlays => Box::new(BooleanSpec),
            ConfigKey::PackagesTempDir => Box::new(OptionalPathSpec),
            ConfigKey::LoggingFile => Box::new(PathSpec),
            ConfigKey::PrefetchEnabled => Box::new(BooleanSpec),
            ConfigKey::PrefetchUdpPort => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchConeAngle => Box::new(PositiveNumberSpec),
            ConfigKey::PrefetchConeDistanceNm => Box::new(PositiveNumberSpec),
            ConfigKey::PrefetchRadialRadiusNm => Box::new(PositiveNumberSpec),
            ConfigKey::PrefetchBatchSize => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchMaxInFlight => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchRadialRadius => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneMaxConcurrentJobs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneStallThresholdSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => Box::new(PositiveIntegerSpec),
        }
    }

    /// Get all supported configuration keys.
    pub fn all() -> &'static [ConfigKey] {
        &[
            ConfigKey::ProviderType,
            ConfigKey::ProviderGoogleApiKey,
            ConfigKey::CacheDirectory,
            ConfigKey::CacheMemorySize,
            ConfigKey::CacheDiskSize,
            ConfigKey::CacheDiskIoProfile,
            ConfigKey::TextureFormat,
            ConfigKey::DownloadTimeout,
            ConfigKey::GenerationThreads,
            ConfigKey::GenerationTimeout,
            ConfigKey::PipelineMaxHttpConcurrent,
            ConfigKey::PipelineMaxCpuConcurrent,
            ConfigKey::PipelineMaxPrefetchInFlight,
            ConfigKey::PipelineRequestTimeoutSecs,
            ConfigKey::PipelineMaxRetries,
            ConfigKey::PipelineRetryBaseDelayMs,
            ConfigKey::PipelineCoalesceChannelCapacity,
            ConfigKey::XplaneSceneryDir,
            ConfigKey::PackagesLibraryUrl,
            ConfigKey::PackagesInstallLocation,
            ConfigKey::PackagesCustomSceneryPath,
            ConfigKey::PackagesAutoInstallOverlays,
            ConfigKey::PackagesTempDir,
            ConfigKey::LoggingFile,
            ConfigKey::PrefetchEnabled,
            ConfigKey::PrefetchUdpPort,
            ConfigKey::PrefetchConeAngle,
            ConfigKey::PrefetchConeDistanceNm,
            ConfigKey::PrefetchRadialRadiusNm,
            ConfigKey::PrefetchBatchSize,
            ConfigKey::PrefetchMaxInFlight,
            ConfigKey::PrefetchRadialRadius,
            ConfigKey::ControlPlaneMaxConcurrentJobs,
            ConfigKey::ControlPlaneStallThresholdSecs,
            ConfigKey::ControlPlaneHealthCheckIntervalSecs,
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs,
        ]
    }
}

// ============================================================================
// Value Specifications (Specification Pattern)
// ============================================================================

/// Trait for value validation specifications.
trait ValueSpecification {
    /// Check if the value satisfies this specification.
    /// Returns Ok(()) if valid, Err(reason) if invalid.
    fn is_satisfied_by(&self, value: &str) -> Result<(), String>;
}

/// Specification that accepts any string value.
struct AnyStringSpec;

impl ValueSpecification for AnyStringSpec {
    fn is_satisfied_by(&self, _value: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Specification that requires the value to be one of a set of options.
struct OneOfSpec {
    options: &'static [&'static str],
}

impl OneOfSpec {
    fn new(options: &'static [&'static str]) -> Self {
        Self { options }
    }
}

impl ValueSpecification for OneOfSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        let lower = value.to_lowercase();
        if self.options.iter().any(|opt| *opt == lower) {
            Ok(())
        } else {
            Err(format!("must be one of: {}", self.options.join(", ")))
        }
    }
}

/// Specification for size values (e.g., "2GB", "500MB").
struct SizeSpec;

impl ValueSpecification for SizeSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        parse_size(value)
            .map(|_| ())
            .map_err(|_| "must be a size like '2GB', '500MB', or '1024KB'".to_string())
    }
}

/// Specification for positive integer values.
struct PositiveIntegerSpec;

impl ValueSpecification for PositiveIntegerSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        value
            .parse::<u64>()
            .map(|_| ())
            .map_err(|_| "must be a positive integer".to_string())
    }
}

/// Specification for positive floating-point number values.
struct PositiveNumberSpec;

impl ValueSpecification for PositiveNumberSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        value
            .parse::<f64>()
            .map_err(|_| "must be a positive number".to_string())
            .and_then(|n| {
                if n >= 0.0 {
                    Ok(())
                } else {
                    Err("must be a positive number".to_string())
                }
            })
    }
}

/// Specification for boolean values.
struct BooleanSpec;

impl ValueSpecification for BooleanSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        let lower = value.to_lowercase();
        let valid = ["true", "false", "yes", "no", "1", "0", "on", "off"];
        if valid.contains(&lower.as_str()) {
            Ok(())
        } else {
            Err("must be true/false, yes/no, 1/0, or on/off".to_string())
        }
    }
}

/// Specification for path values (non-empty).
struct PathSpec;

impl ValueSpecification for PathSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            Err("must be a valid path".to_string())
        } else {
            Ok(())
        }
    }
}

/// Specification for optional path values (empty allowed).
struct OptionalPathSpec;

impl ValueSpecification for OptionalPathSpec {
    fn is_satisfied_by(&self, _value: &str) -> Result<(), String> {
        // Empty is allowed for optional paths
        Ok(())
    }
}

/// Specification for optional URL values.
struct OptionalUrlSpec;

impl ValueSpecification for OptionalUrlSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Ok(());
        }
        if value.starts_with("http://") || value.starts_with("https://") {
            Ok(())
        } else {
            Err("must be a URL starting with 'http://' or 'https://'".to_string())
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Expand ~ to home directory in paths.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Convert path to display string, collapsing home dir to ~.
fn path_to_display(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

/// Convert empty string to None, non-empty to Some.
fn optional_string(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Convert empty string to None, non-empty to Some path with tilde expansion.
fn optional_path(value: &str) -> Option<PathBuf> {
    if value.is_empty() {
        None
    } else {
        Some(expand_tilde(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_key_parsing() {
        assert_eq!(
            "packages.library_url".parse::<ConfigKey>().unwrap(),
            ConfigKey::PackagesLibraryUrl
        );
        assert_eq!(
            "provider.type".parse::<ConfigKey>().unwrap(),
            ConfigKey::ProviderType
        );
        // Case insensitive
        assert_eq!(
            "PACKAGES.LIBRARY_URL".parse::<ConfigKey>().unwrap(),
            ConfigKey::PackagesLibraryUrl
        );
        assert!("invalid.key".parse::<ConfigKey>().is_err());
    }

    #[test]
    fn test_key_name_parts() {
        assert_eq!(ConfigKey::PackagesLibraryUrl.section(), "packages");
        assert_eq!(ConfigKey::PackagesLibraryUrl.key_name(), "library_url");
        assert_eq!(ConfigKey::ProviderType.section(), "provider");
        assert_eq!(ConfigKey::ProviderType.key_name(), "type");
    }

    #[test]
    fn test_get_value() {
        let config = ConfigFile::default();

        assert_eq!(ConfigKey::ProviderType.get(&config), "bing");
        assert_eq!(ConfigKey::DownloadTimeout.get(&config), "30");
        assert_eq!(ConfigKey::PackagesAutoInstallOverlays.get(&config), "false");
    }

    #[test]
    fn test_set_value() {
        let mut config = ConfigFile::default();

        ConfigKey::ProviderType.set(&mut config, "google").unwrap();
        assert_eq!(config.provider.provider_type, "google");

        ConfigKey::DownloadTimeout.set(&mut config, "60").unwrap();
        assert_eq!(config.download.timeout, 60);

        ConfigKey::PackagesAutoInstallOverlays
            .set(&mut config, "true")
            .unwrap();
        assert!(config.packages.auto_install_overlays);
    }

    #[test]
    fn test_validate_provider_type() {
        assert!(ConfigKey::ProviderType.validate("apple").is_ok());
        assert!(ConfigKey::ProviderType.validate("arcgis").is_ok());
        assert!(ConfigKey::ProviderType.validate("bing").is_ok());
        assert!(ConfigKey::ProviderType.validate("go2").is_ok());
        assert!(ConfigKey::ProviderType.validate("google").is_ok());
        assert!(ConfigKey::ProviderType.validate("mapbox").is_ok());
        assert!(ConfigKey::ProviderType.validate("usgs").is_ok());
        assert!(ConfigKey::ProviderType.validate("BING").is_ok()); // Case insensitive
        assert!(ConfigKey::ProviderType.validate("invalid").is_err());
    }

    #[test]
    fn test_validate_url() {
        assert!(ConfigKey::PackagesLibraryUrl.validate("").is_ok());
        assert!(ConfigKey::PackagesLibraryUrl
            .validate("https://example.com")
            .is_ok());
        assert!(ConfigKey::PackagesLibraryUrl
            .validate("http://example.com")
            .is_ok());
        assert!(ConfigKey::PackagesLibraryUrl.validate("not-a-url").is_err());
    }

    #[test]
    fn test_validate_size() {
        assert!(ConfigKey::CacheMemorySize.validate("2GB").is_ok());
        assert!(ConfigKey::CacheMemorySize.validate("500MB").is_ok());
        assert!(ConfigKey::CacheMemorySize.validate("1024KB").is_ok());
        assert!(ConfigKey::CacheMemorySize.validate("invalid").is_err());
    }

    #[test]
    fn test_validate_boolean() {
        for valid in &["true", "false", "yes", "no", "1", "0", "on", "off"] {
            assert!(
                ConfigKey::PackagesAutoInstallOverlays
                    .validate(valid)
                    .is_ok(),
                "Expected '{}' to be valid",
                valid
            );
        }
        assert!(ConfigKey::PackagesAutoInstallOverlays
            .validate("maybe")
            .is_err());
    }

    #[test]
    fn test_validate_positive_integer() {
        assert!(ConfigKey::DownloadTimeout.validate("30").is_ok());
        assert!(ConfigKey::DownloadTimeout.validate("0").is_ok());
        assert!(ConfigKey::DownloadTimeout.validate("-1").is_err());
        assert!(ConfigKey::DownloadTimeout.validate("abc").is_err());
    }

    #[test]
    fn test_set_invalid_value_fails() {
        let mut config = ConfigFile::default();

        let result = ConfigKey::ProviderType.set(&mut config, "invalid");
        assert!(result.is_err());

        // Config should be unchanged
        assert_eq!(config.provider.provider_type, "bing");
    }

    #[test]
    fn test_clear_optional_value() {
        let mut config = ConfigFile::default();

        // Set a value first
        ConfigKey::PackagesLibraryUrl
            .set(&mut config, "https://example.com")
            .unwrap();
        assert!(config.packages.library_url.is_some());

        // Clear it
        ConfigKey::PackagesLibraryUrl.set(&mut config, "").unwrap();
        assert!(config.packages.library_url.is_none());
    }

    #[test]
    fn test_all_keys() {
        let keys = ConfigKey::all();
        assert!(keys.len() > 10); // Sanity check
        assert!(keys.contains(&ConfigKey::ProviderType));
        assert!(keys.contains(&ConfigKey::PackagesLibraryUrl));
    }
}
