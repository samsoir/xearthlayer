# Proportional Prefetch Box Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resize the sliding prefetch box from 4°×4° to 9°×9° and replace the binary heading bias with smooth proportional interpolation, matching X-Plane's observed 6×6 DSF loading area with 1.5° overlap margin.

**Architecture:** The `PrefetchBox` struct changes from `(forward_margin, behind_margin)` to `(extent, max_bias)`. The `bounds()` method uses `0.5 + (max_bias - 0.5) × |component|` to smoothly interpolate the forward/behind split per axis based on heading decomposition. The total extent per axis is constant; only the bias within that extent slides with heading.

**Tech Stack:** Pure Rust, no new dependencies. Changes to `prefetch_box.rs`, `config.rs`, coordinator tests, debug map API.

---

## Design

### Current vs New

| | Current | New |
|---|---------|-----|
| **Total extent** | 4° per axis (3+1) | 9° per axis (configurable, min 7) |
| **Bias model** | Binary: 75/25 if any heading component | Proportional: 50/50 → 80/20 based on |component| |
| **Parameters** | `forward_margin`, `behind_margin` | `extent`, `max_bias` |
| **At cardinal heading** | 3° ahead, 1° behind, 2°/2° perpendicular | 7.2° ahead, 1.8° behind, 4.5°/4.5° perpendicular |
| **At diagonal (45°)** | 3° ahead on both axes (binary) | 6.4° ahead on both axes (proportional, 71%) |
| **Regions covered** | ~16 DSF regions | ~81 DSF regions |

### Bias Formula

```
forward_fraction = 0.5 + (max_bias - 0.5) × |component|
```

Where:
- `component` = `cos(track)` for latitude, `sin(track)` for longitude
- `max_bias` = 0.8 (configurable)
- Result: 0.5 (symmetric) when component is 0, up to 0.8 (80/20) when component is ±1

### Expected bounds at 9° extent, 0.8 max_bias

| Track | North | South | East | West |
|-------|-------|-------|------|------|
| 000° | 7.2° | 1.8° | 4.5° | 4.5° |
| 045° | 6.4° | 2.6° | 6.4° | 2.6° |
| 090° | 4.5° | 4.5° | 7.2° | 1.8° |
| 180° | 1.8° | 7.2° | 4.5° | 4.5° |
| 260° | 2.4° | 6.6° | 1.9° | 7.1° |
| 270° | 4.5° | 4.5° | 1.8° | 7.2° |

---

## File Structure

| File | Change |
|------|--------|
| `xearthlayer/src/prefetch/adaptive/prefetch_box.rs` | Replace struct fields, rewrite `bounds()`, update all tests |
| `xearthlayer/src/prefetch/adaptive/config.rs` | Replace `forward_margin`/`behind_margin` with `box_extent`/`box_max_bias` |
| `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` | Update `PrefetchBox::new()` call, update test configs |
| `xearthlayer/src/debug_map/api.rs` | Update `compute_prefetch_box()` to use new constructor |

---

## Task 1: Rewrite `PrefetchBox` with proportional bias

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/prefetch_box.rs`

This is the core change. Replace the binary bias with proportional interpolation.

- [ ] **Step 1: Write failing test — proportional bias at cardinal heading**

```rust
#[test]
fn test_proportional_due_north() {
    let pbox = PrefetchBox::new(9.0, 0.8);
    let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(48.0, 15.0, 0.0);

    // Due north: lat 80/20 bias, lon 50/50 symmetric
    // Lat: ahead (north) = 9 * 0.8 = 7.2, behind (south) = 9 * 0.2 = 1.8
    assert!((lat_max - 48.0 - 7.2).abs() < 0.01, "North should be 7.2° ahead");
    assert!((48.0 - lat_min - 1.8).abs() < 0.01, "South should be 1.8° behind");

    // Lon: symmetric = 9 * 0.5 = 4.5 each side
    assert!((lon_max - 15.0 - 4.5).abs() < 0.01, "East should be 4.5°");
    assert!((15.0 - lon_min - 4.5).abs() < 0.01, "West should be 4.5°");
}
```

- [ ] **Step 2: Write failing test — proportional bias at diagonal**

```rust
#[test]
fn test_proportional_northeast_45() {
    let pbox = PrefetchBox::new(9.0, 0.8);
    let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(48.0, 15.0, 45.0);

    // At 45°: |cos(45)| = |sin(45)| = 0.707
    // forward_frac = 0.5 + 0.3 * 0.707 = 0.712
    // ahead = 9 * 0.712 = 6.41, behind = 9 * 0.288 = 2.59
    let lat_ahead = lat_max - 48.0;
    let lon_ahead = lon_max - 15.0;
    assert!((lat_ahead - 6.41).abs() < 0.1, "Lat ahead should be ~6.4°, got {}", lat_ahead);
    assert!((lon_ahead - 6.41).abs() < 0.1, "Lon ahead should be ~6.4°, got {}", lon_ahead);

    // Both axes should have equal bias at 45°
    assert!((lat_ahead - lon_ahead).abs() < 0.01, "Equal bias at 45°");
}
```

- [ ] **Step 3: Write failing test — due east symmetric on lat**

```rust
#[test]
fn test_proportional_due_east_lat_symmetric() {
    let pbox = PrefetchBox::new(9.0, 0.8);
    let (lat_min, lat_max, _, _) = pbox.bounds(48.0, 15.0, 90.0);

    // Due east: lat should be symmetric (50/50)
    let lat_north = lat_max - 48.0;
    let lat_south = 48.0 - lat_min;
    assert!((lat_north - 4.5).abs() < 0.01, "Lat north should be 4.5°");
    assert!((lat_south - 4.5).abs() < 0.01, "Lat south should be 4.5°");
}
```

- [ ] **Step 4: Write failing test — total extent is constant regardless of heading**

```rust
#[test]
fn test_total_extent_constant() {
    let pbox = PrefetchBox::new(9.0, 0.8);

    for track in [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
        let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(48.0, 15.0, track);
        let lat_extent = lat_max - lat_min;
        let lon_extent = lon_max - lon_min;
        assert!(
            (lat_extent - 9.0).abs() < 0.01,
            "Lat extent should be 9.0 at track {}, got {}",
            track, lat_extent
        );
        assert!(
            (lon_extent - 9.0).abs() < 0.01,
            "Lon extent should be 9.0 at track {}, got {}",
            track, lon_extent
        );
    }
}
```

- [ ] **Step 5: Write failing test — southern hemisphere**

```rust
#[test]
fn test_proportional_southern_hemisphere() {
    let pbox = PrefetchBox::new(9.0, 0.8);
    let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(-24.0, 134.0, 90.0);

    // Due east in Australia: lat symmetric, lon biased east
    let lat_extent = lat_max - lat_min;
    assert!((lat_extent - 9.0).abs() < 0.01);

    let lon_ahead = lon_max - 134.0;
    assert!((lon_ahead - 7.2).abs() < 0.01, "East should be 7.2° ahead");
}
```

- [ ] **Step 6: Rewrite `PrefetchBox` struct and `bounds()`**

```rust
#[derive(Debug, Clone)]
pub struct PrefetchBox {
    /// Total extent per axis in degrees.
    extent: f64,
    /// Maximum forward bias fraction (0.5 = symmetric, 0.8 = 80/20).
    max_bias: f64,
}

impl PrefetchBox {
    pub fn new(extent: f64, max_bias: f64) -> Self {
        Self { extent, max_bias }
    }

    pub fn bounds(&self, lat: f64, lon: f64, track: f64) -> (f64, f64, f64, f64) {
        let track_rad = track.to_radians();
        let lat_component = track_rad.cos();
        let lon_component = track_rad.sin();

        // Proportional forward fraction: 0.5 (symmetric) to max_bias (fully biased)
        let lat_fwd_frac = 0.5 + (self.max_bias - 0.5) * lat_component.abs();
        let lon_fwd_frac = 0.5 + (self.max_bias - 0.5) * lon_component.abs();

        // Apply direction
        let (lat_min, lat_max) = if lat_component >= 0.0 {
            (lat - self.extent * (1.0 - lat_fwd_frac), lat + self.extent * lat_fwd_frac)
        } else {
            (lat - self.extent * lat_fwd_frac, lat + self.extent * (1.0 - lat_fwd_frac))
        };

        let (lon_min, lon_max) = if lon_component >= 0.0 {
            (lon - self.extent * (1.0 - lon_fwd_frac), lon + self.extent * lon_fwd_frac)
        } else {
            (lon - self.extent * lon_fwd_frac, lon + self.extent * (1.0 - lon_fwd_frac))
        };

        (lat_min, lat_max, lon_min, lon_max)
    }
}
```

Note: The `COMPONENT_THRESHOLD` is no longer needed — the proportional model handles near-zero components gracefully (they produce ~50/50 split naturally).

- [ ] **Step 7: Update existing tests for new constructor and expected values**

All existing tests use `PrefetchBox::new(3.0, 1.0)`. Update to `PrefetchBox::new(9.0, 0.8)` and adjust expected region coordinates accordingly. Some tests may need significant reworking since the box is now much larger.

- [ ] **Step 8: Run all prefetch_box tests**

Run: `cargo test -p xearthlayer --lib prefetch_box -- --nocapture`
Expected: All tests PASS.

- [ ] **Step 9: Commit**

```bash
git commit -m "feat(prefetch): proportional heading bias and 9° extent (#98)"
```

---

## Task 2: Update config to use `extent` and `max_bias`

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs`
- Modify: `xearthlayer/src/config/defaults.rs` (if default constants are defined there)

Replace `forward_margin` and `behind_margin` with `box_extent` and `box_max_bias`.

- [ ] **Step 1: Update `AdaptivePrefetchConfig` fields**

Replace:
```rust
pub forward_margin: f64,
pub behind_margin: f64,
```
With:
```rust
/// Total prefetch box extent per axis in degrees.
/// X-Plane loads a 6×6 DSF area; 9° covers this with 1.5° overlap.
/// Range: 7.0 - 15.0
pub box_extent: f64,
/// Maximum forward bias fraction (0.5 = symmetric, 0.8 = 80/20).
/// Range: 0.5 - 0.9
pub box_max_bias: f64,
```

Update `Default`:
```rust
box_extent: 9.0,
box_max_bias: 0.8,
```

- [ ] **Step 2: Update coordinator `PrefetchBox::new()` call**

In `coordinator/core.rs`, change:
```rust
let prefetch_box = PrefetchBox::new(config.forward_margin, config.behind_margin);
```
To:
```rust
let prefetch_box = PrefetchBox::new(config.box_extent, config.box_max_bias);
```

- [ ] **Step 3: Update debug map API**

In `debug_map/api.rs`, change `compute_prefetch_box()` to use the new config fields:
```rust
let pbox = PrefetchBox::new(config.box_extent, config.box_max_bias);
```

- [ ] **Step 4: Update all test configs**

Find all tests that set `forward_margin: 3.0, behind_margin: 1.0` and replace with `box_extent: 9.0, box_max_bias: 0.8`.

- [ ] **Step 5: Add deprecated keys for old config settings**

Add `"prefetch.forward_margin"` and `"prefetch.behind_margin"` to `DEPRECATED_KEYS` in `config/upgrade.rs`.

- [ ] **Step 6: Run full test suite**

Run: `make test`
Expected: All tests PASS.

- [ ] **Step 7: Commit**

```bash
git commit -m "refactor(config): replace forward/behind_margin with box_extent and box_max_bias (#98)"
```

---

## Task 3: Add config pipeline for `box_extent` and `box_max_bias`

**Files:**
- Modify: `xearthlayer/src/config/keys.rs`
- Modify: `xearthlayer/src/config/settings.rs`
- Modify: `xearthlayer/src/config/parser.rs`
- Modify: `xearthlayer/src/config/writer.rs`
- Modify: `xearthlayer/src/config/defaults.rs`
- Modify: `xearthlayer/src/service/orchestrator_config.rs`

Make the new settings configurable via `config.ini`:

```ini
[prefetch]
; Prefetch box extent in degrees per axis (default: 9.0, min: 7.0)
box_extent = 9.0
; Maximum forward bias fraction (default: 0.8, range: 0.5-0.9)
box_max_bias = 0.8
```

- [ ] **Step 1: Add to `settings.rs`** — `box_extent: f64` and `box_max_bias: f64` fields

- [ ] **Step 2: Add to `defaults.rs`** — `DEFAULT_BOX_EXTENT: f64 = 9.0` and `DEFAULT_BOX_MAX_BIAS: f64 = 0.8`

- [ ] **Step 3: Add to `keys.rs`** — `PrefetchBoxExtent` and `PrefetchBoxMaxBias` variants with validation (extent: 7.0-15.0, bias: 0.5-0.9)

- [ ] **Step 4: Add to `parser.rs`** — INI parsing blocks

- [ ] **Step 5: Add to `writer.rs`** — template lines and format args

- [ ] **Step 6: Wire through `orchestrator_config.rs`** — add to `PrefetchConfig` and `from_config_file()`

- [ ] **Step 7: Run tests**

- [ ] **Step 8: Commit**

```bash
git commit -m "feat(config): add box_extent and box_max_bias INI settings (#98)"
```

---

## Task 4: Documentation

**Files:**
- Modify: `docs/configuration.md`
- Modify: `docs/dev/adaptive-prefetch-design.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update `configuration.md`**

Add `box_extent` and `box_max_bias` to the prefetch settings table. Remove `forward_margin` and `behind_margin` if still listed.

- [ ] **Step 2: Update design doc**

Update the "Sliding Prefetch Box" section to describe proportional bias and 9° extent.

- [ ] **Step 3: Update `CLAUDE.md`**

Update the PrefetchBox description in the Predictive Tile Caching section.

- [ ] **Step 4: Commit**

```bash
git commit -m "docs: update prefetch box documentation for proportional bias (#98)"
```

---

## Task Dependency Graph

```
Task 1 (PrefetchBox rewrite) → Task 2 (Config update) → Task 3 (Config pipeline)
                                                        → Task 4 (Documentation)
```

Tasks 3 and 4 can be done in parallel after Task 2.

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| 9° box generates ~81 regions vs ~16 — more work per tick | `max_tiles_per_cycle` cap still limits throughput. GeoIndex dedup prevents re-submission. |
| Retention area grows to ~11×11 (81+buffer) | Retention is lightweight (DashMap entries). No performance concern. |
| Proportional bias at exactly 0° heading: `cos(0)=1.0` gives 80/20 | Correct — due north should be fully biased north. No threshold needed. |
| Old `forward_margin`/`behind_margin` in user configs | Added to `DEPRECATED_KEYS`. `config upgrade` removes them. New defaults apply. |
| `max_bias=0.8` may not be optimal | Configurable via `box_max_bias` in INI. Can tune without code changes. |
