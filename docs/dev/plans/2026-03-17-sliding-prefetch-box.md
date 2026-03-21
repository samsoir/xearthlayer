# Sliding Prefetch Box Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the boundary-monitor approach with a sliding prefetch box that moves with the aircraft, biased 3° ahead / 1° behind per axis based on track heading, eliminating DSF boundary freezes (#86).

**Architecture:** A new `PrefetchBox` computes a heading-biased rectangle in lat/lon space, enumerates the DSF regions it covers, and returns only regions not yet tracked in GeoIndex. The coordinator's cruise phase calls this every tick instead of checking boundary monitors for crossings. All downstream infrastructure (GeoIndex lifecycle, filter pipeline, region maintenance, tile expansion) is preserved unchanged.

**Tech Stack:** Rust, unit tests (cargo test), existing `DsfRegion`, `GeoIndex`, `BoundaryStrategy` lifecycle methods.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `xearthlayer/src/prefetch/adaptive/prefetch_box.rs` | **Create** | `PrefetchBox` — compute heading-biased box, enumerate DSF regions |
| `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` | Modify | Replace cruise phase boundary logic with `PrefetchBox` call |
| `xearthlayer/src/prefetch/adaptive/mod.rs` | Modify | Add `mod prefetch_box`, update re-exports |
| `xearthlayer/src/prefetch/adaptive/config.rs` | Modify | Add `forward_margin`, `behind_margin` config fields |
| `xearthlayer/src/config/defaults.rs` | Modify | Add default constants for margins |

**Not modified:** `boundary_strategy.rs` (lifecycle methods still used), `boundary_monitor.rs` (kept for now, no longer called in cruise), `scenery_window.rs` (retention + rebuild detection still used), `filtering.rs`, `geo_index/`, `plan_executor.rs`.

**Deferred:** Full config pipeline (`config/keys.rs`, `config/parser.rs`, `config/writer.rs`, `service/orchestrator_config.rs`) for `forward_margin`/`behind_margin` INI settings. The defaults work via `..Default::default()` in the config constructors. Config keys will be added after flight testing confirms the margin values. The spec's `lateral_margin` is intentionally omitted — it's computed as `(forward + behind) / 2`.

---

## Task 1: Create `PrefetchBox` with heading-biased region enumeration

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/prefetch_box.rs`

The core new type. Given aircraft position and track heading, computes a biased rectangle and returns the DSF regions within it.

- [ ] **Step 1: Write failing test — due west heading biases lon west**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_due_west_biases_lon_west() {
        let pbox = PrefetchBox::new(3.0, 1.0);
        let regions = pbox.regions(48.0, 15.0, 270.0);

        // Heading 270° (due west):
        //   lon: 3° west (ahead), 1° east (behind) → lon [12, 16]
        //   lat: symmetric 2° each side → lat [46, 50]
        // DSF regions: floor(lat_min)..floor(lat_max), floor(lon_min)..floor(lon_max)
        //   lat: floor(46.0)..floor(50.0) = 46..49 (4 values)
        //   lon: floor(12.0)..floor(16.0) = 12..15 (4 values)
        // Total: 4 × 4 = 16 regions

        assert!(!regions.is_empty());

        // Must include regions west of aircraft (ahead)
        let has_west = regions.iter().any(|r| r.lon == 12);
        assert!(has_west, "Should include lon=12 (3° west of aircraft at 15°)");

        // Must include region east of aircraft (behind, 1°)
        let has_east = regions.iter().any(|r| r.lon == 15);
        assert!(has_east, "Should include lon=15 (behind aircraft)");

        // Should NOT include lon=17 (2° behind — beyond 1° behind margin)
        let has_far_east = regions.iter().any(|r| r.lon == 17);
        assert!(!has_far_east, "Should not include lon=17 (beyond behind margin)");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_due_west_biases_lon_west -- --nocapture`
Expected: FAIL — module/type does not exist.

- [ ] **Step 3: Write failing test — due south biases lat south**

```rust
#[test]
fn test_due_south_biases_lat_south() {
    let pbox = PrefetchBox::new(3.0, 1.0);
    let regions = pbox.regions(48.0, 15.0, 180.0);

    // Heading 180° (due south):
    //   lat: 3° south (ahead), 1° north (behind) → lat [45, 49]
    //   lon: symmetric → at lat 48°, symmetric margin = 2° each → lon [13, 17]

    let has_south = regions.iter().any(|r| r.lat == 45);
    assert!(has_south, "Should include lat=45 (3° south)");

    let has_north = regions.iter().any(|r| r.lat == 48);
    assert!(has_north, "Should include lat=48 (behind)");

    let has_far_north = regions.iter().any(|r| r.lat == 50);
    assert!(!has_far_north, "Should not include lat=50 (beyond behind margin)");
}
```

- [ ] **Step 4: Write failing test — SW heading biases both axes**

```rust
#[test]
fn test_southwest_biases_both_axes() {
    let pbox = PrefetchBox::new(3.0, 1.0);
    let regions = pbox.regions(48.0, 15.0, 225.0);

    // Heading 225° (SW): both components non-zero
    //   sin(225°) = -0.707 → moving west → lon: 3° west, 1° east
    //   cos(225°) = -0.707 → moving south → lat: 3° south, 1° north

    // Must include far SW corner
    let has_sw = regions.iter().any(|r| r.lat == 45 && r.lon == 12);
    assert!(has_sw, "Should include SW corner (45, 12)");

    // Must NOT include far NE (beyond behind on both axes)
    let has_ne = regions.iter().any(|r| r.lat == 50 && r.lon == 17);
    assert!(!has_ne, "Should not include far NE corner");
}
```

- [ ] **Step 5: Write failing test — exact cardinal has symmetric perpendicular axis**

```rust
#[test]
fn test_exact_cardinal_symmetric_perpendicular() {
    let pbox = PrefetchBox::new(3.0, 1.0);

    // Due north (0°): lat biased north, lon symmetric
    let regions = pbox.regions(48.0, 15.0, 0.0);

    // Lat: 1° behind (south), 3° ahead (north) → [47, 51]
    // Lon: symmetric 2° each side → [13, 17]
    let has_far_north = regions.iter().any(|r| r.lat == 50);
    assert!(has_far_north, "Should include lat=50 (3° north ahead)");

    let lat_min = regions.iter().map(|r| r.lat).min().unwrap();
    let lat_max = regions.iter().map(|r| r.lat).max().unwrap();
    assert_eq!(lat_min, 47, "South edge should be 47 (1° behind)");
    assert_eq!(lat_max, 50, "North edge should be 50 (3° ahead)");

    let lon_min = regions.iter().map(|r| r.lon).min().unwrap();
    let lon_max = regions.iter().map(|r| r.lon).max().unwrap();
    assert_eq!(lon_min, 13, "West should be symmetric 2°");
    assert_eq!(lon_max, 16, "East should be symmetric 2°");
}
```

- [ ] **Step 5b: Write failing test — due east symmetric on lat (floating-point safe)**

```rust
#[test]
fn test_due_east_lat_symmetric_despite_float() {
    let pbox = PrefetchBox::new(3.0, 1.0);

    // Due east (90°): cos(90°) ≈ 6.12e-17 in f64, NOT exact zero.
    // The threshold (1e-6) should treat this as zero → symmetric lat.
    let regions = pbox.regions(48.0, 15.0, 90.0);

    let lat_min = regions.iter().map(|r| r.lat).min().unwrap();
    let lat_max = regions.iter().map(|r| r.lat).max().unwrap();
    assert_eq!(lat_min, 46, "Lat should be symmetric 2° south");
    assert_eq!(lat_max, 49, "Lat should be symmetric 2° north");
}
```

- [ ] **Step 5c: Write failing test — southern hemisphere negative lat**

```rust
#[test]
fn test_southern_hemisphere_negative_lat() {
    let pbox = PrefetchBox::new(3.0, 1.0);

    // Sydney area, heading west
    let regions = pbox.regions(-34.0, 151.0, 270.0);

    // Heading 270° (due west):
    //   lon: 3° west (ahead), 1° east → [148, 152]
    //   lat: symmetric → [-36, -32]
    let has_south = regions.iter().any(|r| r.lat == -36);
    assert!(has_south, "Should include lat=-36 (negative floor)");

    let has_west = regions.iter().any(|r| r.lon == 148);
    assert!(has_west, "Should include lon=148 (3° west ahead)");

    // Verify DSF region signs are correct
    assert!(
        regions.iter().all(|r| r.lat < 0),
        "All regions should have negative lat in southern hemisphere"
    );
}
```

- [ ] **Step 6: Write failing test — filters already-tracked regions**

```rust
#[test]
fn test_new_regions_filters_already_tracked() {
    use crate::geo_index::{GeoIndex, PrefetchedRegion};

    let pbox = PrefetchBox::new(3.0, 1.0);
    let geo_index = GeoIndex::new();

    // Mark one region as already in progress
    let tracked = DsfRegion::new(48, 14);
    geo_index.insert::<PrefetchedRegion>(tracked, PrefetchedRegion::in_progress());

    let new = pbox.new_regions(48.0, 15.0, 270.0, &geo_index);

    // Should not include the tracked region
    assert!(!new.contains(&tracked), "Should exclude already-tracked region");

    // Should still include other regions
    assert!(!new.is_empty(), "Should have untracked regions");
}
```

- [ ] **Step 7: Implement `PrefetchBox`**

```rust
//! Sliding prefetch box for cruise-phase tile prefetching.
//!
//! Computes a heading-biased rectangle around the aircraft position,
//! enumerates the DSF regions (1°×1°) it covers, and filters out
//! regions already tracked in the GeoIndex.

use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion};

/// A heading-aware prefetch region around the aircraft.
///
/// The box biases forward in the direction of travel:
/// - Axes with forward motion: `forward_margin` ahead, `behind_margin` behind
/// - Axes with no motion (exact cardinal perpendicular): symmetric at
///   `(forward_margin + behind_margin) / 2` each side
///
/// Any non-zero heading component on an axis triggers the forward bias.
/// Only exact cardinal headings (0°, 90°, 180°, 270°) produce symmetric
/// perpendicular axes.
#[derive(Debug, Clone)]
pub struct PrefetchBox {
    /// Degrees ahead of aircraft in direction of travel per axis.
    forward_margin: f64,
    /// Degrees behind aircraft per axis.
    behind_margin: f64,
}

impl PrefetchBox {
    /// Create a new prefetch box with the given margins.
    pub fn new(forward_margin: f64, behind_margin: f64) -> Self {
        Self {
            forward_margin,
            behind_margin,
        }
    }

    /// Compute all DSF regions within the heading-biased box.
    pub fn regions(&self, lat: f64, lon: f64, track: f64) -> Vec<DsfRegion> {
        let (lat_min, lat_max, lon_min, lon_max) = self.bounds(lat, lon, track);

        let dsf_lat_min = lat_min.floor() as i32;
        let dsf_lat_max = (lat_max - f64::EPSILON).floor() as i32;
        let dsf_lon_min = lon_min.floor() as i32;
        let dsf_lon_max = (lon_max - f64::EPSILON).floor() as i32;

        let mut result = Vec::with_capacity(
            ((dsf_lat_max - dsf_lat_min + 1) * (dsf_lon_max - dsf_lon_min + 1)) as usize,
        );

        for lat_i in dsf_lat_min..=dsf_lat_max {
            for lon_i in dsf_lon_min..=dsf_lon_max {
                result.push(DsfRegion::new(lat_i, lon_i));
            }
        }

        result
    }

    /// Compute DSF regions in the box that are NOT already tracked in GeoIndex.
    ///
    /// Filters out regions with any `PrefetchedRegion` state (InProgress,
    /// Prefetched, or NoCoverage).
    pub fn new_regions(
        &self,
        lat: f64,
        lon: f64,
        track: f64,
        geo_index: &GeoIndex,
    ) -> Vec<DsfRegion> {
        self.regions(lat, lon, track)
            .into_iter()
            .filter(|r| PrefetchedRegion::should_prefetch(geo_index, r))
            .collect()
    }

    /// Compute the geographic bounds of the box.
    ///
    /// Returns `(lat_min, lat_max, lon_min, lon_max)`.
    /// Threshold below which a heading component is treated as zero.
    /// Prevents floating-point noise from biasing the perpendicular axis
    /// on near-cardinal headings (e.g., cos(90°) ≈ 6e-17 in f64).
    const COMPONENT_THRESHOLD: f64 = 1e-6;

    pub fn bounds(&self, lat: f64, lon: f64, track: f64) -> (f64, f64, f64, f64) {
        let track_rad = track.to_radians();
        let lat_component = track_rad.cos(); // positive = north
        let lon_component = track_rad.sin(); // positive = east

        let symmetric = (self.forward_margin + self.behind_margin) / 2.0;

        // Latitude axis
        let (lat_min, lat_max) = if lat_component > Self::COMPONENT_THRESHOLD {
            // Moving north: ahead = north
            (lat - self.behind_margin, lat + self.forward_margin)
        } else if lat_component < -Self::COMPONENT_THRESHOLD {
            // Moving south: ahead = south
            (lat - self.forward_margin, lat + self.behind_margin)
        } else {
            // Near-cardinal east/west: symmetric
            (lat - symmetric, lat + symmetric)
        };

        // Longitude axis
        let (lon_min, lon_max) = if lon_component > Self::COMPONENT_THRESHOLD {
            // Moving east: ahead = east
            (lon - self.behind_margin, lon + self.forward_margin)
        } else if lon_component < -Self::COMPONENT_THRESHOLD {
            // Moving west: ahead = west
            (lon - self.forward_margin, lon + self.behind_margin)
        } else {
            // Near-cardinal north/south: symmetric
            (lon - symmetric, lon + symmetric)
        };

        (lat_min, lat_max, lon_min, lon_max)
    }
}
```

- [ ] **Step 8: Add module to `mod.rs`**

In `xearthlayer/src/prefetch/adaptive/mod.rs`, add:
```rust
mod prefetch_box;
```
after the existing module declarations, and add to re-exports:
```rust
pub use prefetch_box::PrefetchBox;
```

- [ ] **Step 9: Run all tests to verify they pass**

Run: `cargo test -p xearthlayer prefetch_box -- --nocapture`
Expected: All tests PASS.

- [ ] **Step 10: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/prefetch_box.rs \
       xearthlayer/src/prefetch/adaptive/mod.rs
git commit -m "feat(prefetch): add PrefetchBox with heading-biased region enumeration (#86)"
```

---

## Task 2: Add configuration for prefetch box margins

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs`
- Modify: `xearthlayer/src/config/defaults.rs`

Add `forward_margin` and `behind_margin` to `AdaptivePrefetchConfig` with defaults of 3.0 and 1.0.

- [ ] **Step 1: Write failing test — config has margin fields**

```rust
#[test]
fn test_default_config_has_prefetch_box_margins() {
    let config = AdaptivePrefetchConfig::default();
    assert_eq!(config.forward_margin, 3.0);
    assert_eq!(config.behind_margin, 1.0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_default_config_has_prefetch_box_margins -- --nocapture`
Expected: FAIL — fields do not exist.

- [ ] **Step 3: Add defaults constants**

In `config/defaults.rs`:
```rust
/// Default prefetch box forward margin in degrees.
/// Must be >= X-Plane's ~3.5° look-ahead to ensure tiles are ready
/// before X-Plane requests them.
pub const DEFAULT_PREFETCH_FORWARD_MARGIN: f64 = 3.0;

/// Default prefetch box behind margin in degrees.
/// Keeps recently-passed tiles available for X-Plane's trailing edge.
pub const DEFAULT_PREFETCH_BEHIND_MARGIN: f64 = 1.0;
```

- [ ] **Step 4: Add fields to `AdaptivePrefetchConfig`**

In `config.rs`, add to the struct:
```rust
/// Prefetch box forward margin in degrees (ahead in direction of travel).
pub forward_margin: f64,
/// Prefetch box behind margin in degrees (behind aircraft).
pub behind_margin: f64,
```

And in `Default`:
```rust
forward_margin: 3.0,
behind_margin: 1.0,
```

- [ ] **Step 5: Run all tests**

Run: `cargo test -p xearthlayer -- --nocapture 2>&1 | tail -5`
Expected: All tests PASS.

- [ ] **Step 6: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/config.rs \
       xearthlayer/src/config/defaults.rs
git commit -m "feat(prefetch): add forward_margin and behind_margin config (#86)"
```

---

## Task 3: Replace cruise phase with sliding prefetch box

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs:484-605`

This is the critical integration. Replace the boundary-monitor cruise phase with PrefetchBox region enumeration.

**New cruise flow:**
```
1. Compute PrefetchBox regions from (lat, lon, track)
2. Filter to new regions via GeoIndex
3. For each new region: expand to tiles, mark InProgress or NoCoverage
4. Update retention tracking
5. Sort tiles by distance, build plan
```

- [ ] **Step 1: Write failing test — sliding box generates plan without boundary crossings**

```rust
#[test]
fn test_cruise_uses_sliding_box_not_boundary_monitors() {
    use crate::geo_index::GeoIndex;

    let geo_index = Arc::new(GeoIndex::new());

    let config = AdaptivePrefetchConfig {
        mode: PrefetchMode::Aggressive,
        forward_margin: 3.0,
        behind_margin: 1.0,
        ..Default::default()
    };
    let mut coord = AdaptivePrefetchCoordinator::new(config)
        .with_calibration(test_calibration())
        .with_geo_index(geo_index);

    coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

    // Enter cruise — no scene tracker, no boundary monitors needed
    coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));

    // First cruise tick should generate a plan from the sliding box
    let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

    if coord.status.phase == FlightPhase::Cruise {
        assert!(
            plan.is_some(),
            "Sliding box should generate plan on first cruise tick"
        );
        let plan = plan.unwrap();
        assert!(!plan.tiles.is_empty(), "Plan should have tiles");
        assert_eq!(coord.status.active_strategy, "sliding_box");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_cruise_uses_sliding_box -- --nocapture`
Expected: FAIL — still uses boundary monitors, strategy is "boundary".

- [ ] **Step 3: Write failing test — second tick filters already-submitted regions**

```rust
#[test]
fn test_sliding_box_deduplicates_across_ticks() {
    use crate::geo_index::GeoIndex;

    let geo_index = Arc::new(GeoIndex::new());

    let config = AdaptivePrefetchConfig {
        mode: PrefetchMode::Aggressive,
        forward_margin: 3.0,
        behind_margin: 1.0,
        ..Default::default()
    };
    let mut coord = AdaptivePrefetchCoordinator::new(config)
        .with_calibration(test_calibration())
        .with_geo_index(geo_index);

    coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

    // Enter cruise
    coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));

    // First tick — generates plan, marks regions InProgress
    let plan1 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

    // Second tick at same position — all regions already tracked
    let plan2 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

    if coord.status.phase == FlightPhase::Cruise {
        assert!(plan1.is_some(), "First tick should generate plan");
        assert!(
            plan2.is_none(),
            "Second tick at same position should generate no plan (all regions tracked)"
        );
    }
}
```

- [ ] **Step 4: Implement the new cruise phase**

Replace the `FlightPhase::Cruise` arm (lines 484-605) in `coordinator/core.rs`:

```rust
FlightPhase::Cruise => {
    let (lat, lon) = position;

    // Update retained region tracking (drives eviction of stale state)
    if let Some(ref geo_index) = self.geo_index {
        self.scenery_window.update_retention(lat, lon, geo_index);
    }

    // Compute sliding prefetch box regions
    let new_regions = if let Some(ref geo_index) = self.geo_index {
        self.prefetch_box.new_regions(lat, lon, track, geo_index)
    } else {
        self.prefetch_box.regions(lat, lon, track)
    };

    if new_regions.is_empty() {
        tracing::trace!(
            lat = format!("{:.2}", lat),
            lon = format!("{:.2}", lon),
            track = format!("{:.1}", track),
            "Cruise: no new regions in prefetch box"
        );
        return None;
    }

    // Log the box bounds for debugging
    let (box_lat_min, box_lat_max, box_lon_min, box_lon_max) =
        self.prefetch_box.bounds(lat, lon, track);
    tracing::debug!(
        aircraft = format!("{:.4}°, {:.4}°", lat, lon),
        track = format!("{:.1}°", track),
        box_bounds = format!(
            "[{:.1}:{:.1}N, {:.1}:{:.1}E]",
            box_lat_min, box_lat_max, box_lon_min, box_lon_max
        ),
        new_regions = new_regions.len(),
        "Sliding prefetch box: new regions detected"
    );

    // Expand regions to tiles and track state
    let mut all_tiles = Vec::new();
    for region in &new_regions {
        let tiles = self.get_tiles_for_region(region);
        if tiles.is_empty() {
            if let Some(ref geo_index) = self.geo_index {
                self.boundary_strategy
                    .mark_no_coverage(region, geo_index);
            }
        } else {
            if let Some(ref geo_index) = self.geo_index {
                self.boundary_strategy
                    .mark_in_progress(region, geo_index);
            }
            tracing::debug!(
                region_lat = region.lat,
                region_lon = region.lon,
                tiles = tiles.len(),
                "Prefetch target: region queued"
            );
            all_tiles.extend(tiles);
        }
    }

    if all_tiles.is_empty() {
        return None;
    }

    // Keep SceneryWindow centered on aircraft for retention tracking.
    // update_retention() depends on window position for eviction decisions.
    self.scenery_window.center_on_position(lat, lon);

    // Sort tiles by distance from aircraft
    crate::coord::sort_tiles_by_distance(&mut all_tiles, lat, lon);

    let total = all_tiles.len();
    self.status.active_strategy = "sliding_box";
    PrefetchPlan::with_tiles(all_tiles, &calibration, "sliding_box", 0, total)
}
```

- [ ] **Step 5: Add `prefetch_box` field to `AdaptivePrefetchCoordinator`**

In the struct definition, add:
```rust
/// Sliding prefetch box for cruise-phase region detection.
prefetch_box: PrefetchBox,
```

In `new()`, initialize:
```rust
let prefetch_box = PrefetchBox::new(config.forward_margin, config.behind_margin);
```

Add the import:
```rust
use super::super::prefetch_box::PrefetchBox;
```

- [ ] **Step 6: Run the new tests**

Run: `cargo test -p xearthlayer test_cruise_uses_sliding_box test_sliding_box_deduplicates -- --nocapture`
Expected: Both tests PASS.

- [ ] **Step 7: Run full test suite**

Run: `cargo test -p xearthlayer 2>&1 | grep -E "^test result:"`
Expected: All test suites PASS. Some existing boundary-monitor tests may need updating if they test the cruise arm directly — fix any failures.

- [ ] **Step 8: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs
git commit -m "fix(prefetch): replace boundary monitors with sliding prefetch box in cruise (#86)

The cruise phase now uses a heading-biased sliding box instead of
boundary-monitor edge detection. The box moves with the aircraft
every tick, biased 3° ahead / 1° behind per axis, and prefetches
any new DSF regions that enter the box.

This eliminates the timing gap where X-Plane loaded tiles 3.5° ahead
while the boundary monitor only triggered 1° from the window edge."
```

---

## Task 4: Update long-flight and simulation tests

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` (tests)
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs` (tests)

Update existing tests to work with the new cruise flow.

- [ ] **Step 1: Update `test_long_flight_window_follows_aircraft_not_tracker`**

This test no longer needs to check window bounds — the sliding box doesn't use a window. Replace the assertion with checking that prefetch plans are generated at the correct positions:

```rust
#[test]
fn test_long_flight_generates_plans_at_each_position() {
    use crate::geo_index::GeoIndex;

    let geo_index = Arc::new(GeoIndex::new());

    let config = AdaptivePrefetchConfig {
        mode: PrefetchMode::Aggressive,
        forward_margin: 3.0,
        behind_margin: 1.0,
        ..Default::default()
    };
    let mut coord = AdaptivePrefetchCoordinator::new(config)
        .with_calibration(test_calibration())
        .with_geo_index(geo_index);

    coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

    // Enter cruise
    coord.update((50.0, 7.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    coord.update((50.0, 7.0), 270.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));

    let mut plans_generated = 0;

    // Fly 10° west — each new degree should trigger new regions
    for step in 0..10 {
        let lon = 7.0 - step as f64;
        let plan = coord.update((50.0, lon), 270.0, 200.0, 35000.0);
        if plan.is_some() {
            plans_generated += 1;
        }
    }

    assert!(
        plans_generated >= 5,
        "Should generate plans as aircraft crosses new DSF boundaries, got {}",
        plans_generated
    );
}
```

- [ ] **Step 2: Fix any other failing tests**

Run the full suite and fix any tests that relied on boundary-monitor behavior in the cruise arm. Common fixes:
- Tests using `StableBoundsTracker` for boundary crossings → may need `forward_margin`/`behind_margin` in config
- Tests checking `status.active_strategy == "boundary"` → change to `"sliding_box"`

- [ ] **Step 3: Run full test suite**

Run: `make test`
Expected: All tests PASS.

- [ ] **Step 4: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs \
       xearthlayer/src/prefetch/adaptive/scenery_window.rs
git commit -m "test(prefetch): update tests for sliding prefetch box (#86)"
```

---

## Task 5: Documentation updates

**Files:**
- Modify: `docs/dev/adaptive-prefetch-design.md`
- Modify: `docs/configuration.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update design doc**

Replace the "Window Positioning" section with "Sliding Prefetch Box" explaining:
- Box moves with aircraft every tick
- 3° ahead / 1° behind per axis based on heading
- DSF region enumeration, GeoIndex filtering
- No boundary monitors, no trigger distance

- [ ] **Step 2: Update configuration doc**

Add `forward_margin` and `behind_margin` settings. Note that `trigger_distance`, `load_depth_lat`, `load_depth_lon` are no longer used in cruise (kept for backward compatibility).

- [ ] **Step 3: Update CLAUDE.md**

Update the Predictive Tile Caching section to describe the sliding box model.

- [ ] **Step 4: Commit**

```bash
git add docs/dev/adaptive-prefetch-design.md docs/configuration.md CLAUDE.md
git commit -m "docs(prefetch): document sliding prefetch box model (#86)"
```

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Box generates too many tiles per tick on first cruise entry | GeoIndex filtering ensures only new regions are expanded; `max_tiles_per_cycle` caps submission |
| Heading oscillation causes box to shift rapidly | Any non-zero component biases 3/1 — the bias direction may flip but the 3° margin always covers the current heading |
| Southern hemisphere / antimeridian edge cases | DSF regions use integer floor — works correctly for negative lat/lon. Antimeridian (±180°) is a known edge case but rare for scenery |
| `BoundaryStrategy` lifecycle methods still reference "boundary" in logs | Cosmetic only — these methods are generic region lifecycle, not boundary-specific |
| Existing `trigger_distance` in user configs | Ignored in cruise (no longer read by sliding box). Config key preserved for backward compatibility |
