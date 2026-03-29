# Prefetch Ordering & Takeoff Ramp-Up Design

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix prefetch band misalignment (#58) and takeoff stutters (#62) by re-ordering tiles to match X-Plane's DSF column/row loading pattern and adding a graduated ramp-up after takeoff.

**Architecture:** Two isolated additions to the prefetch coordinator — a `BoundaryPrioritizer` that re-sorts tiles by DSF boundary urgency, and a `TransitionThrottle` state machine that gates submission rate after Ground→Cruise transitions. Existing strategies, band geometry, and four-tier filtering are unchanged.

**Tech Stack:** Rust, existing prefetch framework (`AdaptivePrefetchCoordinator`, `CruiseStrategy`, `PhaseDetector`)

---

## Problem Summary

### #58 — Band Misalignment

The cruise strategy sorts tiles by Euclidean distance from the aircraft. This is direction-blind — a tile 0.5° to the side ranks higher than a tile 1° directly ahead. During a westbound flight from LOWI, prefetch filled tiles east of the aircraft (behind) while X-Plane loaded the next western columns on-demand, causing framerate drops at DSF boundaries.

X-Plane loads scenery in DSF column/row batches. When the aircraft is ~2.5° from a DSF boundary, it triggers loading the next 2+ columns/rows in the direction of travel. Prefetch should fill tiles in this same order.

### #62 — Takeoff Stutters

On Ground→Cruise transition (≥40kt for 2s), the coordinator immediately submits a full cycle of tiles. This creates a CPU/network burst competing with X-Plane's rendering ramp-up during takeoff, causing visible stutters. At takeoff speeds, there's no urgency — the aircraft moves slowly and departure procedures often include turns that the turn detector would block anyway.

## Design

### Component 1: BoundaryPrioritizer

A pure function (or small struct) that re-sorts a `Vec<TileCoord>` by DSF boundary urgency.

**Location:** `prefetch/adaptive/boundary_prioritizer.rs`

**Algorithm:**

1. **Decompose track into axis velocities** — determine which DSF boundaries the aircraft is approaching:
   - Longitude axis: if moving west, next boundary = `floor(lon)`, distance = `lon - floor(lon)`. If moving east, next boundary = `ceil(lon)`, distance = `ceil(lon) - lon`.
   - Latitude axis: same logic with `floor(lat)` / `ceil(lat)`.
   - If the velocity component on an axis is negligible (< threshold), that axis has no approaching boundary.

2. **Predict X-Plane's next load batches** — the next 2 columns/rows the aircraft will cross per active axis form the **urgent set**. Tiles in the urgent set are submitted first.

3. **Two-level sort**:
   - **Primary key**: DSF boundary rank — how many boundaries ahead of the aircraft is this tile's DSF column/row? Rank 0 = next column/row to be loaded, rank 1 = the one after, etc.
   - **Secondary key**: Euclidean distance from aircraft within the same rank (nearest first).

**Example** — aircraft at 47.74°N, 10.33°E, heading 292° (west-northwest):
- Approaching column boundary at 10°E (0.33° away, westward)
- Approaching row boundary at 48°N (0.26° away, northward)
- Rank 0 tiles: DSF column [9°E, 10°E) and row [48°N, 49°N) — submitted first
- Rank 1 tiles: DSF column [8°E, 9°E) and row [49°N, 50°N) — submitted second
- Remaining tiles in band — submitted last, distance-sorted

**Integration point**: Called in the coordinator after the four-tier filter removes cached/patched/disk tiles, before submission. Only re-orders — does not add or remove tiles.

**Interaction with four-tier filter**: The filter already knows what's cached (memory, disk, local tracking, patch regions). Combined with boundary prediction, the coordinator knows what X-Plane has loaded AND what it will load next. The gap is exactly what prefetch fills, in the right order.

### Component 2: TransitionThrottle

A state machine that gates prefetch submission rate after Ground→Cruise transitions.

**Location:** `prefetch/adaptive/transition_throttle.rs`

**States:**
- `Idle` — no throttling, `fraction() = 1.0`
- `GracePeriod(started_at: Instant)` — no submissions, `fraction() = 0.0`
- `RampingUp(started_at: Instant)` — linear ramp from 0.25 to 1.0, `fraction() = 0.25 + 0.75 * (elapsed / RAMP_DURATION)`

**Constants:**
- `GRACE_PERIOD_DURATION: Duration = 45 seconds`
- `RAMP_DURATION: Duration = 30 seconds`
- `RAMP_START_FRACTION: f64 = 0.25`

**Lifecycle:**
1. Phase detector fires Ground→Cruise → `on_phase_change(Ground, Cruise)` → enters `GracePeriod(now)`
2. Each cycle: `fraction()` checks elapsed time. Grace expires at 45s → auto-transitions to `RampingUp(now)`
3. Ramp fraction increases linearly per cycle. At 75s total → `fraction() = 1.0` → auto-transitions to `Idle`
4. Phase detector fires Cruise→Ground → `on_phase_change(Cruise, Ground)` → immediately resets to `Idle`

**Integration point**: In the coordinator's `execute()`, after backpressure check, multiply the submission count by `transition_throttle.fraction()`. This stacks with backpressure reduction (50% at moderate load), so during takeoff with busy pools, effective submission could be as low as 12.5% (0.25 × 0.5).

**Interaction with turn detector**: Complementary. The grace period covers the guaranteed busy window (takeoff roll + initial climb, 45s). The turn detector covers the variable busy window (departure procedure turns). Neither needs to be perfect alone.

### Data Flow (Cruise Cycle)

```
Telemetry arrives
    │
    ▼
PhaseDetector::update() ──phase change?──▶ TransitionThrottle::on_phase_change()
    │
    ▼
TurnDetector::update() ──stable?──▶ if not, skip cycle
    │
    ▼
CruiseStrategy::plan() → Vec<TileCoord> (up to 3000, Euclidean-sorted)
    │
    ▼
Four-tier filter → removes cached / disk / patched tiles
    │
    ▼
BoundaryPrioritizer::prioritize(position, track, tiles) → re-sorted by DSF boundary urgency
    │
    ▼
Backpressure check (executor_load) → may reduce to 50%
    │
    ▼
TransitionThrottle::fraction() → may further reduce (0.0 during grace, 0.25–1.0 during ramp)
    │
    ▼
Submit top N tiles via DdsClient
```

### What Does NOT Change

- **CruiseStrategy / GroundStrategy** — band geometry, DSF tile selection, DDS tile expansion all unchanged
- **BandCalculator** — lead_distance=2, band_width=2 unchanged
- **PhaseDetector / TurnDetector** — detection logic unchanged
- **Four-tier filter** — caching, patching, disk checks unchanged
- **DdsClient / Executor** — submission mechanism unchanged

## Testing Strategy

### BoundaryPrioritizer
- Pure function → unit tests with known positions, tracks, and tile sets
- Test: westbound aircraft, verify tiles in next western column appear before lateral tiles at same distance
- Test: northbound aircraft, verify tiles in next northern row appear first
- Test: northwest diagonal, verify both column and row boundaries considered
- Test: negligible velocity on one axis (due east), verify only column ordering applied
- Test: tiles behind aircraft deprioritized

### TransitionThrottle
- State machine → unit tests for each state and transition
- Test: Ground→Cruise triggers GracePeriod
- Test: fraction() returns 0.0 during grace period
- Test: auto-transitions to RampingUp after 45s
- Test: fraction() returns 0.25 at ramp start, 1.0 at ramp end, linear between
- Test: Cruise→Ground resets to Idle immediately
- Test: fraction() returns 1.0 when Idle

### Integration
- Coordinator test: verify BoundaryPrioritizer is called after filter, before submission
- Coordinator test: verify TransitionThrottle fraction applied to submission count
- Coordinator test: verify grace period suppresses all submissions after Ground→Cruise

## Fallback: Wavefront Strategy (Approach B)

If boundary-aware ordering proves insufficient (tiles still misaligned in edge cases), the fallback is to replace the tile sort entirely with a discrete wavefront model: fill all tiles in DSF column/row N completely, then column/row N+1, etc. This is more invasive (changes the strategy return type) but guarantees X-Plane-matching order. Kept in reserve.

## Issues Addressed

- Closes #58 (Prefetch refinements: cruise band alignment)
- Closes #62 (Prefetch causes stutters during takeoff)
