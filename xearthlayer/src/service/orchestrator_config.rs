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

    /// Whether TUI mode is active (affects logging behavior).
    pub tui_mode: bool,
}

/// Prefetch-specific configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Whether prefetch is enabled.
    pub enabled: bool,

    /// Prefetch strategy name (e.g., "auto", "radial", "heading-aware").
    pub strategy: String,

    /// UDP port for telemetry reception.
    pub udp_port: u16,

    /// Cone half-angle for heading-aware prefetch (degrees).
    pub cone_angle: f32,

    /// Inner radius for radial prefetch (nautical miles).
    pub inner_radius_nm: f32,

    /// Outer radius for radial prefetch (nautical miles).
    pub outer_radius_nm: f32,

    /// Radial radius for simple radial prefetch (tiles).
    pub radial_radius: u8,

    /// Maximum tiles to prefetch per cycle.
    pub max_tiles_per_cycle: usize,

    /// Cycle interval (milliseconds).
    pub cycle_interval_ms: u64,

    /// Circuit breaker configuration.
    pub circuit_breaker: CircuitBreakerConfig,

    /// Rows ahead for tile-based prefetch strategy.
    pub tile_based_rows_ahead: u32,
}

/// Prewarm-specific configuration extracted from ConfigFile.
#[derive(Clone, Debug)]
pub struct PrewarmConfig {
    /// Grid size (N×N DSF tiles centered on airport).
    pub grid_size: u32,

    /// Batch size for concurrent tile generation.
    pub batch_size: usize,
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
            udp_port: config.prefetch.udp_port,
            cone_angle: config.prefetch.cone_angle,
            inner_radius_nm: config.prefetch.inner_radius_nm,
            outer_radius_nm: config.prefetch.outer_radius_nm,
            radial_radius: config.prefetch.radial_radius,
            max_tiles_per_cycle: config.prefetch.max_tiles_per_cycle,
            cycle_interval_ms: config.prefetch.cycle_interval_ms,
            circuit_breaker: CircuitBreakerConfig {
                threshold_jobs_per_sec: config.prefetch.circuit_breaker_threshold,
                open_duration: Duration::from_millis(config.prefetch.circuit_breaker_open_ms),
                half_open_duration: Duration::from_secs(
                    config.prefetch.circuit_breaker_half_open_secs,
                ),
            },
            tile_based_rows_ahead: config.prefetch.tile_based_rows_ahead,
        };

        // Extract prewarm configuration
        let prewarm = PrewarmConfig {
            grid_size: config.prewarm.grid_size,
            batch_size: 50, // Fixed batch size for now
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
