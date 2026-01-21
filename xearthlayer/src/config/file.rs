//! Configuration file handling for ~/.xearthlayer/config.ini.
//!
//! Loads and saves user configuration with sensible defaults.

use super::DiskIoProfile;
use crate::config::size::{format_size, parse_size};
use crate::dds::DdsFormat;
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
    /// Prewarm settings for cold-start cache warming
    pub prewarm: PrewarmSettings,
    /// Patches settings for custom Ortho4XP tile patches
    pub patches: PatchesSettings,
    /// Executor daemon settings for job/task framework
    pub executor: ExecutorSettings,
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
    /// Default: 128 (conservative value stable with all providers)
    ///
    /// Hard limits: 64-256 (values outside this range are clamped).
    /// The ceiling prevents overwhelming imagery providers, which causes
    /// rate limiting (HTTP 429) and cascade failures.
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
    /// Prefetch strategy: "auto", "heading-aware", or "radial" (default: "auto")
    ///
    /// - "auto": Uses heading-aware with graceful degradation to radial
    /// - "heading-aware": Direction-aware cone prefetching (requires telemetry)
    /// - "radial": Simple radius-based prefetching (no heading data required)
    pub strategy: String,
    /// UDP port for X-Plane telemetry (default: 49002 for ForeFlight protocol)
    pub udp_port: u16,
    /// Prediction cone half-angle in degrees (default: 80)
    pub cone_angle: f32,
    /// Inner radius where prefetch zone starts (nautical miles).
    /// This is just inside X-Plane's ~90nm loaded zone. Default: 85nm.
    pub inner_radius_nm: f32,
    /// Outer radius where prefetch zone ends (nautical miles). Default: 120nm.
    pub outer_radius_nm: f32,
    /// Maximum tiles to submit per prefetch cycle. Default: 200.
    /// Lower values reduce bandwidth competition with on-demand requests.
    pub max_tiles_per_cycle: usize,
    /// Interval between prefetch cycles in milliseconds. Default: 2000ms.
    /// Higher values reduce prefetch aggressiveness.
    pub cycle_interval_ms: u64,
    /// Circuit breaker threshold: FUSE jobs per second to trip the breaker.
    /// When X-Plane is loading scenery, FUSE request rate spikes. Default: 10.0.
    pub circuit_breaker_threshold: f64,
    /// How long (milliseconds) high FUSE rate must be sustained to open circuit.
    /// Default: 500ms (0.5 seconds) to catch bursty loads.
    pub circuit_breaker_open_ms: u64,
    /// Cooloff time (seconds) before trying to close the circuit.
    /// Default: 5 seconds.
    pub circuit_breaker_half_open_secs: u64,
    /// Radial prefetcher tile radius (number of tiles in each direction).
    /// Default: 120 tiles. Maximum: 255.
    pub radial_radius: u8,
    /// Tile-based prefetcher: number of DSF tile rows to prefetch ahead.
    /// Higher values prefetch more aggressively but use more bandwidth.
    /// Default: 1 (prefetch the immediate next row).
    pub tile_based_rows_ahead: u32,
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

/// Prewarm configuration for cold-start cache warming.
#[derive(Debug, Clone)]
pub struct PrewarmSettings {
    /// Grid size in DSF tiles (N = N×N grid) around an airport to prewarm.
    /// Default: 8 (8×8 grid = 64 DSF tiles, ~480nm × 480nm at mid-latitudes)
    pub grid_size: u32,
}

/// Patches configuration for custom Ortho4XP tile patches.
#[derive(Debug, Clone)]
pub struct PatchesSettings {
    /// Enable/disable patches functionality.
    /// When enabled, XEL will mount patch tiles from the patches directory.
    /// Default: true
    pub enabled: bool,
    /// Directory containing patch tiles.
    /// Each subdirectory should be a complete Ortho4XP tile with Earth nav data/, terrain/, etc.
    /// Default: ~/.xearthlayer/patches
    pub directory: Option<PathBuf>,
}

/// Executor daemon configuration for the job/task framework.
#[derive(Debug, Clone)]
pub struct ExecutorSettings {
    /// Network resource pool capacity (concurrent HTTP connections).
    /// Default: 128 (clamped to 64-256 range)
    pub network_concurrent: usize,
    /// CPU resource pool capacity (concurrent assemble/encode operations).
    /// Default: num_cpus * 1.25, minimum num_cpus + 2
    pub cpu_concurrent: usize,
    /// Disk I/O resource pool capacity (concurrent disk operations).
    /// Default: 64 for SSD, auto-detected from storage type
    pub disk_io_concurrent: usize,
    /// Maximum concurrent tasks the executor can run.
    /// Default: 128
    pub max_concurrent_tasks: usize,
    /// Internal job channel capacity (job queue size).
    /// Default: 256
    pub job_channel_capacity: usize,
    /// External request channel capacity (request queue from FUSE/prefetch).
    /// Default: 1000
    pub request_channel_capacity: usize,
    /// HTTP request timeout in seconds for individual chunk downloads.
    /// Default: 10 seconds
    pub request_timeout_secs: u64,
    /// Maximum retry attempts per failed chunk download.
    /// Default: 3
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff between retries.
    /// Default: 100ms
    pub retry_base_delay_ms: u64,
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
                library_url: Some(DEFAULT_LIBRARY_URL.to_string()),
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
                strategy: "auto".to_string(),
                udp_port: DEFAULT_PREFETCH_UDP_PORT,
                cone_angle: DEFAULT_PREFETCH_CONE_ANGLE,
                inner_radius_nm: DEFAULT_PREFETCH_INNER_RADIUS_NM,
                outer_radius_nm: DEFAULT_PREFETCH_OUTER_RADIUS_NM,
                max_tiles_per_cycle: DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE,
                cycle_interval_ms: DEFAULT_PREFETCH_CYCLE_INTERVAL_MS,
                circuit_breaker_threshold: DEFAULT_CIRCUIT_BREAKER_THRESHOLD,
                circuit_breaker_open_ms: DEFAULT_CIRCUIT_BREAKER_OPEN_MS,
                circuit_breaker_half_open_secs: DEFAULT_CIRCUIT_BREAKER_HALF_OPEN_SECS,
                radial_radius: DEFAULT_PREFETCH_RADIAL_RADIUS,
                tile_based_rows_ahead: DEFAULT_TILE_BASED_ROWS_AHEAD,
            },
            control_plane: ControlPlaneSettings {
                max_concurrent_jobs: default_max_concurrent_jobs(),
                stall_threshold_secs: DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
                health_check_interval_secs: DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
                semaphore_timeout_secs: DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            },
            prewarm: PrewarmSettings { grid_size: 8 },
            patches: PatchesSettings {
                enabled: true,
                directory: Some(config_dir.join("patches")),
            },
            executor: ExecutorSettings {
                network_concurrent: DEFAULT_EXECUTOR_NETWORK_CONCURRENT,
                cpu_concurrent: default_executor_cpu_concurrent(),
                disk_io_concurrent: DEFAULT_EXECUTOR_DISK_IO_CONCURRENT,
                max_concurrent_tasks: DEFAULT_EXECUTOR_MAX_CONCURRENT_TASKS,
                job_channel_capacity: DEFAULT_EXECUTOR_JOB_CHANNEL_CAPACITY,
                request_channel_capacity: DEFAULT_EXECUTOR_REQUEST_CHANNEL_CAPACITY,
                request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
                max_retries: DEFAULT_MAX_RETRIES,
                retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
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

/// Minimum HTTP concurrency limit.
/// Below this, performance suffers significantly.
pub const MIN_HTTP_CONCURRENT: usize = 64;

/// Maximum HTTP concurrency limit.
/// Above this, providers get rate-limited causing cascade failures.
pub const MAX_HTTP_CONCURRENT: usize = 256;

/// Default HTTP concurrency limit.
/// Conservative default of 128 prevents provider rate limiting while
/// maintaining good performance. Tested stable with Apple/Bing providers.
pub const DEFAULT_HTTP_CONCURRENT: usize = 128;

/// Default HTTP concurrency limit.
/// Returns a conservative value (128) that works reliably with all providers.
pub fn default_http_concurrent() -> usize {
    DEFAULT_HTTP_CONCURRENT
}

/// Clamps HTTP concurrency to valid range and logs a warning if clamped.
fn clamp_http_concurrent(value: usize) -> usize {
    if value < MIN_HTTP_CONCURRENT {
        tracing::warn!(
            requested = value,
            min = MIN_HTTP_CONCURRENT,
            max = MAX_HTTP_CONCURRENT,
            "max_http_concurrent below minimum, clamping to {}",
            MIN_HTTP_CONCURRENT
        );
        MIN_HTTP_CONCURRENT
    } else if value > MAX_HTTP_CONCURRENT {
        tracing::warn!(
            requested = value,
            min = MIN_HTTP_CONCURRENT,
            max = MAX_HTTP_CONCURRENT,
            "max_http_concurrent above maximum, clamping to {} (prevents provider rate limiting)",
            MAX_HTTP_CONCURRENT
        );
        MAX_HTTP_CONCURRENT
    } else {
        value
    }
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
/// Wider angle (80°) covers more area ahead of aircraft.
pub const DEFAULT_PREFETCH_CONE_ANGLE: f32 = 80.0;

/// Default inner radius where prefetch zone starts (nautical miles).
/// Just inside X-Plane's ~90nm loaded zone boundary.
pub const DEFAULT_PREFETCH_INNER_RADIUS_NM: f32 = 85.0;

/// Default outer radius where prefetch zone ends (nautical miles).
/// Extended to 180nm for better look-ahead coverage.
pub const DEFAULT_PREFETCH_OUTER_RADIUS_NM: f32 = 180.0;

/// Default maximum tiles to submit per prefetch cycle.
/// Increased to 200 for faster cache warming.
pub const DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE: usize = 200;

/// Default interval between prefetch cycles in milliseconds.
pub const DEFAULT_PREFETCH_CYCLE_INTERVAL_MS: u64 = 2000;

/// Default circuit breaker threshold: FUSE jobs per second to trip.
/// Normal flight activity is typically 5-30 jobs/sec, scene loading is 90-500+.
/// Set at 50 to cleanly separate flight activity from scene loading.
pub const DEFAULT_CIRCUIT_BREAKER_THRESHOLD: f64 = 50.0;

/// Default duration (milliseconds) high FUSE rate must be sustained to open circuit.
/// Set low (500ms) to catch bursty scene loading patterns quickly.
pub const DEFAULT_CIRCUIT_BREAKER_OPEN_MS: u64 = 500;

/// Default cooloff time (seconds) before trying to close the circuit.
/// Set to 2 seconds for faster recovery after load drops.
pub const DEFAULT_CIRCUIT_BREAKER_HALF_OPEN_SECS: u64 = 2;

/// Default radial prefetcher tile radius.
/// 120 tiles provides wide coverage around aircraft position.
pub const DEFAULT_PREFETCH_RADIAL_RADIUS: u8 = 120;

/// Default tile-based prefetcher rows ahead.
/// 1 row means prefetch the immediate next row of DSF tiles.
pub const DEFAULT_TILE_BASED_ROWS_AHEAD: u32 = 1;

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
// Executor defaults
// =============================================================================

/// Default network resource pool capacity.
/// Conservative default of 128 prevents provider rate limiting.
pub const DEFAULT_EXECUTOR_NETWORK_CONCURRENT: usize = 128;

/// Default disk I/O resource pool capacity for SSD storage.
pub const DEFAULT_EXECUTOR_DISK_IO_CONCURRENT: usize = 64;

/// Default maximum concurrent tasks in the executor.
pub const DEFAULT_EXECUTOR_MAX_CONCURRENT_TASKS: usize = 128;

/// Default job channel capacity (internal job queue).
pub const DEFAULT_EXECUTOR_JOB_CHANNEL_CAPACITY: usize = 256;

/// Default request channel capacity (external request queue).
pub const DEFAULT_EXECUTOR_REQUEST_CHANNEL_CAPACITY: usize = 1000;

/// Default executor CPU concurrent: num_cpus * 1.25, minimum num_cpus + 2
pub fn default_executor_cpu_concurrent() -> usize {
    let cpus = num_cpus();
    ((cpus as f64 * 1.25).ceil() as usize).max(cpus + 2)
}

// =============================================================================
// Package manager defaults
// =============================================================================

/// Default package library URL (XEarthLayer official package library).
pub const DEFAULT_LIBRARY_URL: &str =
    "https://xearthlayer.app/packages/xearthlayer_package_library.txt";

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
                let parsed: usize = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "max_http_concurrent".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
                // Enforce hard limits to prevent provider rate limiting
                config.pipeline.max_http_concurrent = clamp_http_concurrent(parsed);
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
                config.packages.auto_install_overlays = parse_bool(v);
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
                config.prefetch.enabled = parse_bool(v);
            }
            if let Some(v) = section.get("strategy") {
                let v = v.trim().to_lowercase();
                match v.as_str() {
                    "auto" | "heading-aware" | "radial" | "tile-based" => {
                        config.prefetch.strategy = v;
                    }
                    _ => {
                        return Err(ConfigFileError::InvalidValue {
                            section: "prefetch".to_string(),
                            key: "strategy".to_string(),
                            value: v.to_string(),
                            reason: "must be 'auto', 'heading-aware', 'radial', or 'tile-based'"
                                .to_string(),
                        });
                    }
                }
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
            // Deprecated: cone_distance_nm, radial_radius_nm, batch_size, max_in_flight, radial_radius
            // These are ignored if present in config file (removed in v0.2.9/v0.2.11)
            if let Some(v) = section.get("inner_radius_nm") {
                config.prefetch.inner_radius_nm =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "inner_radius_nm".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (nautical miles)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("outer_radius_nm") {
                config.prefetch.outer_radius_nm =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "outer_radius_nm".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (nautical miles)".to_string(),
                    })?;
            }
            // Deprecated: radial_outer_radius_nm, cone_outer_radius_nm, cone_half_angle
            // These are ignored if present in config file (removed in v0.2.9)
            if let Some(v) = section.get("max_tiles_per_cycle") {
                config.prefetch.max_tiles_per_cycle =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "max_tiles_per_cycle".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("cycle_interval_ms") {
                config.prefetch.cycle_interval_ms =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "cycle_interval_ms".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (milliseconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("circuit_breaker_threshold") {
                config.prefetch.circuit_breaker_threshold =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "circuit_breaker_threshold".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive number (FUSE jobs/second)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("circuit_breaker_open_ms") {
                config.prefetch.circuit_breaker_open_ms =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "circuit_breaker_open_ms".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (milliseconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("circuit_breaker_half_open_secs") {
                config.prefetch.circuit_breaker_half_open_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "circuit_breaker_half_open_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("radial_radius") {
                config.prefetch.radial_radius =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "radial_radius".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (tiles)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("tile_based_rows_ahead") {
                config.prefetch.tile_based_rows_ahead =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "tile_based_rows_ahead".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (rows)".to_string(),
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

        // [prewarm] section
        if let Some(section) = ini.section(Some("prewarm")) {
            if let Some(v) = section.get("grid_size") {
                config.prewarm.grid_size =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "prewarm".to_string(),
                        key: "grid_size".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (DSF tiles per side)".to_string(),
                    })?;
            }
        }

        // [patches] section
        if let Some(section) = ini.section(Some("patches")) {
            if let Some(v) = section.get("enabled") {
                config.patches.enabled = parse_bool(v);
            }
            if let Some(v) = section.get("directory") {
                let v = v.trim();
                if !v.is_empty() {
                    config.patches.directory = Some(expand_tilde(v));
                } else {
                    config.patches.directory = None;
                }
            }
        }

        // [executor] section
        if let Some(section) = ini.section(Some("executor")) {
            if let Some(v) = section.get("network_concurrent") {
                let parsed: usize = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "network_concurrent".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
                // Clamp to valid range (same as HTTP concurrent)
                config.executor.network_concurrent = clamp_http_concurrent(parsed);
            }
            if let Some(v) = section.get("cpu_concurrent") {
                config.executor.cpu_concurrent =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "cpu_concurrent".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("disk_io_concurrent") {
                config.executor.disk_io_concurrent =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "disk_io_concurrent".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_concurrent_tasks") {
                config.executor.max_concurrent_tasks =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "max_concurrent_tasks".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("job_channel_capacity") {
                config.executor.job_channel_capacity =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "job_channel_capacity".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("request_channel_capacity") {
                config.executor.request_channel_capacity =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "request_channel_capacity".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("request_timeout_secs") {
                config.executor.request_timeout_secs =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "request_timeout_secs".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (seconds)".to_string(),
                    })?;
            }
            if let Some(v) = section.get("max_retries") {
                config.executor.max_retries =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "max_retries".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("retry_base_delay_ms") {
                config.executor.retry_base_delay_ms =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "executor".to_string(),
                        key: "retry_base_delay_ms".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer (milliseconds)".to_string(),
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
; Base directory for disk cache storage. Chunks are stored in <directory>/chunks/
; If empty, defaults to ~/.cache/xearthlayer (Linux) or platform cache directory
; Example: directory = /mnt/fast-ssd/xearthlayer
directory = {}
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

[executor]
; Job executor daemon settings for tile generation.
; These control resource pools, concurrency, and retry behavior.

; Resource pool capacities (concurrent operations by type)
; Network: HTTP connections for chunk downloads (default: 128, clamped to 64-256)
network_concurrent = {}
; CPU: Assemble + encode operations (default: num_cpus * 1.25)
cpu_concurrent = {}
; Disk I/O: Cache read/write operations (default: 64 for SSD)
disk_io_concurrent = {}

; Job processing limits
; Maximum concurrent tasks the executor can run (default: 128)
max_concurrent_tasks = {}
; Internal job queue capacity (default: 256)
job_channel_capacity = {}
; External request queue from FUSE/prefetch (default: 1000)
request_channel_capacity = {}

; Download behavior
; HTTP request timeout in seconds for individual chunk downloads (default: 10)
request_timeout_secs = {}
; Maximum retry attempts per failed chunk download (default: 3)
max_retries = {}
; Base delay in milliseconds for exponential backoff between retries (default: 100)
; Actual delay = base_delay * 2^attempt (e.g., 100ms, 200ms, 400ms, 800ms)
retry_base_delay_ms = {}

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
; Prefetch strategy (default: auto)
;   auto         - Uses heading-aware with graceful degradation to radial
;   heading-aware - Direction-aware cone prefetching (requires telemetry)
;   radial       - Simple radius-based prefetching (no heading data required)
strategy = {}
; UDP port for telemetry (default: 49002 for ForeFlight protocol)
udp_port = {}

; Zone boundaries (nautical miles)
; Inner radius where prefetch zone starts (default: 85)
; Just inside X-Plane's ~90nm loaded zone boundary
inner_radius_nm = {}
; Outer radius where prefetch zone ends (default: 180)
outer_radius_nm = {}

; Radial prefetcher tile radius (default: 120)
; Higher values prefetch more tiles around aircraft position
radial_radius = {}

; Tile-based prefetcher rows ahead (default: 1)
; Number of DSF tile rows (1° each) to prefetch ahead of aircraft
tile_based_rows_ahead = {}

; Heading-aware cone (prediction cone half-angle in degrees, default: 80)
; Wider angles prefetch more tiles but use more bandwidth
cone_angle = {}

; Cycle limits
; Maximum tiles to submit per prefetch cycle (default: 200)
; Lower values leave more bandwidth for on-demand requests
max_tiles_per_cycle = {}
; Interval between prefetch cycles in milliseconds (default: 2000)
; Higher values reduce prefetch aggressiveness
cycle_interval_ms = {}

; Circuit breaker (pause prefetch during X-Plane scenery loading)
; Only counts FUSE-originated requests, not prefetch jobs
; FUSE jobs per second threshold to trip the breaker (default: 5.0)
circuit_breaker_threshold = {}
; Duration (milliseconds) high FUSE rate must be sustained to open circuit (default: 500)
circuit_breaker_open_ms = {}
; Cooloff time (seconds) before trying to close the circuit (default: 5)
circuit_breaker_half_open_secs = {}

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

[prewarm]
; Settings for cold-start cache pre-warming.
; Use with --airport ICAO to pre-load tiles around an airport before flight.

; Grid size in DSF tiles (N = N×N grid) around the airport to prewarm.
; 8 = 8×8 grid = 64 DSF tiles, approximately 480nm × 480nm at mid-latitudes.
; Each DSF tile is 1° × 1° (roughly 60nm × 60nm at equator).
grid_size = {}

[patches]
; Settings for custom Ortho4XP tile patches (airport addon mesh/elevation support).
; Patches provide custom elevation/mesh data while XEL generates textures dynamically.

; Enable/disable patches functionality (default: true)
; When enabled, XEL will mount patch tiles from the patches directory.
enabled = {}
; Directory containing patch tiles (default: ~/.xearthlayer/patches)
; Each subdirectory should be a complete Ortho4XP tile with:
;   - Earth nav data/*.dsf (custom mesh/elevation)
;   - terrain/*.ter (terrain definition files)
;   - textures/ (optional - XEL generates these on-demand)
; Priority is determined by alphabetical folder naming (A < B < Z)
directory = {}
"#,
            self.provider.provider_type,
            google_api_key,
            mapbox_access_token,
            path_to_string(&self.cache.directory),
            format_size(self.cache.memory_size),
            format_size(self.cache.disk_size),
            self.cache.disk_io_profile.as_str(),
            self.texture.format.to_string().to_lowercase(),
            self.download.timeout,
            self.generation.threads,
            self.generation.timeout,
            // Executor settings
            self.executor.network_concurrent,
            self.executor.cpu_concurrent,
            self.executor.disk_io_concurrent,
            self.executor.max_concurrent_tasks,
            self.executor.job_channel_capacity,
            self.executor.request_channel_capacity,
            self.executor.request_timeout_secs,
            self.executor.max_retries,
            self.executor.retry_base_delay_ms,
            scenery_dir,
            library_url,
            install_location,
            custom_scenery_path,
            auto_install_overlays,
            temp_dir,
            path_to_string(&self.logging.file),
            self.prefetch.enabled,
            self.prefetch.strategy,
            self.prefetch.udp_port,
            self.prefetch.inner_radius_nm,
            self.prefetch.outer_radius_nm,
            self.prefetch.radial_radius,
            self.prefetch.tile_based_rows_ahead,
            self.prefetch.cone_angle,
            self.prefetch.max_tiles_per_cycle,
            self.prefetch.cycle_interval_ms,
            self.prefetch.circuit_breaker_threshold,
            self.prefetch.circuit_breaker_open_ms,
            self.prefetch.circuit_breaker_half_open_secs,
            self.control_plane.max_concurrent_jobs,
            self.control_plane.stall_threshold_secs,
            self.control_plane.health_check_interval_secs,
            self.control_plane.semaphore_timeout_secs,
            self.prewarm.grid_size,
            self.patches.enabled,
            self.patches
                .directory
                .as_ref()
                .map(|p| path_to_string(p))
                .unwrap_or_default(),
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

/// Parse a boolean value from a config string.
/// Accepts: true/false, yes/no, 1/0, on/off (case-insensitive)
fn parse_bool(value: &str) -> bool {
    let v = value.trim().to_lowercase();
    v == "true" || v == "1" || v == "yes" || v == "on"
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
        // Default library URL is the official XEarthLayer package library
        assert_eq!(
            config.packages.library_url,
            Some(DEFAULT_LIBRARY_URL.to_string())
        );
        assert!(config.packages.temp_dir.is_none());
    }

    #[test]
    fn test_http_concurrent_clamped_to_ceiling() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value above maximum gets clamped to 256
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 500
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, MAX_HTTP_CONCURRENT);
    }

    #[test]
    fn test_http_concurrent_clamped_to_floor() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value below minimum gets clamped to 64
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 10
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, MIN_HTTP_CONCURRENT);
    }

    #[test]
    fn test_http_concurrent_in_range_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value within range is unchanged
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 128
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, 128);
    }

    #[test]
    fn test_parse_bool_true_values() {
        // Test all accepted "true" values
        assert!(parse_bool("true"));
        assert!(parse_bool("TRUE"));
        assert!(parse_bool("True"));
        assert!(parse_bool("yes"));
        assert!(parse_bool("YES"));
        assert!(parse_bool("Yes"));
        assert!(parse_bool("1"));
        assert!(parse_bool("on"));
        assert!(parse_bool("ON"));
        assert!(parse_bool("On"));
    }

    #[test]
    fn test_parse_bool_false_values() {
        // Test values that should be false
        assert!(!parse_bool("false"));
        assert!(!parse_bool("FALSE"));
        assert!(!parse_bool("no"));
        assert!(!parse_bool("NO"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("off"));
        assert!(!parse_bool("OFF"));
        // Invalid values also return false
        assert!(!parse_bool("invalid"));
        assert!(!parse_bool(""));
        assert!(!parse_bool("maybe"));
    }

    #[test]
    fn test_parse_bool_with_whitespace() {
        // Test that whitespace is trimmed
        assert!(parse_bool("  true  "));
        assert!(parse_bool("\ttrue\n"));
        assert!(parse_bool("  yes "));
        assert!(!parse_bool("  false  "));
        assert!(!parse_bool("  no  "));
    }

    #[test]
    fn test_patches_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[patches]
enabled = true
directory = /custom/patches/dir
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert!(config.patches.enabled);
        assert_eq!(
            config.patches.directory,
            Some(PathBuf::from("/custom/patches/dir"))
        );
    }

    #[test]
    fn test_patches_config_defaults() {
        let config = ConfigFile::default();
        assert!(config.patches.enabled);
        // Default directory is ~/.xearthlayer/patches
        assert!(config.patches.directory.is_some());
        let dir = config.patches.directory.unwrap();
        assert!(dir.ends_with("patches"));
    }

    #[test]
    fn test_patches_config_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[patches]
enabled = false
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert!(!config.patches.enabled);
    }
}
