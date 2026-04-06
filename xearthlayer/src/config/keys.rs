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
    // General settings
    GeneralUpdateCheck,

    // Provider settings
    ProviderType,
    ProviderGoogleApiKey,
    ProviderMapboxAccessToken,

    // Cache settings
    CacheDirectory,
    CacheMemorySize,
    CacheDiskSize,
    CacheDdsDiskRatio,
    CacheDiskIoProfile,

    // Texture settings
    TextureFormat,
    TextureCompressor,
    TextureGpuDevice,

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
    PackagesConcurrentDownloads,

    // Logging settings
    LoggingFile,

    // Prefetch settings
    PrefetchEnabled,
    PrefetchStrategy,
    PrefetchMode,
    PrefetchWebApiPort,
    PrefetchMaxTilesPerCycle,
    PrefetchCycleIntervalMs,

    // Adaptive prefetch calibration settings
    PrefetchCalibrationAggressiveThreshold,
    PrefetchCalibrationOpportunisticThreshold,
    PrefetchCalibrationSampleDuration,

    // Transition ramp settings
    PrefetchTakeoffClimbFt,
    PrefetchTakeoffTimeoutSecs,
    PrefetchLandingHysteresisSecs,
    PrefetchRampDurationSecs,
    PrefetchRampStartFraction,

    // Boundary-driven prefetch settings
    PrefetchWindowBuffer,
    PrefetchStaleRegionTimeout,
    PrefetchDefaultWindowRows,
    PrefetchWindowLonExtent,
    PrefetchBoxExtent,
    PrefetchBoxMaxBias,

    // Control plane settings
    ControlPlaneMaxConcurrentJobs,
    ControlPlaneStallThresholdSecs,
    ControlPlaneHealthCheckIntervalSecs,
    ControlPlaneSemaphoreTimeoutSecs,

    // Prewarm settings
    PrewarmGridRows,
    PrewarmGridCols,

    // Patches settings
    PatchesEnabled,
    PatchesDirectory,

    // Executor settings
    ExecutorNetworkConcurrent,
    ExecutorCpuConcurrent,
    ExecutorDiskIoConcurrent,
    ExecutorMaxConcurrentTasks,
    ExecutorJobChannelCapacity,
    ExecutorRequestChannelCapacity,
    ExecutorRequestTimeoutSecs,
    ExecutorMaxRetries,
    ExecutorRetryBaseDelayMs,

    // FUSE settings
    FuseMaxBackground,
    FuseCongestionThreshold,
}

impl FromStr for ConfigKey {
    type Err = ConfigKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "general.update_check" => Ok(ConfigKey::GeneralUpdateCheck),

            "provider.type" => Ok(ConfigKey::ProviderType),
            "provider.google_api_key" => Ok(ConfigKey::ProviderGoogleApiKey),
            "provider.mapbox_access_token" => Ok(ConfigKey::ProviderMapboxAccessToken),

            "cache.directory" => Ok(ConfigKey::CacheDirectory),
            "cache.memory_size" => Ok(ConfigKey::CacheMemorySize),
            "cache.disk_size" => Ok(ConfigKey::CacheDiskSize),
            "cache.dds_disk_ratio" => Ok(ConfigKey::CacheDdsDiskRatio),
            "cache.disk_io_profile" => Ok(ConfigKey::CacheDiskIoProfile),

            "texture.format" => Ok(ConfigKey::TextureFormat),
            "texture.compressor" => Ok(ConfigKey::TextureCompressor),
            "texture.gpu_device" => Ok(ConfigKey::TextureGpuDevice),

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
            "packages.concurrent_downloads" => Ok(ConfigKey::PackagesConcurrentDownloads),

            "logging.file" => Ok(ConfigKey::LoggingFile),

            "prefetch.enabled" => Ok(ConfigKey::PrefetchEnabled),
            "prefetch.strategy" => Ok(ConfigKey::PrefetchStrategy),
            "prefetch.mode" => Ok(ConfigKey::PrefetchMode),
            "prefetch.web_api_port" => Ok(ConfigKey::PrefetchWebApiPort),
            "prefetch.max_tiles_per_cycle" => Ok(ConfigKey::PrefetchMaxTilesPerCycle),
            "prefetch.cycle_interval_ms" => Ok(ConfigKey::PrefetchCycleIntervalMs),
            "prefetch.calibration_aggressive_threshold" => {
                Ok(ConfigKey::PrefetchCalibrationAggressiveThreshold)
            }
            "prefetch.calibration_opportunistic_threshold" => {
                Ok(ConfigKey::PrefetchCalibrationOpportunisticThreshold)
            }
            "prefetch.calibration_sample_duration" => {
                Ok(ConfigKey::PrefetchCalibrationSampleDuration)
            }
            "prefetch.takeoff_climb_ft" => Ok(ConfigKey::PrefetchTakeoffClimbFt),
            "prefetch.takeoff_timeout_secs" => Ok(ConfigKey::PrefetchTakeoffTimeoutSecs),
            "prefetch.landing_hysteresis_secs" => Ok(ConfigKey::PrefetchLandingHysteresisSecs),
            "prefetch.ramp_duration_secs" => Ok(ConfigKey::PrefetchRampDurationSecs),
            "prefetch.ramp_start_fraction" => Ok(ConfigKey::PrefetchRampStartFraction),
            "prefetch.window_buffer" => Ok(ConfigKey::PrefetchWindowBuffer),
            "prefetch.stale_region_timeout" => Ok(ConfigKey::PrefetchStaleRegionTimeout),
            "prefetch.default_window_rows" => Ok(ConfigKey::PrefetchDefaultWindowRows),
            "prefetch.window_lon_extent" => Ok(ConfigKey::PrefetchWindowLonExtent),
            "prefetch.box_extent" => Ok(ConfigKey::PrefetchBoxExtent),
            "prefetch.box_max_bias" => Ok(ConfigKey::PrefetchBoxMaxBias),

            "control_plane.max_concurrent_jobs" => Ok(ConfigKey::ControlPlaneMaxConcurrentJobs),
            "control_plane.stall_threshold_secs" => Ok(ConfigKey::ControlPlaneStallThresholdSecs),
            "control_plane.health_check_interval_secs" => {
                Ok(ConfigKey::ControlPlaneHealthCheckIntervalSecs)
            }
            "control_plane.semaphore_timeout_secs" => {
                Ok(ConfigKey::ControlPlaneSemaphoreTimeoutSecs)
            }

            // Prewarm settings
            "prewarm.grid_rows" => Ok(ConfigKey::PrewarmGridRows),
            "prewarm.grid_cols" => Ok(ConfigKey::PrewarmGridCols),

            // Patches settings
            "patches.enabled" => Ok(ConfigKey::PatchesEnabled),
            "patches.directory" => Ok(ConfigKey::PatchesDirectory),

            // Executor settings
            "executor.network_concurrent" => Ok(ConfigKey::ExecutorNetworkConcurrent),
            "executor.cpu_concurrent" => Ok(ConfigKey::ExecutorCpuConcurrent),
            "executor.disk_io_concurrent" => Ok(ConfigKey::ExecutorDiskIoConcurrent),
            "executor.max_concurrent_tasks" => Ok(ConfigKey::ExecutorMaxConcurrentTasks),
            "executor.job_channel_capacity" => Ok(ConfigKey::ExecutorJobChannelCapacity),
            "executor.request_channel_capacity" => Ok(ConfigKey::ExecutorRequestChannelCapacity),
            "executor.request_timeout_secs" => Ok(ConfigKey::ExecutorRequestTimeoutSecs),
            "executor.max_retries" => Ok(ConfigKey::ExecutorMaxRetries),
            "executor.retry_base_delay_ms" => Ok(ConfigKey::ExecutorRetryBaseDelayMs),

            // FUSE settings
            "fuse.max_background" => Ok(ConfigKey::FuseMaxBackground),
            "fuse.congestion_threshold" => Ok(ConfigKey::FuseCongestionThreshold),

            _ => Err(ConfigKeyError::UnknownKey(s.to_string())),
        }
    }
}

impl ConfigKey {
    /// Get the canonical key name (e.g., "packages.library_url").
    pub fn name(&self) -> &'static str {
        match self {
            ConfigKey::GeneralUpdateCheck => "general.update_check",
            ConfigKey::ProviderType => "provider.type",
            ConfigKey::ProviderGoogleApiKey => "provider.google_api_key",
            ConfigKey::ProviderMapboxAccessToken => "provider.mapbox_access_token",
            ConfigKey::CacheDirectory => "cache.directory",
            ConfigKey::CacheMemorySize => "cache.memory_size",
            ConfigKey::CacheDiskSize => "cache.disk_size",
            ConfigKey::CacheDdsDiskRatio => "cache.dds_disk_ratio",
            ConfigKey::CacheDiskIoProfile => "cache.disk_io_profile",
            ConfigKey::TextureFormat => "texture.format",
            ConfigKey::TextureCompressor => "texture.compressor",
            ConfigKey::TextureGpuDevice => "texture.gpu_device",
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
            ConfigKey::PackagesConcurrentDownloads => "packages.concurrent_downloads",
            ConfigKey::LoggingFile => "logging.file",
            ConfigKey::PrefetchEnabled => "prefetch.enabled",
            ConfigKey::PrefetchStrategy => "prefetch.strategy",
            ConfigKey::PrefetchMode => "prefetch.mode",
            ConfigKey::PrefetchWebApiPort => "prefetch.web_api_port",
            ConfigKey::PrefetchMaxTilesPerCycle => "prefetch.max_tiles_per_cycle",
            ConfigKey::PrefetchCycleIntervalMs => "prefetch.cycle_interval_ms",
            ConfigKey::PrefetchCalibrationAggressiveThreshold => {
                "prefetch.calibration_aggressive_threshold"
            }
            ConfigKey::PrefetchCalibrationOpportunisticThreshold => {
                "prefetch.calibration_opportunistic_threshold"
            }
            ConfigKey::PrefetchCalibrationSampleDuration => "prefetch.calibration_sample_duration",
            ConfigKey::PrefetchTakeoffClimbFt => "prefetch.takeoff_climb_ft",
            ConfigKey::PrefetchTakeoffTimeoutSecs => "prefetch.takeoff_timeout_secs",
            ConfigKey::PrefetchLandingHysteresisSecs => "prefetch.landing_hysteresis_secs",
            ConfigKey::PrefetchRampDurationSecs => "prefetch.ramp_duration_secs",
            ConfigKey::PrefetchRampStartFraction => "prefetch.ramp_start_fraction",
            ConfigKey::PrefetchWindowBuffer => "prefetch.window_buffer",
            ConfigKey::PrefetchStaleRegionTimeout => "prefetch.stale_region_timeout",
            ConfigKey::PrefetchDefaultWindowRows => "prefetch.default_window_rows",
            ConfigKey::PrefetchWindowLonExtent => "prefetch.window_lon_extent",
            ConfigKey::PrefetchBoxExtent => "prefetch.box_extent",
            ConfigKey::PrefetchBoxMaxBias => "prefetch.box_max_bias",
            ConfigKey::ControlPlaneMaxConcurrentJobs => "control_plane.max_concurrent_jobs",
            ConfigKey::ControlPlaneStallThresholdSecs => "control_plane.stall_threshold_secs",
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => {
                "control_plane.health_check_interval_secs"
            }
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => "control_plane.semaphore_timeout_secs",

            // Prewarm settings
            ConfigKey::PrewarmGridRows => "prewarm.grid_rows",
            ConfigKey::PrewarmGridCols => "prewarm.grid_cols",

            // Patches settings
            ConfigKey::PatchesEnabled => "patches.enabled",
            ConfigKey::PatchesDirectory => "patches.directory",

            // Executor settings
            ConfigKey::ExecutorNetworkConcurrent => "executor.network_concurrent",
            ConfigKey::ExecutorCpuConcurrent => "executor.cpu_concurrent",
            ConfigKey::ExecutorDiskIoConcurrent => "executor.disk_io_concurrent",
            ConfigKey::ExecutorMaxConcurrentTasks => "executor.max_concurrent_tasks",
            ConfigKey::ExecutorJobChannelCapacity => "executor.job_channel_capacity",
            ConfigKey::ExecutorRequestChannelCapacity => "executor.request_channel_capacity",
            ConfigKey::ExecutorRequestTimeoutSecs => "executor.request_timeout_secs",
            ConfigKey::ExecutorMaxRetries => "executor.max_retries",
            ConfigKey::ExecutorRetryBaseDelayMs => "executor.retry_base_delay_ms",

            // FUSE settings
            ConfigKey::FuseMaxBackground => "fuse.max_background",
            ConfigKey::FuseCongestionThreshold => "fuse.congestion_threshold",
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
            ConfigKey::GeneralUpdateCheck => config.general.update_check.to_string(),
            ConfigKey::ProviderType => config.provider.provider_type.clone(),
            ConfigKey::ProviderGoogleApiKey => {
                config.provider.google_api_key.clone().unwrap_or_default()
            }
            ConfigKey::ProviderMapboxAccessToken => config
                .provider
                .mapbox_access_token
                .clone()
                .unwrap_or_default(),
            ConfigKey::CacheDirectory => path_to_display(&config.cache.directory),
            ConfigKey::CacheMemorySize => format_size(config.cache.memory_size),
            ConfigKey::CacheDiskSize => format_size(config.cache.disk_size),
            ConfigKey::CacheDdsDiskRatio => config.cache.dds_disk_ratio.to_string(),
            ConfigKey::CacheDiskIoProfile => config.cache.disk_io_profile.as_str().to_string(),
            ConfigKey::TextureFormat => config.texture.format.to_string().to_lowercase(),
            ConfigKey::TextureCompressor => config.texture.compressor.clone(),
            ConfigKey::TextureGpuDevice => config.texture.gpu_device.clone(),
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
            ConfigKey::PackagesConcurrentDownloads => {
                config.packages.concurrent_downloads.to_string()
            }
            ConfigKey::LoggingFile => path_to_display(&config.logging.file),
            ConfigKey::PrefetchEnabled => config.prefetch.enabled.to_string(),
            ConfigKey::PrefetchStrategy => config.prefetch.strategy.clone(),
            ConfigKey::PrefetchMode => config.prefetch.mode.clone(),
            ConfigKey::PrefetchWebApiPort => config.prefetch.web_api_port.to_string(),
            ConfigKey::PrefetchMaxTilesPerCycle => config.prefetch.max_tiles_per_cycle.to_string(),
            ConfigKey::PrefetchCycleIntervalMs => config.prefetch.cycle_interval_ms.to_string(),
            ConfigKey::PrefetchCalibrationAggressiveThreshold => {
                config.prefetch.calibration_aggressive_threshold.to_string()
            }
            ConfigKey::PrefetchCalibrationOpportunisticThreshold => config
                .prefetch
                .calibration_opportunistic_threshold
                .to_string(),
            ConfigKey::PrefetchCalibrationSampleDuration => {
                config.prefetch.calibration_sample_duration.to_string()
            }
            ConfigKey::PrefetchTakeoffClimbFt => config.prefetch.takeoff_climb_ft.to_string(),
            ConfigKey::PrefetchTakeoffTimeoutSecs => {
                config.prefetch.takeoff_timeout_secs.to_string()
            }
            ConfigKey::PrefetchLandingHysteresisSecs => {
                config.prefetch.landing_hysteresis_secs.to_string()
            }
            ConfigKey::PrefetchRampDurationSecs => config.prefetch.ramp_duration_secs.to_string(),
            ConfigKey::PrefetchRampStartFraction => config.prefetch.ramp_start_fraction.to_string(),
            ConfigKey::PrefetchWindowBuffer => config.prefetch.window_buffer.to_string(),
            ConfigKey::PrefetchStaleRegionTimeout => {
                config.prefetch.stale_region_timeout.to_string()
            }
            ConfigKey::PrefetchDefaultWindowRows => config.prefetch.default_window_rows.to_string(),
            ConfigKey::PrefetchWindowLonExtent => config.prefetch.window_lon_extent.to_string(),
            ConfigKey::PrefetchBoxExtent => config.prefetch.box_extent.to_string(),
            ConfigKey::PrefetchBoxMaxBias => config.prefetch.box_max_bias.to_string(),
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

            // Prewarm settings
            ConfigKey::PrewarmGridRows => config.prewarm.grid_rows.to_string(),
            ConfigKey::PrewarmGridCols => config.prewarm.grid_cols.to_string(),

            // Patches settings
            ConfigKey::PatchesEnabled => config.patches.enabled.to_string(),
            ConfigKey::PatchesDirectory => config
                .patches
                .directory
                .as_ref()
                .map(|p| path_to_display(p))
                .unwrap_or_default(),

            // Executor settings
            ConfigKey::ExecutorNetworkConcurrent => config.executor.network_concurrent.to_string(),
            ConfigKey::ExecutorCpuConcurrent => config.executor.cpu_concurrent.to_string(),
            ConfigKey::ExecutorDiskIoConcurrent => config.executor.disk_io_concurrent.to_string(),
            ConfigKey::ExecutorMaxConcurrentTasks => {
                config.executor.max_concurrent_tasks.to_string()
            }
            ConfigKey::ExecutorJobChannelCapacity => {
                config.executor.job_channel_capacity.to_string()
            }
            ConfigKey::ExecutorRequestChannelCapacity => {
                config.executor.request_channel_capacity.to_string()
            }
            ConfigKey::ExecutorRequestTimeoutSecs => {
                config.executor.request_timeout_secs.to_string()
            }
            ConfigKey::ExecutorMaxRetries => config.executor.max_retries.to_string(),
            ConfigKey::ExecutorRetryBaseDelayMs => config.executor.retry_base_delay_ms.to_string(),

            // FUSE settings
            ConfigKey::FuseMaxBackground => config.fuse.max_background.to_string(),
            ConfigKey::FuseCongestionThreshold => config.fuse.congestion_threshold.to_string(),
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
            ConfigKey::GeneralUpdateCheck => {
                let v = value.to_lowercase();
                config.general.update_check = v == "true" || v == "1" || v == "yes" || v == "on";
            }
            ConfigKey::ProviderType => {
                config.provider.provider_type = value.to_lowercase();
            }
            ConfigKey::ProviderGoogleApiKey => {
                config.provider.google_api_key = optional_string(value);
            }
            ConfigKey::ProviderMapboxAccessToken => {
                config.provider.mapbox_access_token = optional_string(value);
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
            ConfigKey::CacheDdsDiskRatio => {
                config.cache.dds_disk_ratio = value.parse().unwrap();
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
            ConfigKey::TextureCompressor => {
                config.texture.compressor = value.to_lowercase();
            }
            ConfigKey::TextureGpuDevice => {
                config.texture.gpu_device = value.to_string();
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
            ConfigKey::PackagesConcurrentDownloads => {
                config.packages.concurrent_downloads = value.parse().unwrap();
            }
            ConfigKey::LoggingFile => {
                config.logging.file = expand_tilde(value);
            }
            ConfigKey::PrefetchEnabled => {
                let v = value.to_lowercase();
                config.prefetch.enabled = v == "true" || v == "1" || v == "yes" || v == "on";
            }
            ConfigKey::PrefetchStrategy => {
                config.prefetch.strategy = value.to_lowercase();
            }
            ConfigKey::PrefetchMode => {
                config.prefetch.mode = value.to_lowercase();
            }
            ConfigKey::PrefetchWebApiPort => {
                config.prefetch.web_api_port = value.parse().unwrap();
            }
            ConfigKey::PrefetchMaxTilesPerCycle => {
                config.prefetch.max_tiles_per_cycle = value.parse().unwrap();
            }
            ConfigKey::PrefetchCycleIntervalMs => {
                config.prefetch.cycle_interval_ms = value.parse().unwrap();
            }
            ConfigKey::PrefetchCalibrationAggressiveThreshold => {
                config.prefetch.calibration_aggressive_threshold = value.parse().unwrap();
            }
            ConfigKey::PrefetchCalibrationOpportunisticThreshold => {
                config.prefetch.calibration_opportunistic_threshold = value.parse().unwrap();
            }
            ConfigKey::PrefetchCalibrationSampleDuration => {
                config.prefetch.calibration_sample_duration = value.parse().unwrap();
            }
            ConfigKey::PrefetchTakeoffClimbFt => {
                config.prefetch.takeoff_climb_ft = value.parse().unwrap();
            }
            ConfigKey::PrefetchTakeoffTimeoutSecs => {
                config.prefetch.takeoff_timeout_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchLandingHysteresisSecs => {
                config.prefetch.landing_hysteresis_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchRampDurationSecs => {
                config.prefetch.ramp_duration_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchRampStartFraction => {
                config.prefetch.ramp_start_fraction = value.parse().unwrap();
            }
            ConfigKey::PrefetchWindowBuffer => {
                config.prefetch.window_buffer = value.parse().unwrap();
            }
            ConfigKey::PrefetchStaleRegionTimeout => {
                config.prefetch.stale_region_timeout = value.parse().unwrap();
            }
            ConfigKey::PrefetchDefaultWindowRows => {
                config.prefetch.default_window_rows = value.parse().unwrap();
            }
            ConfigKey::PrefetchWindowLonExtent => {
                config.prefetch.window_lon_extent = value.parse().unwrap();
            }
            ConfigKey::PrefetchBoxExtent => {
                config.prefetch.box_extent = value.parse().unwrap();
            }
            ConfigKey::PrefetchBoxMaxBias => {
                config.prefetch.box_max_bias = value.parse().unwrap();
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

            // Prewarm settings
            ConfigKey::PrewarmGridRows => {
                config.prewarm.grid_rows = value.parse().unwrap();
            }
            ConfigKey::PrewarmGridCols => {
                config.prewarm.grid_cols = value.parse().unwrap();
            }

            // Patches settings
            ConfigKey::PatchesEnabled => {
                let v = value.to_lowercase();
                config.patches.enabled = v == "true" || v == "1" || v == "yes" || v == "on";
            }
            ConfigKey::PatchesDirectory => {
                config.patches.directory = optional_path(value);
            }

            // Executor settings
            ConfigKey::ExecutorNetworkConcurrent => {
                config.executor.network_concurrent = value.parse().unwrap();
            }
            ConfigKey::ExecutorCpuConcurrent => {
                config.executor.cpu_concurrent = value.parse().unwrap();
            }
            ConfigKey::ExecutorDiskIoConcurrent => {
                config.executor.disk_io_concurrent = value.parse().unwrap();
            }
            ConfigKey::ExecutorMaxConcurrentTasks => {
                config.executor.max_concurrent_tasks = value.parse().unwrap();
            }
            ConfigKey::ExecutorJobChannelCapacity => {
                config.executor.job_channel_capacity = value.parse().unwrap();
            }
            ConfigKey::ExecutorRequestChannelCapacity => {
                config.executor.request_channel_capacity = value.parse().unwrap();
            }
            ConfigKey::ExecutorRequestTimeoutSecs => {
                config.executor.request_timeout_secs = value.parse().unwrap();
            }
            ConfigKey::ExecutorMaxRetries => {
                config.executor.max_retries = value.parse().unwrap();
            }
            ConfigKey::ExecutorRetryBaseDelayMs => {
                config.executor.retry_base_delay_ms = value.parse().unwrap();
            }

            // FUSE settings
            ConfigKey::FuseMaxBackground => {
                config.fuse.max_background = value.parse().unwrap();
            }
            ConfigKey::FuseCongestionThreshold => {
                config.fuse.congestion_threshold = value.parse().unwrap();
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
            ConfigKey::GeneralUpdateCheck => Box::new(BooleanSpec),
            ConfigKey::ProviderType => Box::new(OneOfSpec::new(&[
                "apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs",
            ])),
            ConfigKey::ProviderGoogleApiKey => Box::new(AnyStringSpec),
            ConfigKey::ProviderMapboxAccessToken => Box::new(AnyStringSpec),
            ConfigKey::CacheDirectory => Box::new(PathSpec),
            ConfigKey::CacheMemorySize => Box::new(SizeSpec),
            ConfigKey::CacheDiskSize => Box::new(SizeSpec),
            ConfigKey::CacheDdsDiskRatio => Box::new(FloatRangeSpec::new(0.0, 1.0)),
            ConfigKey::CacheDiskIoProfile => {
                Box::new(OneOfSpec::new(&["auto", "hdd", "ssd", "nvme"]))
            }
            ConfigKey::TextureFormat => Box::new(OneOfSpec::new(&["bc1", "bc3"])),
            ConfigKey::TextureCompressor => Box::new(OneOfSpec::new(&["software", "ispc", "gpu"])),
            ConfigKey::TextureGpuDevice => Box::new(NonEmptyStringSpec),
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
            ConfigKey::PackagesConcurrentDownloads => Box::new(IntegerRangeSpec::new(1, 10)),
            ConfigKey::LoggingFile => Box::new(PathSpec),
            ConfigKey::PrefetchEnabled => Box::new(BooleanSpec),
            ConfigKey::PrefetchStrategy => Box::new(OneOfSpec::new(&["auto", "adaptive"])),
            ConfigKey::PrefetchMode => Box::new(OneOfSpec::new(&[
                "auto",
                "aggressive",
                "opportunistic",
                "disabled",
            ])),
            ConfigKey::PrefetchWebApiPort => Box::new(IntegerRangeSpec::new(1024, 65535)),
            ConfigKey::PrefetchMaxTilesPerCycle => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchCycleIntervalMs => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchCalibrationAggressiveThreshold => Box::new(PositiveNumberSpec),
            ConfigKey::PrefetchCalibrationOpportunisticThreshold => Box::new(PositiveNumberSpec),
            ConfigKey::PrefetchCalibrationSampleDuration => Box::new(PositiveIntegerSpec),
            ConfigKey::PrefetchTakeoffClimbFt => Box::new(FloatRangeSpec::new(200.0, 5000.0)),
            ConfigKey::PrefetchTakeoffTimeoutSecs => Box::new(IntegerRangeSpec::new(30, 300)),
            ConfigKey::PrefetchLandingHysteresisSecs => Box::new(IntegerRangeSpec::new(5, 60)),
            ConfigKey::PrefetchRampDurationSecs => Box::new(IntegerRangeSpec::new(10, 120)),
            ConfigKey::PrefetchRampStartFraction => Box::new(FloatRangeSpec::new(0.1, 0.5)),
            ConfigKey::PrefetchWindowBuffer => Box::new(IntegerRangeSpec::new(0, 3)),
            ConfigKey::PrefetchStaleRegionTimeout => Box::new(IntegerRangeSpec::new(30, 600)),
            ConfigKey::PrefetchDefaultWindowRows => Box::new(IntegerRangeSpec::new(2, 12)),
            ConfigKey::PrefetchWindowLonExtent => Box::new(FloatRangeSpec::new(1.0, 10.0)),
            ConfigKey::PrefetchBoxExtent => Box::new(FloatRangeSpec::new(7.0, 15.0)),
            ConfigKey::PrefetchBoxMaxBias => Box::new(FloatRangeSpec::new(0.5, 0.9)),
            ConfigKey::ControlPlaneMaxConcurrentJobs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneStallThresholdSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneHealthCheckIntervalSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs => Box::new(PositiveIntegerSpec),

            // Prewarm settings
            ConfigKey::PrewarmGridRows => Box::new(PositiveIntegerSpec),
            ConfigKey::PrewarmGridCols => Box::new(PositiveIntegerSpec),

            // Patches settings
            ConfigKey::PatchesEnabled => Box::new(BooleanSpec),
            ConfigKey::PatchesDirectory => Box::new(OptionalPathSpec),

            // Executor settings
            ConfigKey::ExecutorNetworkConcurrent => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorCpuConcurrent => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorDiskIoConcurrent => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorMaxConcurrentTasks => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorJobChannelCapacity => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorRequestChannelCapacity => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorRequestTimeoutSecs => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorMaxRetries => Box::new(PositiveIntegerSpec),
            ConfigKey::ExecutorRetryBaseDelayMs => Box::new(PositiveIntegerSpec),

            // FUSE settings — range: 1-1024 for both
            ConfigKey::FuseMaxBackground => Box::new(IntegerRangeSpec::new(1, 1024)),
            ConfigKey::FuseCongestionThreshold => Box::new(IntegerRangeSpec::new(1, 1024)),
        }
    }

    /// Get all supported configuration keys.
    ///
    /// Note: Deprecated keys (like pipeline.*) are NOT included here.
    /// They remain parseable for backwards compatibility but are not
    /// expected in new configs. See upgrade.rs DEPRECATED_KEYS for the list.
    pub fn all() -> &'static [ConfigKey] {
        &[
            ConfigKey::GeneralUpdateCheck,
            ConfigKey::ProviderType,
            ConfigKey::ProviderGoogleApiKey,
            ConfigKey::ProviderMapboxAccessToken,
            ConfigKey::CacheDirectory,
            ConfigKey::CacheMemorySize,
            ConfigKey::CacheDiskSize,
            ConfigKey::CacheDdsDiskRatio,
            ConfigKey::CacheDiskIoProfile,
            ConfigKey::TextureFormat,
            ConfigKey::TextureCompressor,
            ConfigKey::TextureGpuDevice,
            ConfigKey::DownloadTimeout,
            ConfigKey::GenerationThreads,
            ConfigKey::GenerationTimeout,
            // Note: pipeline.* keys are deprecated - use executor.* instead
            ConfigKey::XplaneSceneryDir,
            ConfigKey::PackagesLibraryUrl,
            ConfigKey::PackagesInstallLocation,
            ConfigKey::PackagesCustomSceneryPath,
            ConfigKey::PackagesAutoInstallOverlays,
            ConfigKey::PackagesTempDir,
            ConfigKey::PackagesConcurrentDownloads,
            ConfigKey::LoggingFile,
            ConfigKey::PrefetchEnabled,
            ConfigKey::PrefetchStrategy,
            ConfigKey::PrefetchMode,
            ConfigKey::PrefetchWebApiPort,
            ConfigKey::PrefetchMaxTilesPerCycle,
            ConfigKey::PrefetchCycleIntervalMs,
            ConfigKey::PrefetchCalibrationAggressiveThreshold,
            ConfigKey::PrefetchCalibrationOpportunisticThreshold,
            ConfigKey::PrefetchCalibrationSampleDuration,
            ConfigKey::PrefetchTakeoffClimbFt,
            ConfigKey::PrefetchTakeoffTimeoutSecs,
            ConfigKey::PrefetchLandingHysteresisSecs,
            ConfigKey::PrefetchRampDurationSecs,
            ConfigKey::PrefetchRampStartFraction,
            ConfigKey::PrefetchWindowBuffer,
            ConfigKey::PrefetchStaleRegionTimeout,
            ConfigKey::PrefetchDefaultWindowRows,
            ConfigKey::PrefetchWindowLonExtent,
            ConfigKey::PrefetchBoxExtent,
            ConfigKey::PrefetchBoxMaxBias,
            ConfigKey::ControlPlaneMaxConcurrentJobs,
            ConfigKey::ControlPlaneStallThresholdSecs,
            ConfigKey::ControlPlaneHealthCheckIntervalSecs,
            ConfigKey::ControlPlaneSemaphoreTimeoutSecs,
            // Prewarm settings
            ConfigKey::PrewarmGridRows,
            ConfigKey::PrewarmGridCols,
            // Patches settings
            ConfigKey::PatchesEnabled,
            ConfigKey::PatchesDirectory,
            // Executor settings
            ConfigKey::ExecutorNetworkConcurrent,
            ConfigKey::ExecutorCpuConcurrent,
            ConfigKey::ExecutorDiskIoConcurrent,
            ConfigKey::ExecutorMaxConcurrentTasks,
            ConfigKey::ExecutorJobChannelCapacity,
            ConfigKey::ExecutorRequestChannelCapacity,
            ConfigKey::ExecutorRequestTimeoutSecs,
            ConfigKey::ExecutorMaxRetries,
            ConfigKey::ExecutorRetryBaseDelayMs,
            // FUSE settings
            ConfigKey::FuseMaxBackground,
            ConfigKey::FuseCongestionThreshold,
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

/// Specification that requires a non-empty string value.
struct NonEmptyStringSpec;

impl ValueSpecification for NonEmptyStringSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            Err("must be a non-empty string".to_string())
        } else {
            Ok(())
        }
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

/// Specification for integer values within an inclusive range.
struct IntegerRangeSpec {
    min: u64,
    max: u64,
}

impl IntegerRangeSpec {
    fn new(min: u64, max: u64) -> Self {
        Self { min, max }
    }
}

impl ValueSpecification for IntegerRangeSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        value
            .parse::<u64>()
            .map_err(|_| format!("must be an integer between {} and {}", self.min, self.max))
            .and_then(|n| {
                if n >= self.min && n <= self.max {
                    Ok(())
                } else {
                    Err(format!(
                        "must be an integer between {} and {}",
                        self.min, self.max
                    ))
                }
            })
    }
}

/// Specification for floating-point values within an inclusive range.
struct FloatRangeSpec {
    min: f64,
    max: f64,
}

impl FloatRangeSpec {
    fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }
}

impl ValueSpecification for FloatRangeSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        value
            .parse::<f64>()
            .map_err(|_| format!("must be a number between {} and {}", self.min, self.max))
            .and_then(|n| {
                if n >= self.min && n <= self.max {
                    Ok(())
                } else {
                    Err(format!(
                        "must be a number between {} and {}",
                        self.min, self.max
                    ))
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

    #[test]
    fn test_integer_range_spec() {
        let spec = IntegerRangeSpec::new(30, 300);
        assert!(spec.is_satisfied_by("30").is_ok());
        assert!(spec.is_satisfied_by("300").is_ok());
        assert!(spec.is_satisfied_by("90").is_ok());
        assert!(spec.is_satisfied_by("29").is_err());
        assert!(spec.is_satisfied_by("301").is_err());
        assert!(spec.is_satisfied_by("abc").is_err());
    }

    #[test]
    fn test_float_range_spec() {
        let spec = FloatRangeSpec::new(0.1, 0.5);
        assert!(spec.is_satisfied_by("0.1").is_ok());
        assert!(spec.is_satisfied_by("0.5").is_ok());
        assert!(spec.is_satisfied_by("0.25").is_ok());
        assert!(spec.is_satisfied_by("0.09").is_err());
        assert!(spec.is_satisfied_by("0.51").is_err());
        assert!(spec.is_satisfied_by("abc").is_err());
    }

    #[test]
    fn test_config_key_takeoff_climb_ft_round_trip() {
        let mut config = ConfigFile::default();
        let key = ConfigKey::PrefetchTakeoffClimbFt;
        assert_eq!(key.get(&config), "1000"); // default
        key.set(&mut config, "2000").unwrap();
        assert_eq!(key.get(&config), "2000");
    }

    #[test]
    fn test_window_buffer_config_key() {
        let key = ConfigKey::from_str("prefetch.window_buffer").unwrap();
        assert_eq!(key.name(), "prefetch.window_buffer");
        assert!(key.validate("1").is_ok());
        assert!(key.validate("0").is_ok());
        assert!(key.validate("3").is_ok());
        assert!(key.validate("4").is_err());
    }

    #[test]
    fn test_stale_region_timeout_config_key() {
        let key = ConfigKey::from_str("prefetch.stale_region_timeout").unwrap();
        assert_eq!(key.name(), "prefetch.stale_region_timeout");
        assert!(key.validate("120").is_ok());
        assert!(key.validate("30").is_ok());
        assert!(key.validate("600").is_ok());
        assert!(key.validate("29").is_err());
        assert!(key.validate("601").is_err());
    }

    #[test]
    fn test_default_window_rows_config_key() {
        let key = ConfigKey::from_str("prefetch.default_window_rows").unwrap();
        assert_eq!(key.name(), "prefetch.default_window_rows");
        assert!(key.validate("3").is_ok());
        assert!(key.validate("12").is_ok());
        assert!(key.validate("2").is_ok());
        assert!(key.validate("1").is_err());
        assert!(key.validate("13").is_err());
    }

    #[test]
    fn test_window_lon_extent_config_key() {
        let key = ConfigKey::from_str("prefetch.window_lon_extent").unwrap();
        assert_eq!(key.name(), "prefetch.window_lon_extent");
        assert!(key.validate("3.0").is_ok());
        assert!(key.validate("1.0").is_ok());
        assert!(key.validate("10.0").is_ok());
        assert!(key.validate("0.5").is_err());
        assert!(key.validate("11.0").is_err());
    }

    #[test]
    fn test_new_prefetch_keys_in_all() {
        let all = ConfigKey::all();
        assert!(all.contains(&ConfigKey::PrefetchWindowBuffer));
        assert!(all.contains(&ConfigKey::PrefetchStaleRegionTimeout));
        assert!(all.contains(&ConfigKey::PrefetchDefaultWindowRows));
        assert!(all.contains(&ConfigKey::PrefetchWindowLonExtent));
    }

    #[test]
    fn test_boundary_prefetch_keys_round_trip() {
        let mut config = ConfigFile::default();

        ConfigKey::PrefetchWindowBuffer
            .set(&mut config, "2")
            .unwrap();
        assert_eq!(ConfigKey::PrefetchWindowBuffer.get(&config), "2");

        ConfigKey::PrefetchStaleRegionTimeout
            .set(&mut config, "300")
            .unwrap();
        assert_eq!(ConfigKey::PrefetchStaleRegionTimeout.get(&config), "300");

        ConfigKey::PrefetchDefaultWindowRows
            .set(&mut config, "8")
            .unwrap();
        assert_eq!(ConfigKey::PrefetchDefaultWindowRows.get(&config), "8");

        ConfigKey::PrefetchWindowLonExtent
            .set(&mut config, "4.0")
            .unwrap();
        assert_eq!(ConfigKey::PrefetchWindowLonExtent.get(&config), "4");
    }

    #[test]
    fn test_config_key_ramp_start_fraction_validation() {
        let key = ConfigKey::PrefetchRampStartFraction;
        assert!(key.validate("0.25").is_ok());
        assert!(key.validate("0.1").is_ok());
        assert!(key.validate("0.5").is_ok());
        assert!(key.validate("0.05").is_err());
        assert!(key.validate("0.6").is_err());
    }

    #[test]
    fn test_dds_disk_ratio_config_key_parse() {
        let key: ConfigKey = "cache.dds_disk_ratio".parse().unwrap();
        assert_eq!(key, ConfigKey::CacheDdsDiskRatio);
        assert_eq!(key.name(), "cache.dds_disk_ratio");
    }

    #[test]
    fn test_dds_disk_ratio_validation() {
        let key = ConfigKey::CacheDdsDiskRatio;
        assert!(key.validate("0.0").is_ok());
        assert!(key.validate("0.6").is_ok());
        assert!(key.validate("1.0").is_ok());
        assert!(key.validate("0.75").is_ok());
        assert!(key.validate("-0.1").is_err());
        assert!(key.validate("1.1").is_err());
        assert!(key.validate("abc").is_err());
    }

    #[test]
    fn test_dds_disk_ratio_round_trip() {
        let mut config = ConfigFile::default();
        let key = ConfigKey::CacheDdsDiskRatio;
        assert_eq!(key.get(&config), "0.6"); // default
        key.set(&mut config, "0.75").unwrap();
        assert_eq!(key.get(&config), "0.75");
        assert!((config.cache.dds_disk_ratio - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_dds_disk_ratio_in_all_keys() {
        let all = ConfigKey::all();
        assert!(all.contains(&ConfigKey::CacheDdsDiskRatio));
    }

    #[test]
    fn test_texture_compressor_key_parse() {
        let key: ConfigKey = "texture.compressor".parse().unwrap();
        assert_eq!(key, ConfigKey::TextureCompressor);
        assert_eq!(key.name(), "texture.compressor");
    }

    #[test]
    fn test_texture_gpu_device_key_parse() {
        let key: ConfigKey = "texture.gpu_device".parse().unwrap();
        assert_eq!(key, ConfigKey::TextureGpuDevice);
        assert_eq!(key.name(), "texture.gpu_device");
    }

    #[test]
    fn test_texture_compressor_validation() {
        let key = ConfigKey::TextureCompressor;
        assert!(key.validate("software").is_ok());
        assert!(key.validate("ispc").is_ok());
        assert!(key.validate("gpu").is_ok());
        assert!(key.validate("invalid").is_err());
    }

    #[test]
    fn test_texture_gpu_device_validation() {
        let key = ConfigKey::TextureGpuDevice;
        assert!(key.validate("integrated").is_ok());
        assert!(key.validate("discrete").is_ok());
        assert!(key.validate("Radeon").is_ok());
        assert!(key.validate("RTX 5090").is_ok());
        assert!(key.validate("").is_err());
        assert!(key.validate("   ").is_err());
    }
}
