//! Configuration for the adaptive prefetch system.
//!
//! This module provides configuration types for the adaptive prefetch coordinator,
//! calibration thresholds, and strategy tuning parameters.
//!
//! # Configuration Levels
//!
//! The configuration is organized into three tiers:
//!
//! 1. **User-facing options**: Simple toggles and mode selection (`enabled`, `mode`)
//! 2. **Advanced tuning**: Parameters for users who understand the system
//! 3. **Calibration thresholds**: Performance-based mode selection
//!
//! # Example Configuration (INI)
//!
//! ```ini
//! [prefetch]
//! enabled = true
//! mode = auto
//! strategy = adaptive
//!
//! [prefetch.calibration]
//! aggressive_threshold = 30
//! opportunistic_threshold = 10
//! sample_duration = 60
//! ```

use std::time::Duration;

use crate::config::defaults::{
    DEFAULT_BOX_MAX_SPEED, DEFAULT_BOX_MIN_EXTENT, DEFAULT_BOX_MIN_SPEED,
};
use crate::config::PrefetchSettings;
use crate::service::PrefetchConfig;

/// Configuration for the adaptive prefetch system.
///
/// This is the top-level configuration that controls all aspects of
/// adaptive prefetching, including mode selection, strategy parameters,
/// and calibration thresholds.
#[derive(Debug, Clone)]
pub struct AdaptivePrefetchConfig {
    /// Whether prefetch is enabled at all.
    pub enabled: bool,

    /// Strategy mode selection.
    ///
    /// - `Auto`: Select mode based on calibration results
    /// - `Aggressive`: Always use position-based triggers
    /// - `Opportunistic`: Always use circuit breaker triggers
    /// - `Disabled`: Never prefetch (manual override)
    pub mode: PrefetchMode,

    /// Automatically disable prefetch if performance is too low.
    ///
    /// When `Auto`, prefetch is disabled if throughput falls below
    /// `calibration.opportunistic_threshold` during calibration.
    pub low_performance_killswitch: KillswitchMode,

    /// Maximum tiles per prefetch cycle.
    ///
    /// Caps tiles submitted in a single prefetch operation.
    /// Controls queue depth, not processing rate (see `ResourcePool` prefetch fraction).
    /// Range: 50 - 500
    pub max_tiles_per_cycle: u32,

    /// Ground strategy ring radius (degrees).
    ///
    /// Radius of prefetch ring when aircraft is on ground.
    /// Range: 0.5 - 2.0
    pub ground_ring_radius: f64,

    /// Calibration thresholds and parameters.
    pub calibration: CalibrationConfig,

    /// Ground speed threshold for ground vs cruise detection (knots).
    ///
    /// Aircraft with ground speed below this are considered on the ground.
    pub ground_speed_threshold_kt: f32,

    /// Altitude climb (feet) above takeoff MSL to release transition hold.
    pub takeoff_climb_ft: f32,

    /// Maximum time before timeout release if climb threshold not reached.
    pub takeoff_timeout: Duration,

    /// Sustained duration at GS < 40kt before Cruise→Ground transition.
    pub landing_hysteresis: Duration,

    /// Duration of linear ramp from start fraction to full rate.
    pub ramp_duration: Duration,

    /// Starting prefetch fraction when ramp begins.
    pub ramp_start_fraction: f64,

    // Boundary-driven prefetch settings
    /// Buffer tiles for retention.
    ///
    /// Extra tiles to retain beyond the visible window.
    /// Range: 0 - 3
    pub window_buffer: u8,

    /// InProgress staleness timeout.
    ///
    /// How long a region can stay InProgress before being considered stale.
    pub stale_region_timeout: Duration,

    /// Assumed window height in DSF tiles.
    ///
    /// Used when the actual window size is unknown.
    /// Range: 2 - 12
    pub default_window_rows: usize,

    /// Longitude extent in degrees for dynamic column computation.
    ///
    /// Columns computed as `ceil(lon_extent / cos(latitude))`.
    /// Range: 1.0 - 10.0
    pub window_lon_extent: f64,

    // Sliding prefetch box settings
    /// Total prefetch box extent per axis in degrees.
    ///
    /// X-Plane loads a ~6×6 DSF area around the aircraft. 9° covers
    /// this with 1.5° overlap on all sides, ensuring tiles are ready
    /// before X-Plane crosses into the next DSF region.
    /// Range: 7.0 - 15.0
    pub box_extent: f64,

    /// Maximum forward bias fraction (0.5 = symmetric, 0.8 = 80/20).
    ///
    /// Controls how much the prefetch box shifts forward in the
    /// direction of travel. At 0.8, the primary axis gets 80% ahead
    /// and 20% behind; perpendicular axes get 50/50.
    /// Range: 0.5 - 0.9
    pub box_max_bias: f64,

    /// Minimum prefetch box extent (degrees) at low ground speed.
    ///
    /// When the aircraft is at or below `box_min_speed`, the box uses this
    /// extent. Shrinks the prefetch region during approach and taxi.
    pub box_min_extent: f64,

    /// Ground speed (knots) at which the box uses `box_min_extent`.
    ///
    /// Speeds at or below this value produce the minimum extent.
    pub box_min_speed: f32,

    /// Ground speed (knots) at which the box uses `box_extent` (maximum).
    ///
    /// Speeds at or above this value produce the full `box_extent`.
    pub box_max_speed: f32,
}

impl Default for AdaptivePrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: PrefetchMode::Auto,
            low_performance_killswitch: KillswitchMode::Auto,
            max_tiles_per_cycle: 200,
            ground_ring_radius: 1.0,
            calibration: CalibrationConfig::default(),
            ground_speed_threshold_kt: 40.0,
            takeoff_climb_ft: 1000.0,
            takeoff_timeout: Duration::from_secs(90),
            landing_hysteresis: Duration::from_secs(15),
            ramp_duration: Duration::from_secs(30),
            ramp_start_fraction: 0.25,
            window_buffer: 1,
            stale_region_timeout: Duration::from_secs(120),
            default_window_rows: 3,
            window_lon_extent: 3.0,
            box_extent: 6.5,
            box_max_bias: 0.8,
            box_min_extent: DEFAULT_BOX_MIN_EXTENT,
            box_min_speed: DEFAULT_BOX_MIN_SPEED,
            box_max_speed: DEFAULT_BOX_MAX_SPEED,
        }
    }
}

impl AdaptivePrefetchConfig {
    /// Create adaptive config from the config file's prefetch settings.
    ///
    /// Maps the user-facing `PrefetchSettings` to the internal `AdaptivePrefetchConfig`,
    /// including calibration thresholds and mode selection.
    #[allow(dead_code)] // May be used in the future for direct config file access
    pub fn from_prefetch_settings(settings: &PrefetchSettings) -> Self {
        let mode = settings.mode.parse().unwrap_or(PrefetchMode::Auto);

        Self {
            enabled: settings.enabled,
            mode,
            max_tiles_per_cycle: settings.max_tiles_per_cycle as u32,
            calibration: CalibrationConfig {
                aggressive_threshold: settings.calibration_aggressive_threshold,
                opportunistic_threshold: settings.calibration_opportunistic_threshold,
                sample_duration: Duration::from_secs(settings.calibration_sample_duration),
                ..Default::default()
            },
            takeoff_climb_ft: settings.takeoff_climb_ft,
            takeoff_timeout: Duration::from_secs(settings.takeoff_timeout_secs),
            landing_hysteresis: Duration::from_secs(settings.landing_hysteresis_secs),
            ramp_duration: Duration::from_secs(settings.ramp_duration_secs),
            ramp_start_fraction: settings.ramp_start_fraction,
            window_buffer: settings.window_buffer,
            stale_region_timeout: Duration::from_secs(settings.stale_region_timeout),
            default_window_rows: settings.default_window_rows,
            window_lon_extent: settings.window_lon_extent,
            box_extent: settings.box_extent,
            box_max_bias: settings.box_max_bias,
            ..Default::default()
        }
    }

    /// Create adaptive config from the orchestrator's prefetch config.
    ///
    /// This is the primary constructor used by the service orchestrator.
    pub fn from_prefetch_config(config: &PrefetchConfig) -> Self {
        let mode = config.mode.parse().unwrap_or(PrefetchMode::Auto);

        Self {
            enabled: config.enabled,
            mode,
            max_tiles_per_cycle: config.max_tiles_per_cycle as u32,
            calibration: CalibrationConfig {
                aggressive_threshold: config.calibration_aggressive_threshold,
                opportunistic_threshold: config.calibration_opportunistic_threshold,
                sample_duration: Duration::from_secs(config.calibration_sample_duration),
                ..Default::default()
            },
            takeoff_climb_ft: config.takeoff_climb_ft,
            takeoff_timeout: Duration::from_secs(config.takeoff_timeout_secs),
            landing_hysteresis: Duration::from_secs(config.landing_hysteresis_secs),
            ramp_duration: Duration::from_secs(config.ramp_duration_secs),
            ramp_start_fraction: config.ramp_start_fraction,
            window_buffer: config.window_buffer,
            stale_region_timeout: Duration::from_secs(config.stale_region_timeout),
            default_window_rows: config.default_window_rows,
            window_lon_extent: config.window_lon_extent,
            box_extent: config.box_extent,
            box_max_bias: config.box_max_bias,
            ..Default::default()
        }
    }
}

/// Strategy mode for prefetch triggering.
///
/// Controls when and how prefetch jobs are submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefetchMode {
    /// Select trigger mode based on calibration results.
    ///
    /// This is the recommended setting. The system measures throughput
    /// during the initial load and selects the appropriate mode.
    #[default]
    Auto,

    /// Position-based trigger (0.3° into DSF tile).
    ///
    /// Use this for fast connections that can complete prefetch
    /// well before X-Plane needs the tiles.
    Aggressive,

    /// Circuit breaker trigger (when X-Plane finishes loading).
    ///
    /// Use this for moderate connections. Prefetch happens during
    /// quiet periods when X-Plane isn't actively loading.
    Opportunistic,

    /// Prefetch disabled (manual override).
    ///
    /// Use this for debugging or when prefetch causes issues.
    Disabled,
}

impl std::fmt::Display for PrefetchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefetchMode::Auto => write!(f, "auto"),
            PrefetchMode::Aggressive => write!(f, "aggressive"),
            PrefetchMode::Opportunistic => write!(f, "opportunistic"),
            PrefetchMode::Disabled => write!(f, "disabled"),
        }
    }
}

impl std::str::FromStr for PrefetchMode {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "aggressive" => Self::Aggressive,
            "opportunistic" => Self::Opportunistic,
            "disabled" => Self::Disabled,
            _ => Self::Auto,
        })
    }
}

/// Low performance killswitch mode.
///
/// Controls automatic disabling of prefetch when performance is too low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KillswitchMode {
    /// Automatically disable prefetch if throughput is too low.
    ///
    /// This is the recommended setting. If calibration shows that
    /// prefetch won't complete in time, it's disabled to avoid
    /// wasting resources.
    #[default]
    Auto,

    /// Never automatically disable prefetch.
    ///
    /// Use this if you want to force prefetch even on slow connections.
    /// May result in prefetch not completing before X-Plane needs tiles.
    Never,
}

impl std::fmt::Display for KillswitchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KillswitchMode::Auto => write!(f, "auto"),
            KillswitchMode::Never => write!(f, "never"),
        }
    }
}

impl std::str::FromStr for KillswitchMode {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "never" => Self::Never,
            _ => Self::Auto,
        })
    }
}

/// Configuration for performance calibration.
///
/// These thresholds determine which mode is selected during calibration
/// and how often recalibration occurs.
#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    /// Minimum throughput for aggressive mode (tiles/sec).
    ///
    /// If measured throughput exceeds this, aggressive (position-based)
    /// prefetch is enabled.
    pub aggressive_threshold: f64,

    /// Minimum throughput for opportunistic mode (tiles/sec).
    ///
    /// If measured throughput is between this and `aggressive_threshold`,
    /// opportunistic (circuit breaker) prefetch is enabled.
    /// Below this threshold, prefetch is disabled.
    pub opportunistic_threshold: f64,

    /// How long to measure throughput during initial calibration (seconds).
    ///
    /// Longer duration gives more accurate results but delays flight start.
    pub sample_duration: Duration,

    /// Minimum number of samples required for valid calibration.
    ///
    /// If fewer than this many tiles are generated during calibration,
    /// the calibration is considered incomplete.
    pub min_samples: usize,

    /// Confidence threshold for valid calibration (0.0 - 1.0).
    ///
    /// Calibration confidence is based on sample size and variance.
    /// Below this threshold, the calibration is considered unreliable.
    pub confidence_threshold: f64,

    /// Rolling recalibration window (how much recent history to consider).
    pub rolling_window: Duration,

    /// How often to check for recalibration.
    pub recalibration_interval: Duration,

    /// Degradation threshold (percentage of baseline).
    ///
    /// If current throughput falls below this percentage of baseline,
    /// the mode is downgraded.
    pub degradation_threshold: f64,

    /// Recovery threshold (percentage of baseline).
    ///
    /// If current throughput rises above this percentage of baseline,
    /// the mode can be upgraded.
    pub recovery_threshold: f64,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            aggressive_threshold: 30.0,
            opportunistic_threshold: 10.0,
            sample_duration: Duration::from_secs(60),
            min_samples: 50,
            confidence_threshold: 0.7,
            rolling_window: Duration::from_secs(300), // 5 minutes
            recalibration_interval: Duration::from_secs(900), // 15 minutes
            degradation_threshold: 0.7,
            recovery_threshold: 0.9,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AdaptivePrefetchConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, PrefetchMode::Auto);
        assert_eq!(config.max_tiles_per_cycle, 200);
    }

    #[test]
    fn test_prefetch_mode_parsing() {
        assert_eq!("auto".parse::<PrefetchMode>().unwrap(), PrefetchMode::Auto);
        assert_eq!(
            "aggressive".parse::<PrefetchMode>().unwrap(),
            PrefetchMode::Aggressive
        );
        assert_eq!(
            "opportunistic".parse::<PrefetchMode>().unwrap(),
            PrefetchMode::Opportunistic
        );
        assert_eq!(
            "disabled".parse::<PrefetchMode>().unwrap(),
            PrefetchMode::Disabled
        );
        assert_eq!(
            "unknown".parse::<PrefetchMode>().unwrap(),
            PrefetchMode::Auto
        );
    }

    #[test]
    fn test_prefetch_mode_display() {
        assert_eq!(format!("{}", PrefetchMode::Auto), "auto");
        assert_eq!(format!("{}", PrefetchMode::Aggressive), "aggressive");
        assert_eq!(format!("{}", PrefetchMode::Opportunistic), "opportunistic");
        assert_eq!(format!("{}", PrefetchMode::Disabled), "disabled");
    }

    #[test]
    fn test_killswitch_mode_parsing() {
        assert_eq!(
            "auto".parse::<KillswitchMode>().unwrap(),
            KillswitchMode::Auto
        );
        assert_eq!(
            "never".parse::<KillswitchMode>().unwrap(),
            KillswitchMode::Never
        );
        assert_eq!(
            "unknown".parse::<KillswitchMode>().unwrap(),
            KillswitchMode::Auto
        );
    }

    #[test]
    fn test_calibration_thresholds() {
        let config = CalibrationConfig::default();
        // Aggressive threshold should be higher than opportunistic
        assert!(config.aggressive_threshold > config.opportunistic_threshold);
        // Thresholds should be positive
        assert!(config.aggressive_threshold > 0.0);
        assert!(config.opportunistic_threshold > 0.0);
    }

    #[test]
    fn test_ground_detection_thresholds() {
        let config = AdaptivePrefetchConfig::default();
        // Ground speed threshold (40kt is reasonable for taxi)
        assert_eq!(config.ground_speed_threshold_kt, 40.0);
    }

    #[test]
    fn test_default_config_boundary_prefetch() {
        let config = AdaptivePrefetchConfig::default();
        assert_eq!(config.window_buffer, 1);
        assert_eq!(config.stale_region_timeout, Duration::from_secs(120));
        assert_eq!(config.default_window_rows, 3);
        assert_eq!(config.window_lon_extent, 3.0);
    }

    #[test]
    fn test_from_prefetch_settings_boundary_fields() {
        let settings = PrefetchSettings {
            enabled: true,
            mode: "auto".to_string(),
            web_api_port: 8086,
            max_tiles_per_cycle: 200,
            cycle_interval_ms: 2000,
            calibration_aggressive_threshold: 30.0,
            calibration_opportunistic_threshold: 10.0,
            calibration_sample_duration: 60,
            takeoff_climb_ft: 1000.0,
            takeoff_timeout_secs: 90,
            landing_hysteresis_secs: 15,
            ramp_duration_secs: 30,
            ramp_start_fraction: 0.25,
            window_buffer: 2,
            stale_region_timeout: 300,
            default_window_rows: 4,
            window_lon_extent: 4.0,
            box_extent: 11.0,
            box_max_bias: 0.7,
        };

        let config = AdaptivePrefetchConfig::from_prefetch_settings(&settings);
        assert_eq!(config.window_buffer, 2);
        assert_eq!(config.stale_region_timeout, Duration::from_secs(300));
        assert_eq!(config.default_window_rows, 4);
        assert_eq!(config.window_lon_extent, 4.0);
    }

    #[test]
    fn test_default_config_transition_ramp() {
        let config = AdaptivePrefetchConfig::default();
        assert_eq!(config.takeoff_climb_ft, 1000.0);
        assert_eq!(config.takeoff_timeout, Duration::from_secs(90));
        assert_eq!(config.landing_hysteresis, Duration::from_secs(15));
        assert_eq!(config.ramp_duration, Duration::from_secs(30));
        assert_eq!(config.ramp_start_fraction, 0.25);
    }
}
