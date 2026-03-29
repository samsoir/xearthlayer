# Prefetch Ordering & Takeoff Ramp-Up Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix prefetch band misalignment (#58) and takeoff stutters (#62) by re-ordering tiles to match X-Plane's DSF column/row loading pattern and adding a graduated ramp-up after takeoff.

**Architecture:** Two isolated additions to the prefetch coordinator — a `BoundaryPrioritizer` that re-sorts tiles by DSF boundary urgency, and a `TransitionThrottle` state machine that gates submission rate after Ground→Cruise transitions. Existing strategies, band geometry, and four-tier filtering are unchanged.

**Tech Stack:** Rust, `TileCoord` coordinate types, `DsfTileCoord` DSF coordinates, `AdaptivePrefetchCoordinator`, `PhaseDetector` flight phase detection

**Design Document:** `docs/plans/2026-02-24-prefetch-ordering-and-ramp-design.md`

---

## Task 1: TransitionThrottle — Tests

Build the `TransitionThrottle` state machine with TDD. This is the simpler component, so we start here.

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/transition_throttle.rs`

**Step 1: Create the file with test scaffolding and minimal types**

Create `xearthlayer/src/prefetch/adaptive/transition_throttle.rs`:

```rust
//! Takeoff transition throttle for prefetch submission rate.
//!
//! After a Ground→Cruise phase transition, prefetch submissions are
//! suppressed during a grace period, then gradually ramped up to full
//! rate. This prevents CPU/network burst during takeoff when X-Plane
//! needs system resources most.
//!
//! # State Machine
//!
//! ```text
//! ┌──────┐  Ground→Cruise  ┌─────────────┐  45s elapsed  ┌───────────┐  30s elapsed  ┌──────┐
//! │ Idle │ ───────────────▶ │ GracePeriod │ ────────────▶ │ RampingUp │ ────────────▶ │ Idle │
//! └──────┘                  └─────────────┘               └───────────┘               └──────┘
//!    ▲                            │                              │
//!    │         Cruise→Ground      │        Cruise→Ground         │
//!    └────────────────────────────┘──────────────────────────────┘
//! ```

use std::time::{Duration, Instant};

use super::phase_detector::FlightPhase;

/// Duration after Ground→Cruise before any prefetch submissions begin.
///
/// 45 seconds covers the takeoff roll and initial climb. By this point
/// the aircraft is typically through initial departure turns and X-Plane
/// rendering load has stabilized.
const GRACE_PERIOD_DURATION: Duration = Duration::from_secs(45);

/// Duration of the linear ramp from initial fraction to full rate.
///
/// Combined with the grace period, full prefetch rate is reached
/// ~75 seconds after takeoff.
const RAMP_DURATION: Duration = Duration::from_secs(30);

/// Starting submission fraction when ramp begins (25%).
const RAMP_START_FRACTION: f64 = 0.25;

/// Transition throttle state.
#[derive(Debug)]
enum ThrottleState {
    /// No throttling active. `fraction()` returns 1.0.
    Idle,
    /// Grace period after Ground→Cruise. `fraction()` returns 0.0.
    GracePeriod { started_at: Instant },
    /// Linear ramp from 0.25 to 1.0. `fraction()` interpolates.
    RampingUp { started_at: Instant },
}

/// Gates prefetch submission rate after Ground→Cruise transitions.
///
/// Call [`on_phase_change`] when the [`PhaseDetector`] reports a transition.
/// Call [`fraction`] each cycle to get the current submission multiplier.
///
/// # Example
///
/// ```ignore
/// let mut throttle = TransitionThrottle::new();
///
/// // Phase detector fires Ground→Cruise
/// throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);
///
/// // During grace period
/// assert_eq!(throttle.fraction(), 0.0);
/// ```
#[derive(Debug)]
pub struct TransitionThrottle {
    state: ThrottleState,
}

impl TransitionThrottle {
    /// Create a new throttle in the idle state.
    pub fn new() -> Self {
        Self {
            state: ThrottleState::Idle,
        }
    }

    /// Notify the throttle of a flight phase change.
    ///
    /// - Ground→Cruise: enters grace period (no submissions for 45s)
    /// - Cruise→Ground: immediately resets to idle
    /// - All other transitions: no effect
    pub fn on_phase_change(&mut self, from: FlightPhase, to: FlightPhase) {
        match (from, to) {
            (FlightPhase::Ground, FlightPhase::Cruise) => {
                self.state = ThrottleState::GracePeriod {
                    started_at: Instant::now(),
                };
                tracing::info!(
                    grace_period_secs = GRACE_PERIOD_DURATION.as_secs(),
                    "Takeoff transition — entering grace period"
                );
            }
            (FlightPhase::Cruise, FlightPhase::Ground) => {
                self.state = ThrottleState::Idle;
                tracing::debug!("Landing transition — throttle reset to idle");
            }
            _ => {} // Same phase, no-op
        }
    }

    /// Get the current submission fraction (0.0 to 1.0).
    ///
    /// The coordinator multiplies its submission count by this value.
    /// Auto-transitions between states based on elapsed time.
    ///
    /// - `Idle`: 1.0
    /// - `GracePeriod`: 0.0 (transitions to `RampingUp` after 45s)
    /// - `RampingUp`: 0.25–1.0 linear (transitions to `Idle` after 30s)
    pub fn fraction(&mut self) -> f64 {
        match self.state {
            ThrottleState::Idle => 1.0,
            ThrottleState::GracePeriod { started_at } => {
                if started_at.elapsed() >= GRACE_PERIOD_DURATION {
                    tracing::info!(
                        ramp_duration_secs = RAMP_DURATION.as_secs(),
                        start_fraction = RAMP_START_FRACTION,
                        "Grace period complete — beginning ramp-up"
                    );
                    self.state = ThrottleState::RampingUp {
                        started_at: Instant::now(),
                    };
                    RAMP_START_FRACTION
                } else {
                    0.0
                }
            }
            ThrottleState::RampingUp { started_at } => {
                let elapsed = started_at.elapsed();
                if elapsed >= RAMP_DURATION {
                    tracing::info!("Ramp-up complete — full prefetch rate");
                    self.state = ThrottleState::Idle;
                    1.0
                } else {
                    let progress = elapsed.as_secs_f64() / RAMP_DURATION.as_secs_f64();
                    RAMP_START_FRACTION + (1.0 - RAMP_START_FRACTION) * progress
                }
            }
        }
    }

    /// Whether the throttle is actively suppressing submissions.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, ThrottleState::Idle)
    }
}

impl Default for TransitionThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_throttle_is_idle() {
        let mut throttle = TransitionThrottle::new();
        assert_eq!(throttle.fraction(), 1.0);
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_ground_to_cruise_enters_grace_period() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);

        assert!(throttle.is_active());
        assert_eq!(throttle.fraction(), 0.0);
    }

    #[test]
    fn test_cruise_to_ground_resets_to_idle() {
        let mut throttle = TransitionThrottle::new();
        // Enter grace period
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);
        assert!(throttle.is_active());

        // Land — reset to idle
        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);
    }

    #[test]
    fn test_same_phase_is_noop() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);

        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Cruise);
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_grace_period_transitions_to_ramp() {
        let mut throttle = TransitionThrottle::new();
        // Manually set grace period with an instant in the past
        throttle.state = ThrottleState::GracePeriod {
            started_at: Instant::now() - GRACE_PERIOD_DURATION - Duration::from_millis(1),
        };

        // fraction() should auto-transition to ramp
        let f = throttle.fraction();
        assert!(
            (f - RAMP_START_FRACTION).abs() < 0.01,
            "Should return ramp start fraction, got {}",
            f
        );
        assert!(throttle.is_active());
    }

    #[test]
    fn test_ramp_completes_to_idle() {
        let mut throttle = TransitionThrottle::new();
        // Set ramp with an instant past the ramp duration
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now() - RAMP_DURATION - Duration::from_millis(1),
        };

        let f = throttle.fraction();
        assert_eq!(f, 1.0, "Should return 1.0 after ramp completes");
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_ramp_fraction_is_linear() {
        let mut throttle = TransitionThrottle::new();

        // Set ramp at exactly halfway through
        let halfway = RAMP_DURATION / 2;
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now() - halfway,
        };

        let f = throttle.fraction();
        // At 50% progress: 0.25 + 0.75 * 0.5 = 0.625
        let expected = RAMP_START_FRACTION + (1.0 - RAMP_START_FRACTION) * 0.5;
        assert!(
            (f - expected).abs() < 0.05,
            "Halfway fraction should be ~{:.2}, got {:.2}",
            expected,
            f
        );
    }

    #[test]
    fn test_ramp_fraction_at_start() {
        let mut throttle = TransitionThrottle::new();
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now(),
        };

        let f = throttle.fraction();
        assert!(
            (f - RAMP_START_FRACTION).abs() < 0.05,
            "Start fraction should be ~{:.2}, got {:.2}",
            RAMP_START_FRACTION,
            f
        );
    }

    #[test]
    fn test_cruise_to_ground_during_ramp_resets() {
        let mut throttle = TransitionThrottle::new();
        // Set ramp partway through
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now() - Duration::from_secs(10),
        };

        // Land while ramping — should reset
        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);
    }

    #[test]
    fn test_default_is_idle() {
        let mut throttle = TransitionThrottle::default();
        assert_eq!(throttle.fraction(), 1.0);
        assert!(!throttle.is_active());
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p xearthlayer transition_throttle -- --nocapture`
Expected: All 9 tests PASS

**Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/transition_throttle.rs
git commit -m "feat(prefetch): add TransitionThrottle state machine (#62)

Suppresses prefetch submissions for 45s after Ground→Cruise transition,
then ramps from 25% to 100% over 30s. Full rate at ~75s after takeoff."
```

---

## Task 2: Register TransitionThrottle module

Wire the new module into the module tree so it compiles and is accessible.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs`

**Step 1: Add module declaration and re-export**

In `xearthlayer/src/prefetch/adaptive/mod.rs`, add after line 80 (`mod turn_detector;`):

```rust
mod transition_throttle;
```

Add to the re-exports (after line 95, the `pub use turn_detector` line):

```rust
pub use transition_throttle::TransitionThrottle;
```

**Step 2: Verify compilation and tests**

Run: `cargo test -p xearthlayer transition_throttle -- --nocapture`
Expected: All 9 tests PASS

Run: `cargo check -p xearthlayer`
Expected: Clean compilation

**Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/mod.rs
git commit -m "refactor(prefetch): register transition_throttle module (#62)"
```

---

## Task 3: BoundaryPrioritizer — Tests

Build the `BoundaryPrioritizer` that re-sorts tiles by DSF boundary urgency.

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/boundary_prioritizer.rs`

**Step 1: Create the file with implementation and tests**

Create `xearthlayer/src/prefetch/adaptive/boundary_prioritizer.rs`:

```rust
//! DSF boundary-aware tile prioritization for cruise prefetch.
//!
//! Re-sorts prefetch tiles so that tiles in DSF columns/rows the aircraft
//! is about to cross are submitted first, matching X-Plane's scenery
//! loading pattern.
//!
//! # Background
//!
//! X-Plane loads scenery in 1°×1° DSF tile batches. When the aircraft
//! approaches a DSF boundary (~2.5° away), it loads the next 2+ columns
//! or rows. The default Euclidean distance sort doesn't account for
//! heading — tiles to the side or behind can rank higher than tiles
//! directly ahead.
//!
//! # Algorithm
//!
//! 1. Decompose aircraft track into latitude/longitude velocity components
//! 2. Determine which DSF boundaries the aircraft is approaching per axis
//! 3. Assign each tile a boundary rank (0 = next boundary, 1 = one after, ...)
//! 4. Sort by rank (primary), then by distance within rank (secondary)

use crate::coord::TileCoord;

/// Minimum velocity component (as cos/sin of track) to consider an axis active.
///
/// Below this threshold, the aircraft is moving nearly parallel to the axis
/// and we don't predict boundary crossings on that axis.
const AXIS_VELOCITY_THRESHOLD: f64 = 0.15; // ~81° from axis

/// Re-sort tiles by DSF boundary urgency.
///
/// Tiles in DSF columns/rows the aircraft is about to cross are prioritized
/// over tiles that are closer by Euclidean distance but not in the aircraft's
/// path.
///
/// # Arguments
///
/// * `position` - Aircraft position (lat, lon) in degrees
/// * `track` - Ground track in degrees (0-360, true north)
/// * `tiles` - Tiles to re-sort (modified in place)
///
/// # Algorithm
///
/// For each active axis (lat/lon based on track decomposition):
/// 1. Compute the next DSF boundary the aircraft will cross
/// 2. Assign each tile a "boundary rank" — how many DSF boundaries ahead
/// 3. Sort by minimum rank across active axes, then by distance
pub fn prioritize(position: (f64, f64), track: f64, tiles: &mut Vec<TileCoord>) {
    if tiles.is_empty() {
        return;
    }

    let (lat, lon) = position;
    let track_rad = track.to_radians();

    // Decompose track into axis velocity components.
    // sin(track) = east/west component (positive = east)
    // cos(track) = north/south component (positive = north)
    let lon_velocity = track_rad.sin(); // positive = eastbound
    let lat_velocity = track_rad.cos(); // positive = northbound

    let lon_active = lon_velocity.abs() > AXIS_VELOCITY_THRESHOLD;
    let lat_active = lat_velocity.abs() > AXIS_VELOCITY_THRESHOLD;

    // If neither axis is active (shouldn't happen with normal tracks),
    // fall back to Euclidean distance sort
    if !lon_active && !lat_active {
        sort_by_distance(tiles, lat, lon);
        return;
    }

    tiles.sort_by(|a, b| {
        let rank_a = tile_boundary_rank(a, lat, lon, lat_velocity, lon_velocity, lat_active, lon_active);
        let rank_b = tile_boundary_rank(b, lat, lon, lat_velocity, lon_velocity, lat_active, lon_active);

        rank_a
            .partial_cmp(&rank_b)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                // Secondary sort: distance within same rank
                let (lat_a, lon_a) = a.to_lat_lon();
                let (lat_b, lon_b) = b.to_lat_lon();
                let dist_a = (lat_a - lat).powi(2) + (lon_a - lon).powi(2);
                let dist_b = (lat_b - lat).powi(2) + (lon_b - lon).powi(2);
                dist_a
                    .partial_cmp(&dist_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}

/// Calculate boundary rank for a single tile.
///
/// Returns a rank value where lower = more urgent:
/// - Rank 0: tile is in the next DSF column/row the aircraft will cross
/// - Rank 1: tile is in the second DSF column/row ahead
/// - Rank N: tile is N boundaries ahead
///
/// For tiles behind the aircraft on an active axis, a high penalty rank
/// is assigned. When both axes are active, the minimum rank across
/// axes is used (a tile that's rank 0 on either axis is urgent).
fn tile_boundary_rank(
    tile: &TileCoord,
    aircraft_lat: f64,
    aircraft_lon: f64,
    lat_velocity: f64,
    lon_velocity: f64,
    lat_active: bool,
    lon_active: bool,
) -> f64 {
    let (tile_lat, tile_lon) = tile.to_lat_lon();
    let tile_dsf_lat = tile_lat.floor();
    let tile_dsf_lon = tile_lon.floor();

    let mut min_rank = f64::MAX;

    if lat_active {
        let rank = axis_rank(aircraft_lat, tile_dsf_lat, lat_velocity);
        min_rank = min_rank.min(rank);
    }

    if lon_active {
        let rank = axis_rank(aircraft_lon, tile_dsf_lon, lon_velocity);
        min_rank = min_rank.min(rank);
    }

    min_rank
}

/// Calculate boundary rank along a single axis.
///
/// `aircraft_pos`: aircraft position on this axis (e.g., latitude)
/// `tile_dsf`: floor of tile position on this axis (DSF column/row start)
/// `velocity`: velocity component (positive = increasing, e.g., northbound/eastbound)
///
/// Returns:
/// - 0.0 for the next DSF column/row ahead
/// - 1.0 for the one after that
/// - 100.0+ for tiles behind the aircraft on this axis (penalty)
fn axis_rank(aircraft_pos: f64, tile_dsf: f64, velocity: f64) -> f64 {
    let aircraft_dsf = aircraft_pos.floor();

    if velocity > 0.0 {
        // Moving in positive direction (north/east)
        // Next boundary is at aircraft_dsf + 1
        // Tiles ahead: tile_dsf >= aircraft_dsf
        if tile_dsf >= aircraft_dsf {
            // How many DSF boundaries ahead? tile_dsf - aircraft_dsf gives 0 for same cell,
            // 1 for next cell, etc. We subtract one to make the NEXT cell rank 0.
            let ahead = tile_dsf - aircraft_dsf;
            if ahead == 0.0 {
                // Same DSF cell as aircraft — rank 0 (aircraft's current cell)
                // but slightly deprioritize vs the next cell
                0.5
            } else {
                ahead - 1.0 // next cell = 0, cell after = 1, ...
            }
        } else {
            // Behind the aircraft
            100.0 + (aircraft_dsf - tile_dsf)
        }
    } else {
        // Moving in negative direction (south/west)
        // Next boundary is at aircraft_dsf (approaching from above)
        // Tiles ahead: tile_dsf <= aircraft_dsf
        if tile_dsf <= aircraft_dsf {
            let ahead = aircraft_dsf - tile_dsf;
            if ahead == 0.0 {
                // Same DSF cell as aircraft
                0.5
            } else {
                ahead - 1.0
            }
        } else {
            // Behind the aircraft
            100.0 + (tile_dsf - aircraft_dsf)
        }
    }
}

/// Fallback: sort tiles by Euclidean distance from aircraft.
fn sort_by_distance(tiles: &mut Vec<TileCoord>, lat: f64, lon: f64) {
    tiles.sort_by(|a, b| {
        let (lat_a, lon_a) = a.to_lat_lon();
        let (lat_b, lon_b) = b.to_lat_lon();
        let dist_a = (lat_a - lat).powi(2) + (lon_a - lon).powi(2);
        let dist_b = (lat_b - lat).powi(2) + (lon_b - lon).powi(2);
        dist_a
            .partial_cmp(&dist_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::to_tile_coords;

    /// Helper: create a TileCoord from lat/lon at zoom 14.
    fn tile_at(lat: f64, lon: f64) -> TileCoord {
        to_tile_coords(lat, lon, 14).expect("valid tile coords")
    }

    /// Helper: get the DSF column (floor of longitude) for a tile.
    fn tile_dsf_lon(tile: &TileCoord) -> i32 {
        let (_, lon) = tile.to_lat_lon();
        lon.floor() as i32
    }

    /// Helper: get the DSF row (floor of latitude) for a tile.
    fn tile_dsf_lat(tile: &TileCoord) -> i32 {
        let (lat, _) = tile.to_lat_lon();
        lat.floor() as i32
    }

    #[test]
    fn test_westbound_prioritizes_next_western_column() {
        // Aircraft at 47.74°N, 10.33°E, heading 270° (due west)
        // Next column west: DSF lon 9 (tiles in [9°, 10°))
        // Current column: DSF lon 10 (tiles in [10°, 11°))
        let position = (47.74, 10.33);
        let track = 270.0;

        let mut tiles = vec![
            tile_at(47.5, 10.5), // Same column as aircraft (DSF lon 10)
            tile_at(47.5, 9.5),  // Next column west (DSF lon 9) — should be first
            tile_at(47.5, 8.5),  // Two columns west (DSF lon 8)
            tile_at(47.5, 11.5), // Behind aircraft (DSF lon 11)
        ];

        prioritize(position, track, &mut tiles);

        // Next column west (lon 9) should come first
        assert_eq!(tile_dsf_lon(&tiles[0]), 9, "First tile should be in next western column");
        // Behind (lon 11) should come last
        assert_eq!(
            tile_dsf_lon(tiles.last().unwrap()),
            11,
            "Last tile should be behind aircraft"
        );
    }

    #[test]
    fn test_northbound_prioritizes_next_northern_row() {
        // Aircraft at 47.3°N, 10.5°E, heading 0° (due north)
        // Next row north: DSF lat 48 (tiles in [48°, 49°))
        let position = (47.3, 10.5);
        let track = 0.0;

        let mut tiles = vec![
            tile_at(47.5, 10.5), // Same row (DSF lat 47)
            tile_at(48.5, 10.5), // Next row north (DSF lat 48) — should be first
            tile_at(49.5, 10.5), // Two rows north (DSF lat 49)
            tile_at(46.5, 10.5), // Behind aircraft (DSF lat 46)
        ];

        prioritize(position, track, &mut tiles);

        assert_eq!(tile_dsf_lat(&tiles[0]), 48, "First tile should be in next northern row");
        assert_eq!(
            tile_dsf_lat(tiles.last().unwrap()),
            46,
            "Last tile should be behind aircraft"
        );
    }

    #[test]
    fn test_diagonal_northwest_considers_both_axes() {
        // Aircraft at 47.7°N, 10.3°E, heading 315° (northwest)
        // Approaching: column 9 (west) AND row 48 (north)
        let position = (47.7, 10.3);
        let track = 315.0;

        let mut tiles = vec![
            tile_at(48.5, 10.5), // Next row north, same column — urgent
            tile_at(47.5, 9.5),  // Same row, next column west — urgent
            tile_at(47.5, 10.5), // Same row, same column — less urgent
            tile_at(46.5, 11.5), // Behind on both axes — lowest priority
        ];

        prioritize(position, track, &mut tiles);

        // The two urgent tiles (rank 0 on at least one axis) should be first
        let first_two_dsf: Vec<(i32, i32)> = tiles[..2]
            .iter()
            .map(|t| (tile_dsf_lat(t), tile_dsf_lon(t)))
            .collect();
        assert!(
            first_two_dsf.contains(&(48, 10)) || first_two_dsf.contains(&(47, 9)),
            "First two tiles should include tiles at next boundary"
        );

        // Behind tile should be last
        let last = tiles.last().unwrap();
        assert_eq!(tile_dsf_lat(last), 46);
        assert_eq!(tile_dsf_lon(last), 11);
    }

    #[test]
    fn test_due_east_only_considers_longitude() {
        // Aircraft at 47.5°N, 10.3°E, heading 90° (due east)
        // cos(90°) ≈ 0 → latitude axis inactive
        // Only longitude axis matters
        let position = (47.5, 10.3);
        let track = 90.0;

        let mut tiles = vec![
            tile_at(47.5, 10.5), // Same column
            tile_at(47.5, 11.5), // Next column east — should be first
            tile_at(47.5, 12.5), // Two columns east
            tile_at(47.5, 9.5),  // Behind (west)
        ];

        prioritize(position, track, &mut tiles);

        assert_eq!(tile_dsf_lon(&tiles[0]), 11, "First tile should be next eastern column");
        assert_eq!(
            tile_dsf_lon(tiles.last().unwrap()),
            9,
            "Last tile should be behind aircraft"
        );
    }

    #[test]
    fn test_empty_tiles_is_noop() {
        let mut tiles: Vec<TileCoord> = vec![];
        prioritize((47.5, 10.5), 270.0, &mut tiles);
        assert!(tiles.is_empty());
    }

    #[test]
    fn test_tiles_within_same_rank_sorted_by_distance() {
        // Two tiles in the same DSF column but different distances
        let position = (47.5, 10.8);
        let track = 270.0; // West

        let near = tile_at(47.5, 9.8); // In next column, near
        let far = tile_at(47.2, 9.2);  // In next column, far

        let mut tiles = vec![far, near]; // Start with far first

        prioritize(position, track, &mut tiles);

        // Both in DSF column 9, but near should come first
        assert_eq!(tile_dsf_lon(&tiles[0]), tile_dsf_lon(&tiles[1]));
        let (lat_0, lon_0) = tiles[0].to_lat_lon();
        let (lat_1, lon_1) = tiles[1].to_lat_lon();
        let dist_0 = (lat_0 - position.0).powi(2) + (lon_0 - position.1).powi(2);
        let dist_1 = (lat_1 - position.0).powi(2) + (lon_1 - position.1).powi(2);
        assert!(dist_0 <= dist_1, "Nearer tile should come first within same rank");
    }

    #[test]
    fn test_axis_rank_positive_direction() {
        // Moving north (positive lat velocity)
        // Aircraft at 47.3 → DSF 47
        // Tile in DSF 48 → next cell ahead → rank 0
        // Tile in DSF 49 → two cells ahead → rank 1
        // Tile in DSF 47 → same cell → rank 0.5
        // Tile in DSF 46 → behind → 100+
        assert_eq!(axis_rank(47.3, 48.0, 1.0), 0.0);
        assert_eq!(axis_rank(47.3, 49.0, 1.0), 1.0);
        assert_eq!(axis_rank(47.3, 47.0, 1.0), 0.5);
        assert!(axis_rank(47.3, 46.0, 1.0) >= 100.0);
    }

    #[test]
    fn test_axis_rank_negative_direction() {
        // Moving west (negative lon velocity)
        // Aircraft at 10.3 → DSF 10
        // Tile in DSF 9 → next cell ahead (west) → rank 0
        // Tile in DSF 8 → two cells ahead → rank 1
        // Tile in DSF 10 → same cell → rank 0.5
        // Tile in DSF 11 → behind → 100+
        assert_eq!(axis_rank(10.3, 9.0, -1.0), 0.0);
        assert_eq!(axis_rank(10.3, 8.0, -1.0), 1.0);
        assert_eq!(axis_rank(10.3, 10.0, -1.0), 0.5);
        assert!(axis_rank(10.3, 11.0, -1.0) >= 100.0);
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p xearthlayer boundary_prioritizer -- --nocapture`
Expected: All 10 tests PASS

**Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/boundary_prioritizer.rs
git commit -m "feat(prefetch): add BoundaryPrioritizer for DSF-aware tile ordering (#58)

Re-sorts prefetch tiles by DSF boundary rank so tiles in the next
column/row the aircraft will cross are submitted first. Uses track
decomposition to determine which axes are active."
```

---

## Task 4: Register BoundaryPrioritizer module

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs`

**Step 1: Add module declaration and re-export**

In `xearthlayer/src/prefetch/adaptive/mod.rs`, add after the `mod band_calculator;` line:

```rust
mod boundary_prioritizer;
```

Add to the re-exports (after the `pub use band_calculator` line):

```rust
pub use boundary_prioritizer::prioritize as prioritize_by_boundary;
```

**Step 2: Verify compilation and tests**

Run: `cargo test -p xearthlayer boundary_prioritizer -- --nocapture`
Expected: All 10 tests PASS

Run: `cargo check -p xearthlayer`
Expected: Clean compilation

**Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/mod.rs
git commit -m "refactor(prefetch): register boundary_prioritizer module (#58)"
```

---

## Task 5: Integrate TransitionThrottle into coordinator

Wire the `TransitionThrottle` into the coordinator's phase change detection and submission logic.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`

**Step 1: Add TransitionThrottle to the coordinator**

In the imports at the top of `core.rs`, add:

```rust
use super::super::transition_throttle::TransitionThrottle;
```

In the `AdaptivePrefetchCoordinator` struct definition, add after the `turn_detector` field:

```rust
    /// Transition throttle for takeoff ramp-up.
    transition_throttle: TransitionThrottle,
```

In the `new()` constructor, add after `let turn_detector = TurnDetector::new(&config);`:

```rust
        let transition_throttle = TransitionThrottle::new();
```

And add `transition_throttle,` to the struct literal (after `turn_detector,`).

**Step 2: Notify throttle on phase changes in `update()`**

In the `update()` method, after the `self.phase_detector.update(ground_speed_kt, agl_ft);` line (currently around line 318), detect if the phase changed and notify the throttle:

Find this block:

```rust
        // Update phase detector
        self.phase_detector.update(ground_speed_kt, agl_ft);
        let phase = self.phase_detector.current_phase();
        self.status.phase = phase;
```

Replace with:

```rust
        // Update phase detector and notify transition throttle on phase change
        let previous_phase = self.phase_detector.current_phase();
        let phase_changed = self.phase_detector.update(ground_speed_kt, agl_ft);
        let phase = self.phase_detector.current_phase();
        self.status.phase = phase;

        if phase_changed {
            self.transition_throttle.on_phase_change(previous_phase, phase);
        }
```

**Step 3: Apply throttle fraction in `execute()`**

In the `execute()` method, after the backpressure `max_tiles` calculation (the `let max_tiles = if load > ...` block, ending around line 430), add the transition throttle:

Find this block (the end of the backpressure section):

```rust
        } else {
            plan.tiles.len()
        };
```

Add after it:

```rust
        // Apply transition throttle (takeoff ramp-up)
        let max_tiles = if self.transition_throttle.is_active() {
            let fraction = self.transition_throttle.fraction();
            if fraction == 0.0 {
                tracing::debug!("Transition throttle — grace period, deferring");
                return 0;
            }
            let throttled = ((max_tiles as f64) * fraction).ceil() as usize;
            tracing::debug!(
                fraction = format!("{:.0}%", fraction * 100.0),
                full = max_tiles,
                throttled_to = throttled,
                "Transition throttle — ramping up"
            );
            throttled
        } else {
            max_tiles
        };
```

**Step 4: Run tests**

Run: `cargo test -p xearthlayer coordinator -- --nocapture`
Expected: All existing coordinator tests PASS (the throttle starts in Idle, so `fraction() = 1.0` — no behavior change for existing tests)

Run: `cargo check -p xearthlayer`
Expected: Clean compilation

**Step 5: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs
git commit -m "feat(prefetch): integrate TransitionThrottle into coordinator (#62)

Notifies throttle on phase changes and applies submission fraction
in execute(). Stacks with backpressure reduction."
```

---

## Task 6: Integrate BoundaryPrioritizer into coordinator

Wire the `BoundaryPrioritizer` into the coordinator's `process_telemetry()` method, after the four-tier filter and before submission.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`

**Step 1: Add import**

Add to imports:

```rust
use super::super::boundary_prioritizer;
```

**Step 2: Add prioritization call in `process_telemetry()`**

In `process_telemetry()`, find the section after the disk filter (after `let disk_filtered = patch_skipped + disk_skipped;` around line 678). Before the `let submitted = if plan.is_empty()` block, add:

```rust
        // Re-sort tiles by DSF boundary urgency (Issue #58)
        // Tiles in DSF columns/rows the aircraft is about to cross
        // are prioritized over tiles that are merely close by distance.
        if !plan.tiles.is_empty() {
            boundary_prioritizer::prioritize(position, track, &mut plan.tiles);
        }
```

**Step 3: Run tests**

Run: `cargo test -p xearthlayer coordinator -- --nocapture`
Expected: All existing coordinator tests PASS

Run: `cargo test -p xearthlayer prefetch -- --nocapture`
Expected: All prefetch tests PASS

**Step 4: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs
git commit -m "feat(prefetch): integrate BoundaryPrioritizer into coordinator (#58)

Re-sorts tiles by DSF boundary urgency after four-tier filter,
before submission. Tiles in the next column/row X-Plane will load
are submitted first."
```

---

## Task 7: Add coordinator integration tests

Add tests that verify the throttle and prioritizer are properly integrated.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` (test module)

**Step 1: Add integration tests**

Add these tests to the `#[cfg(test)] mod tests` block at the bottom of `core.rs`:

```rust
    // ─────────────────────────────────────────────────────────────────────────
    // Transition throttle integration tests (#62)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_phase_change_activates_throttle() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Start on ground
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        assert!(!coord.transition_throttle.is_active());

        // Trigger cruise (high speed, wait for hysteresis)
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);

        // Throttle should now be active (grace period)
        assert!(coord.transition_throttle.is_active());
    }

    #[test]
    fn test_throttle_resets_on_landing() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Transition to cruise
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        assert!(coord.transition_throttle.is_active());

        // Transition back to ground
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        assert!(!coord.transition_throttle.is_active());
    }
```

**Step 2: Run tests**

Run: `cargo test -p xearthlayer coordinator -- --nocapture`
Expected: All tests PASS including the two new ones

**Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs
git commit -m "test(prefetch): add coordinator integration tests for throttle (#62)"
```

---

## Task 8: Update documentation

Update the design docs and module docs to reflect the new components.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs` (module structure docs)
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/mod.rs` (coordinator docs)

**Step 1: Update adaptive module docs**

In `xearthlayer/src/prefetch/adaptive/mod.rs`, update the module structure ASCII art (around line 29-44) to include the new modules:

```rust
//! ```text
//! adaptive/
//! ├── mod.rs                  # This file - module exports
//! ├── config.rs               # Configuration types
//! ├── calibration/            # Performance calibration (submodule)
//! │   ├── mod.rs              # Calibration module exports
//! │   ├── types.rs            # StrategyMode, PerformanceCalibration
//! │   ├── observer.rs         # ThroughputObserver trait
//! │   ├── calibrator.rs       # Initial calibration
//! │   └── rolling.rs          # Rolling recalibration
//! ├── strategy.rs             # AdaptivePrefetchStrategy trait
//! ├── band_calculator.rs      # DSF-aligned band geometry
//! ├── boundary_prioritizer.rs # DSF boundary-aware tile ordering (#58)
//! ├── cruise_strategy.rs      # Cruise flight prefetch
//! ├── ground_strategy.rs      # Ground operations prefetch
//! ├── phase_detector.rs       # Ground/cruise detection
//! ├── turn_detector.rs        # Track stability monitoring
//! ├── transition_throttle.rs  # Takeoff ramp-up throttle (#62)
//! └── coordinator.rs          # Central orchestration
//! ```
```

**Step 2: Update coordinator module docs**

In `xearthlayer/src/prefetch/adaptive/coordinator/mod.rs`, update the module doc (around line 8-9) to mention the new components:

```rust
//! - Turn detection for prefetch pausing
//! - Transition throttle for takeoff ramp-up
//! - DSF boundary-aware tile prioritization
//! - Strategy selection and execution
```

**Step 3: Run full test suite**

Run: `make pre-commit`
Expected: All formatting, clippy, and tests pass

**Step 4: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/mod.rs xearthlayer/src/prefetch/adaptive/coordinator/mod.rs
git commit -m "docs(prefetch): update module docs for boundary prioritizer and transition throttle (#58, #62)"
```

---

## Task 9: Final verification

Run the complete verification suite.

**Step 1: Run pre-commit checks**

Run: `make pre-commit`
Expected: All fmt + clippy + tests pass with zero warnings

**Step 2: Review the git log**

Run: `git log --oneline main..HEAD`
Expected: 8 clean commits, each focused on one concern

**Step 3: Transition to finishing-a-development-branch skill**

Use `superpowers:finishing-a-development-branch` to present options for completing the work.

---

## Files Summary

| File | Task | Change |
|------|------|--------|
| `prefetch/adaptive/transition_throttle.rs` | 1 | New: TransitionThrottle state machine |
| `prefetch/adaptive/boundary_prioritizer.rs` | 3 | New: BoundaryPrioritizer sort function |
| `prefetch/adaptive/mod.rs` | 2, 4, 8 | Module registration + doc update |
| `prefetch/adaptive/coordinator/core.rs` | 5, 6, 7 | Wire throttle + prioritizer + integration tests |
| `prefetch/adaptive/coordinator/mod.rs` | 8 | Doc update |
