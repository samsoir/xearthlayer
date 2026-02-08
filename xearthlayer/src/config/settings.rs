//! Settings structs for all configuration sections.
//!
//! Each struct represents one `[section]` of the INI config file.
//! These are pure data types with no parsing or serialization logic.

use super::DiskIoProfile;
use crate::dds::DdsFormat;
use std::path::PathBuf;

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
    /// Online network settings for VATSIM/IVAO/PilotEdge position
    pub online_network: OnlineNetworkSettings,
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
    /// Prefetch strategy: "auto" or "adaptive" (both use adaptive prefetch)
    ///
    /// The adaptive prefetch system automatically selects the best strategy
    /// based on flight phase (ground/cruise) and system throughput.
    pub strategy: String,
    /// Adaptive prefetch mode: "auto", "aggressive", "opportunistic", or "disabled"
    ///
    /// - "auto": Select mode based on calibration results (recommended)
    /// - "aggressive": Position-based triggers (fast connections)
    /// - "opportunistic": Circuit breaker triggers (moderate connections)
    /// - "disabled": Disable prefetch
    pub mode: String,
    /// UDP port for X-Plane telemetry (default: 49002 for ForeFlight protocol)
    pub udp_port: u16,
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

    // Adaptive prefetch calibration settings
    /// Minimum throughput for aggressive mode (tiles/sec).
    /// If measured throughput exceeds this, aggressive (position-based) prefetch is enabled.
    /// Default: 30.0
    pub calibration_aggressive_threshold: f64,
    /// Minimum throughput for opportunistic mode (tiles/sec).
    /// If measured throughput is between this and aggressive_threshold,
    /// opportunistic (circuit breaker) prefetch is enabled.
    /// Below this threshold, prefetch is disabled.
    /// Default: 10.0
    pub calibration_opportunistic_threshold: f64,
    /// How long to measure throughput during initial calibration (seconds).
    /// Default: 60
    pub calibration_sample_duration: u64,
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

/// Online network settings for position from ATC networks (VATSIM, IVAO, PilotEdge).
#[derive(Debug, Clone)]
pub struct OnlineNetworkSettings {
    /// Enable/disable online network position fetching.
    /// Default: false
    pub enabled: bool,
    /// Network type: "vatsim", "ivao", or "pilotedge".
    /// Default: "vatsim"
    pub network_type: String,
    /// Pilot identifier (CID for VATSIM).
    /// Default: 0 (disabled)
    pub pilot_id: u64,
    /// API URL (for VATSIM, the status endpoint).
    /// Default: "https://status.vatsim.net/status.json"
    pub api_url: String,
    /// Poll interval in seconds.
    /// Default: 15
    pub poll_interval_secs: u64,
    /// Maximum age in seconds before data is considered stale.
    /// Default: 60
    pub max_stale_secs: u64,
}

impl Default for OnlineNetworkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            network_type: "vatsim".to_string(),
            pilot_id: 0,
            api_url: crate::aircraft_position::network::DEFAULT_VATSIM_API_URL.to_string(),
            poll_interval_secs: crate::aircraft_position::network::DEFAULT_POLL_INTERVAL_SECS,
            max_stale_secs: crate::aircraft_position::network::DEFAULT_MAX_STALE_SECS,
        }
    }
}
