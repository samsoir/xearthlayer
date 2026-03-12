//! Coordinator status types for UI and monitoring.
//!
//! This module contains the [`CoordinatorStatus`] struct that provides
//! a snapshot of the coordinator's current state for display in the TUI
//! and logging.

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
/// - `throttled` - Whether circuit breaker is throttling prefetch
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

    /// Whether throttled by circuit breaker.
    pub throttled: bool,
}

impl Default for CoordinatorStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: StrategyMode::Opportunistic,
            phase: FlightPhase::Ground,
            active_strategy: "none",
            last_prefetch_count: 0,
            throttled: false,
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
        assert!(!status.throttled);
    }

    #[test]
    fn test_status_clone() {
        let status = CoordinatorStatus {
            enabled: false,
            mode: StrategyMode::Aggressive,
            phase: FlightPhase::Cruise,
            active_strategy: "cruise",
            last_prefetch_count: 10,
            throttled: true,
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
