//! Prefetch activation conditions.
//!
//! This module provides a trait-based approach for determining when prefetching
//! should be active. Different conditions can be composed or replaced to support
//! various flight phases and scenarios.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::{PrefetchCondition, MinimumSpeedCondition, AircraftState};
//!
//! let condition = MinimumSpeedCondition::new(30.0); // 30 knots minimum
//! let state = AircraftState::new(45.0, -122.0, 90.0, 150.0, 10000.0);
//!
//! if condition.should_prefetch(&state) {
//!     // Aircraft is moving fast enough, proceed with prefetch
//! }
//! ```

use super::state::AircraftState;

/// Trait for determining if prefetching should be active.
///
/// Implementations can check various conditions such as:
/// - Minimum ground speed
/// - Altitude ranges (e.g., only prefetch below certain altitudes)
/// - Flight phase (takeoff, cruise, landing)
/// - Geographic restrictions
pub trait PrefetchCondition: Send + Sync {
    /// Determine if prefetching should be active for the given aircraft state.
    ///
    /// # Arguments
    ///
    /// * `state` - Current aircraft state from telemetry
    ///
    /// # Returns
    ///
    /// `true` if prefetching should be active, `false` otherwise.
    fn should_prefetch(&self, state: &AircraftState) -> bool;

    /// Human-readable description of this condition for logging.
    fn description(&self) -> &str;
}

/// Prefetch condition based on minimum ground speed.
///
/// Prefetching only activates when the aircraft is moving faster than a
/// configured threshold. This prevents unnecessary predictions when the
/// aircraft is stationary, taxiing, or moving slowly on the ground.
#[derive(Debug, Clone)]
pub struct MinimumSpeedCondition {
    /// Minimum ground speed in knots.
    min_speed_knots: f32,
}

impl MinimumSpeedCondition {
    /// Default minimum speed threshold in knots.
    pub const DEFAULT_MIN_SPEED: f32 = 30.0;

    /// Create a new minimum speed condition.
    ///
    /// # Arguments
    ///
    /// * `min_speed_knots` - Minimum ground speed in knots
    pub fn new(min_speed_knots: f32) -> Self {
        Self { min_speed_knots }
    }
}

impl Default for MinimumSpeedCondition {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MIN_SPEED)
    }
}

impl PrefetchCondition for MinimumSpeedCondition {
    fn should_prefetch(&self, state: &AircraftState) -> bool {
        state.ground_speed >= self.min_speed_knots
    }

    fn description(&self) -> &str {
        "minimum ground speed"
    }
}

/// Condition that always allows prefetching.
///
/// Useful for testing or when no conditions should be applied.
#[derive(Debug, Clone, Default)]
pub struct AlwaysActiveCondition;

impl PrefetchCondition for AlwaysActiveCondition {
    fn should_prefetch(&self, _state: &AircraftState) -> bool {
        true
    }

    fn description(&self) -> &str {
        "always active"
    }
}

/// Condition that never allows prefetching.
///
/// Useful for testing or when prefetch should be disabled.
#[derive(Debug, Clone, Default)]
pub struct NeverActiveCondition;

impl PrefetchCondition for NeverActiveCondition {
    fn should_prefetch(&self, _state: &AircraftState) -> bool {
        false
    }

    fn description(&self) -> &str {
        "never active"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimum_speed_condition_below_threshold() {
        let condition = MinimumSpeedCondition::new(30.0);
        let state = AircraftState::new(45.0, -122.0, 90.0, 20.0, 1000.0);

        assert!(!condition.should_prefetch(&state));
    }

    #[test]
    fn test_minimum_speed_condition_at_threshold() {
        let condition = MinimumSpeedCondition::new(30.0);
        let state = AircraftState::new(45.0, -122.0, 90.0, 30.0, 1000.0);

        assert!(condition.should_prefetch(&state));
    }

    #[test]
    fn test_minimum_speed_condition_above_threshold() {
        let condition = MinimumSpeedCondition::new(30.0);
        let state = AircraftState::new(45.0, -122.0, 90.0, 150.0, 10000.0);

        assert!(condition.should_prefetch(&state));
    }

    #[test]
    fn test_minimum_speed_condition_default() {
        let condition = MinimumSpeedCondition::default();
        assert_eq!(condition.min_speed_knots, 30.0);
    }

    #[test]
    fn test_always_active_condition() {
        let condition = AlwaysActiveCondition;
        let state = AircraftState::new(45.0, -122.0, 90.0, 0.0, 0.0);

        assert!(condition.should_prefetch(&state));
    }

    #[test]
    fn test_never_active_condition() {
        let condition = NeverActiveCondition;
        let state = AircraftState::new(45.0, -122.0, 90.0, 500.0, 35000.0);

        assert!(!condition.should_prefetch(&state));
    }
}
