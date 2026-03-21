//! Shared status updates for TUI display.
//!
//! Extracted from `core.rs` to isolate the logic that maps coordinator
//! internal state to the [`SharedPrefetchStatus`] consumed by the dashboard.

use crate::coord::TileCoord;
use crate::prefetch::state::{DetailedPrefetchStats, SharedPrefetchStatus};

use super::super::calibration::StrategyMode;
use super::super::phase_detector::FlightPhase;
use super::super::strategy::PrefetchPlan;
use super::status::CoordinatorStatus;

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// Aggregate cycle statistics passed to status update functions.
pub(crate) struct CycleStats {
    pub total_cycles: u64,
    pub total_tiles_submitted: u64,
    pub total_cache_hits: u64,
    pub total_deferred_cycles: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public functions
// ─────────────────────────────────────────────────────────────────────────────

/// Update shared status with current position, plan details, and submission count.
pub(crate) fn update_status_with_plan(
    shared_status: &SharedPrefetchStatus,
    coordinator_status: &CoordinatorStatus,
    position: (f64, f64),
    plan: &PrefetchPlan,
    submitted: usize,
    stats: &CycleStats,
) {
    shared_status.update_inferred_position(position.0, position.1);

    let prefetch_mode = phase_to_prefetch_mode(coordinator_status.phase);
    shared_status.update_prefetch_mode(prefetch_mode);

    let circuit_state = None;

    let loading_tiles: Vec<(i32, i32)> = plan
        .tiles
        .iter()
        .take(10)
        .map(|t: &TileCoord| {
            let (lat, lon) = t.to_lat_lon();
            (lat.floor() as i32, lon.floor() as i32)
        })
        .collect();

    let detailed = DetailedPrefetchStats {
        cycles: stats.total_cycles,
        tiles_submitted_last_cycle: submitted as u64,
        tiles_submitted_total: stats.total_tiles_submitted,
        cache_hits: stats.total_cache_hits,
        ttl_skipped: 0,
        active_zoom_levels: {
            let mut zooms: Vec<u8> = plan.tiles.iter().map(|t| t.zoom).collect();
            zooms.sort_unstable();
            zooms.dedup();
            zooms
        },
        is_active: submitted > 0,
        circuit_state,
        loading_tiles,
        deferred_cycles: stats.total_deferred_cycles,
    };
    shared_status.update_detailed_stats(detailed);
}

/// Update shared status with position only.
pub(crate) fn update_status_position(shared_status: &SharedPrefetchStatus, position: (f64, f64)) {
    shared_status.update_inferred_position(position.0, position.1);
}

/// Update shared status when no plan was generated.
pub(crate) fn update_status_no_plan(
    shared_status: &SharedPrefetchStatus,
    coordinator_status: &CoordinatorStatus,
    stats: &CycleStats,
) {
    let prefetch_mode = if !coordinator_status.enabled {
        crate::prefetch::state::PrefetchMode::Idle
    } else if coordinator_status.throttled {
        crate::prefetch::state::PrefetchMode::CircuitOpen
    } else if coordinator_status.mode == StrategyMode::Disabled {
        crate::prefetch::state::PrefetchMode::Idle
    } else {
        phase_to_prefetch_mode(coordinator_status.phase)
    };
    shared_status.update_prefetch_mode(prefetch_mode);

    let circuit_state = None;

    let detailed = DetailedPrefetchStats {
        cycles: stats.total_cycles,
        tiles_submitted_last_cycle: 0,
        tiles_submitted_total: stats.total_tiles_submitted,
        cache_hits: stats.total_cache_hits,
        ttl_skipped: 0,
        active_zoom_levels: vec![],
        is_active: false,
        circuit_state,
        loading_tiles: vec![],
        deferred_cycles: stats.total_deferred_cycles,
    };
    shared_status.update_detailed_stats(detailed);
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn phase_to_prefetch_mode(phase: FlightPhase) -> crate::prefetch::state::PrefetchMode {
    match phase {
        FlightPhase::Ground => crate::prefetch::state::PrefetchMode::Radial,
        FlightPhase::Transition => crate::prefetch::state::PrefetchMode::Idle,
        FlightPhase::Cruise => crate::prefetch::state::PrefetchMode::TileBased,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::coordinator::test_support::test_calibration;

    fn test_stats(cycles: u64, submitted: u64, hits: u64, deferred: u64) -> CycleStats {
        CycleStats {
            total_cycles: cycles,
            total_tiles_submitted: submitted,
            total_cache_hits: hits,
            total_deferred_cycles: deferred,
        }
    }

    #[test]
    fn test_phase_to_prefetch_mode() {
        assert_eq!(
            phase_to_prefetch_mode(FlightPhase::Ground),
            crate::prefetch::state::PrefetchMode::Radial
        );
        assert_eq!(
            phase_to_prefetch_mode(FlightPhase::Transition),
            crate::prefetch::state::PrefetchMode::Idle
        );
        assert_eq!(
            phase_to_prefetch_mode(FlightPhase::Cruise),
            crate::prefetch::state::PrefetchMode::TileBased
        );
    }

    #[test]
    fn test_update_status_position() {
        let status = SharedPrefetchStatus::new();
        update_status_position(&status, (50.0, 8.0));

        let snapshot = status.snapshot();
        assert!(snapshot.aircraft.is_some());
        let ac = snapshot.aircraft.unwrap();
        assert!((ac.latitude - 50.0).abs() < 0.001);
        assert!((ac.longitude - 8.0).abs() < 0.001);
    }

    #[test]
    fn test_update_status_no_plan_idle_when_disabled() {
        let status = SharedPrefetchStatus::new();
        let mut coord_status = CoordinatorStatus::default();
        coord_status.enabled = false;

        update_status_no_plan(&status, &coord_status, &test_stats(0, 0, 0, 0));

        let snapshot = status.snapshot();
        assert_eq!(
            snapshot.prefetch_mode,
            crate::prefetch::state::PrefetchMode::Idle
        );
    }

    #[test]
    fn test_update_status_no_plan_throttled() {
        let status = SharedPrefetchStatus::new();
        let mut coord_status = CoordinatorStatus::default();
        coord_status.enabled = true;
        coord_status.throttled = true;

        update_status_no_plan(&status, &coord_status, &test_stats(5, 100, 50, 2));

        let snapshot = status.snapshot();
        let detailed = snapshot.detailed_stats.unwrap();
        assert_eq!(detailed.cycles, 5);
        assert_eq!(detailed.tiles_submitted_total, 100);
        assert_eq!(detailed.cache_hits, 50);
        assert_eq!(detailed.deferred_cycles, 2);
    }

    #[test]
    fn test_update_status_with_plan_sets_activity() {
        let status = SharedPrefetchStatus::new();
        let mut coord_status = CoordinatorStatus::default();
        coord_status.enabled = true;
        coord_status.phase = FlightPhase::Cruise;

        let cal = test_calibration();
        let tiles = vec![
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            TileCoord {
                row: 101,
                col: 200,
                zoom: 14,
            },
        ];
        let plan = PrefetchPlan::with_tiles(tiles, &cal, "boundary", 0, 2);

        update_status_with_plan(
            &status,
            &coord_status,
            (50.0, 8.0),
            &plan,
            2,
            &test_stats(10, 200, 50, 1),
        );

        let snapshot = status.snapshot();
        assert_eq!(
            snapshot.prefetch_mode,
            crate::prefetch::state::PrefetchMode::TileBased
        );
        let detailed = snapshot.detailed_stats.unwrap();
        assert!(detailed.is_active);
        assert_eq!(detailed.tiles_submitted_last_cycle, 2);
        assert_eq!(detailed.tiles_submitted_total, 200);
        assert!(!detailed.active_zoom_levels.is_empty());
    }
}
