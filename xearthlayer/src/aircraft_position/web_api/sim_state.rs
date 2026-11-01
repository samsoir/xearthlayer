//! X-Plane sim state from direct dataref observation.
//!
//! Replaces heuristic detection (CircuitBreaker, BurstDetector,
//! PhaseDetector ground speed) with explicit sim state read from
//! X-Plane's Web API datarefs.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::datarefs;

/// Thread-safe shared sim state for cross-component access.
///
/// Updated by the [`super::WebApiAdapter`], read by the prefetch coordinator.
pub type SharedSimState = Arc<RwLock<SimState>>;

/// Create a new [`SharedSimState`] with default values.
pub fn shared_sim_state() -> SharedSimState {
    Arc::new(RwLock::new(SimState::default()))
}

/// X-Plane sim state from direct dataref observation.
///
/// Each field maps to a specific X-Plane dataref, providing definitive
/// answers to questions that the old system inferred from heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimState {
    /// Sim is paused. Prefetch continues opportunistically — the sim
    /// is waiting, so XEL has full system resources with no contention.
    pub paused: bool,
    /// Aircraft is on the ground (any gear weight-on-wheels).
    /// Replaces `PhaseDetector` ground speed threshold.
    pub on_ground: bool,
    /// X-Plane is asynchronously loading scenery.
    /// Replaces `CircuitBreaker` and `BurstDetector`.
    pub scenery_loading: bool,
    /// Sim is in replay mode. No real flight — skip prefetch.
    pub replay: bool,
    /// Sim speed multiplier (1 = normal, 2 = 2x, etc.).
    pub sim_speed: i32,
    /// X-Plane's smoothed frame rate in whole frames per second, for display.
    /// `None` until X-Plane reports a usable frame period, or when its
    /// version does not publish one.
    pub frame_rate: Option<u16>,
}

impl Default for SimState {
    /// Default assumes normal flight (safe for prefetch).
    ///
    /// This is used when the Web API hasn't connected yet —
    /// we don't want to inhibit prefetch by default.
    fn default() -> Self {
        Self {
            paused: false,
            on_ground: false,
            scenery_loading: false,
            replay: false,
            sim_speed: 1,
            frame_rate: None,
        }
    }
}

impl SimState {
    /// Parse sim state from raw dataref values.
    ///
    /// Missing values default to the safe state (no inhibit).
    pub fn from_dataref_values(values: &HashMap<String, f64>) -> Self {
        Self {
            paused: values.get(datarefs::PAUSED).is_some_and(|v| *v as i32 != 0),
            on_ground: values
                .get(datarefs::ON_GROUND)
                .is_some_and(|v| *v as i32 != 0),
            scenery_loading: values
                .get(datarefs::SCENERY_LOADING)
                .is_some_and(|v| *v as i32 != 0),
            replay: values
                .get(datarefs::IS_REPLAY)
                .is_some_and(|v| *v as i32 != 0),
            sim_speed: values.get(datarefs::SIM_SPEED).map_or(1, |v| *v as i32),
            frame_rate: values
                .get(datarefs::FRAME_RATE_PERIOD)
                .and_then(|period| frame_rate_from_period(*period)),
        }
    }

    /// Whether prefetch should run in this state.
    ///
    /// Blocked by:
    /// - `scenery_loading` — X-Plane is saturated with IO
    /// - `replay` — no real flight, prefetch is wasted
    ///
    /// Allowed during:
    /// - `paused` — opportunistic, XEL has full resources
    /// - `on_ground` — ground strategy handles this phase
    pub fn should_prefetch(&self) -> bool {
        !self.scenery_loading && !self.replay
    }
}

/// Whole frames per second from a frame period in seconds, or `None` when
/// the period is not a positive finite number.
fn frame_rate_from_period(period: f64) -> Option<u16> {
    (period.is_finite() && period > 0.0).then(|| (1.0 / period).round().min(u16::MAX as f64) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sim_state_from_dataref_values() {
        let mut values = HashMap::new();
        values.insert(datarefs::PAUSED.to_string(), 0.0_f64);
        values.insert(datarefs::ON_GROUND.to_string(), 1.0);
        values.insert(datarefs::SCENERY_LOADING.to_string(), 0.0);
        values.insert(datarefs::IS_REPLAY.to_string(), 0.0);
        values.insert(datarefs::SIM_SPEED.to_string(), 1.0);

        let state = SimState::from_dataref_values(&values);
        assert!(!state.paused);
        assert!(state.on_ground);
        assert!(!state.scenery_loading);
        assert!(!state.replay);
        assert_eq!(state.sim_speed, 1);
    }

    #[test]
    fn test_sim_state_all_active() {
        let mut values = HashMap::new();
        values.insert(datarefs::PAUSED.to_string(), 1.0);
        values.insert(datarefs::ON_GROUND.to_string(), 1.0);
        values.insert(datarefs::SCENERY_LOADING.to_string(), 1.0);
        values.insert(datarefs::IS_REPLAY.to_string(), 1.0);
        values.insert(datarefs::SIM_SPEED.to_string(), 4.0);

        let state = SimState::from_dataref_values(&values);
        assert!(state.paused);
        assert!(state.on_ground);
        assert!(state.scenery_loading);
        assert!(state.replay);
        assert_eq!(state.sim_speed, 4);
    }

    #[test]
    fn frame_rate_is_the_reciprocal_of_the_frame_period() {
        let mut values = HashMap::new();
        values.insert(datarefs::FRAME_RATE_PERIOD.to_string(), 1.0 / 45.0);
        assert_eq!(SimState::from_dataref_values(&values).frame_rate, Some(45));

        // Rounded, not truncated: 1/29.6 is 29.6 fps.
        values.insert(datarefs::FRAME_RATE_PERIOD.to_string(), 1.0 / 29.6);
        assert_eq!(SimState::from_dataref_values(&values).frame_rate, Some(30));
    }

    #[test]
    fn frame_rate_is_unknown_without_a_usable_period() {
        assert_eq!(
            SimState::from_dataref_values(&HashMap::new()).frame_rate,
            None
        );
        for period in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut values = HashMap::new();
            values.insert(datarefs::FRAME_RATE_PERIOD.to_string(), period);
            assert_eq!(
                SimState::from_dataref_values(&values).frame_rate,
                None,
                "period {period}"
            );
        }
    }

    #[test]
    fn test_sim_state_missing_values_default_safe() {
        let values = HashMap::new();
        let state = SimState::from_dataref_values(&values);
        assert!(!state.paused);
        assert!(!state.on_ground);
        assert!(!state.scenery_loading);
        assert!(!state.replay);
        assert_eq!(state.sim_speed, 1);
        assert!(state.should_prefetch());
    }

    #[test]
    fn test_should_prefetch_normal_flight() {
        let flying = SimState {
            paused: false,
            on_ground: false,
            scenery_loading: false,
            replay: false,
            sim_speed: 1,
            frame_rate: None,
        };
        assert!(flying.should_prefetch());
    }

    #[test]
    fn test_should_prefetch_blocked_by_scenery_loading() {
        let loading = SimState {
            scenery_loading: true,
            ..SimState::default()
        };
        assert!(!loading.should_prefetch());
    }

    #[test]
    fn test_should_prefetch_blocked_by_replay() {
        let replay = SimState {
            replay: true,
            ..SimState::default()
        };
        assert!(!replay.should_prefetch());
    }

    #[test]
    fn test_should_prefetch_allowed_when_paused() {
        let paused = SimState {
            paused: true,
            ..SimState::default()
        };
        assert!(paused.should_prefetch());
    }

    #[test]
    fn test_should_prefetch_allowed_on_ground() {
        let ground = SimState {
            on_ground: true,
            ..SimState::default()
        };
        assert!(ground.should_prefetch());
    }

    #[test]
    fn test_default_allows_prefetch() {
        let state = SimState::default();
        assert!(state.should_prefetch());
        assert!(!state.on_ground);
    }
}
