//! Default values and constants for all configuration settings.
//!
//! Contains all `DEFAULT_*` constants, CPU-aware helper functions,
//! and the `ConfigFile::default()` implementation.

use std::path::PathBuf;

use super::settings::*;
use super::DiskIoProfile;
use crate::dds::DdsFormat;

// =============================================================================
// CPU helpers
// =============================================================================

/// Get the number of available CPU cores.
pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Default HTTP concurrency limit.
/// Returns a conservative value (128) that works reliably with all providers.
pub fn default_http_concurrent() -> usize {
    DEFAULT_HTTP_CONCURRENT
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

/// Default max concurrent jobs: num_cpus × scaling factor
pub fn default_max_concurrent_jobs() -> usize {
    num_cpus() * DEFAULT_CONTROL_PLANE_JOB_SCALING_FACTOR
}

/// Default executor CPU concurrent: num_cpus * 1.25, minimum num_cpus + 2
pub fn default_executor_cpu_concurrent() -> usize {
    let cpus = num_cpus();
    ((cpus as f64 * 1.25).ceil() as usize).max(cpus + 2)
}

/// Clamps HTTP concurrency to valid range and logs a warning if clamped.
pub(super) fn clamp_http_concurrent(value: usize) -> usize {
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

// =============================================================================
// HTTP concurrency limits
// =============================================================================

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

// =============================================================================
// Pipeline defaults
// =============================================================================

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

/// Default maximum tiles to submit per prefetch cycle.
/// This controls queue depth, not processing rate — actual CPU consumption
/// is governed by the prefetch fraction in `ResourcePool`.
pub const DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE: usize = 200;

/// Default interval between prefetch cycles in milliseconds.
pub const DEFAULT_PREFETCH_CYCLE_INTERVAL_MS: u64 = 2000;

// =============================================================================
// Adaptive prefetch calibration defaults
// =============================================================================

/// Default aggressive threshold: 30 tiles/sec.
/// Systems exceeding this use position-based prefetch triggers.
pub const DEFAULT_CALIBRATION_AGGRESSIVE_THRESHOLD: f64 = 30.0;

/// Default opportunistic threshold: 10 tiles/sec.
/// Systems between this and aggressive use circuit breaker triggers.
/// Below this, prefetch is disabled.
pub const DEFAULT_CALIBRATION_OPPORTUNISTIC_THRESHOLD: f64 = 10.0;

/// Default calibration sample duration: 60 seconds.
/// How long to measure throughput during initial calibration.
pub const DEFAULT_CALIBRATION_SAMPLE_DURATION: u64 = 60;

// =============================================================================
// Transition ramp defaults
// =============================================================================

/// Default altitude climb (feet) above takeoff MSL to release transition hold.
pub const DEFAULT_TAKEOFF_CLIMB_FT: f32 = 1000.0;

/// Default maximum seconds before timeout release if climb threshold not reached.
pub const DEFAULT_TAKEOFF_TIMEOUT_SECS: u64 = 90;

/// Default sustained seconds at GS < 40kt before Cruise→Ground transition.
pub const DEFAULT_LANDING_HYSTERESIS_SECS: u64 = 15;

/// Default duration (seconds) of linear ramp from start fraction to full rate.
pub const DEFAULT_RAMP_DURATION_SECS: u64 = 30;

/// Default starting prefetch fraction when ramp begins.
pub const DEFAULT_RAMP_START_FRACTION: f64 = 0.25;

// =============================================================================
// Boundary-driven prefetch defaults
// =============================================================================

/// Default boundary trigger distance in degrees.
/// Must be < half the window height (1.5° for 3° window) to create a dead
/// zone at center where crossings don't fire. 1.0° gives 0.5° dead zone
/// on each side after re-centering.
pub const DEFAULT_PREFETCH_TRIGGER_DISTANCE: f64 = 1.0;

/// Default load depth for latitude boundary crossings (ROW loads).
/// Empirically measured: 3 rows deep × 3-4 cols wide (73% of FUSE burst events).
pub const DEFAULT_PREFETCH_LOAD_DEPTH_LAT: u8 = 3;

/// Default load depth for longitude boundary crossings (COLUMN loads).
/// Empirically measured: 2 cols deep × 3-4 rows wide (64% of FUSE burst events).
pub const DEFAULT_PREFETCH_LOAD_DEPTH_LON: u8 = 2;

/// Default buffer tiles for retention.
pub const DEFAULT_PREFETCH_WINDOW_BUFFER: u8 = 1;

/// Default InProgress staleness timeout in seconds.
pub const DEFAULT_PREFETCH_STALE_REGION_TIMEOUT: u64 = 120;

/// Default assumed window height in DSF tiles.
/// Empirically measured at EDDF, YPAD, WSSS: ~3° latitude (constant worldwide).
pub const DEFAULT_PREFETCH_DEFAULT_WINDOW_ROWS: usize = 3;

/// Default longitude extent in degrees for scenery window computation.
/// X-Plane loads ~330km × 330km; cols = ceil(this / cos(lat)).
/// Actual column count varies by latitude via `lon_tiles_for_latitude()`.
pub const DEFAULT_PREFETCH_WINDOW_LON_EXTENT: f64 = 3.0;

// =============================================================================
// FUSE defaults
// =============================================================================

/// Default maximum pending background FUSE requests.
///
/// The Linux kernel queues background requests (readahead, async reads) and
/// throttles when `congestion_threshold` is exceeded. The kernel default of
/// 12/9 severely limits X-Plane's concurrent scenery reads, causing freezes
/// at DSF boundaries. 256 allows the full tile pipeline to stay saturated.
pub const DEFAULT_FUSE_MAX_BACKGROUND: u16 = 256;

/// Default congestion threshold for background FUSE requests.
///
/// When pending background requests exceed this, the kernel starts throttling.
/// Set to 75% of `max_background` as per kernel convention.
pub const DEFAULT_FUSE_CONGESTION_THRESHOLD: u16 = DEFAULT_FUSE_MAX_BACKGROUND * 3 / 4; // 192

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

// =============================================================================
// Texture defaults
// =============================================================================

/// Default mipmap count (5 levels: 4096 → 2048 → 1024 → 512 → 256).
pub const DEFAULT_MIPMAP_COUNT: usize = 5;

/// Default texture compressor backend: ISPC SIMD.
pub const DEFAULT_COMPRESSOR: &str = "ispc";

/// Default GPU device selector: prefer integrated GPU.
pub const DEFAULT_GPU_DEVICE: &str = "integrated";

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

// =============================================================================
// ConfigFile::default()
// =============================================================================

impl Default for ConfigFile {
    fn default() -> Self {
        let config_dir = super::file::config_directory();
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
                compressor: DEFAULT_COMPRESSOR.to_string(),
                gpu_device: DEFAULT_GPU_DEVICE.to_string(),
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
                strategy: "adaptive".to_string(),
                mode: "auto".to_string(),
                udp_port: DEFAULT_PREFETCH_UDP_PORT,
                max_tiles_per_cycle: DEFAULT_PREFETCH_MAX_TILES_PER_CYCLE,
                cycle_interval_ms: DEFAULT_PREFETCH_CYCLE_INTERVAL_MS,
                calibration_aggressive_threshold: DEFAULT_CALIBRATION_AGGRESSIVE_THRESHOLD,
                calibration_opportunistic_threshold: DEFAULT_CALIBRATION_OPPORTUNISTIC_THRESHOLD,
                calibration_sample_duration: DEFAULT_CALIBRATION_SAMPLE_DURATION,
                takeoff_climb_ft: DEFAULT_TAKEOFF_CLIMB_FT,
                takeoff_timeout_secs: DEFAULT_TAKEOFF_TIMEOUT_SECS,
                landing_hysteresis_secs: DEFAULT_LANDING_HYSTERESIS_SECS,
                ramp_duration_secs: DEFAULT_RAMP_DURATION_SECS,
                ramp_start_fraction: DEFAULT_RAMP_START_FRACTION,
                trigger_distance: DEFAULT_PREFETCH_TRIGGER_DISTANCE,
                load_depth_lat: DEFAULT_PREFETCH_LOAD_DEPTH_LAT,
                load_depth_lon: DEFAULT_PREFETCH_LOAD_DEPTH_LON,
                window_buffer: DEFAULT_PREFETCH_WINDOW_BUFFER,
                stale_region_timeout: DEFAULT_PREFETCH_STALE_REGION_TIMEOUT,
                default_window_rows: DEFAULT_PREFETCH_DEFAULT_WINDOW_ROWS,
                window_lon_extent: DEFAULT_PREFETCH_WINDOW_LON_EXTENT,
            },
            control_plane: ControlPlaneSettings {
                max_concurrent_jobs: default_max_concurrent_jobs(),
                stall_threshold_secs: DEFAULT_CONTROL_PLANE_STALL_THRESHOLD_SECS,
                health_check_interval_secs: DEFAULT_CONTROL_PLANE_HEALTH_CHECK_INTERVAL_SECS,
                semaphore_timeout_secs: DEFAULT_CONTROL_PLANE_SEMAPHORE_TIMEOUT_SECS,
            },
            prewarm: PrewarmSettings {
                grid_rows: 3,
                grid_cols: 4,
            },
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
            fuse: FuseSettings {
                max_background: DEFAULT_FUSE_MAX_BACKGROUND,
                congestion_threshold: DEFAULT_FUSE_CONGESTION_THRESHOLD,
            },
        }
    }
}
