//! Time budget validation for prefetch plans.
//!
//! This module provides functions to validate whether a prefetch plan
//! can complete before X-Plane triggers scenery loading.

use super::super::strategy::PrefetchPlan;

/// Validate that a plan can complete in time.
///
/// Uses position within DSF tile and ground speed to estimate
/// available time before X-Plane triggers scenery loading.
///
/// # Arguments
///
/// * `plan` - The prefetch plan to validate
/// * `position` - Aircraft position (lat, lon) in degrees
/// * `ground_speed_kt` - Ground speed in knots
/// * `time_budget_margin` - Safety margin (0.0-1.0, e.g., 0.7 = 70%)
///
/// # Returns
///
/// `true` if the plan can likely complete before X-Plane triggers,
/// `false` otherwise.
///
/// # Algorithm
///
/// 1. Convert ground speed to degrees per second
/// 2. Calculate distance to X-Plane's trigger point (0.6° from DSF boundary)
/// 3. Compute available time based on speed
/// 4. Compare required time (from plan) against available time with margin
#[allow(dead_code)] // Will be used in future phase
pub fn can_complete_in_time(
    plan: &PrefetchPlan,
    position: (f64, f64),
    ground_speed_kt: f32,
    time_budget_margin: f64,
) -> bool {
    // Convert ground speed to degrees per second
    // 1 knot ≈ 1.852 km/h, 1° ≈ 111 km at equator
    let speed_deg_per_sec = (ground_speed_kt as f64 * 1.852) / (111.0 * 3600.0);

    if speed_deg_per_sec < 0.0001 {
        // Stationary or very slow - no time constraint
        return true;
    }

    // Estimate distance to X-Plane's trigger (0.6° from DSF boundary)
    let (lat, _lon) = position;
    let current_dsf_lat = lat.floor();
    let position_in_dsf = lat - current_dsf_lat;

    // X-Plane triggers at 0.6° into the next DSF
    // So we have: (1.0 - position_in_dsf) + 0.6 = distance to trigger
    let distance_to_trigger = (1.0 - position_in_dsf) + 0.6;
    let time_available_secs = distance_to_trigger / speed_deg_per_sec;

    let time_required_secs = plan.estimated_completion_ms as f64 / 1000.0;

    let can_complete = time_required_secs <= time_available_secs * time_budget_margin;

    tracing::debug!(
        time_required_secs = format!("{:.1}", time_required_secs),
        time_available_secs = format!("{:.1}", time_available_secs),
        margin = format!("{:.0}%", time_budget_margin * 100.0),
        can_complete = can_complete,
        "Time budget check"
    );

    can_complete
}

/// Calculate the time available before X-Plane triggers scenery loading.
///
/// This is useful for determining how many tiles can be prefetched
/// in the available time window.
///
/// # Arguments
///
/// * `position` - Aircraft position (lat, lon) in degrees
/// * `ground_speed_kt` - Ground speed in knots
///
/// # Returns
///
/// Time in seconds until X-Plane triggers, or `None` if stationary.
#[allow(dead_code)] // Will be used in future phase
pub fn time_until_trigger(position: (f64, f64), ground_speed_kt: f32) -> Option<f64> {
    let speed_deg_per_sec = (ground_speed_kt as f64 * 1.852) / (111.0 * 3600.0);

    if speed_deg_per_sec < 0.0001 {
        return None; // Stationary
    }

    let (lat, _lon) = position;
    let current_dsf_lat = lat.floor();
    let position_in_dsf = lat - current_dsf_lat;
    let distance_to_trigger = (1.0 - position_in_dsf) + 0.6;

    Some(distance_to_trigger / speed_deg_per_sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::prefetch::adaptive::calibration::PerformanceCalibration;
    use std::time::Instant;

    fn test_calibration() -> PerformanceCalibration {
        PerformanceCalibration {
            throughput_tiles_per_sec: 25.0,
            avg_tile_generation_ms: 40,
            tile_generation_stddev_ms: 10,
            confidence: 0.9,
            calibrated_at: Instant::now(),
            baseline_throughput: 25.0,
            sample_count: 100,
        }
    }

    #[test]
    fn test_time_budget_stationary() {
        let plan = PrefetchPlan::empty("test");

        // Stationary - should always be OK
        assert!(can_complete_in_time(&plan, (53.5, 9.5), 0.0, 0.7));
    }

    #[test]
    fn test_time_budget_slow_speed() {
        let plan = PrefetchPlan::empty("test");

        // Very slow - should be OK
        assert!(can_complete_in_time(&plan, (53.5, 9.5), 0.001, 0.7));
    }

    #[test]
    fn test_time_budget_fast_flight() {
        let cal = test_calibration();

        // Create a large plan with long completion time
        let mut plan = PrefetchPlan::with_tiles(
            vec![
                TileCoord {
                    row: 100,
                    col: 200,
                    zoom: 14
                };
                100
            ],
            &cal,
            "test",
            0,
            100,
        );
        plan.estimated_completion_ms = 60000; // 60 seconds

        // At 450 knots, time budget is tight
        // This verifies the calculation doesn't panic
        let _result = can_complete_in_time(&plan, (53.1, 9.5), 450.0, 0.7);
    }

    #[test]
    fn test_time_until_trigger_stationary() {
        assert!(time_until_trigger((53.5, 9.5), 0.0).is_none());
    }

    #[test]
    fn test_time_until_trigger_moving() {
        let time = time_until_trigger((53.1, 9.5), 120.0);
        assert!(time.is_some());
        // At 120 knots, should have meaningful time
        assert!(time.unwrap() > 0.0);
    }

    #[test]
    fn test_position_at_dsf_boundary() {
        let plan = PrefetchPlan::empty("test");

        // At DSF boundary (lat = 53.0), maximum time to trigger
        let result = can_complete_in_time(&plan, (53.0, 9.5), 100.0, 0.7);
        assert!(result); // Empty plan should always complete

        // At lat = 53.9, close to next DSF, less time
        let result2 = can_complete_in_time(&plan, (53.9, 9.5), 100.0, 0.7);
        assert!(result2); // Empty plan should still complete
    }
}
