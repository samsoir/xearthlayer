//! Telemetry processing utilities for the adaptive prefetch coordinator.
//!
//! This module provides helper functions for processing aircraft telemetry
//! data, including track extraction and cycle timing.

use std::time::{Duration, Instant};

use crate::prefetch::state::AircraftState;

/// Extract ground track from aircraft state.
///
/// Uses heading as track fallback until track derivation is fully integrated.
/// The design doc notes: "heading can be used as a fallback (less accurate
/// in crosswind conditions)".
///
/// # Future Enhancement
///
/// Track should be derived from successive GPS positions:
/// ```ignore
/// let track = atan2(delta_lon, delta_lat).to_degrees();
/// ```
///
/// This would provide true ground track instead of nose heading,
/// which differs in crosswind conditions.
pub fn extract_track(state: &AircraftState) -> f64 {
    // Use heading as track (fallback)
    state.heading as f64
}

/// Check if enough time has passed since the last cycle.
///
/// Used to rate-limit prefetch cycles to avoid excessive processing
/// when telemetry updates are very frequent.
///
/// # Arguments
///
/// * `last_cycle` - When the last prefetch cycle completed
/// * `min_interval` - Minimum time between cycles
///
/// # Returns
///
/// `true` if enough time has passed for a new cycle.
pub fn should_run_cycle(last_cycle: Instant, min_interval: Duration) -> bool {
    Instant::now().duration_since(last_cycle) >= min_interval
}

/// Check if telemetry is stale (no recent updates).
///
/// # Arguments
///
/// * `last_telemetry` - When telemetry was last received
/// * `stale_threshold` - How long without updates before considering stale
///
/// # Returns
///
/// `true` if telemetry is stale or was never received.
#[allow(dead_code)] // Public utility function for external use
pub fn is_telemetry_stale(last_telemetry: Option<Instant>, stale_threshold: Duration) -> bool {
    match last_telemetry {
        Some(last) => last.elapsed() > stale_threshold,
        None => true, // Never received = stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_track() {
        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0, false);

        let track = extract_track(&state);

        // Track should be heading as f64
        assert!((track - 90.0).abs() < 0.001);
    }

    #[test]
    fn test_extract_track_north() {
        let state = AircraftState::new(53.5, 9.5, 0.0, 250.0, 35000.0, false);
        assert!((extract_track(&state) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_extract_track_south() {
        let state = AircraftState::new(53.5, 9.5, 180.0, 250.0, 35000.0, false);
        assert!((extract_track(&state) - 180.0).abs() < 0.001);
    }

    #[test]
    fn test_should_run_cycle_just_started() {
        let now = Instant::now();
        // Just started - should not run yet
        assert!(!should_run_cycle(now, Duration::from_secs(2)));
    }

    #[test]
    fn test_should_run_cycle_after_interval() {
        let past = Instant::now() - Duration::from_secs(3);
        // After interval - should run
        assert!(should_run_cycle(past, Duration::from_secs(2)));
    }

    #[test]
    fn test_should_run_cycle_exactly_at_interval() {
        let past = Instant::now() - Duration::from_secs(2);
        // At exactly the interval - should run
        assert!(should_run_cycle(past, Duration::from_secs(2)));
    }

    #[test]
    fn test_is_telemetry_stale_none() {
        // Never received telemetry = stale
        assert!(is_telemetry_stale(None, Duration::from_secs(5)));
    }

    #[test]
    fn test_is_telemetry_stale_fresh() {
        // Just received = not stale
        let now = Some(Instant::now());
        assert!(!is_telemetry_stale(now, Duration::from_secs(5)));
    }

    #[test]
    fn test_is_telemetry_stale_old() {
        // Old telemetry = stale
        let old = Some(Instant::now() - Duration::from_secs(10));
        assert!(is_telemetry_stale(old, Duration::from_secs(5)));
    }

    #[test]
    fn test_is_telemetry_stale_just_under_threshold() {
        // Just under the threshold - should NOT be stale
        let just_under = Some(Instant::now() - Duration::from_millis(4900));
        assert!(
            !is_telemetry_stale(just_under, Duration::from_secs(5)),
            "Should not be stale when under threshold"
        );
    }

    #[test]
    fn test_is_telemetry_stale_just_over_threshold() {
        // Just over the threshold - should be stale
        let just_over = Some(Instant::now() - Duration::from_millis(5100));
        assert!(
            is_telemetry_stale(just_over, Duration::from_secs(5)),
            "Should be stale when over threshold"
        );
    }
}
