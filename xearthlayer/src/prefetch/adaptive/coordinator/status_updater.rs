//! Shared status updates for TUI display.
//!
//! Extracted from `core.rs` to isolate the logic that maps coordinator
//! internal state to the [`SharedPrefetchStatus`] consumed by the dashboard.

use crate::coord::TileCoord;
use crate::prefetch::state::{DetailedPrefetchStats, SharedPrefetchStatus, StateInputs};

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

/// Update shared status with plan details and submission count.
pub(crate) fn update_status_with_plan(
    shared_status: &SharedPrefetchStatus,
    coordinator_status: &CoordinatorStatus,
    plan: &PrefetchPlan,
    submitted: usize,
    stats: &CycleStats,
) {
    shared_status.update_state_inputs(state_inputs(coordinator_status, submitted > 0));
    shared_status.update_box_extent(coordinator_status.box_extent);
    shared_status.update_box_bounds(coordinator_status.box_bounds);

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
        active_zoom_levels: {
            let mut zooms: Vec<u8> = plan.tiles.iter().map(|t| t.zoom).collect();
            zooms.sort_unstable();
            zooms.dedup();
            zooms
        },
        loading_tiles,
        deferred_cycles: stats.total_deferred_cycles,
    };
    shared_status.update_detailed_stats(detailed);
}

/// Update shared status when no plan was generated.
pub(crate) fn update_status_no_plan(
    shared_status: &SharedPrefetchStatus,
    coordinator_status: &CoordinatorStatus,
    stats: &CycleStats,
) {
    shared_status.update_state_inputs(state_inputs(coordinator_status, false));
    shared_status.update_box_extent(coordinator_status.box_extent);
    shared_status.update_box_bounds(coordinator_status.box_bounds);

    let detailed = DetailedPrefetchStats {
        cycles: stats.total_cycles,
        tiles_submitted_last_cycle: 0,
        tiles_submitted_total: stats.total_tiles_submitted,
        cache_hits: stats.total_cache_hits,
        active_zoom_levels: vec![],
        loading_tiles: vec![],
        deferred_cycles: stats.total_deferred_cycles,
    };
    shared_status.update_detailed_stats(detailed);
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// The coordinator's half of [`StateInputs`]. The demand pause and the lane's
/// occupancy live with the lane gate, so the dashboard adds them from telemetry
/// and resolves once.
fn state_inputs(status: &CoordinatorStatus, lane_busy: bool) -> StateInputs {
    StateInputs {
        enabled: status.enabled,
        demand_paused: false,
        phase: status.phase,
        deferred_last_cycle: status.deferred_last_cycle,
        throttle_fraction: status.throttle_fraction,
        lane_busy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::coordinator::test_support::test_calibration;
    use crate::prefetch::adaptive::FlightPhase;
    use crate::prefetch::state::{resolve_state, PrefetchState};

    fn test_stats(cycles: u64, submitted: u64, hits: u64, deferred: u64) -> CycleStats {
        CycleStats {
            total_cycles: cycles,
            total_tiles_submitted: submitted,
            total_cache_hits: hits,
            total_deferred_cycles: deferred,
        }
    }

    #[test]
    fn test_coordinator_state_reflects_deferral_and_ramp() {
        let mut status = CoordinatorStatus {
            phase: FlightPhase::Cruise,
            ..Default::default()
        };
        assert_eq!(
            resolve_state(state_inputs(&status, false)),
            PrefetchState::Idle
        );
        assert_eq!(
            resolve_state(state_inputs(&status, true)),
            PrefetchState::Active
        );

        status.throttle_fraction = Some(0.25);
        assert_eq!(
            resolve_state(state_inputs(&status, true)),
            PrefetchState::Ramping { percent: 25 }
        );

        status.deferred_last_cycle = true;
        assert_eq!(
            resolve_state(state_inputs(&status, true)),
            PrefetchState::DeferredLoad
        );
    }

    #[test]
    fn test_update_status_no_plan_disabled_when_disabled() {
        let status = SharedPrefetchStatus::new();
        let mut coord_status = CoordinatorStatus::default();
        coord_status.enabled = false;

        update_status_no_plan(&status, &coord_status, &test_stats(0, 0, 0, 0));

        let snapshot = status.snapshot();
        assert_eq!(snapshot.coordinator_state(), PrefetchState::Disabled);
    }

    #[test]
    fn test_update_status_no_plan_carries_cycle_stats() {
        let status = SharedPrefetchStatus::new();
        let mut coord_status = CoordinatorStatus::default();
        coord_status.enabled = true;
        coord_status.deferred_last_cycle = true;

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
            &plan,
            2,
            &test_stats(10, 200, 50, 1),
        );

        let snapshot = status.snapshot();
        assert_eq!(snapshot.coordinator_state(), PrefetchState::Active);
        let detailed = snapshot.detailed_stats.unwrap();
        assert_eq!(detailed.tiles_submitted_last_cycle, 2);
        assert_eq!(detailed.tiles_submitted_total, 200);
        assert!(!detailed.active_zoom_levels.is_empty());
    }
}
