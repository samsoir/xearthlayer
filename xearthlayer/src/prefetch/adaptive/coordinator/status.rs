//! Coordinator status types for UI and monitoring.
//!
//! This module contains the [`CoordinatorStatus`] struct that provides
//! a snapshot of the coordinator's current state for display in the TUI
//! and logging.

use crate::prefetch::state::BoxBoundsSnapshot;

use super::super::calibration::StrategyMode;
use super::super::phase_detector::FlightPhase;

/// Current coordinator state for status reporting.
///
/// This struct captures the coordinator's state at a point in time
/// for display in the TUI dashboard and structured logging.
///
/// # Fields
///
/// All fields are public for easy access by UI components:
/// - `enabled` - Whether prefetch is currently active
/// - `mode` - Current strategy mode (Aggressive/Opportunistic/Disabled)
/// - `phase` - Current flight phase (Ground/Cruise)
/// - `active_strategy` - Name of the currently active strategy
/// - `last_prefetch_count` - Tiles prefetched in the last cycle
/// - `deferred_last_cycle` - Whether the last cycle was skipped under load
/// - `throttle_fraction` - Takeoff throttle throughput, `None` at full speed
/// - `box_extent` - Current prefetch box extent in degrees (Cruise phase only)
#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    /// Whether prefetch is currently enabled.
    pub enabled: bool,

    /// Current strategy mode (from calibration or config).
    pub mode: StrategyMode,

    /// Current flight phase.
    pub phase: FlightPhase,

    /// Name of active strategy.
    pub active_strategy: &'static str,

    /// Tiles prefetched in the last cycle.
    pub last_prefetch_count: usize,

    /// Whether the most recent plan was deferred for lane or memory load.
    pub deferred_last_cycle: bool,

    /// Takeoff throttle throughput (0.0 to 1.0); `None` once it is idle.
    pub throttle_fraction: Option<f64>,

    /// Current prefetch box extent in degrees (Cruise phase only; 0.0 otherwise).
    pub box_extent: f64,

    /// Current prefetch box bounds (as actually used this cycle).
    ///
    /// `None` when prefetch is inactive (disabled, Transition, no position).
    /// The debug map reads this via `SharedPrefetchStatus` to render the
    /// overlay without recomputing — guaranteeing the UI matches reality
    /// even when bias/extent diverge between ground and cruise phases.
    pub box_bounds: Option<BoxBoundsSnapshot>,
}

impl Default for CoordinatorStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: StrategyMode::Opportunistic,
            phase: FlightPhase::Ground,
            active_strategy: "none",
            last_prefetch_count: 0,
            deferred_last_cycle: false,
            throttle_fraction: None,
            box_extent: 0.0,
            box_bounds: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_default() {
        let status = CoordinatorStatus::default();
        assert!(status.enabled);
        assert_eq!(status.mode, StrategyMode::Opportunistic);
        assert_eq!(status.phase, FlightPhase::Ground);
        assert_eq!(status.active_strategy, "none");
        assert_eq!(status.last_prefetch_count, 0);
        assert!(!status.deferred_last_cycle);
        assert_eq!(status.throttle_fraction, None);
        assert_eq!(status.box_extent, 0.0);
    }

    #[test]
    fn test_status_clone() {
        let status = CoordinatorStatus {
            enabled: false,
            mode: StrategyMode::Aggressive,
            phase: FlightPhase::Cruise,
            active_strategy: "cruise",
            last_prefetch_count: 10,
            deferred_last_cycle: true,
            throttle_fraction: Some(0.5),
            box_extent: 5.5,
            box_bounds: None,
        };

        let cloned = status.clone();
        assert_eq!(cloned.enabled, status.enabled);
        assert_eq!(cloned.mode, status.mode);
        assert_eq!(cloned.phase, status.phase);
    }

    #[test]
    fn test_status_debug() {
        let status = CoordinatorStatus::default();
        let debug = format!("{:?}", status);
        assert!(debug.contains("CoordinatorStatus"));
        assert!(debug.contains("enabled"));
        assert!(debug.contains("mode"));
    }
}
