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
;   adaptive     - Self-calibrating DSF-aligned prefetch (recommended)
; Legacy strategies (radial, heading-aware, tile-based) are deprecated
; and will automatically use adaptive instead.
strategy = {}
; Adaptive prefetch mode (default: auto)
;   auto         - Select mode based on throughput calibration (recommended)
;   aggressive   - Position-based triggers (fast connections)
;   opportunistic - Circuit breaker triggers (moderate connections)
;   disabled     - Disable prefetch entirely
mode = {}
; UDP port for telemetry (default: 49002 for ForeFlight protocol)
udp_port = {}

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

; Adaptive prefetch calibration (for strategy = adaptive)
; Minimum throughput for aggressive mode (tiles/sec, default: 30)
; Systems exceeding this use position-based prefetch triggers
calibration_aggressive_threshold = {}
; Minimum throughput for opportunistic mode (tiles/sec, default: 10)
; Systems between this and aggressive use circuit breaker triggers
; Below this, prefetch is disabled
calibration_opportunistic_threshold = {}
; How long to measure throughput during initial calibration (seconds, default: 60)
calibration_sample_duration = {}

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

[online_network]
; Online ATC network position (VATSIM, IVAO, PilotEdge).
; Provides pilot position from network APIs as a position source for the APT system.

; Enable/disable online network position fetching (default: false)
enabled = {}
; Network type: vatsim, ivao, or pilotedge (default: vatsim)
network_type = {}
; Pilot identifier (CID for VATSIM, default: 0 = disabled)
pilot_id = {}
; API URL (for VATSIM, the status endpoint)
api_url = {}
; Poll interval in seconds (default: 15)
poll_interval_secs = {}
; Maximum age in seconds before data is considered stale (default: 60)
max_stale_secs = {}
"#,
        config.provider.provider_type,
        google_api_key,
        mapbox_access_token,
        path_to_string(&config.cache.directory),
        format_size(config.cache.memory_size),
        format_size(config.cache.disk_size),
        config.cache.disk_io_profile.as_str(),
        config.texture.format.to_string().to_lowercase(),
        config.download.timeout,
        config.generation.threads,
        config.generation.timeout,
        // Executor settings
        config.executor.network_concurrent,
        config.executor.cpu_concurrent,
        config.executor.disk_io_concurrent,
        config.executor.max_concurrent_tasks,
        config.executor.job_channel_capacity,
        config.executor.request_channel_capacity,
        config.executor.request_timeout_secs,
        config.executor.max_retries,
        config.executor.retry_base_delay_ms,
        scenery_dir,
        library_url,
        install_location,
        custom_scenery_path,
        auto_install_overlays,
        temp_dir,
        path_to_string(&config.logging.file),
        config.prefetch.enabled,
        config.prefetch.strategy,
        config.prefetch.mode,
        config.prefetch.udp_port,
        config.prefetch.max_tiles_per_cycle,
        config.prefetch.cycle_interval_ms,
        config.prefetch.circuit_breaker_threshold,
        config.prefetch.circuit_breaker_open_ms,
        config.prefetch.circuit_breaker_half_open_secs,
        config.prefetch.calibration_aggressive_threshold,
        config.prefetch.calibration_opportunistic_threshold,
        config.prefetch.calibration_sample_duration,
        config.control_plane.max_concurrent_jobs,
        config.control_plane.stall_threshold_secs,
        config.control_plane.health_check_interval_secs,
        config.control_plane.semaphore_timeout_secs,
        config.prewarm.grid_size,
        config.patches.enabled,
        config
            .patches
            .directory
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default(),
        config.online_network.enabled,
        config.online_network.network_type,
        config.online_network.pilot_id,
        config.online_network.api_url,
        config.online_network.poll_interval_secs,
        config.online_network.max_stale_secs,
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
