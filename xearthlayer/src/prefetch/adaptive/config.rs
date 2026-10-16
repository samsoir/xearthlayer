//! Configuration for the adaptive prefetch system.
//!
//! This module provides configuration types for the adaptive prefetch coordinator
//! and its strategy tuning parameters.
//!
//! # Configuration Levels
//!
//! 1. **User-facing options**: Simple toggles and mode selection (`enabled`, `mode`)
//! 2. **Advanced tuning**: Parameters for users who understand the system
//!
//! # Example Configuration (INI)
//!
//! ```ini
//! [prefetch]
//! enabled = true
//! mode = opportunistic
//! ```

use std::time::Duration;

use crate::config::defaults::{
    DEFAULT_BOX_MAX_SPEED, DEFAULT_BOX_MIN_EXTENT, DEFAULT_BOX_MIN_SPEED,
    DEFAULT_PREFETCH_STALE_REGION_TIMEOUT,
};
use crate::service::PrefetchConfig;

/// Configuration for the adaptive prefetch system.
///
/// This is the top-level configuration that controls all aspects of
/// adaptive prefetching, including mode selection and strategy parameters.
#[derive(Debug, Clone)]
pub struct AdaptivePrefetchConfig {
    /// Whether prefetch is enabled at all.
    pub enabled: bool,

    /// Strategy mode selection.
    ///
    /// - `Aggressive`: Always use position-based triggers
    /// - `Opportunistic`: Always use circuit breaker triggers
    /// - `Disabled`: Never prefetch (manual override)
    pub mode: PrefetchMode,

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

    // Prefetch box settings (used by both ground and cruise phases)
    /// Total prefetch box extent per axis in degrees.
    ///
    /// - On the ground: used directly (symmetric bias 0.5, square box).
    /// - In cruise: the maximum of a speed-proportional ramp from
    ///   `box_min_extent` up to this value, with forward heading bias.
    ///
    /// Default 7.0° produces a 7×7 DSF grid (49 tiles) at full extent —
    /// comfortably beyond X-Plane's ~3° scenery window.
    /// Range: 7.0 - 15.0 (enforced by `ConfigKey` validation)
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
            mode: PrefetchMode::Opportunistic,
            ground_speed_threshold_kt: 40.0,
            takeoff_climb_ft: 1000.0,
            takeoff_timeout: Duration::from_secs(90),
            landing_hysteresis: Duration::from_secs(15),
            ramp_duration: Duration::from_secs(30),
            ramp_start_fraction: 0.25,
            window_buffer: 1,
            stale_region_timeout: Duration::from_secs(DEFAULT_PREFETCH_STALE_REGION_TIMEOUT),
            box_extent: 7.0,
            box_max_bias: 0.8,
            box_min_extent: DEFAULT_BOX_MIN_EXTENT,
            box_min_speed: DEFAULT_BOX_MIN_SPEED,
            box_max_speed: DEFAULT_BOX_MAX_SPEED,
        }
    }
}

impl AdaptivePrefetchConfig {
    /// Create adaptive config from the orchestrator's prefetch config.
    ///
    /// This is the primary constructor used by the service orchestrator.
    pub fn from_prefetch_config(config: &PrefetchConfig) -> Self {
        let mode = config.mode.parse().unwrap_or_default();

        Self {
            enabled: config.enabled,
            mode,
            takeoff_climb_ft: config.takeoff_climb_ft,
            takeoff_timeout: Duration::from_secs(config.takeoff_timeout_secs),
            landing_hysteresis: Duration::from_secs(config.landing_hysteresis_secs),
            ramp_duration: Duration::from_secs(config.ramp_duration_secs),
            ramp_start_fraction: config.ramp_start_fraction,
            window_buffer: config.window_buffer,
            stale_region_timeout: Duration::from_secs(config.stale_region_timeout),
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
    /// Position-based trigger (0.3° into DSF tile).
    ///
    /// Use this for fast connections that can complete prefetch
    /// well before X-Plane needs the tiles.
    Aggressive,

    /// Circuit breaker trigger (when X-Plane finishes loading).
    ///
    /// The default. Prefetch happens during quiet periods when X-Plane
    /// isn't actively loading.
    #[default]
    Opportunistic,

    /// Prefetch disabled (manual override).
    ///
    /// Use this for debugging or when prefetch causes issues.
    Disabled,
}

impl std::fmt::Display for PrefetchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefetchMode::Aggressive => write!(f, "aggressive"),
            PrefetchMode::Opportunistic => write!(f, "opportunistic"),
            PrefetchMode::Disabled => write!(f, "disabled"),
        }
    }
}

impl std::str::FromStr for PrefetchMode {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Anything else, including the retired `auto`, is the default.
        Ok(match s.to_lowercase().as_str() {
            "aggressive" => Self::Aggressive,
            "disabled" => Self::Disabled,
            _ => Self::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AdaptivePrefetchConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, PrefetchMode::Opportunistic);
        assert_eq!(config.box_extent, 7.0);
    }

    #[test]
    fn default_prefetch_mode_is_opportunistic() {
        assert_eq!(PrefetchMode::default(), PrefetchMode::Opportunistic);
    }

    #[test]
    fn test_prefetch_mode_parsing() {
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
            PrefetchMode::Opportunistic
        );
    }

    #[test]
    fn legacy_auto_mode_parses_as_opportunistic() {
        // Every config written before #277 says `mode = auto`, and auto always
        // resolved to opportunistic because no calibration was ever supplied.
        assert_eq!(
            "auto".parse::<PrefetchMode>().unwrap(),
            PrefetchMode::Opportunistic
        );
    }

    #[test]
    fn test_prefetch_mode_display() {
        assert_eq!(format!("{}", PrefetchMode::Aggressive), "aggressive");
        assert_eq!(format!("{}", PrefetchMode::Opportunistic), "opportunistic");
        assert_eq!(format!("{}", PrefetchMode::Disabled), "disabled");
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
    }

    fn prefetch_config(mode: &str) -> PrefetchConfig {
        PrefetchConfig {
            enabled: true,
            mode: mode.to_string(),
            web_api_port: 8086,
            cycle_interval_ms: 2000,
            takeoff_climb_ft: 1000.0,
            takeoff_timeout_secs: 90,
            landing_hysteresis_secs: 15,
            ramp_duration_secs: 30,
            ramp_start_fraction: 0.25,
            window_buffer: 2,
            stale_region_timeout: 300,
            box_extent: 11.0,
            box_max_bias: 0.7,
        }
    }

    #[test]
    fn test_from_prefetch_config_boundary_fields() {
        let config = AdaptivePrefetchConfig::from_prefetch_config(&prefetch_config("aggressive"));
        assert_eq!(config.window_buffer, 2);
        assert_eq!(config.stale_region_timeout, Duration::from_secs(300));
        assert_eq!(config.box_extent, 11.0);
    }

    #[test]
    fn from_prefetch_config_honours_each_explicit_mode() {
        for (value, mode) in [
            ("aggressive", PrefetchMode::Aggressive),
            ("opportunistic", PrefetchMode::Opportunistic),
            ("disabled", PrefetchMode::Disabled),
        ] {
            let config = AdaptivePrefetchConfig::from_prefetch_config(&prefetch_config(value));
            assert_eq!(config.mode, mode, "{value}");
        }
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
