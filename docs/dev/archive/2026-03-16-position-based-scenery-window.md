# Position-Based Scenery Window Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix scenery window drift during long flights (#86) by replacing SceneTracker-derived bounds with position-based window management.

**Architecture:** The scenery window becomes a fixed-size box (3° lat × cos-lat lon) that stays in place until a boundary crossing fires. When a crossing is detected and prefetch targets are generated, the window re-centers on the aircraft's current position. If the aircraft is outside the window entirely (e.g., rapid movement, teleport), the window also re-centers unconditionally. SceneTracker is no longer involved in window positioning — only used for burst/rebuild detection.

**Tech Stack:** Rust, unit tests (cargo test), existing `BoundaryMonitor` and `SceneryWindow` types.

---

## Design Rationale

### The Drift Problem

Currently, `update_from_tracker()` in `Ready` state slides the window's boundary monitors to match `SceneTracker::loaded_bounds()`. Since `loaded_bounds()` computes a bounding box over **all tiles ever requested**, it only grows. After flying 2°+ from departure:

- Monitor edges span departure-to-current (5°+ wide instead of ~3°)
- The `cross_range` in coordinator becomes bloated
- Prefetch generates tiles at the departure longitude, not the aircraft's position

### Position-Based Model

Instead of inferring window position from empirical FUSE data, we **assume** a fixed window size and move it based on the aircraft's known position:

1. Window initialized centered on aircraft (existing `Assumed` state lazy init)
2. Window stays **fixed** — monitors don't slide
3. Aircraft approaches edge → crossing fires → prefetch generates targets
4. **After** processing crossings: window re-centers on aircraft position
5. Aircraft is now at center → well outside trigger zone → no crossings until next edge
6. **Safety net:** If aircraft is outside window entirely (dist < 0), re-center unconditionally

### Measuring State Recovery

After `check_for_rebuild()` detects a teleport/settings change, it resets to `Measuring` state.
The next `update_from_tracker()` call matches the `Measuring` arm and transitions back to `Ready`
with monitors centered on the tracker center. This path is **unchanged** — the `Ready` no-op
only affects the post-initialization sliding, not the recovery transition.

### Trigger Distance Constraint

With a 3° window (half = 1.5°) and the current `trigger_distance = 1.5°`, the trigger zone covers the **entire** window — every position fires a crossing. This worked before because tracker-derived bounds made the effective window 4-6° wide.

**Fix:** Reduce `trigger_distance` from 1.5° to 1.0°. This creates a 0.5° dead zone on each side (1° total at center). After re-centering, the aircraft is 1.5° from each edge, safely outside the 1.0° trigger zone.

**Verification math** (aircraft flying north at 50.8°N):
```
Window: [49.0, 52.0] (3° centered on 50.5)
Aircraft at 50.8: dist_to_max = 52.0 - 50.8 = 1.2° > 1.0° → no crossing
Aircraft at 51.2: dist_to_max = 52.0 - 51.2 = 0.8° < 1.0° → crossing fires (urgency 0.2)
Re-center on 51.2: window → [49.7, 52.7]
Aircraft at 51.2: dist_to_max = 52.7 - 51.2 = 1.5° > 1.0° → no crossing ✓
Aircraft at 51.2: dist_to_min = 51.2 - 49.7 = 1.5° > 1.0° → no crossing ✓
Next crossing: aircraft must fly 0.5° more (to 51.7°) before north edge triggers again
```

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `xearthlayer/src/prefetch/adaptive/scenery_window.rs` | Modify | Add `center_on_position()`, remove tracker sliding in Ready state |
| `xearthlayer/src/prefetch/adaptive/config.rs` | Modify | Change `trigger_distance` default: 1.5 → 1.0 |
| `xearthlayer/src/config/defaults.rs` | Modify | Change `DEFAULT_PREFETCH_TRIGGER_DISTANCE`: 1.5 → 1.0 |
| `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` | Modify | Wire up `center_on_position` after crossings, remove tracker sliding |

---

## Task 1: Add `center_on_position()` to SceneryWindow

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs`

This method re-centers the boundary monitors on the aircraft's current position,
using the configured window dimensions. It also recomputes columns for the
current latitude (the Assumed state lazy-init computes cols at equator baseline).

- [ ] **Step 1: Write failing test — basic centering**

```rust
#[test]
fn test_center_on_position_moves_window() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    // Lazy-init monitors at (50.0, 7.0) via check_boundaries
    window.set_assumed_dimensions(3, 50.0);
    window.check_boundaries(50.0, 7.0);

    // Window should be centered on (50.0, 7.0)
    let bounds = window.window_bounds().unwrap();
    assert!((bounds.0 - 48.5).abs() < 0.01); // min_lat
    assert!((bounds.1 - 51.5).abs() < 0.01); // max_lat

    // Re-center on new position (52.0, 9.0)
    window.center_on_position(52.0, 9.0);

    let bounds = window.window_bounds().unwrap();
    // At lat 52°: cols = ceil(3.0/cos(52°)) = ceil(4.87) = 5
    assert!((bounds.0 - 50.5).abs() < 0.01, "min_lat should be 50.5, got {}", bounds.0);
    assert!((bounds.1 - 53.5).abs() < 0.01, "max_lat should be 53.5, got {}", bounds.1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_center_on_position_moves_window -- --nocapture`
Expected: FAIL — `center_on_position` method does not exist.

- [ ] **Step 3: Write failing test — centering clears trigger zone**

```rust
#[test]
fn test_center_on_position_clears_trigger_zone() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    window.set_assumed_dimensions(3, 50.0);

    // Aircraft near north edge — should trigger crossing
    window.check_boundaries(50.0, 7.0);  // init monitors
    let crossings = window.check_boundaries(51.2, 7.0);
    assert!(!crossings.is_empty(), "Should fire crossing near north edge");

    // Re-center on aircraft position
    window.center_on_position(51.2, 7.0);

    // After centering, aircraft is at center — no crossings
    let crossings = window.check_boundaries(51.2, 7.0);
    assert!(crossings.is_empty(), "No crossing expected after re-centering");
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_center_on_position_clears_trigger_zone -- --nocapture`
Expected: FAIL — `center_on_position` method does not exist.

- [ ] **Step 5: Write failing test — centering updates cols for latitude**

```rust
#[test]
fn test_center_on_position_recomputes_cols_for_latitude() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    // Start at equator: cols = ceil(3.0/cos(0)) = 3
    window.set_assumed_dimensions(3, 0.0);
    assert_eq!(window.window_size(), Some((3, 3)));

    // Lazy-init at equator
    window.check_boundaries(0.0, 7.0);

    // Re-center at 60°N: cols = ceil(3.0/cos(60°)) = ceil(6.0) = 6
    window.center_on_position(60.0, 7.0);
    assert_eq!(window.window_size(), Some((3, 6)));
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_center_on_position_recomputes_cols -- --nocapture`
Expected: FAIL — `center_on_position` method does not exist.

- [ ] **Step 7: Write failing test — center_on_position is no-op before monitors init**

```rust
#[test]
fn test_center_on_position_noop_before_init() {
    let mut window = SceneryWindow::new(SceneryWindowConfig::default());
    // No set_assumed_dimensions, no check_boundaries — monitors are None
    window.center_on_position(50.0, 7.0); // should not panic
    assert!(window.window_bounds().is_none());
}
```

- [ ] **Step 8: Write failing test — is_aircraft_outside detection**

```rust
#[test]
fn test_is_aircraft_outside_window() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    window.set_assumed_dimensions(3, 50.0);
    window.check_boundaries(50.0, 7.0); // init monitors

    // At center — inside
    assert!(!window.is_aircraft_outside(50.0, 7.0));
    // Near edge — inside
    assert!(!window.is_aircraft_outside(51.0, 7.0));
    // Beyond north edge — outside
    assert!(window.is_aircraft_outside(52.0, 7.0));
    // Beyond south edge — outside
    assert!(window.is_aircraft_outside(47.0, 7.0));
    // Beyond east edge — outside
    assert!(window.is_aircraft_outside(50.0, 12.0));
    // No monitors — not outside (can't tell)
    let window2 = SceneryWindow::new(config.clone());
    assert!(!window2.is_aircraft_outside(99.0, 99.0));
}
```

- [ ] **Step 9: Implement `center_on_position()` and `is_aircraft_outside()`**

In `SceneryWindow`:
```rust
/// Re-center the window on the aircraft's current position.
///
/// Moves both boundary monitors so the aircraft is at the center
/// of the configured window dimensions. Also recomputes longitude
/// columns for the current latitude.
///
/// No-op if monitors haven't been initialized yet.
pub fn center_on_position(&mut self, lat: f64, lon: f64) {
    let (rows, _old_cols) = match self.window_size {
        Some(size) => size,
        None => return,
    };

    // Recompute cols for current latitude
    let cols = self.config.compute_cols(lat);
    self.window_size = Some((rows, cols));

    let half_rows = rows as f64 / 2.0;
    let half_cols = cols as f64 / 2.0;

    if let Some(ref mut lat_mon) = self.lat_monitor {
        lat_mon.update_edges(lat - half_rows, lat + half_rows);
    }
    if let Some(ref mut lon_mon) = self.lon_monitor {
        lon_mon.update_edges(lon - half_cols, lon + half_cols);
    }

    self.last_bounds = Some((
        lat - half_rows,
        lat + half_rows,
        lon - half_cols,
        lon + half_cols,
    ));

    debug!(
        lat = format!("{:.2}", lat),
        lon = format!("{:.2}", lon),
        rows,
        cols,
        "scenery window: re-centered on aircraft position"
    );
}

/// Returns `true` if the aircraft is outside the current window bounds.
///
/// Used as a safety net: if the aircraft moves rapidly (or initialization
/// centered the window elsewhere), re-centering is triggered unconditionally.
pub fn is_aircraft_outside(&self, lat: f64, lon: f64) -> bool {
    match self.window_bounds() {
        Some((min_lat, max_lat, min_lon, max_lon)) => {
            lat < min_lat || lat > max_lat || lon < min_lon || lon > max_lon
        }
        None => false, // No monitors yet — can't be "outside"
    }
}
```

- [ ] **Step 10: Run all tests to verify they pass**

Run: `cargo test -p xearthlayer test_center_on_position test_is_aircraft_outside -- --nocapture`
Expected: All 5 tests PASS.

- [ ] **Step 11: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/scenery_window.rs
git commit -m "feat(prefetch): add center_on_position() to SceneryWindow (#86)"
```

---

## Task 2: Reduce trigger_distance default from 1.5° to 1.0°

**Files:**
- Modify: `xearthlayer/src/config/defaults.rs:202`
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs:147`
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs:59`

The trigger distance must be less than half the window height (1.5°) for the
position-based model to have a dead zone at center. Reducing from 1.5° to 1.0°
gives 0.5° of margin on each side.

- [ ] **Step 1: Write failing test — default trigger distance is 1.0**

```rust
// In config.rs tests
#[test]
fn test_default_trigger_distance_is_one() {
    let config = AdaptivePrefetchConfig::default();
    assert_eq!(config.trigger_distance, 1.0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_default_trigger_distance_is_one -- --nocapture`
Expected: FAIL — current default is 1.5.

- [ ] **Step 3: Update defaults in all three locations**

- `xearthlayer/src/config/defaults.rs:202`: `pub const DEFAULT_PREFETCH_TRIGGER_DISTANCE: f64 = 1.0;`
- `xearthlayer/src/prefetch/adaptive/config.rs:147`: `trigger_distance: 1.0,`
- `xearthlayer/src/prefetch/adaptive/scenery_window.rs:59`: `trigger_distance: 1.0,`

- [ ] **Step 4: Fix existing tests that assert trigger_distance is 1.5**

Update `xearthlayer/src/prefetch/adaptive/config.rs:468`:
```rust
assert_eq!(config.trigger_distance, 1.0);
```

Update any scenery_window.rs tests using the default config that relied on 1.5° behavior:
- `test_check_boundaries_returns_predictions_when_near_edge` — aircraft at 51.0 with window max 51.5: dist=0.5 < 1.0 → still fires ✓
- `test_check_boundaries_returns_empty_when_centered` — explicitly sets trigger_distance: 0.5, no change needed
- `test_check_boundaries_both_axes` — aircraft at (51.0, 9.0), window (48.5, 51.5, 4.5, 9.5): lat dist=0.5 < 1.0 → fires; lon dist=0.5 < 1.0 → fires ✓
- `test_check_boundaries_sorted_by_urgency_descending` — aircraft at (51.0, 9.2), dist 0.5 and 0.3 < 1.0 → both fire ✓

The existing default-config tests should still pass because they use positions close to edges (0.5° from edge), which is within the new 1.0° trigger.

Review `test_trigger_near_both_edges_small_window` in boundary_monitor.rs — uses explicit trigger 1.5°, no default → no change needed.

- [ ] **Step 5: Run all tests to verify**

Run: `cargo test -p xearthlayer -- --nocapture 2>&1 | tail -5`
Expected: All tests PASS.

- [ ] **Step 6: Commit**

```bash
git add xearthlayer/src/config/defaults.rs \
       xearthlayer/src/prefetch/adaptive/config.rs \
       xearthlayer/src/prefetch/adaptive/scenery_window.rs
git commit -m "fix(prefetch): reduce trigger_distance to 1.0° for position-based window (#86)"
```

---

## Task 3: Remove tracker-based sliding from Ready state

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs:174-191`

The `update_from_tracker()` method currently slides monitors to tracker bounds
in `Ready` state — this is the root cause of drift. Remove this behavior.
Keep the `Uninitialized/Assumed → Ready` transition path for backward
compatibility with code that still calls `update_from_tracker()`.

- [ ] **Step 1: Write failing test — tracker bounds don't drift window in Ready state**

```rust
#[test]
fn test_ready_state_ignores_tracker_bound_changes() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    let tracker = std::sync::Arc::new(MockSceneTracker::new());

    // Initialize to Ready centered at (50.0, 7.0)
    tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));
    window.update_from_tracker(tracker.as_ref());
    assert!(matches!(window.state(), WindowState::Ready));

    let initial_bounds = window.window_bounds().unwrap();

    // Tracker bounds grow (simulating departure drift)
    tracker.set_bounds(make_bounds(43.0, 53.0, 0.0, 11.0));
    window.update_from_tracker(tracker.as_ref());

    // Window bounds should NOT have changed
    let after_bounds = window.window_bounds().unwrap();
    assert_eq!(initial_bounds, after_bounds,
        "Window should not drift with tracker bounds in Ready state");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_ready_state_ignores_tracker_bound_changes -- --nocapture`
Expected: FAIL — current code slides monitors to new tracker bounds.

- [ ] **Step 3: Remove Ready-state sliding in `update_from_tracker()`**

Replace `scenery_window.rs` lines 174-191 (the `WindowState::Ready` arm) with:

```rust
WindowState::Ready => {
    // In position-based mode, Ready state no longer slides
    // monitors from tracker bounds. Window position is managed
    // by center_on_position() called after boundary crossings.
    // Tracker data is only used for the initial Uninitialized → Ready
    // transition.
}
```

- [ ] **Step 4: Update existing sliding tests**

The following 3 tests relied on tracker-based sliding and must be replaced
with position-based equivalents:

**Replace `test_ready_state_slides_monitors_on_tracker_update`** with:
```rust
#[test]
fn test_ready_state_slides_via_center_on_position() {
    let config = SceneryWindowConfig {
        trigger_distance: 0.3,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    let tracker = std::sync::Arc::new(MockSceneTracker::new());

    // Transition to Ready via tracker (initial positioning)
    tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
    window.update_from_tracker(tracker.as_ref());
    assert!(matches!(window.state(), WindowState::Ready));

    // Aircraft at center — no crossings
    let crossings = window.check_boundaries(52.5, 10.0);
    assert!(crossings.is_empty(), "No crossing expected at center");

    // Aircraft moves south — re-center window on new position
    window.center_on_position(50.5, 10.0);

    // Aircraft near new south edge (50.5 + 0.2 = 50.7, within 0.3 trigger of 49.0)
    // At lat 50.5: window = [49.0, 52.0]. 50.7 is 1.7° from min → outside 0.3 trigger
    // Need to check near actual south edge: 49.0 + 0.2 = 49.2
    let crossings = window.check_boundaries(49.2, 10.0);
    assert!(
        !crossings.is_empty(),
        "Should detect crossing near south edge after re-centering"
    );
}
```

**Replace `test_window_slides_continuously_through_multiple_updates`** with:
```rust
#[test]
fn test_window_slides_continuously_via_center_on_position() {
    let config = SceneryWindowConfig {
        trigger_distance: 0.3,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    let tracker = std::sync::Arc::new(MockSceneTracker::new());

    // Initialize to Ready
    tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
    window.update_from_tracker(tracker.as_ref());
    assert!(matches!(window.state(), WindowState::Ready));

    let mut crossing_count = 0;

    // Simulate flying south: each step re-centers 1° further south
    for step in 0..5 {
        let aircraft_lat = 52.5 - step as f64;
        window.center_on_position(aircraft_lat, 10.0);

        // Check near south edge (aircraft_lat - half_rows + 0.2)
        let south_edge = aircraft_lat - 1.5; // half of 3 rows
        let near_south = south_edge + 0.2;
        let crossings = window.check_boundaries(near_south, 10.0);
        if !crossings.is_empty() {
            crossing_count += 1;
        }
    }

    assert!(
        crossing_count >= 3,
        "Expected crossings at multiple positions, got {}",
        crossing_count
    );
}
```

**Replace `test_window_bounds_reflect_latest_tracker_update`** with:
```rust
#[test]
fn test_window_bounds_reflect_center_on_position() {
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    let tracker = std::sync::Arc::new(MockSceneTracker::new());

    // Initialize to Ready
    tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
    window.update_from_tracker(tracker.as_ref());

    // Re-center on (50.0, 9.0)
    // At lat 50°: cols = ceil(3.0/cos(50°)) = 5, half_cols = 2.5
    window.center_on_position(50.0, 9.0);

    let bounds = window.window_bounds();
    let (min_lat, max_lat, min_lon, max_lon) = bounds.unwrap();
    assert!((min_lat - 48.5).abs() < 0.01);
    assert!((max_lat - 51.5).abs() < 0.01);
    assert!((min_lon - 6.5).abs() < 0.01);
    assert!((max_lon - 11.5).abs() < 0.01);
}
```

- [ ] **Step 5: Run all scenery_window tests**

Run: `cargo test -p xearthlayer scenery_window -- --nocapture`
Expected: All tests PASS.

- [ ] **Step 6: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/scenery_window.rs
git commit -m "refactor(prefetch): remove tracker-based sliding from Ready state (#86)"
```

---

## Task 4: Wire up position-based centering in coordinator

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs:484-498`

Replace the `update_from_tracker(tracker)` call in cruise phase with
position-based centering. The window re-centers when crossings fire OR when
the aircraft is outside the window entirely (safety net for rapid movement).

**Flow change:**
```
BEFORE:                          AFTER:
1. update_from_tracker()         1. check if aircraft outside window → re-center
2. update_retention()            2. check_boundaries(lat, lon)
3. check_boundaries()            3. if crossings: generate targets
4. if crossings: generate        4. if crossings: center_on_position(lat, lon)
                                 5. update_retention()
```

- [ ] **Step 1: Write failing test — long flight doesn't cause drift**

```rust
#[test]
fn test_long_flight_window_follows_aircraft_not_tracker() {
    use crate::geo_index::GeoIndex;

    // Create a tracker that returns growing bounds (simulating departure drift)
    let tracker: Arc<dyn SceneTracker> =
        Arc::new(StableBoundsTracker::with_bounds(47.0, 53.0, 3.0, 11.0));
    let geo_index = Arc::new(GeoIndex::new());

    let config = AdaptivePrefetchConfig {
        mode: PrefetchMode::Aggressive,
        trigger_distance: 1.0,
        ..Default::default()
    };
    let mut coord = AdaptivePrefetchCoordinator::new(config)
        .with_calibration(test_calibration())
        .with_scene_tracker(tracker)
        .with_geo_index(geo_index);

    coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

    // Enter cruise phase
    coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Fly 10° north — if window drifts, crossings would target departure lon
    for step in 0..10 {
        let lat = 50.0 + step as f64;
        coord.update((lat, 7.0), 0.0, 200.0, 35000.0);
    }

    // Window should be near aircraft (lat ~60), not spanning 47-60
    let window_bounds = coord.scenery_window.window_bounds();
    if let Some((min_lat, max_lat, _, _)) = window_bounds {
        let span = max_lat - min_lat;
        assert!(
            span <= 5.0,
            "Window span should be ~3° (configured), not {} (drifted)",
            span
        );
        assert!(
            max_lat >= 58.0,
            "Window max_lat should be near aircraft (~60°), got {}",
            max_lat
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xearthlayer test_long_flight_window_follows_aircraft -- --nocapture`
Expected: FAIL — window drifts with tracker bounds.

- [ ] **Step 3: Modify cruise phase in coordinator**

`is_aircraft_outside()` was already implemented in Task 1.

In `coordinator/core.rs`, replace the cruise phase body (lines 484-598).
Remove:
```rust
// Update scenery window from scene tracker
if let Some(ref tracker) = self.scene_tracker {
    self.scenery_window.update_from_tracker(tracker.as_ref());
}
```

Add **before** crossings check (safety net for aircraft outside window):
```rust
// Safety net: if aircraft is outside window (rapid movement, init edge case),
// re-center unconditionally so crossings can fire.
if self.scenery_window.is_aircraft_outside(lat, lon) {
    self.scenery_window.center_on_position(lat, lon);
}
```

Add **after** crossings processing (after targets generated, before returning plan):
```rust
// Re-center window on aircraft after processing crossings.
// This ensures the window follows the aircraft without
// relying on SceneTracker bounds (which drift over long flights).
if !crossings.is_empty() {
    self.scenery_window.center_on_position(lat, lon);
}
```

Keep `update_retention()` call — it already uses aircraft position.

- [ ] **Step 4: Update existing coordinator tests**

**Critical analysis:** With the new behavior, the coordinator no longer calls
`update_from_tracker()` in cruise phase. The window starts in `Assumed` state
(set at coordinator construction, line 213). On the first cruise tick,
`update_from_tracker()` was previously transitioning the window to `Ready`.
Now, the `check_boundaries()` lazy init (line 242 in scenery_window.rs)
initializes monitors centered on the aircraft position. This means the
window is centered on the **aircraft**, not on the tracker's center.

The following tests need updating because they position the aircraft at
(52.0, 7.0) which becomes the window center with lazy init. The window
would be [50.5, 53.5] × [4.5, 9.5] (at lat 52°, cols=5). Aircraft at center
is 1.5° from edges, which is > 1.0° trigger → no crossing.

**`test_coordinator_with_scenery_window_returns_plan_on_boundary`** (line 1500):
Fix by positioning aircraft near window edge. Use (53.2, 7.0) — window
centered on first call at (53.2, 7.0) gives [51.7, 54.7]. Subsequent calls
at the same position won't trigger. Instead, position at (52.0, 7.0) for
init, then move to (53.2, 7.0) on the boundary-triggering tick:
```rust
// Init at center → window [50.5, 53.5]
coord.update((52.0, 7.0), 0.0, 200.0, 35000.0); // ground
std::thread::sleep(...);
coord.update((52.0, 7.0), 0.0, 200.0, 35000.0); // → cruise, lazy init
std::thread::sleep(...);
// Aircraft near north edge: 53.0 is 0.5° from 53.5 → fires with trigger=1.0
let plan = coord.update((53.0, 7.0), 0.0, 200.0, 35000.0);
```

**`test_boundary_plan_tiles_sorted_by_distance_from_aircraft`** (line 1551):
Same fix — init at (52.0, 7.0), trigger at (53.0, 7.0).

**`test_coordinator_boundary_uses_geo_index_filtering`** (line 1636):
Same fix — init at (52.0, 7.0), trigger at (53.0, 7.0). Also update the
GeoIndex pre-population to match the expected boundary dsf_coord (now based
on window centered on 52.0, max_lat=53.5, so dsf_coord=53 is still correct).

**`test_coordinator_no_plan_when_far_from_boundary`** (line 1604):
Aircraft at (50.0, 7.0) — window centered on (50.0, 7.0) gives [48.5, 51.5].
Aircraft at center is 1.5° from edges, > 1.0° trigger. **Still passes** — no
change needed. But the reasoning is now "aircraft at center of position-based
window" rather than "aircraft at center of tracker-derived window".

**`test_pending_tiles_retained_on_channel_full`** (line 1855):
This test already uses explicit `trigger_distance: 1.0` and `default_window_rows: 6`,
and positions the aircraft at (50.0, 10.0) for init, then (52.5, 10.0) near edge.
With a 6-row window centered on (50.0, 10.0): [47.0, 53.0]. Aircraft at 52.5
is 0.5° from 53.0, within 1.0° trigger. **Still passes** — no change needed.
But note: after the crossing fires and `center_on_position(52.5, 10.0)` is called,
the window re-centers to [49.5, 55.5]. The second `process_telemetry` call with
the same state won't fire a new crossing (aircraft at center). This is correct —
the test checks pending tile draining, not new crossings.

- [ ] **Step 5: Run all coordinator tests**

Run: `cargo test -p xearthlayer coordinator -- --nocapture`
Expected: All tests PASS.

- [ ] **Step 6: Run full test suite**

Run: `make test`
Expected: All tests PASS.

- [ ] **Step 7: Commit**
Note: `scenery_window.rs` may also be staged if `is_aircraft_outside` wasn't committed in Task 1.

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/core.rs
git commit -m "fix(prefetch): position-based window centering replaces tracker sliding (#86)

The scenery window no longer drifts during long flights. Instead of
sliding monitors to match ever-growing SceneTracker bounds, the window
re-centers on the aircraft position after each boundary crossing.

Closes #86"
```

---

## Task 5: Integration test — full flight simulation

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/scenery_window.rs` (add test)

End-to-end test simulating a 10° flight with boundary crossings, verifying
the window stays near the aircraft and crossings fire at correct positions.

- [ ] **Step 1: Write integration test**

```rust
#[test]
fn test_full_flight_simulation_no_drift() {
    // Simulate LFMN departure, fly 10° NW
    let config = SceneryWindowConfig {
        trigger_distance: 1.0,
        ..SceneryWindowConfig::default()
    };
    let mut window = SceneryWindow::new(config);
    window.set_assumed_dimensions(3, 43.0); // Nice, France

    // Initialize monitors at departure
    window.check_boundaries(43.7, 7.3);

    let mut total_crossings = 0;
    let mut last_crossing_lat = 43.7_f64;

    // Fly NW at ~0.1° increments (simulating ~10km steps)
    for step in 0..100 {
        let lat = 43.7 + step as f64 * 0.1;
        let lon = 7.3 - step as f64 * 0.05;

        let crossings = window.check_boundaries(lat, lon);
        if !crossings.is_empty() {
            total_crossings += 1;
            last_crossing_lat = lat;

            // Re-center after crossing (as coordinator would)
            window.center_on_position(lat, lon);
        }

        // Verify window stays near aircraft at all times
        if let Some((min_lat, max_lat, _, _)) = window.window_bounds() {
            assert!(
                lat >= min_lat && lat <= max_lat,
                "Aircraft at {:.1}° should be inside window [{:.1}, {:.1}]",
                lat, min_lat, max_lat
            );
            let span = max_lat - min_lat;
            assert!(
                span <= 4.0,
                "Window span {:.1}° should not exceed 4° (configured 3° + margin)",
                span
            );
        }
    }

    // Should have had multiple crossings over 10° of travel
    assert!(
        total_crossings >= 5,
        "Expected >=5 crossings over 10°, got {}",
        total_crossings
    );
    // Last crossing should be near the aircraft's final position
    assert!(
        last_crossing_lat > 50.0,
        "Last crossing at {:.1}° should be near final position, not departure",
        last_crossing_lat
    );
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p xearthlayer test_full_flight_simulation -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/scenery_window.rs
git commit -m "test(prefetch): add full flight simulation test for position-based window (#86)"
```

---

## Task 6: Documentation updates

**Files:**
- Modify: `docs/dev/adaptive-prefetch-design.md` — update window positioning section
- Modify: `docs/configuration.md` — update trigger_distance default (1.5 → 1.0)

- [ ] **Step 1: Update design doc**

Add a "Window Positioning" section explaining the position-based model:
- Window is fixed-size, not derived from SceneTracker
- Re-centers on aircraft after boundary crossings
- Trigger distance constraint: must be < half window height

- [ ] **Step 2: Update config doc**

Change trigger_distance default value documentation from 1.5 to 1.0.

- [ ] **Step 3: Commit**

```bash
git add docs/dev/adaptive-prefetch-design.md docs/configuration.md
git commit -m "docs(prefetch): update design and config for position-based window (#86)"
```

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Trigger distance 1.0° may be too narrow for slow aircraft | Can be tuned via `prefetch.trigger_distance` config — no code change needed |
| Cols recomputation on every centering | `compute_cols()` is a trivial `ceil(extent/cos(lat))` — negligible cost |
| Existing users with `trigger_distance=1.5` in config | Config override takes precedence; only default changes. Note: users with 1.5° and 3° window will have the entire window as trigger zone — they should update to ≤1.0° |
| World rebuild detection still uses SceneTracker | Unchanged — `check_for_rebuild()` is independent of window positioning |
| Aircraft outside window (rapid movement, teleport) | Safety net: `is_aircraft_outside()` check triggers unconditional re-centering before crossing detection |
| High-latitude cols explosion (e.g., lat 85° → cols=35) | `compute_cols()` uses `ceil(3.0/cos(85°))` = 35. Large but correct — X-Plane's window IS wider at high latitudes. No overflow risk (usize). Retention area scales accordingly. |
| First-flight cold start (no tracker bounds) | Works: window stays in Assumed state, `check_boundaries()` lazy-inits monitors centered on aircraft. No `update_from_tracker()` needed. |
| `DEFAULT_PREFETCH_TRIGGER_DISTANCE` doc comment | Update rationale: old comment referenced X-Plane's 1.0° trigger for lead time; new rationale is dead-zone requirement for position-based model |
