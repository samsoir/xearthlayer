//! INI serialization logic for converting `ConfigFile` → INI string.
//!
//! This module contains the `to_config_string()` function that produces
//! the commented INI representation written to `config.ini`.

use std::path::Path;

use super::settings::ConfigFile;
use super::size::format_size;

/// Convert a `ConfigFile` to a commented INI string for saving.
pub(super) fn to_config_string(config: &ConfigFile) -> String {
    let google_api_key = config.provider.google_api_key.as_deref().unwrap_or("");
    let mapbox_access_token = config.provider.mapbox_access_token.as_deref().unwrap_or("");
    let scenery_dir = config
        .xplane
        .scenery_dir
        .as_ref()
        .map(|p| path_to_string(p))
        .unwrap_or_default();
    let library_url = config.packages.library_url.as_deref().unwrap_or("");
    let install_location = config
        .packages
        .install_location
        .as_ref()
        .map(|p| path_to_string(p))
        .unwrap_or_default();
    let custom_scenery_path = config
        .packages
        .custom_scenery_path
        .as_ref()
        .map(|p| path_to_string(p))
        .unwrap_or_default();
    let auto_install_overlays = if config.packages.auto_install_overlays {
        "true"
    } else {
        "false"
    };
    let temp_dir = config
        .packages
        .temp_dir
        .as_ref()
        .map(|p| path_to_string(p))
        .unwrap_or_default();

    let update_check = if config.general.update_check {
        "true"
    } else {
        "false"
    };

    format!(
        r#"[general]
; Check for new versions on startup (default: true)
; When enabled, performs a single HTTP request once per day (no telemetry)
update_check = {}

[provider]
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
; Fraction of disk cache allocated to DDS tiles (default: 0.6)
; Remainder goes to raw imagery chunk cache. Range: 0.0 - 1.0
dds_disk_ratio = {}
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
; Texture compressor backend: software, ispc (SIMD), or gpu (wgpu)
; ispc is recommended for best CPU performance
compressor = {}
; GPU device selector for gpu compressor: integrated, discrete, or adapter name substring
gpu_device = {}

[generation]
; Number of threads for parallel tile generation (default: num_cpus / 2)
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
; CPU: Assemble + encode operations (default: num_cpus / 2)
cpu_concurrent = {}
; Disk I/O: Cache read/write operations (default: 64 for SSD)
disk_io_concurrent = {}
; Maximum concurrent DDS tile jobs (default: num_cpus / 2)
max_concurrent_jobs = {}

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
; Temporary directory for package downloads (default: ~/.xearthlayer/tmp)
; Large packages are downloaded here before extraction
temp_dir = {}
; Number of concurrent part downloads (1-10, default: 5)
concurrent_downloads = {}

[logging]
; Log file path (default: ~/.xearthlayer/xearthlayer.log)
file = {}

[prefetch]
; Enable predictive tile prefetching based on X-Plane telemetry (default: true)
; Requires X-Plane to send ForeFlight data: Settings > Network > Send to ForeFlight
enabled = {}
; Adaptive prefetch mode (default: auto)
;   auto         - Select mode based on throughput calibration (recommended)
;   aggressive   - Position-based triggers (fast connections)
;   opportunistic - Circuit breaker triggers (moderate connections)
;   disabled     - Disable prefetch entirely
mode = {}
; Web API port for X-Plane SimState polling (default: 8086, range: 1024-65535)
web_api_port = {}

; Interval between prefetch cycles in milliseconds (default: 2000)
; Higher values reduce prefetch aggressiveness
cycle_interval_ms = {}

; Adaptive prefetch calibration
; Minimum throughput for aggressive mode (tiles/sec, default: 30)
; Systems exceeding this use position-based prefetch triggers
calibration_aggressive_threshold = {}
; Minimum throughput for opportunistic mode (tiles/sec, default: 10)
; Systems between this and aggressive use circuit breaker triggers
; Below this, prefetch is disabled
calibration_opportunistic_threshold = {}
; How long to measure throughput during initial calibration (seconds, default: 60)
calibration_sample_duration = {}

; Transition ramp settings (takeoff phase management)
; Altitude climb (feet) above takeoff MSL to release transition hold (default: 1000, range: 200-5000)
takeoff_climb_ft = {}
; Maximum seconds before timeout release if climb not reached (default: 90, range: 30-300)
takeoff_timeout_secs = {}
; Sustained seconds at GS < 40kt before landing detection (default: 15, range: 5-60)
landing_hysteresis_secs = {}
; Duration (secs) of linear ramp from start fraction to full rate (default: 30, range: 10-120)
ramp_duration_secs = {}
; Starting prefetch fraction when ramp begins (default: 0.25, range: 0.1-0.5)
ramp_start_fraction = {}

; Boundary-driven prefetch settings
; Buffer tiles for retention beyond visible window (default: 1, range: 0-3)
window_buffer = {}
; InProgress staleness timeout in seconds (default: 120, range: 30-600)
stale_region_timeout = {}

; Prefetch box settings (used by both ground and cruise phases)
; Total prefetch box extent per axis in degrees (default: 7.0, range: 3.0-15.0)
; On the ground: used directly with symmetric bias, producing a square box centered on the aircraft.
; In cruise: the maximum of a speed-proportional ramp from box_min_extent, with heading bias.
box_extent = {}
; Maximum forward bias fraction (default: 0.8, range: 0.5-0.9)
; Controls how much the prefetch box shifts forward in the direction of travel
; 0.5 = symmetric, 0.8 = 80% ahead / 20% behind on the primary axis
box_max_bias = {}

[prewarm]
; Settings for cold-start cache pre-warming.
; Use with --airport ICAO to pre-load tiles around an airport before flight.

; Grid rows (latitude extent) in DSF tiles around the airport to prewarm (default: 3)
grid_rows = {}
; Grid columns (longitude extent) in DSF tiles around the airport to prewarm (default: 4)
grid_cols = {}

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

[fuse]
; FUSE kernel settings for concurrent background request limits.
; Higher values allow more concurrent X-Plane scenery reads, preventing
; freezes at DSF boundaries. Only modify if you understand FUSE internals.

; Maximum pending background FUSE requests before the kernel queues (default: 256, range: 1-1024)
; The Linux kernel default of 12 severely limits X-Plane's concurrent scenery reads.
max_background = {}
; Congestion threshold for background FUSE requests (default: 192, range: 1-1024)
; Kernel starts throttling when pending requests exceed this. Convention: 75% of max_background.
congestion_threshold = {}
"#,
        update_check,
        config.provider.provider_type,
        google_api_key,
        mapbox_access_token,
        path_to_string(&config.cache.directory),
        format_size(config.cache.memory_size),
        format_size(config.cache.disk_size),
        config.cache.dds_disk_ratio,
        config.cache.disk_io_profile.as_str(),
        config.texture.format.to_string().to_lowercase(),
        config.texture.compressor,
        config.texture.gpu_device,
        config.generation.threads,
        config.generation.timeout,
        // Executor settings
        config.executor.network_concurrent,
        config.executor.cpu_concurrent,
        config.executor.disk_io_concurrent,
        config.control_plane.max_concurrent_jobs,
        config.executor.request_timeout_secs,
        config.executor.max_retries,
        config.executor.retry_base_delay_ms,
        scenery_dir,
        library_url,
        install_location,
        custom_scenery_path,
        auto_install_overlays,
        temp_dir,
        config.packages.concurrent_downloads,
        path_to_string(&config.logging.file),
        config.prefetch.enabled,
        config.prefetch.mode,
        config.prefetch.web_api_port,
        config.prefetch.cycle_interval_ms,
        config.prefetch.calibration_aggressive_threshold,
        config.prefetch.calibration_opportunistic_threshold,
        config.prefetch.calibration_sample_duration,
        config.prefetch.takeoff_climb_ft,
        config.prefetch.takeoff_timeout_secs,
        config.prefetch.landing_hysteresis_secs,
        config.prefetch.ramp_duration_secs,
        config.prefetch.ramp_start_fraction,
        config.prefetch.window_buffer,
        config.prefetch.stale_region_timeout,
        config.prefetch.box_extent,
        config.prefetch.box_max_bias,
        config.prewarm.grid_rows,
        config.prewarm.grid_cols,
        config.patches.enabled,
        config
            .patches
            .directory
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default(),
        // FUSE settings
        config.fuse.max_background,
        config.fuse.congestion_threshold,
    )
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
    use super::super::defaults::*;
    use super::super::settings::ConfigFile;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        let mut config = ConfigFile::default();
        config.provider.provider_type = "google".to_string();
        config.provider.google_api_key = Some("test-api-key".to_string());
        config.cache.memory_size = 4 * 1024 * 1024 * 1024; // 4GB
        config.executor.request_timeout_secs = 60;

        config.save_to(&config_path).unwrap();

        let loaded = ConfigFile::load_from(&config_path).unwrap();

        assert_eq!(loaded.provider.provider_type, "google");
        assert_eq!(
            loaded.provider.google_api_key,
            Some("test-api-key".to_string())
        );
        assert_eq!(loaded.cache.memory_size, 4 * 1024 * 1024 * 1024); // 4GB from config
        assert_eq!(loaded.executor.request_timeout_secs, 60);
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
    fn test_patches_config_defaults() {
        let config = ConfigFile::default();
        assert!(config.patches.enabled);
        // Default directory is ~/.xearthlayer/patches
        assert!(config.patches.directory.is_some());
        let dir = config.patches.directory.unwrap();
        assert!(dir.ends_with("patches"));
    }
}
