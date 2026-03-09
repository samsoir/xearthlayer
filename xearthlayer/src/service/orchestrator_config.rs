//! Configuration for the ServiceOrchestrator.
//!
//! This module provides `OrchestratorConfig` which consolidates all configuration
//! needed to start the complete XEarthLayer backend services stack.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{ConfigFile, DiskIoProfile};
use crate::prefetch::CircuitBreakerConfig;
use crate::provider::ProviderConfig;
use crate::service::ServiceConfig;

/// Configuration for the ServiceOrchestrator.
///
/// This struct consolidates all configuration needed to start the complete
/// backend services stack: cache, APT, prefetch, scene tracker, and FUSE mounts.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::service::OrchestratorConfig;
///
/// let config = OrchestratorConfig::from_config_file(
///     &config_file,
///     provider_config,
///     service_config,
///     custom_scenery_path,
///     patches_dir,
/// );
///
/// let orchestrator = ServiceOrchestrator::start(config).await?;
/// ```
#[derive(Clone, Debug)]
pub struct OrchestratorConfig {
    /// Provider configuration (Bing, Go2, Google, etc.).
    pub provider: ProviderConfig,

    /// Service configuration (texture, download, pipeline settings).
    pub service: ServiceConfig,

    /// Custom Scenery directory (where FUSE mounts are placed).
    pub custom_scenery_path: PathBuf,

    /// Patches directory (user's custom Ortho4XP tile patches).
    pub patches_dir: PathBuf,

    /// Whether patches are enabled.
    pub patches_enabled: bool,

    /// Install location for packages.
    pub packages_install_location: PathBuf,

    /// Disk I/O profile for storage-specific tuning.
    pub disk_io_profile: DiskIoProfile,

    /// Prefetch configuration.
    pub prefetch: PrefetchConfig,

    /// Prewarm configuration.
    pub prewarm: PrewarmConfig,

    /// Online network position configuration.
    pub online_network: OnlineNetworkConfig,

    /// FUSE kernel configuration.
    pub fuse: FuseConfig,

    /// Whether TUI mode is active (affects logging behavior).
    pub tui_mode: bool,
}

/// Prefetch-specific configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Whether prefetch is enabled.
    pub enabled: bool,

    /// Prefetch strategy name ("auto" or "adaptive").
    pub strategy: String,

    /// Adaptive prefetch mode: "auto", "aggressive", "opportunistic", or "disabled".
    pub mode: String,

    /// UDP port for telemetry reception.
    pub udp_port: u16,

    /// Maximum tiles to prefetch per cycle.
    pub max_tiles_per_cycle: usize,

    /// Cycle interval (milliseconds).
    pub cycle_interval_ms: u64,

    /// Circuit breaker configuration.
    pub circuit_breaker: CircuitBreakerConfig,

    /// Calibration: aggressive mode threshold (tiles/sec).
    pub calibration_aggressive_threshold: f64,

    /// Calibration: opportunistic mode threshold (tiles/sec).
    pub calibration_opportunistic_threshold: f64,

    /// Calibration: sample duration (seconds).
    pub calibration_sample_duration: u64,

    /// Altitude climb (feet) above takeoff MSL to release transition hold.
    pub takeoff_climb_ft: f32,

    /// Maximum seconds before timeout release if climb threshold not reached.
    pub takeoff_timeout_secs: u64,

    /// Sustained seconds at GS < 40kt before Cruise→Ground transition.
    pub landing_hysteresis_secs: u64,

    /// Duration (seconds) of linear ramp from start fraction to full rate.
    pub ramp_duration_secs: u64,

    /// Starting prefetch fraction when ramp begins.
    pub ramp_start_fraction: f64,

    /// Boundary trigger distance in degrees.
    pub trigger_distance: f64,

    /// Load depth for latitude boundary crossings (ROW loads).
    pub load_depth_lat: u8,

    /// Load depth for longitude boundary crossings (COLUMN loads).
    pub load_depth_lon: u8,

    /// Buffer tiles for retention.
    pub window_buffer: u8,

    /// InProgress staleness timeout in seconds.
    pub stale_region_timeout: u64,

    /// Assumed window height in DSF tiles.
    pub default_window_rows: usize,

    /// Longitude extent in degrees for dynamic column computation.
    pub window_lon_extent: f64,
}

/// Prewarm-specific configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct PrewarmConfig {
    /// Grid rows (latitude extent in DSF tiles centered on airport).
    pub grid_rows: u32,

    /// Grid columns (longitude extent in DSF tiles centered on airport).
    pub grid_cols: u32,

    /// Batch size for concurrent tile generation.
    pub batch_size: usize,
}

/// FUSE kernel configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct FuseConfig {
    /// Maximum pending background FUSE requests.
    pub max_background: u16,

    /// Congestion threshold for background FUSE requests.
    pub congestion_threshold: u16,
}

/// Online network position configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct OnlineNetworkConfig {
    /// Whether online network position fetching is enabled.
    pub enabled: bool,

    /// Network type: "vatsim", "ivao", or "pilotedge".
    pub network_type: String,

    /// Pilot identifier (CID for VATSIM).
    pub pilot_id: u64,

    /// API URL (for VATSIM, the status endpoint).
    pub api_url: String,

    /// Poll interval in seconds.
    pub poll_interval_secs: u64,

    /// Maximum age in seconds before data is considered stale.
    pub max_stale_secs: u64,
}

impl OrchestratorConfig {
    /// Create orchestrator configuration from CLI config file.
    ///
    /// This factory method extracts all necessary settings from the CLI's
    /// `ConfigFile` and organizes them for the orchestrator.
    ///
    /// # Arguments
    ///
    /// * `config` - The loaded CLI configuration file
    /// * `provider` - Provider configuration (resolved from CLI args and config)
    /// * `service` - Service configuration (built from CLI config)
    /// * `custom_scenery_path` - Path to X-Plane's Custom Scenery directory
    /// * `packages_install_location` - Path where packages are installed
    /// * `tui_mode` - Whether running in TUI mode
    pub fn from_config_file(
        config: &ConfigFile,
        provider: ProviderConfig,
        service: ServiceConfig,
        custom_scenery_path: PathBuf,
        packages_install_location: PathBuf,
        tui_mode: bool,
    ) -> Self {
        // Determine patches directory
        let patches_dir = if config.patches.enabled {
            config.patches.directory.clone().unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".xearthlayer")
                    .join("patches")
            })
        } else {
            // Use a non-existent path when patches are disabled
            PathBuf::from("/nonexistent")
        };

        // Extract prefetch configuration
        let prefetch = PrefetchConfig {
            enabled: config.prefetch.enabled,
            strategy: config.prefetch.strategy.clone(),
            mode: config.prefetch.mode.clone(),
            udp_port: config.prefetch.udp_port,
            max_tiles_per_cycle: config.prefetch.max_tiles_per_cycle,
            cycle_interval_ms: config.prefetch.cycle_interval_ms,
            circuit_breaker: CircuitBreakerConfig {
                open_duration: Duration::from_millis(config.prefetch.circuit_breaker_open_ms),
                half_open_duration: Duration::from_secs(
                    config.prefetch.circuit_breaker_half_open_secs,
                ),
            },
            calibration_aggressive_threshold: config.prefetch.calibration_aggressive_threshold,
            calibration_opportunistic_threshold: config
                .prefetch
                .calibration_opportunistic_threshold,
            calibration_sample_duration: config.prefetch.calibration_sample_duration,
            takeoff_climb_ft: config.prefetch.takeoff_climb_ft,
            takeoff_timeout_secs: config.prefetch.takeoff_timeout_secs,
            landing_hysteresis_secs: config.prefetch.landing_hysteresis_secs,
            ramp_duration_secs: config.prefetch.ramp_duration_secs,
            ramp_start_fraction: config.prefetch.ramp_start_fraction,
            trigger_distance: config.prefetch.trigger_distance,
            load_depth_lat: config.prefetch.load_depth_lat,
            load_depth_lon: config.prefetch.load_depth_lon,
            window_buffer: config.prefetch.window_buffer,
            stale_region_timeout: config.prefetch.stale_region_timeout,
            default_window_rows: config.prefetch.default_window_rows,
            window_lon_extent: config.prefetch.window_lon_extent,
        };

        // Extract prewarm configuration
        let prewarm = PrewarmConfig {
            grid_rows: config.prewarm.grid_rows,
            grid_cols: config.prewarm.grid_cols,
            batch_size: 50, // Fixed batch size for now
        };

        // Extract online network configuration
        let online_network = OnlineNetworkConfig {
            enabled: config.online_network.enabled,
            network_type: config.online_network.network_type.clone(),
            pilot_id: config.online_network.pilot_id,
            api_url: config.online_network.api_url.clone(),
            poll_interval_secs: config.online_network.poll_interval_secs,
            max_stale_secs: config.online_network.max_stale_secs,
        };

        // Extract FUSE configuration
        let fuse = FuseConfig {
            max_background: config.fuse.max_background,
            congestion_threshold: config.fuse.congestion_threshold,
        };

        Self {
            provider,
            service,
            custom_scenery_path,
            patches_dir,
            patches_enabled: config.patches.enabled,
            packages_install_location,
            disk_io_profile: config.cache.disk_io_profile,
            prefetch,
            prewarm,
            online_network,
            fuse,
            tui_mode,
        }
    }

    /// Check if prefetch is enabled.
    pub fn prefetch_enabled(&self) -> bool {
        self.prefetch.enabled
    }

    /// Check if cache is enabled.
    pub fn cache_enabled(&self) -> bool {
        self.service.cache_enabled()
    }

    /// Get the provider name.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_config() -> (ConfigFile, ProviderConfig, ServiceConfig, PathBuf, PathBuf) {
        let temp = tempdir().unwrap();
        let custom_scenery = temp.path().join("custom_scenery");
        let packages_dir = temp.path().join("packages");

        std::fs::create_dir_all(&custom_scenery).unwrap();
        std::fs::create_dir_all(&packages_dir).unwrap();

        // Create ConfigFile with default settings
        let config = ConfigFile::default();

        let provider = ProviderConfig::bing();
        let service = ServiceConfig::default();

        (config, provider, service, custom_scenery, packages_dir)
    }

    #[test]
    fn test_orchestrator_config_creation() {
        let (config, provider, service, custom_scenery, packages_dir) = create_test_config();

        let orch_config = OrchestratorConfig::from_config_file(
            &config,
            provider,
            service,
            custom_scenery.clone(),
            packages_dir.clone(),
            false,
        );

        assert_eq!(orch_config.custom_scenery_path, custom_scenery);
        assert_eq!(orch_config.packages_install_location, packages_dir);
        assert!(!orch_config.tui_mode);
    }

    #[test]
    fn test_prefetch_config_extraction() {
        let (config, provider, service, custom_scenery, packages_dir) = create_test_config();

        let orch_config = OrchestratorConfig::from_config_file(
            &config,
            provider,
            service,
            custom_scenery,
            packages_dir,
            true,
        );

        // Verify prefetch config was extracted
        assert_eq!(orch_config.prefetch.udp_port, config.prefetch.udp_port);
        assert_eq!(orch_config.prefetch.strategy, config.prefetch.strategy);
    }
}
