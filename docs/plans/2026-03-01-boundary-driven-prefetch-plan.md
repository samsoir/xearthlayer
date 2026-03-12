# Boundary-Driven Prefetch Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the dead-reckoning band-based cruise prefetch with a boundary-driven model that mirrors X-Plane's position-only, row/column loading behaviour.

**Architecture:** Three-window model (X-Plane Window / XEL Window / Target Window) with dual boundary monitors (latitude and longitude). SceneryWindow observes the SceneTracker to derive window dimensions and predict boundary crossings. BoundaryStrategy generates row/column tile lists from predictions. GeoIndex extended with RetainedRegion and PrefetchedRegion layers.

**Tech Stack:** Rust, tokio (async), tracing (structured logging), DashMap (concurrent maps via GeoIndex)

**Design Doc:** `docs/dev/adaptive-prefetch-design.md` v2.0

---

## Task 1: GeoIndex — Add RetainedRegion Layer Type

**Files:**
- Modify: `xearthlayer/src/geo_index/layers.rs`
- Test: `xearthlayer/src/geo_index/layers.rs` (inline tests)

**Context:** The `GeoIndex` is a type-keyed spatial database using `TypeId` → `DashMap<DsfRegion, T>`. Currently it only has `PatchCoverage`. We need two new layer types. This task adds the first: `RetainedRegion`, which marks DSF regions the SceneryWindow infers X-Plane holds in memory.

**Step 1: Write the failing test**

In `xearthlayer/src/geo_index/layers.rs`, add to the existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn test_retained_region_is_geo_layer() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);
    index.insert::<RetainedRegion>(region, RetainedRegion);
    assert!(index.contains::<RetainedRegion>(&region));
}

#[test]
fn test_retained_region_removal() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);
    index.insert::<RetainedRegion>(region, RetainedRegion);
    index.remove::<RetainedRegion>(&region);
    assert!(!index.contains::<RetainedRegion>(&region));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer geo_index::layers::tests::test_retained_region -- --nocapture`
Expected: FAIL — `RetainedRegion` not defined

**Step 3: Write minimal implementation**

In `xearthlayer/src/geo_index/layers.rs`, add after `PatchCoverage`:

```rust
/// Marks a DSF region as retained by X-Plane (inferred from SceneTracker
/// observations and the sliding window + buffer model).
///
/// Inserted/removed by `SceneryWindow` each coordinator cycle.
#[derive(Debug, Clone, Copy)]
pub struct RetainedRegion;

impl GeoLayer for RetainedRegion {}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p xearthlayer geo_index::layers::tests::test_retained_region`
Expected: PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/geo_index/layers.rs
git commit -m "feat(geo_index): add RetainedRegion layer type (#58)"
```

---

## Task 2: GeoIndex — Add PrefetchedRegion Layer Type with Four States

**Files:**
- Modify: `xearthlayer/src/geo_index/layers.rs`
- Test: `xearthlayer/src/geo_index/layers.rs` (inline tests)

**Context:** The `PrefetchedRegion` layer tracks the state of each DSF region in the XEL Window. Four states: absent (not in the index), `InProgress`, `Prefetched`, `NoCoverage`. This is the two-phase commit model.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_prefetched_region_in_progress() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
    let state = index.get::<PrefetchedRegion>(&region).unwrap();
    assert!(state.is_in_progress());
    assert!(!state.is_prefetched());
    assert!(!state.is_no_coverage());
}

#[test]
fn test_prefetched_region_confirmed() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
    let state = index.get::<PrefetchedRegion>(&region).unwrap();
    assert!(state.is_prefetched());
}

#[test]
fn test_prefetched_region_no_coverage() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());
    let state = index.get::<PrefetchedRegion>(&region).unwrap();
    assert!(state.is_no_coverage());
}

#[test]
fn test_prefetched_region_staleness_timeout() {
    use std::time::Duration;
    let state = PrefetchedRegion::in_progress();
    // Fresh InProgress should not be stale
    assert!(!state.is_stale(Duration::from_secs(120)));
}

#[test]
fn test_prefetched_region_should_prefetch() {
    let index = GeoIndex::new();
    let region = DsfRegion::new(50, 9);

    // Absent → should prefetch
    assert!(PrefetchedRegion::should_prefetch(&index, &region));

    // InProgress → should NOT prefetch
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
    assert!(!PrefetchedRegion::should_prefetch(&index, &region));

    // Prefetched → should NOT prefetch
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
    assert!(!PrefetchedRegion::should_prefetch(&index, &region));

    // NoCoverage → should NOT prefetch
    index.insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());
    assert!(!PrefetchedRegion::should_prefetch(&index, &region));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer geo_index::layers::tests::test_prefetched_region -- --nocapture`
Expected: FAIL — `PrefetchedRegion` not defined

**Step 3: Write minimal implementation**

```rust
use std::time::Instant;

/// Tracks the prefetch state of a DSF region in the XEL Window.
///
/// Follows a two-phase commit pattern:
/// - `InProgress`: tiles submitted to executor, awaiting cache confirmation
/// - `Prefetched`: all tiles confirmed in cache
/// - `NoCoverage`: no scenery data exists for this region (skip silently)
///
/// Regions not in the index are considered "absent" (eligible for prefetch).
#[derive(Debug, Clone)]
pub struct PrefetchedRegion {
    state: RegionState,
    since: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionState {
    InProgress,
    Prefetched,
    NoCoverage,
}

impl PrefetchedRegion {
    pub fn in_progress() -> Self {
        Self { state: RegionState::InProgress, since: Instant::now() }
    }

    pub fn prefetched() -> Self {
        Self { state: RegionState::Prefetched, since: Instant::now() }
    }

    pub fn no_coverage() -> Self {
        Self { state: RegionState::NoCoverage, since: Instant::now() }
    }

    pub fn is_in_progress(&self) -> bool {
        self.state == RegionState::InProgress
    }

    pub fn is_prefetched(&self) -> bool {
        self.state == RegionState::Prefetched
    }

    pub fn is_no_coverage(&self) -> bool {
        self.state == RegionState::NoCoverage
    }

    /// Returns true if this InProgress region has exceeded the staleness timeout.
    pub fn is_stale(&self, timeout: std::time::Duration) -> bool {
        self.state == RegionState::InProgress && self.since.elapsed() >= timeout
    }

    /// Returns true if the given region should be prefetched.
    /// A region should be prefetched if it is absent from the index
    /// (no entry = never evaluated).
    pub fn should_prefetch(index: &GeoIndex, region: &DsfRegion) -> bool {
        !index.contains::<PrefetchedRegion>(region)
    }
}

impl GeoLayer for PrefetchedRegion {}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer geo_index::layers::tests::test_prefetched_region`
Expected: PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/geo_index/layers.rs
git commit -m "feat(geo_index): add PrefetchedRegion layer with four-state model (#58)"
```

---

## Task 3: GeoIndex — Export New Layer Types

**Files:**
- Modify: `xearthlayer/src/geo_index/mod.rs`

**Context:** Ensure `RetainedRegion`, `PrefetchedRegion`, and `RegionState` are publicly exported from the `geo_index` module so other modules can use them.

**Step 1: Check current exports**

Read `xearthlayer/src/geo_index/mod.rs` and verify what's currently exported. Add the new types to the public API.

**Step 2: Update module exports**

Add to the public exports in `mod.rs`:

```rust
pub use layers::{RetainedRegion, PrefetchedRegion, RegionState};
```

**Step 3: Verify compilation**

Run: `cargo check -p xearthlayer`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add xearthlayer/src/geo_index/mod.rs
git commit -m "feat(geo_index): export RetainedRegion and PrefetchedRegion types (#58)"
```

---

## Task 4: BoundaryMonitor — Core Position vs Edge Logic

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/boundary_monitor.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs`

**Context:** The `BoundaryMonitor` is a simple struct that compares the aircraft's position on one axis to the window edges on that axis. It produces `BoundaryCrossing` predictions when the aircraft is within `trigger_distance` of an edge. Two instances will be created (lat and lon) but they share the same struct. This is the core logic — keep it simple.

**Step 1: Write the failing tests**

Create `xearthlayer/src/prefetch/adaptive/boundary_monitor.rs` with tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_trigger_when_far_from_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(50.0); // middle of window
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_trigger_near_max_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(52.0); // 1.0° from north edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].axis, BoundaryAxis::Latitude);
        assert_eq!(predictions[0].dsf_coord, 53); // next row north
        assert!(predictions[0].urgency > 0.0);
    }

    #[test]
    fn test_trigger_near_min_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(48.0); // 1.0° from south edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].dsf_coord, 46); // next row south
    }

    #[test]
    fn test_urgency_increases_closer_to_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let far = monitor.check(51.8);   // 1.2° from edge
        let close = monitor.check(52.7); // 0.3° from edge
        assert!(close[0].urgency > far[0].urgency);
    }

    #[test]
    fn test_no_trigger_both_edges_far() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Longitude, 3.0, 11.0, 1.5);
        let predictions = monitor.check(7.0); // middle
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_longitude_trigger_east_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Longitude, 3.0, 11.0, 1.5);
        let predictions = monitor.check(10.0); // 1.0° from east edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].axis, BoundaryAxis::Longitude);
        assert_eq!(predictions[0].dsf_coord, 11);
    }

    #[test]
    fn test_default_depth_is_three() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(52.0);
        assert_eq!(predictions[0].depth, 3);
    }

    #[test]
    fn test_update_edges() {
        let mut monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        monitor.update_edges(48.0, 54.0); // window slid north
        // Now 52.8 is 1.2° from north edge (54.0) — should trigger
        let predictions = monitor.check(52.8);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].dsf_coord, 54);
    }

    #[test]
    fn test_trigger_near_both_edges_small_window() {
        // Window only 3° wide — position near both edges
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 49.0, 52.0, 1.5);
        let predictions = monitor.check(50.5); // 1.5° from both edges
        // Should trigger both directions
        assert_eq!(predictions.len(), 2);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer prefetch::adaptive::boundary_monitor::tests -- --nocapture`
Expected: FAIL — module doesn't exist yet

**Step 3: Write minimal implementation**

```rust
//! Position-based boundary monitor for the adaptive prefetch system.
//!
//! Each `BoundaryMonitor` watches one axis (latitude or longitude) and
//! produces `BoundaryCrossing` predictions when the aircraft approaches
//! the edge of the inferred X-Plane scenery window. Two monitors (one
//! per axis) mirror X-Plane's own dual boundary monitor architecture.

/// Which axis a boundary crossing occurs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryAxis {
    /// Latitude boundary — crossing triggers a ROW load.
    Latitude,
    /// Longitude boundary — crossing triggers a COLUMN load.
    Longitude,
}

/// A predicted boundary crossing with urgency.
#[derive(Debug, Clone)]
pub struct BoundaryCrossing {
    /// Which axis is being crossed.
    pub axis: BoundaryAxis,
    /// The DSF coordinate of the first row/column to load.
    pub dsf_coord: i16,
    /// How urgent: 0.0 = just triggered, 1.0 = imminent.
    pub urgency: f64,
    /// How many DSF tiles deep to load (typically 3).
    pub depth: u8,
}

/// Default load depth matching X-Plane's observed 3-deep strip loading.
const DEFAULT_LOAD_DEPTH: u8 = 3;

/// Monitors one axis (latitude or longitude) for boundary crossings.
///
/// Compares the aircraft's position on this axis to the window edges.
/// When the aircraft is within `trigger_distance` of an edge, produces
/// a `BoundaryCrossing` prediction.
#[derive(Debug, Clone)]
pub struct BoundaryMonitor {
    axis: BoundaryAxis,
    window_min: f64,
    window_max: f64,
    trigger_distance: f64,
    load_depth: u8,
}

impl BoundaryMonitor {
    /// Creates a new monitor for the given axis with window edges.
    pub fn new(axis: BoundaryAxis, window_min: f64, window_max: f64, trigger_distance: f64) -> Self {
        Self {
            axis,
            window_min,
            window_max,
            trigger_distance,
            load_depth: DEFAULT_LOAD_DEPTH,
        }
    }

    /// Creates a new monitor with custom load depth.
    pub fn with_load_depth(mut self, depth: u8) -> Self {
        self.load_depth = depth;
        self
    }

    /// Update the window edges (called when window slides or is re-derived).
    pub fn update_edges(&mut self, new_min: f64, new_max: f64) {
        self.window_min = new_min;
        self.window_max = new_max;
    }

    /// Check the aircraft position against window edges.
    ///
    /// Returns boundary crossing predictions for any edge within
    /// `trigger_distance`. May return 0, 1, or 2 predictions
    /// (both edges if the window is small).
    pub fn check(&self, position: f64) -> Vec<BoundaryCrossing> {
        let mut predictions = Vec::new();

        let dist_to_max = self.window_max - position;
        let dist_to_min = position - self.window_min;

        // Check max edge (north or east)
        if dist_to_max < self.trigger_distance && dist_to_max >= 0.0 {
            let urgency = 1.0 - (dist_to_max / self.trigger_distance);
            let dsf_coord = self.window_max as i16;
            predictions.push(BoundaryCrossing {
                axis: self.axis,
                dsf_coord,
                urgency: urgency.clamp(0.0, 1.0),
                depth: self.load_depth,
            });
        }

        // Check min edge (south or west)
        if dist_to_min < self.trigger_distance && dist_to_min >= 0.0 {
            let urgency = 1.0 - (dist_to_min / self.trigger_distance);
            let dsf_coord = (self.window_min as i16) - 1;
            predictions.push(BoundaryCrossing {
                axis: self.axis,
                dsf_coord,
                urgency: urgency.clamp(0.0, 1.0),
                depth: self.load_depth,
            });
        }

        predictions
    }

    /// Current window minimum edge.
    pub fn window_min(&self) -> f64 {
        self.window_min
    }

    /// Current window maximum edge.
    pub fn window_max(&self) -> f64 {
        self.window_max
    }
}
```

Also add to `xearthlayer/src/prefetch/adaptive/mod.rs`:

```rust
pub mod boundary_monitor;
```

And add to the public exports:

```rust
pub use boundary_monitor::{BoundaryMonitor, BoundaryAxis, BoundaryCrossing};
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer prefetch::adaptive::boundary_monitor::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/boundary_monitor.rs xearthlayer/src/prefetch/adaptive/mod.rs
git commit -m "feat(prefetch): add BoundaryMonitor with position-based edge detection (#58)"
```

---

## Task 5: SceneryWindow — State Machine and Window Derivation

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/scenery_window.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs`

**Context:** The `SceneryWindow` is the central computational model. It has a state machine (Uninitialized → Assumed → Measuring → Ready), derives window dimensions from the SceneTracker, manages dual BoundaryMonitors, and maintains retention inference via GeoIndex. This task implements the state machine and window derivation. Retention inference and monitor integration come in subsequent tasks.

**Important:** The `SceneryWindow` depends on the `SceneTracker` trait. For testing, you'll need to create a mock. Check how the existing `SceneTracker` trait is defined in `xearthlayer/src/scene_tracker/` — it exposes `loaded_bounds()`, `loaded_regions()`, `requested_tiles()`, `is_burst_active()`, etc.

**Step 1: Write the failing tests**

The tests should cover:
- `new()` starts in `Uninitialized`
- Transitions to `Assumed` when `set_assumed_dimensions()` is called
- Transitions from `Uninitialized` to `Measuring` when bounds are first available
- Transitions from `Measuring` to `Ready` when bounds are stable (same dimensions for 2 consecutive checks)
- `window_size()` returns `None` before ready, `Some(rows, cols)` after
- `is_ready()` reflects the state

Use a mock SceneTracker that lets you control `loaded_bounds()` return values. The existing codebase uses `Arc<dyn SceneTracker>` — check the trait definition in `scene_tracker/mod.rs` for the exact signature.

**Step 2: Implement the state machine**

```rust
pub enum WindowState {
    Uninitialized,
    Assumed,
    Measuring { last_bounds: (usize, usize), stable_checks: u8 },
    Ready,
}

pub struct SceneryWindow {
    state: WindowState,
    window_size: Option<(usize, usize)>,
    buffer: u8,
    default_rows: usize,
    default_cols: usize,
    lat_monitor: Option<BoundaryMonitor>,
    lon_monitor: Option<BoundaryMonitor>,
    trigger_distance: f64,
    load_depth: u8,
}
```

Key methods:
- `new(config) -> Self` — starts Uninitialized
- `set_assumed_dimensions(rows, cols)` — for ocean/sparse starts
- `update_window_from_bounds(bounds: &GeoBounds)` — drives state transitions
- `window_size() -> Option<(usize, usize)>`
- `is_ready() -> bool`
- `state() -> &WindowState`

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add SceneryWindow state machine and window derivation (#58)"
```

---

## Task 6: SceneryWindow — Boundary Monitor Integration

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs`

**Context:** Wire the dual BoundaryMonitors into the SceneryWindow. When the window enters `Ready` (or `Assumed`) state, initialise the lat and lon monitors with the derived (or default) window edges. The `update()` method should call both monitors and return combined predictions.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_update_returns_predictions_when_near_edge() {
    // Setup: window derived (Ready state), aircraft near north edge
    // Call update(position) → should return lat crossing prediction
}

#[test]
fn test_update_returns_empty_when_far_from_edges() {
    // Aircraft in middle of window → no predictions
}

#[test]
fn test_diagonal_both_monitors_trigger() {
    // Aircraft near both north and east edges → both predictions returned
}

#[test]
fn test_predictions_sorted_by_urgency() {
    // Near east edge (urgency 0.8) and near north edge (urgency 0.4)
    // East prediction should come first
}

#[test]
fn test_no_predictions_before_ready() {
    // Window in Uninitialized state → update returns empty
}
```

**Step 2: Implement**

Add `update(position: (f64, f64)) -> Vec<BoundaryCrossing>` that:
1. If not ready: return empty
2. Call `lat_monitor.check(position.0)`
3. Call `lon_monitor.check(position.1)`
4. Combine and sort by urgency descending (most urgent first)
5. Return

Also add `refine_with_track(predictions: &mut Vec<BoundaryCrossing>, track: f64)` for optional track enrichment — filters predictions to only the direction the aircraft is moving.

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): integrate boundary monitors into SceneryWindow (#58)"
```

---

## Task 7: SceneryWindow — Retention Inference

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs`

**Context:** Each cycle, the SceneryWindow computes which DSF regions X-Plane likely retains in memory. Regions inside `window + buffer` centered on the aircraft are retained. Regions that were retained but are now outside are evicted. All changes are written to the GeoIndex `RetainedRegion` layer. All eviction/retention decisions logged at DEBUG.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_retention_adds_regions_within_window_plus_buffer() {
    // Window 6×8, buffer 1, aircraft at (50.5, 7.5)
    // Retained area: lat 46..57, lon 2..14 (window + buffer each side)
    // Check GeoIndex has RetainedRegion for regions in range
}

#[test]
fn test_retention_evicts_regions_outside_buffer() {
    // Insert RetainedRegion at (40, 0) — far from aircraft
    // Call update_retention() — should remove it
}

#[test]
fn test_retention_preserves_regions_inside_buffer() {
    // Insert RetainedRegion at edge of buffer — should NOT be evicted
}
```

**Step 2: Implement**

Add `update_retention(position: (f64, f64), geo_index: &GeoIndex)` that:
1. Calculate retained area: window centered on aircraft position ± buffer
2. Add `RetainedRegion` entries for all DSF regions in retained area
3. Scan existing `RetainedRegion` entries — remove any outside the retained area
4. Log each add/remove at `tracing::debug!`

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add retention inference to SceneryWindow (#58)"
```

---

## Task 8: SceneryWindow — World Rebuild Detection

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs`

**Context:** Detect when X-Plane is rebuilding the world (teleport, rendering change) vs. extending the window (normal flight). Reset to `Measuring` state on rebuild.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_world_rebuild_detected_when_burst_covers_most_of_window() {
    // Window 6×8, burst covers >50% of window area and centered on aircraft
    // Should reset to Measuring
}

#[test]
fn test_normal_extension_not_detected_as_rebuild() {
    // Burst adds a single row at the edge — NOT a rebuild
}
```

**Step 2: Implement**

Add `check_for_rebuild(burst_bounds: &GeoBounds, aircraft: (f64, f64)) -> bool` that:
1. Compare burst area to window area (>50% coverage)
2. Check if burst is centered on aircraft (not at an edge)
3. If rebuild: clear GeoIndex layers, reset state, return true

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add world rebuild detection to SceneryWindow (#58)"
```

---

## Task 9: BoundaryStrategy — Row and Column Generation

**Files:**
- Create: `xearthlayer/src/prefetch/adaptive/boundary_strategy.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs`

**Context:** The `BoundaryStrategy` replaces `CruiseStrategy`. It takes `BoundaryCrossing` predictions from `SceneryWindow` and generates concrete DDS tile lists. This task implements the core row/column DSF region generation and the set difference with the XEL Window.

**Important:** The `BoundaryStrategy` implements `AdaptivePrefetchStrategy` (defined in `strategy.rs`). Check the trait signature — it currently takes `(position, track, calibration, already_cached)`. We will need to modify the trait or add a new method. Prefer adding a new method specific to boundary-driven prefetch since the trait is also used by `GroundStrategy`.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_row_generation_for_lat_crossing() {
    // BoundaryCrossing: axis=Latitude, dsf_coord=50, depth=3
    // Window width: lon 3..10 (8 columns)
    // Should generate DSF regions:
    //   depth 0: [(50,3), (50,4), ..., (50,10)]
    //   depth 1: [(51,3), (51,4), ..., (51,10)]
    //   depth 2: [(52,3), (52,4), ..., (52,10)]
    // Total: 24 regions
}

#[test]
fn test_column_generation_for_lon_crossing() {
    // BoundaryCrossing: axis=Longitude, dsf_coord=11, depth=3
    // Window height: lat 47..52 (6 rows)
    // Should generate 18 DSF regions
}

#[test]
fn test_set_difference_excludes_prefetched_regions() {
    // Put some regions as Prefetched in GeoIndex
    // Generate row → those regions should be excluded
}

#[test]
fn test_set_difference_excludes_in_progress_regions() {
    // InProgress regions should also be excluded
}

#[test]
fn test_set_difference_excludes_no_coverage_regions() {
    // NoCoverage regions excluded
}

#[test]
fn test_absent_regions_included_in_diff() {
    // Regions NOT in GeoIndex should be included
}

#[test]
fn test_depth_ordering_in_output() {
    // depth 0 tiles should come before depth 1, before depth 2
}

#[test]
fn test_southbound_row_generation() {
    // dsf_coord = 46 (south of aircraft), depth=3
    // Should generate rows: 46, 45, 44 (going south)
}
```

**Step 2: Implement**

Key struct:

```rust
pub struct BoundaryStrategy {
    // Dependencies injected at construction
}

impl BoundaryStrategy {
    pub fn generate_target_regions(
        &self,
        predictions: &[BoundaryCrossing],
        window_size: (usize, usize),  // (rows, cols)
        window_center: (f64, f64),     // aircraft position
        geo_index: &GeoIndex,
    ) -> Vec<TargetDsfRegion>;
}

pub struct TargetDsfRegion {
    pub region: DsfRegion,
    pub depth: u8,      // 0 = most urgent
    pub axis: BoundaryAxis,
    pub urgency: f64,   // from the BoundaryCrossing
}
```

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add BoundaryStrategy with row/column generation (#58)"
```

---

## Task 10: BoundaryStrategy — DDS Tile Expansion and Filtering

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/boundary_strategy.rs`

**Context:** Expand DSF regions from Task 9 into concrete DDS tiles. Check SceneryIndex for coverage (mark NoCoverage if absent). Apply the four-tier filter. Produce the final `PrefetchPlan`.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_expand_dsf_to_dds_tiles() {
    // Given a DSF region with SceneryIndex data → returns DDS tiles
}

#[test]
fn test_no_coverage_marked_when_scenery_index_empty() {
    // DSF region with no SceneryIndex entries → mark NoCoverage in GeoIndex
}

#[test]
fn test_four_tier_filter_removes_cached_tiles() {
    // Tiles in already_cached set → filtered out
}

#[test]
fn test_produces_prefetch_plan() {
    // Full pipeline: predictions → regions → tiles → plan
}

#[test]
fn test_regions_marked_in_progress_after_plan() {
    // After plan generation, submitted regions should be InProgress in GeoIndex
}
```

**Step 2: Implement**

Add a `calculate_prefetch_from_predictions()` method that:
1. Call `generate_target_regions()` from Task 9
2. For each region: check SceneryIndex or use fallback tile generation
3. Mark NoCoverage regions in GeoIndex
4. Filter tiles through `already_cached` set
5. Order by (depth, distance)
6. Return `PrefetchPlan`
7. Mark submitted regions as `InProgress` in GeoIndex

Reuse the `get_dds_tiles_in_dsf()` pattern from the existing `CruiseStrategy` — it queries SceneryIndex and falls back to a 4×4 grid.

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add DDS tile expansion and filtering to BoundaryStrategy (#58)"
```

---

## Task 11: Configuration — Add New Settings, Deprecate Old

**Files:**
- Modify: `xearthlayer/src/config/keys.rs`
- Modify: `xearthlayer/src/config/upgrade.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs`

**Context:** Add 6 new config keys (`trigger_distance`, `load_depth`, `window_buffer`, `stale_region_timeout`, `default_window_rows`, `default_window_cols`). Deprecate 7 old keys (`trigger_position`, `lead_distance`, `band_width`, `track_stability_threshold`, `turn_threshold`, `track_stability_duration`, `time_budget_margin`). Update `AdaptivePrefetchConfig` to use new fields.

**Step 1: Write the failing tests**

```rust
// In config/keys.rs tests:
#[test]
fn test_trigger_distance_config_key() {
    let key = ConfigKey::from_str("prefetch.trigger_distance").unwrap();
    assert!(key.validate("1.5").is_ok());
    assert!(key.validate("0.3").is_err()); // below 0.5 minimum
    assert!(key.validate("5.0").is_err()); // above 3.0 maximum
}

// Similar tests for load_depth, window_buffer, stale_region_timeout,
// default_window_rows, default_window_cols

// In config/upgrade.rs tests:
#[test]
fn test_deprecated_prefetch_keys() {
    assert!(DEPRECATED_KEYS.contains(&"prefetch.trigger_position"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.lead_distance"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.band_width"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.track_stability_threshold"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.turn_threshold"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.track_stability_duration"));
    assert!(DEPRECATED_KEYS.contains(&"prefetch.time_budget_margin"));
}
```

**Step 2: Implement**

1. Add new `ConfigKey` variants with validation ranges
2. Add deprecated keys to `DEPRECATED_KEYS` array
3. Update `AdaptivePrefetchConfig`:
   - Add: `trigger_distance: f64` (default 1.5), `load_depth: u8` (default 3), `window_buffer: u8` (default 1), `stale_region_timeout: Duration` (default 120s), `default_window_rows: usize` (default 6), `default_window_cols: usize` (default 8)
   - Remove fields for deprecated settings (but keep them in the struct for now — remove in Task 14 when coordinator is rewired)

**Step 3: Run tests, verify pass**

**Step 4: Run `cargo test -p xearthlayer config` to verify no regressions**

**Step 5: Commit**

```bash
git commit -m "feat(config): add boundary-driven prefetch settings, deprecate band settings (#58)"
```

---

## Task 12: Coordinator — Wire SceneryWindow and BoundaryStrategy

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`

**Context:** This is the main integration task. The coordinator needs to:
1. Own a `SceneryWindow` instance
2. Call `scenery_window.update()` each cycle to get predictions
3. When in Cruise phase, use `BoundaryStrategy` instead of `CruiseStrategy`
4. Remove the `TurnDetector` dependency (monitors handle turns naturally)
5. Keep `PhaseDetector`, `TransitionThrottle`, `GroundStrategy` unchanged

**Important:** The coordinator struct has many fields and builder methods. Read the full struct definition before modifying. The key change is in the `update()` method: replace the cruise strategy call with SceneryWindow + BoundaryStrategy.

**Step 1: Write the failing tests**

Add tests in the coordinator test module (check existing tests in `core.rs`):

```rust
#[test]
fn test_coordinator_with_scenery_window_returns_plan_on_boundary() {
    // Configure coordinator with mock SceneTracker showing a ready window
    // Position aircraft near the window edge
    // Call update() → should return a PrefetchPlan with boundary tiles
}

#[test]
fn test_coordinator_no_plan_when_far_from_boundary() {
    // Aircraft in middle of window → update() returns None
}

#[test]
fn test_coordinator_handles_turn_without_pause() {
    // Change track from north to east between updates
    // Should produce predictions on the new axis without delay
}
```

**Step 2: Implement**

1. Add `scenery_window: SceneryWindow` field to coordinator
2. Add `boundary_strategy: BoundaryStrategy` field
3. Add `with_scene_tracker(tracker: Arc<dyn SceneTracker>) -> Self` builder
4. In `update()`:
   - Keep PhaseDetector update
   - Keep TransitionThrottle notifications
   - Remove TurnDetector update and `is_stable()` check
   - When Cruise: call `scenery_window.update(position)` for predictions, then `boundary_strategy.calculate_prefetch_from_predictions()`
   - When Ground: keep GroundStrategy (unchanged)
   - When Transition: return None (unchanged)
5. In `execute()`: keep unchanged (backpressure, throttle, submission loop all stay)

**Step 3: Run ALL coordinator tests**

Run: `cargo test -p xearthlayer prefetch::adaptive::coordinator`
Expected: Existing tests may fail due to removed TurnDetector — update or remove those tests.

**Step 4: Fix broken tests**

- Remove tests that relied on TurnDetector blocking prefetch during turns
- Update tests that expect CruiseStrategy to use BoundaryStrategy instead
- Keep all tests for PhaseDetector, TransitionThrottle, backpressure, execute()

**Step 5: Commit**

```bash
git commit -m "feat(prefetch): integrate SceneryWindow and BoundaryStrategy into coordinator (#58)"
```

---

## Task 13: Service Wiring — Pass SceneTracker to Coordinator

**Files:**
- Modify: `xearthlayer/src/service/facade.rs` or wherever the coordinator is built
- Modify: `xearthlayer/src/runtime/orchestrator.rs` if coordinator is built there

**Context:** The coordinator now needs a `SceneTracker` reference. The `SceneTracker` is owned by `MountManager`. Wire it into the coordinator builder chain.

**Step 1: Find where the coordinator is built**

Search for `AdaptivePrefetchCoordinator::with_defaults()` or `AdaptivePrefetchCoordinator::new(` in the service/runtime code.

**Step 2: Add `.with_scene_tracker(scene_tracker)` to the builder chain**

The `MountManager` has `scene_tracker()` accessor. Pass it through.

**Step 3: Verify compilation**

Run: `cargo check -p xearthlayer`
Expected: Compiles

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): wire SceneTracker into coordinator via service layer (#58)"
```

---

## Task 14: Remove Deprecated Components

**Files:**
- Delete: `xearthlayer/src/prefetch/adaptive/cruise_strategy.rs`
- Delete: `xearthlayer/src/prefetch/adaptive/band_calculator.rs`
- Delete: `xearthlayer/src/prefetch/adaptive/boundary_prioritizer.rs`
- Delete: `xearthlayer/src/prefetch/adaptive/turn_detector.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/mod.rs` (remove module declarations + exports)
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` (remove fields + imports)
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs` (remove deprecated fields from struct)

**Context:** Now that the boundary-driven model is integrated, remove the old components cleanly.

**Step 1: Remove module declarations from `mod.rs`**

Remove:
```rust
pub mod band_calculator;
pub mod boundary_prioritizer;
pub mod cruise_strategy;
pub mod turn_detector;
```

And their corresponding `pub use` exports.

**Step 2: Remove deprecated fields from `AdaptivePrefetchConfig`**

Remove: `trigger_position`, `lead_distance`, `band_width`, `track_stability_threshold`, `turn_threshold`, `track_stability_duration`, `time_budget_margin`

Update `Default` impl and `from_prefetch_settings()` / `from_prefetch_config()` accordingly.

**Step 3: Remove from coordinator**

Remove `turn_detector` and `cruise_strategy` fields, imports, and any remaining references.

**Step 4: Delete the files**

```bash
rm xearthlayer/src/prefetch/adaptive/cruise_strategy.rs
rm xearthlayer/src/prefetch/adaptive/band_calculator.rs
rm xearthlayer/src/prefetch/adaptive/boundary_prioritizer.rs
rm xearthlayer/src/prefetch/adaptive/turn_detector.rs
```

**Step 5: Verify compilation and tests**

Run: `cargo test -p xearthlayer prefetch`
Expected: All remaining tests pass. Some tests from deleted files are gone — that's expected.

**Step 6: Commit**

```bash
git commit -m "refactor(prefetch): remove CruiseStrategy, BandCalculator, BoundaryPrioritizer, TurnDetector (#58)"
```

---

## Task 15: Region Completion Tracking

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/boundary_strategy.rs`

**Context:** Implement the "confirm" phase of the two-phase commit. When tiles for a DSF region are confirmed in cache, move the region from `InProgress` to `Prefetched`. Also implement the staleness timeout — if `InProgress` for more than `stale_region_timeout`, revert to absent.

**Step 1: Write the failing tests**

```rust
#[test]
fn test_region_transitions_to_prefetched_on_completion() {
    // Mark region InProgress
    // Simulate all tiles cached
    // Call completion check → region should be Prefetched
}

#[test]
fn test_stale_region_reverts_to_absent() {
    // Mark region InProgress with old timestamp
    // Call staleness check → region should be removed
}
```

**Step 2: Implement**

Add a periodic staleness sweep to the coordinator cycle:
1. Iterate `PrefetchedRegion` entries in GeoIndex
2. For any `InProgress` entry where `is_stale(timeout)` → remove from index
3. Log at `tracing::debug!`

For completion confirmation:
- After tiles are submitted, the executor processes them asynchronously
- The simplest approach: in the next coordinator cycle, check if tiles for InProgress regions exist in memory cache or on disk
- If all tiles confirmed → promote to `Prefetched`

**Step 3: Run tests, verify pass**

**Step 4: Commit**

```bash
git commit -m "feat(prefetch): add region completion tracking and staleness timeout (#58)"
```

---

## Task 16: Documentation Updates

**Files:**
- Modify: `docs/configuration.md` (if it has prefetch settings documentation)
- Verify: `docs/dev/adaptive-prefetch-design.md` is up to date (already done in design phase)
- Modify: `CLAUDE.md` — update the Predictive Tile Caching section to reflect boundary-driven model

**Step 1: Update configuration documentation**

Add new settings, mark deprecated ones.

**Step 2: Update CLAUDE.md**

In the "Predictive Tile Caching" section (#11), update to mention:
- `BoundaryStrategy` — boundary-driven prefetch using dual monitors
- `SceneryWindow` — three-window model (X-Plane / XEL / Target)
- Removed: CruiseStrategy, BandCalculator, BoundaryPrioritizer, TurnDetector

**Step 3: Commit**

```bash
git commit -m "docs(prefetch): update documentation for boundary-driven prefetch model (#58)"
```

---

## Task 17: Pre-Commit Verification

**Step 1: Run full verification**

```bash
make pre-commit
```

Expected: Format OK, Clippy clean, all tests pass.

**Step 2: Fix any issues**

Address any formatting, lint, or test failures.

**Step 3: Final commit if needed**

```bash
git commit -m "style(prefetch): address pre-commit findings (#58)"
```

---

## Task Dependencies

```
Task 1 (RetainedRegion) ─────┐
Task 2 (PrefetchedRegion) ───┼─▶ Task 3 (exports) ──┐
                              │                       │
Task 4 (BoundaryMonitor) ────┼───────────────────────┼─▶ Task 5 (SceneryWindow state machine)
                              │                       │        │
                              │                       │        ├─▶ Task 6 (monitor integration)
                              │                       │        ├─▶ Task 7 (retention)
                              │                       │        └─▶ Task 8 (rebuild detection)
                              │                       │
                              │                       └─▶ Task 9 (BoundaryStrategy rows/cols)
                              │                                │
                              │                                └─▶ Task 10 (tile expansion)
                              │
Task 11 (config) ─────────────┤
                              │
                              └─▶ Task 12 (coordinator integration)
                                       │
                                       ├─▶ Task 13 (service wiring)
                                       ├─▶ Task 14 (remove old components)
                                       └─▶ Task 15 (completion tracking)
                                                │
                                                └─▶ Task 16 (docs)
                                                        │
                                                        └─▶ Task 17 (pre-commit)
```

---

## Files Summary

| File | Task | Action |
|------|------|--------|
| `geo_index/layers.rs` | 1, 2 | Add RetainedRegion, PrefetchedRegion |
| `geo_index/mod.rs` | 3 | Export new types |
| `prefetch/adaptive/boundary_monitor.rs` | 4 | Create — position vs edge logic |
| `prefetch/adaptive/scenery_window.rs` | 5, 6, 7, 8 | Create — three-window model |
| `prefetch/adaptive/boundary_strategy.rs` | 9, 10 | Create — row/column tile generation |
| `prefetch/adaptive/mod.rs` | 4, 5, 9, 14 | Module declarations and exports |
| `prefetch/adaptive/config.rs` | 11 | Add new fields, keep deprecated temporarily |
| `config/keys.rs` | 11 | Add new ConfigKey variants |
| `config/upgrade.rs` | 11 | Add deprecated keys |
| `prefetch/adaptive/coordinator/core.rs` | 12, 14, 15 | Wire SceneryWindow, remove old deps |
| Service layer (facade/orchestrator) | 13 | Pass SceneTracker to coordinator |
| `prefetch/adaptive/cruise_strategy.rs` | 14 | Delete |
| `prefetch/adaptive/band_calculator.rs` | 14 | Delete |
| `prefetch/adaptive/boundary_prioritizer.rs` | 14 | Delete |
| `prefetch/adaptive/turn_detector.rs` | 14 | Delete |
| `docs/configuration.md` | 16 | Update settings docs |
| `CLAUDE.md` | 16 | Update architecture section |
| `docs/dev/adaptive-prefetch-design.md` | (done) | Already updated to v2.0 |
