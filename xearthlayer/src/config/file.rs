//! Configuration file handling for ~/.xearthlayer/config.ini.
//!
//! Loads and saves user configuration with sensible defaults.

use crate::config::size::{format_size, parse_size};
use crate::dds::DdsFormat;
use crate::pipeline::DiskIoProfile;
use ini::Ini;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Configuration file errors.
#[derive(Debug, Error)]
pub enum ConfigFileError {
    /// Failed to read config file
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] ini::Error),

    /// Failed to write config file
    #[error("Failed to write config file: {0}")]
    WriteError(String),

    /// Invalid configuration value
    #[error("Invalid configuration: {section}.{key} = '{value}' - {reason}")]
    InvalidValue {
        section: String,
        key: String,
        value: String,
        reason: String,
    },

    /// Failed to create config directory
    #[error("Failed to create config directory: {0}")]
    DirectoryError(std::io::Error),
}

/// Complete application configuration loaded from config.ini.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// Provider settings
    pub provider: ProviderSettings,
    /// Cache settings
    pub cache: CacheSettings,
    /// Texture settings
    pub texture: TextureSettings,
    /// Download settings
    pub download: DownloadSettings,
    /// Generation settings
    pub generation: GenerationSettings,
    /// Pipeline settings for concurrency and retry behavior
    pub pipeline: PipelineSettings,
    /// X-Plane settings
    pub xplane: XPlaneSettings,
    /// Package manager settings
    pub packages: PackagesSettings,
    /// Logging settings
    pub logging: LoggingSettings,
    /// Prefetch settings for predictive tile caching
    pub prefetch: PrefetchSettings,
    /// Control plane settings for job management and health monitoring
    pub control_plane: ControlPlaneSettings,
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    /// Provider type: "apple", "arcgis", "bing", "go2", "google", "mapbox", or "usgs"
    pub provider_type: String,
    /// Google Maps API key (only required for "google" provider)
    pub google_api_key: Option<String>,
    /// MapBox access token (only required for "mapbox" provider)
    pub mapbox_access_token: Option<String>,
}

/// Cache configuration.
#[derive(Debug, Clone)]
pub struct CacheSettings {
    /// Cache directory path
    pub directory: PathBuf,
    /// Memory cache size in bytes
    pub memory_size: usize,
    /// Disk cache size in bytes
    pub disk_size: usize,
    /// Disk I/O profile for tuning concurrency based on storage type
    pub disk_io_profile: DiskIoProfile,
}

/// Texture configuration.
#[derive(Debug, Clone)]
pub struct TextureSettings {
    /// DDS format: BC1 or BC3
    pub format: DdsFormat,
}

/// Download configuration.
#[derive(Debug, Clone)]
pub struct DownloadSettings {
    /// Timeout in seconds for HTTP requests.
    pub timeout: u64,
}

/// Generation configuration.
#[derive(Debug, Clone)]
pub struct GenerationSettings {
    /// Number of threads for parallel tile generation.
    /// Default: number of CPU cores.
    pub threads: usize,
    /// Timeout in seconds for generating a single tile.
    /// If exceeded, returns a magenta placeholder.
    /// Default: 10 seconds.
    pub timeout: u64,
}

/// Pipeline configuration for concurrency and retry behavior.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    /// Maximum concurrent HTTP requests across all tiles.
    /// Default: min(num_cpus * 16, 256)
    pub max_http_concurrent: usize,
    /// Maximum concurrent CPU-bound operations (assemble + encode stages).
    /// Default: num_cpus * 1.25, minimum num_cpus + 2
    pub max_cpu_concurrent: usize,
    /// Maximum concurrent prefetch jobs in flight.
    /// Default: max(num_cpus / 4, 2) - leaves 75% of resources for on-demand
    pub max_prefetch_in_flight: usize,
    /// HTTP request timeout in seconds for individual chunk downloads.
    /// Default: 10 seconds
    pub request_timeout_secs: u64,
    /// Maximum retry attempts per failed chunk download.
    /// Default: 3
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff between retries.
    /// Actual delay = base_delay * 2^attempt (e.g., 100ms, 200ms, 400ms, 800ms)
    /// Default: 100
    pub retry_base_delay_ms: u64,
    /// Broadcast channel capacity for request coalescing.
    /// Default: 16
    pub coalesce_channel_capacity: usize,
}

/// X-Plane configuration.
#[derive(Debug, Clone)]
pub struct XPlaneSettings {
    /// Custom Scenery directory (None = auto-detect)
    pub scenery_dir: Option<PathBuf>,
}

/// Package manager configuration.
#[derive(Debug, Clone)]
pub struct PackagesSettings {
    /// URL to the package library index.
    /// This is where the package manager fetches the list of available packages.
    pub library_url: Option<String>,
    /// Local directory for installed packages (default: ~/.xearthlayer/packages).
    pub install_location: Option<PathBuf>,
    /// X-Plane Custom Scenery directory for overlay symlinks.
    /// If None, auto-detects from xplane.scenery_dir or ~/.x-plane/x-plane_install_12.txt
    pub custom_scenery_path: Option<PathBuf>,
    /// Automatically install overlay packages when installing ortho for same region.
    pub auto_install_overlays: bool,
    /// Temporary directory for downloads (default: system temp dir).
    pub temp_dir: Option<PathBuf>,
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingSettings {
    /// Log file path
    pub file: PathBuf,
}

/// Prefetch configuration for predictive tile caching.
#[derive(Debug, Clone)]
pub struct PrefetchSettings {
    /// Enable predictive tile prefetching based on X-Plane telemetry
    pub enabled: bool,
    /// UDP port for X-Plane telemetry (default: 49002 for ForeFlight protocol)
    pub udp_port: u16,
    /// Prediction cone half-angle in degrees (default: 45)
    pub cone_angle: f32,
    /// Cone distance ahead in nautical miles (default: 100)
    pub cone_distance_nm: f32,
    /// Radial buffer radius in nautical miles (default: 60)
    pub radial_radius_nm: f32,
    /// Maximum prefetch requests per prediction cycle (default: 500)
    pub batch_size: usize,
    /// Maximum concurrent in-flight prefetch requests (default: 2000)
    pub max_in_flight: usize,
    /// Radial prefetcher tile radius (default: 3, giving a 7×7 = 49 tile grid)
    pub radial_radius: u8,
}

/// Control plane configuration for job management and health monitoring.
#[derive(Debug, Clone)]
pub struct ControlPlaneSettings {
    /// Maximum concurrent jobs (DDS requests) allowed.
    /// Default: num_cpus × 2
    pub max_concurrent_jobs: usize,
    /// Time threshold in seconds for considering a job stalled.
    /// Jobs in the same stage longer than this are recovered.
    /// Default: 60 seconds
    pub stall_threshold_secs: u64,
    /// Interval in seconds for health monitoring checks.
    /// Default: 5 seconds
    pub health_check_interval_secs: u64,
    /// Timeout in seconds for acquiring a job slot.
    /// On-demand requests wait up to this long before failing.
    /// Default: 30 seconds
    pub semaphore_timeout_secs: u64,
}

impl Default for ConfigFile {
    fn default() -> Self {
        let config_dir = config_directory();
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("xearthlayer");

        Self {
            provider: ProviderSettings {
                provider_type: "bing".to_string(),
                google_api_key: None,
                mapbox_access_token: None,
            },
            cache: CacheSettings {
                directory: cache_dir,
                memory_size: DEFAULT_MEMORY_CACHE_SIZE,
                disk_size: DEFAULT_DISK_CACHE_SIZE,
                disk_io_profile: DiskIoProfile::Auto,
            },
            texture: TextureSettings {
                format: DdsFormat::BC1,
            },
            download: DownloadSettings {
                timeout: DEFAULT_DOWNLOAD_TIMEOUT_SECS,
            },
            generation: GenerationSettings {
                threads: num_cpus(),
                timeout: DEFAULT_GENERATION_TIMEOUT_SECS,
            },
            pipeline: PipelineSettings {
                max_http_concurrent: default_http_concurrent(),
                max_cpu_concurrent: default_cpu_concurrent(),
                max_prefetch_in_flight: default_prefetch_in_flight(),
                request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
                max_retries: DEFAULT_MAX_RETRIES,
                retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
                coalesce_channel_capacity: DEFAULT_COALESCE_CHANNEL_CAPACITY,
            },
            xplane: XPlaneSettings { scenery_dir: None },
            packages: PackagesSettings {
                library_url: None,
                install_location: None,
                custom_scenery_path: None,
                auto_install_overlays: false,
                temp_dir: None,
            },
            logging: LoggingSettings {
                file: config_dir.join("xearthlayer.log"),
            },
            prefetch: PrefetchSettings {
                enabled: true,
                udp_port: DEFAULT_PREFETCH_UDP_PORT,
                cone_angle: DEFAULT_PREFETCH_CONE_ANGLE,
                cone_distance_nm: DEFAULT_PREFETCH_CONE_DISTANCE_NM,
                radial_radius_nm: DEFAULT_PREFETCH_RADIAL_RADIUS_NM,
                batch_size: DEFAULT_PREFETCH_BATCH_SIZE,
                max_in_flight: DEFAULT_PREFETCH_MAX_IN_FLIGHT,
                radial_radius: DEFAULT_PREFETCH_RADIAL_RADIUS,
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

/// Get the number of available CPU cores.
pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Default HTTP concurrency limit: min(num_cpus * 16, 256)
pub fn default_http_concurrent() -> usize {
    (num_cpus() * 16).min(256)
}

/// Default CPU concurrency limit: num_cpus * 1.25, minimum num_cpus + 2
pub fn default_cpu_concurrent() -> usize {
    let cpus = num_cpus();
    ((cpus as f64 * 1.25).ceil() as usize).max(cpus + 2)
}

/// Default prefetch in-flight limit: max(num_cpus / 4, 2)
pub fn default_prefetch_in_flight() -> usize {
    (num_cpus() / 4).max(2)
}

/// Default request timeout in seconds.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Default maximum retry attempts.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default retry backoff base delay in milliseconds.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 100;

/// Default coalesce channel capacity.
pub const DEFAULT_COALESCE_CHANNEL_CAPACITY: usize = 16;

/// Default maximum concurrent downloads per tile.
pub const DEFAULT_MAX_CONCURRENT_DOWNLOADS: usize = 256;

// =============================================================================
// Cache defaults
// =============================================================================

/// Default memory cache size (2GB).
pub const DEFAULT_MEMORY_CACHE_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// Default disk cache size (20GB).
pub const DEFAULT_DISK_CACHE_SIZE: usize = 20 * 1024 * 1024 * 1024;

// =============================================================================
// Download defaults
// =============================================================================

/// Default download timeout in seconds.
pub const DEFAULT_DOWNLOAD_TIMEOUT_SECS: u64 = 30;

// =============================================================================
// Generation defaults
// =============================================================================

/// Default generation timeout in seconds.
pub const DEFAULT_GENERATION_TIMEOUT_SECS: u64 = 10;

// =============================================================================
// Prefetch defaults
// =============================================================================

/// Default UDP port for X-Plane telemetry (ForeFlight protocol).
pub const DEFAULT_PREFETCH_UDP_PORT: u16 = 49002;

/// Default prediction cone half-angle in degrees.
pub const DEFAULT_PREFETCH_CONE_ANGLE: f32 = 45.0;

/// Default cone distance ahead in nautical miles.
pub const DEFAULT_PREFETCH_CONE_DISTANCE_NM: f32 = 100.0;

/// Default radial buffer radius in nautical miles.
pub const DEFAULT_PREFETCH_RADIAL_RADIUS_NM: f32 = 60.0;

/// Default maximum tiles per prediction cycle.
pub const DEFAULT_PREFETCH_BATCH_SIZE: usize = 500;

/// Default maximum concurrent prefetch requests.
pub const DEFAULT_PREFETCH_MAX_IN_FLIGHT: usize = 2000;

/// Default radial prefetcher tile radius (7×7 = 49 tiles).
pub const DEFAULT_PREFETCH_RADIAL_RADIUS: u8 = 3;

// =============================================================================
// Control plane defaults
// =============================================================================

/// Default stall threshold in seconds (jobs stuck longer than this are recovered).
pub const DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS: u64 = 60;

/// Default health check interval in seconds.
pub const DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS: u64 = 5;

/// Default semaphore timeout in seconds for acquiring a job slot.
pub const DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS: u64 = 30;

/// Scaling factor for max concurrent jobs relative to CPU count.
pub const DEFAULT_CONTROL_PLANE_JOB_SCALING_FACTOR: usize = 2;

/// Default max concurrent jobs: num_cpus × scaling factor
pub fn default_max_concurrent_jobs() -> usize {
    num_cpus() * DEFAULT_CONTROL_PLANE_JOB_SCALING_FACTOR
}

// =============================================================================
// Texture defaults
// =============================================================================

/// Default mipmap count (5 levels: 4096 → 2048 → 1024 → 512 → 256).
pub const DEFAULT_MIPMAP_COUNT: usize = 5;

// =============================================================================
// Download config defaults (for DownloadConfig struct)
// =============================================================================

/// Default parallel downloads per tile.
pub const DEFAULT_PARALLEL_DOWNLOADS: usize = 32;

impl ConfigFile {
    /// Load configuration from the default path (~/.xearthlayer/config.ini).
    ///
    /// If the file doesn't exist, creates it with defaults.
    pub fn load() -> Result<Self, ConfigFileError> {
        let path = config_file_path();
        Self::load_from(&path)
    }

    /// Load configuration from a specific path.
    ///
    /// If the file doesn't exist, returns defaults.
    pub fn load_from(path: &Path) -> Result<Self, ConfigFileError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let ini = Ini::load_from_file(path)?;
        Self::from_ini(&ini)
    }

    /// Save configuration to the default path (~/.xearthlayer/config.ini).
    pub fn save(&self) -> Result<(), ConfigFileError> {
        let path = config_file_path();
        self.save_to(&path)
    }

    /// Save configuration to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigFileError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigFileError::DirectoryError)?;
        }

        let content = self.to_config_string();
        std::fs::write(path, content).map_err(|e| ConfigFileError::WriteError(e.to_string()))
    }

    /// Create ConfigFile from parsed INI.
    fn from_ini(ini: &Ini) -> Result<Self, ConfigFileError> {
        let mut config = Self::default();

        // [provider] section
        if let Some(section) = ini.section(Some("provider")) {
            if let Some(v) = section.get("type") {
                let v = v.to_lowercase();
                let valid_providers =
                    ["apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs"];
                if !valid_providers.contains(&v.as_str()) {
                    return Err(ConfigFileError::InvalidValue {
                        section: "provider".to_string(),
                        key: "type".to_string(),
                        value: v,
                        reason: "must be one of: apple, arcgis, bing, go2, google, mapbox, usgs"
                            .to_string(),
                    });
                }
                config.provider.provider_type = v;
            }
            if let Some(v) = section.get("google_api_key") {
                let v = v.trim();
                if !v.is_empty() {
                    config.provider.google_api_key = Some(v.to_string());
                }
            }
            if let Some(v) = section.get("mapbox_access_token") {
                let v = v.trim();
                if !v.is_empty() {
                    config.provider.mapbox_access_token = Some(v.to_string());
                }
            }
        }

        // [cache] section
        if let Some(section) = ini.section(Some("cache")) {
            if let Some(v) = section.get("directory") {
                let v = v.trim();
                if !v.is_empty() {
                    config.cache.directory = expand_tilde(v);
                }
            }
            if let Some(v) = section.get("memory_size") {
                config.cache.memory_size =
                    parse_size(v).map_err(|_| ConfigFileError::InvalidValue {
                        section: "cache".to_string(),
                        key: "memory_size".to_string(),
                        value: v.to_string(),
                        reason: "expected format like '2GB', '500MB', or '1024KB'".to_string(),
                    })?;
            }
            if let Some(v) = section.get("disk_size") {
                config.cache.disk_size =
                    parse_size(v).map_err(|_| ConfigFileError::InvalidValue {
                        section: "cache".to_string(),
                        key: "disk_size".to_string(),
                        value: v.to_string(),
                        reason: "expected format like '20GB', '500MB', or '1024KB'".to_string(),
                    })?;
            }
            if let Some(v) = section.get("disk_io_profile") {
                config.cache.disk_io_profile =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "cache".to_string(),
                        key: "disk_io_profile".to_string(),
                        value: v.to_string(),
                        reason: "must be one of: auto, hdd, ssd, nvme".to_string(),
                    })?;
            }
        }

        // [texture] section
        if let Some(section) = ini.section(Some("texture")) {
            if let Some(v) = section.get("format") {
                let v = v.to_lowercase();
                config.texture.format = match v.as_str() {
                    "bc1" => DdsFormat::BC1,
                    "bc3" => DdsFormat::BC3,
                    _ => {
                        return Err(ConfigFileError::InvalidValue {
                            section: "texture".to_string(),
                            key: "format".to_string(),
                            value: v,
                            reason: "must be 'bc1' or 'bc3'".to_string(),
                        });
                    }
                };
            }
        }

        // [download] section
        if let Some(section) = ini.section(Some("download")) {
            if let Some(v) = section.get("timeout") {
                config.download.timeout = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "download".to_string(),
                    key: "timeout".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
            }
        }

        // [generation] section
        if let Some(section) = ini.section(Some("generation")) {
            if let Some(v) = section.get("threads") {
                config.generation.threads =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "generation".to_string(),
                        key: "threads".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("timeout") {
                config.generation.timeout =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "generation".to_string(),
                        key: "timeout".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
        }

        // [pipeline] section
        if let Some(section) = ini.section(Some("pipeline")) {
            if let Some(v) = section.get("max_http_concurrent") {
                config.pipeline.max_http_concurrent =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "max_http_concurrent".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_cpu_concurrent") {
                config.pipeline.max_cpu_concurrent =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "max_cpu_concurrent".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_prefetch_in_flight") {
                config.pipeline.max_prefetch_in_flight =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "max_prefetch_in_flight".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("request_timeout_secs") {
                config.pipeline.request_timeout_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "request_timeout_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_retries") {
                config.pipeline.max_retries =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "max_retries".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("retry_base_delay_ms") {
                config.pipeline.retry_base_delay_ms =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "retry_base_delay_ms".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (milliseconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("coalesce_channel_capacity") {
                config.pipeline.coalesce_channel_capacity =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "pipeline".to_string(),
                        key: "coalesce_channel_capacity".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
        }

        // [xplane] section
        if let Some(section) = ini.section(Some("xplane")) {
            if let Some(v) = section.get("scenery_dir") {
                let v = v.trim();
                if !v.is_empty() {
                    config.xplane.scenery_dir = Some(expand_tilde(v));
                }
            }
        }

        // [packages] section
        if let Some(section) = ini.section(Some("packages")) {
            if let Some(v) = section.get("library_url") {
                let v = v.trim();
                if !v.is_empty() {
                    config.packages.library_url = Some(v.to_string());
                }
            }
            if let Some(v) = section.get("install_location") {
                let v = v.trim();
                if !v.is_empty() {
                    config.packages.install_location = Some(expand_tilde(v));
                }
            }
            if let Some(v) = section.get("custom_scenery_path") {
                let v = v.trim();
                if !v.is_empty() {
                    config.packages.custom_scenery_path = Some(expand_tilde(v));
                }
            }
            if let Some(v) = section.get("auto_install_overlays") {
                let v = v.trim().to_lowercase();
                config.packages.auto_install_overlays = v == "true" || v == "1" || v == "yes";
            }
            if let Some(v) = section.get("temp_dir") {
                let v = v.trim();
                if !v.is_empty() {
                    config.packages.temp_dir = Some(expand_tilde(v));
                }
            }
        }

        // [logging] section
        if let Some(section) = ini.section(Some("logging")) {
            if let Some(v) = section.get("file") {
                let v = v.trim();
                if !v.is_empty() {
                    config.logging.file = expand_tilde(v);
                }
            }
        }

        // [prefetch] section
        if let Some(section) = ini.section(Some("prefetch")) {
            if let Some(v) = section.get("enabled") {
                let v = v.trim().to_lowercase();
                config.prefetch.enabled = v == "true" || v == "1" || v == "yes" || v == "on";
            }
            if let Some(v) = section.get("udp_port") {
                config.prefetch.udp_port =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "udp_port".to_string(),
                        value: v.to_string(),
                        reason: "must be a valid port number (1-65535)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("cone_angle") {
                config.prefetch.cone_angle =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "cone_angle".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (degrees)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("cone_distance_nm") {
                config.prefetch.cone_distance_nm =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "cone_distance_nm".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (nautical miles)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("radial_radius_nm") {
                config.prefetch.radial_radius_nm =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "radial_radius_nm".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (nautical miles)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("batch_size") {
                config.prefetch.batch_size =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "batch_size".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_in_flight") {
                config.prefetch.max_in_flight =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "max_in_flight".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("radial_radius") {
                config.prefetch.radial_radius =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "radial_radius".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (1-20)".to_string(),
                    })?;
            }
        }

        // [control_plane] section
        if let Some(section) = ini.section(Some("control_plane")) {
            if let Some(v) = section.get("max_concurrent_jobs") {
                config.control_plane.max_concurrent_jobs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "control_plane".to_string(),
                        key: "max_concurrent_jobs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("stall_threshold_secs") {
                config.control_plane.stall_threshold_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "control_plane".to_string(),
                        key: "stall_threshold_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("health_check_interval_secs") {
                config.control_plane.health_check_interval_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "control_plane".to_string(),
                        key: "health_check_interval_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("semaphore_timeout_secs") {
                config.control_plane.semaphore_timeout_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "control_plane".to_string(),
                        key: "semaphore_timeout_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
        }

        Ok(config)
    }

    /// Convert to INI format with proper comments.
    fn to_config_string(&self) -> String {
        let google_api_key = self.provider.google_api_key.as_deref().unwrap_or("");
        let mapbox_access_token = self.provider.mapbox_access_token.as_deref().unwrap_or("");
        let scenery_dir = self
            .xplane
            .scenery_dir
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let library_url = self.packages.library_url.as_deref().unwrap_or("");
        let install_location = self
            .packages
            .install_location
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let custom_scenery_path = self
            .packages
            .custom_scenery_path
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let auto_install_overlays = if self.packages.auto_install_overlays {
            "true"
        } else {
            "false"
        };
        let temp_dir = self
            .packages
            .temp_dir
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();

        format!(
            r#"[provider]
; Imagery provider:
;   apple  - Apple Maps (free, tokens auto-acquired via DuckDuckGo)
;   arcgis - ArcGIS World Imagery (free, global coverage)
;   bing   - Bing Maps (free, no key required)
;   go2    - Google Maps via public tile servers (free, no key required, same as Ortho4XP)
;   google - Google Maps official API (paid, requires API key)
;   mapbox - MapBox satellite (free tier available, requires access token)
;   usgs   - USGS orthoimagery (free, US coverage only)
type = {}
; Google Maps API key (only required when type = google)
; Get one at: https://console.cloud.google.com (enable Map Tiles API)
google_api_key = {}
; MapBox access token (only required when type = mapbox)
; Get one at: https://www.mapbox.com/
mapbox_access_token = {}

[cache]
; Cache directory for storing downloaded tiles (default: ~/.cache/xearthlayer)
; Uncomment and modify to use a custom location:
; directory = /path/to/custom/cache
; Memory cache size (default: 2GB) - uses RAM for fastest access
; Supports: KB, MB, GB suffixes (e.g., 500MB, 2GB, 4GB)
memory_size = {}
; Disk cache size (default: 20GB) - persistent storage for tiles
; Supports: KB, MB, GB suffixes (e.g., 10GB, 20GB, 50GB)
disk_size = {}
; Disk I/O concurrency profile based on storage type (default: auto)
;   auto - Auto-detect storage type (recommended)
;   hdd  - Spinning disk (conservative: 1-4 concurrent ops)
;   ssd  - SATA/AHCI SSD (moderate: ~32-64 concurrent ops)
;   nvme - NVMe SSD (aggressive: ~128-256 concurrent ops)
disk_io_profile = {}

[texture]
; DDS compression format: bc1 (smaller, opaque) or bc3 (larger, with alpha)
; bc1 recommended for satellite imagery
format = {}

[download]
; Timeout in seconds for HTTP requests (default: 30)
timeout = {}

[generation]
; Number of threads for parallel tile generation (default: number of CPU cores)
; WARNING: Do not set this higher than your CPU core count
threads = {}
; Timeout in seconds for generating a single tile (default: 10)
; If exceeded, returns a magenta placeholder texture
timeout = {}

[pipeline]
; Advanced concurrency and retry settings. Defaults are tuned for most systems.
; Only modify if you understand the implications.

; Maximum concurrent HTTP requests across all tiles (default: min(num_cpus * 16, 256))
; Higher values increase throughput but may overwhelm network stack
max_http_concurrent = {}
; Maximum concurrent CPU-bound operations for tile assembly and encoding
; (default: num_cpus * 1.25, minimum num_cpus + 2)
max_cpu_concurrent = {}
; Maximum concurrent prefetch jobs (default: max(num_cpus / 4, 2))
; Lower values leave more resources for on-demand tile requests
max_prefetch_in_flight = {}
; HTTP request timeout in seconds for individual chunk downloads (default: 10)
request_timeout_secs = {}
; Maximum retry attempts per failed chunk download (default: 3)
max_retries = {}
; Base delay in milliseconds for exponential backoff between retries (default: 100)
; Actual delay = base_delay * 2^attempt (e.g., 100ms, 200ms, 400ms, 800ms)
retry_base_delay_ms = {}
; Broadcast channel capacity for request coalescing (default: 16)
; Rarely needs adjustment
coalesce_channel_capacity = {}

[xplane]
; X-Plane Custom Scenery directory for mounting scenery packs
; If empty, auto-detects from ~/.x-plane/x-plane_install_12.txt
; This is also where packages are installed by 'xearthlayer packages install'
scenery_dir = {}

[packages]
; URL to the XEarthLayer package library index
; This is where available packages are discovered for 'xearthlayer packages check/install'
library_url = {}
; Local directory for installed packages (default: ~/.xearthlayer/packages)
install_location = {}
; X-Plane Custom Scenery directory for overlay symlinks
; If empty, uses xplane.scenery_dir or auto-detects
custom_scenery_path = {}
; Automatically install overlay packages when installing ortho for same region
auto_install_overlays = {}
; Temporary directory for package downloads (default: system temp dir)
; Large packages are downloaded here before extraction
temp_dir = {}

[logging]
; Log file path (default: ~/.xearthlayer/xearthlayer.log)
file = {}

[prefetch]
; Enable predictive tile prefetching based on X-Plane telemetry (default: true)
; Requires X-Plane to send ForeFlight data: Settings > Network > Send to ForeFlight
enabled = {}
; UDP port for telemetry (default: 49002 for ForeFlight protocol)
udp_port = {}
; Prediction cone half-angle in degrees (default: 45)
; Wider angles prefetch more tiles but use more bandwidth
cone_angle = {}
; Cone distance ahead in nautical miles (default: 100)
; How far ahead to prefetch tiles along flight path
cone_distance_nm = {}
; Radial buffer radius in nautical miles (default: 60)
; Catches lateral movements and unexpected direction changes
radial_radius_nm = {}
; Maximum tiles to submit per prediction cycle (default: 500)
batch_size = {}
; Maximum concurrent prefetch requests (default: 2000)
max_in_flight = {}
; Radial prefetcher tile radius (default: 3, giving 7x7 = 49 tiles)
; Higher values prefetch more tiles around aircraft position
radial_radius = {}

[control_plane]
; Advanced settings for job management and health monitoring.
; Defaults are tuned for most systems. Only modify if you understand the implications.

; Maximum concurrent DDS requests (jobs) allowed (default: num_cpus × 2)
; Higher values increase parallelism but may cause resource contention
max_concurrent_jobs = {}
; Time threshold in seconds for considering a job stalled (default: 60)
; Jobs in the same stage longer than this are automatically recovered
stall_threshold_secs = {}
; Interval in seconds for health monitoring checks (default: 5)
; More frequent checks detect issues faster but add overhead
health_check_interval_secs = {}
; Timeout in seconds for acquiring a job slot (default: 30)
; On-demand requests wait up to this long before timing out
semaphore_timeout_secs = {}
"#,
            self.provider.provider_type,
            google_api_key,
            mapbox_access_token,
            format_size(self.cache.memory_size),
            format_size(self.cache.disk_size),
            self.cache.disk_io_profile.as_str(),
            self.texture.format.to_string().to_lowercase(),
            self.download.timeout,
            self.generation.threads,
            self.generation.timeout,
            self.pipeline.max_http_concurrent,
            self.pipeline.max_cpu_concurrent,
            self.pipeline.max_prefetch_in_flight,
            self.pipeline.request_timeout_secs,
            self.pipeline.max_retries,
            self.pipeline.retry_base_delay_ms,
            self.pipeline.coalesce_channel_capacity,
            scenery_dir,
            library_url,
            install_location,
            custom_scenery_path,
            auto_install_overlays,
            temp_dir,
            path_to_string(&self.logging.file),
            self.prefetch.enabled,
            self.prefetch.udp_port,
            self.prefetch.cone_angle,
            self.prefetch.cone_distance_nm,
            self.prefetch.radial_radius_nm,
            self.prefetch.batch_size,
            self.prefetch.max_in_flight,
            self.prefetch.radial_radius,
            self.control_plane.max_concurrent_jobs,
            self.control_plane.stall_threshold_secs,
            self.control_plane.health_check_interval_secs,
            self.control_plane.semaphore_timeout_secs,
        )
    }

    /// Create the default config file if it doesn't exist.
    ///
    /// Returns the path to the config file.
    pub fn ensure_exists() -> Result<PathBuf, ConfigFileError> {
        let path = config_file_path();
        if !path.exists() {
            let config = Self::default();
            config.save_to(&path)?;
        }
        Ok(path)
    }
}

/// Get the path to the config directory (~/.xearthlayer).
pub fn config_directory() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
}

/// Get the path to the config file (~/.xearthlayer/config.ini).
pub fn config_file_path() -> PathBuf {
    config_directory().join("config.ini")
}

/// Expand ~ to home directory in paths.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Convert path to string, collapsing home dir to ~.
fn path_to_string(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = ConfigFile::default();

        assert_eq!(config.provider.provider_type, "bing");
        assert!(config.provider.google_api_key.is_none());
        assert_eq!(config.cache.memory_size, DEFAULT_MEMORY_CACHE_SIZE);
        assert_eq!(config.cache.disk_size, DEFAULT_DISK_CACHE_SIZE);
        assert_eq!(config.texture.format, DdsFormat::BC1);
        assert_eq!(config.download.timeout, DEFAULT_DOWNLOAD_TIMEOUT_SECS);
        assert!(config.xplane.scenery_dir.is_none());
    }

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        let mut config = ConfigFile::default();
        config.provider.provider_type = "google".to_string();
        config.provider.google_api_key = Some("test-api-key".to_string());
        config.cache.memory_size = 4 * 1024 * 1024 * 1024; // 4GB
        config.download.timeout = 60;

        config.save_to(&config_path).unwrap();

        let loaded = ConfigFile::load_from(&config_path).unwrap();

        assert_eq!(loaded.provider.provider_type, "google");
        assert_eq!(
            loaded.provider.google_api_key,
            Some("test-api-key".to_string())
        );
        assert_eq!(loaded.cache.memory_size, 4 * 1024 * 1024 * 1024); // 4GB from config
        assert_eq!(loaded.download.timeout, 60);
    }

    #[test]
    fn test_load_nonexistent_returns_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("nonexistent.ini");

        let config = ConfigFile::load_from(&config_path).unwrap();
        let default = ConfigFile::default();

        assert_eq!(
            config.provider.provider_type,
            default.provider.provider_type
        );
        assert_eq!(config.download.timeout, default.download.timeout);
    }

    #[test]
    fn test_invalid_provider_type() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[provider]
type = invalid
"#,
        )
        .unwrap();

        let result = ConfigFile::load_from(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Check for the updated error message that includes all providers
        assert!(err.to_string().contains("must be one of:"));
        assert!(err.to_string().contains("bing"));
    }

    #[test]
    fn test_invalid_cache_size() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[cache]
memory_size = 2TB
"#,
        )
        .unwrap();

        let result = ConfigFile::load_from(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("memory_size"));
    }

    #[test]
    fn test_human_readable_sizes() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[cache]
memory_size = 4GB
disk_size = 50GB
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.cache.memory_size, 4 * 1024 * 1024 * 1024);
        assert_eq!(config.cache.disk_size, 50 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_expand_tilde() {
        let path = expand_tilde("~/test/path");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(path, home.join("test/path"));
        }

        // Non-tilde paths should be unchanged
        let path = expand_tilde("/absolute/path");
        assert_eq!(path, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_partial_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Only specify some settings, rest should use defaults
        std::fs::write(
            &config_path,
            r#"
[provider]
type = google
google_api_key = my-key

[download]
timeout = 45
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();

        // Specified values
        assert_eq!(config.provider.provider_type, "google");
        assert_eq!(config.provider.google_api_key, Some("my-key".to_string()));
        assert_eq!(config.download.timeout, 45);

        // Default values
        assert_eq!(config.cache.memory_size, DEFAULT_MEMORY_CACHE_SIZE);
        assert_eq!(config.texture.format, DdsFormat::BC1);
    }

    #[test]
    fn test_packages_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[packages]
library_url = https://example.com/library.txt
temp_dir = /tmp/xearthlayer
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(
            config.packages.library_url,
            Some("https://example.com/library.txt".to_string())
        );
        assert_eq!(
            config.packages.temp_dir,
            Some(PathBuf::from("/tmp/xearthlayer"))
        );
    }

    #[test]
    fn test_packages_config_defaults() {
        let config = ConfigFile::default();
        assert!(config.packages.library_url.is_none());
        assert!(config.packages.temp_dir.is_none());
    }
}
