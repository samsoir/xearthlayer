# Adaptive Tile-Based Prefetch System

**Technical Design Document**

**Project**: XEarthLayer
**Date**: January 2026
**Version**: 2.0
**Status**: Design Phase

---

## Table of Contents

1. [Overview](#overview)
2. [Goals and Non-Goals](#goals-and-non-goals)
3. [Background Research](#background-research)
4. [System Architecture](#system-architecture)
5. [Three-Window Model](#three-window-model)
6. [SceneryWindow](#scenerywindow)
7. [BoundaryStrategy](#boundarystrategy)
8. [Performance Calibration](#performance-calibration)
9. [Integration Points](#integration-points)
10. [Configuration](#configuration)
11. [Error Handling & Edge Cases](#error-handling--edge-cases)
12. [Testing Strategy](#testing-strategy)

---

## Overview

### Purpose

The Adaptive Prefetch System predicts which scenery tiles X-Plane will request next and pre-loads them before X-Plane needs them. This eliminates loading stutters during flight by ensuring tiles are already in memory or on disk when X-Plane requests them.

### Key Innovation

The system mirrors X-Plane's own loading model — two independent boundary monitors (latitude and longitude) that trigger row/column loads when the aircraft approaches the edge of the loaded scenery area. By observing what X-Plane has loaded via FUSE requests and predicting the next boundary crossing, XEL does what X-Plane is going to do, before X-Plane does it.

### Design Principles

- **Observe, don't assume**: Derive X-Plane's scenery window dimensions from actual FUSE request data, not hardcoded constants
- **Mirror X-Plane's model**: Two independent boundary monitors, position-based triggers, row/column loads
- **Three windows**: Track X-Plane's window, XEL's prefetched window, and the target window separately
- **Performance-aware**: Don't prefetch if the connection can't complete before X-Plane needs it
- **Non-interfering**: Respect executor backpressure and transition throttling

---

## Goals and Non-Goals

### Goals

1. **Predict boundary crossings** — determine which DSF boundary (lat or lon) the aircraft will cross next, using position relative to the observed window edge
2. **Prefetch exact rows/columns** — generate the tiles X-Plane will request, matching its 3-deep strip loading pattern
3. **Observe window dimensions** — derive X-Plane's scenery window size from actual FUSE request data during initial scene load
4. **Stay ahead** — trigger prefetch early enough to complete before X-Plane's reactive loading fires
5. **Handle all flight phases** — turns, diagonal flight, teleports, rendering changes, sparse/ocean regions
6. **Self-calibrate** based on actual system and network performance

### Non-Goals

1. **Predict X-Plane's internal memory state precisely** — we infer with a buffer zone
2. **Flight plan parsing** — future enhancement
3. **Prewarm replacement** — prewarm (loading entire regions) remains separate from prefetch (loading ahead during flight)
4. **Solve extremely slow connections** — if throughput is too low, prefetch won't help

---

## Background Research

This design is based on empirical research documented in:

- **[X-Plane 12 Scenery Loading Behavior v1.1](xplane-scenery-loading-whitepaper.md)** — White paper on X-Plane's loading patterns (5 flights, 10.5+ hours, 4M+ FUSE requests)
- **[Flight Test Data](../../tests/flight-data/)** — Compressed flight logs used for analysis

### Key Findings Informing Design

| Finding | Design Impact |
|---------|---------------|
| X-Plane uses position-only loading (no heading/speed/track) | Use position-based boundary monitors, not track projection |
| Two independent boundary monitors (lat and lon) | Mirror with dual `BoundaryMonitor` structs |
| ROW loads on latitude crossings, COLUMN loads on longitude crossings | Generate rows/columns, not rectangular bands |
| 3 DSF tiles deep per loading event | `load_depth = 3` default |
| 4-6 DSF tiles wide per loading event | Derive from observed window dimensions |
| Trigger at ~0.6° into DSF toward boundary | Prefetch when aircraft within `trigger_distance` of window edge |
| Speed doesn't affect trigger position | Position-based monitors, not time-based |
| No turn detection — purely position vs. loaded area | No TurnDetector needed; monitors handle turns naturally |
| Scenery window: 2 behind + aircraft + 3 ahead | Window dimensions derived empirically |
| Post-turn radius fill over 10-20 minutes | Low-priority lateral gap filling |
| Initial load reveals window dimensions | Derive window size from SceneTracker during initial load |
| Diagonal flight fires both monitors independently | Both monitors check independently each cycle |

### Test Flight Data

| Flight | Route | Key Learning |
|--------|-------|--------------|
| EDDH → EDDF | Southbound | Band loading pattern, 0.6° trigger position |
| EDDH → EKCH | Northeast diagonal | Both lat/lon bands load simultaneously, 29s separation |
| EDDH → LFMN | Southbound with turns | Turn adaptation timing (20-40s), position-only |
| KJFK → EGLL | High-speed transatlantic | Speed independence, ocean behavior |
| LFLL diagonal orbit | Multi-heading (317°/47°/139°) | Dual boundary monitors, row/column separation, post-turn radius fill |

---

## System Architecture

### Component Diagram

```
┌───────────────────────────────────────────────────────────────────────────┐
│                      AdaptivePrefetchCoordinator                          │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │ • Updates SceneryWindow each cycle                                  │  │
│  │ • Selects strategy by flight phase                                  │  │
│  │ • Submits prefetch jobs to DdsClient                                │  │
│  │ • Applies backpressure + transition throttle                        │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│         │                    │                      │                     │
│         ▼                    ▼                      ▼                     │
│  ┌─────────────┐     ┌─────────────────┐     ┌─────────────┐            │
│  │ Ground      │     │ Boundary        │     │ (Future)    │            │
│  │ Strategy    │     │ Strategy        │     │ FlightPlan  │            │
│  ├─────────────┤     ├─────────────────┤     │ Strategy    │            │
│  │ Ring around │     │ Row/column from │     │             │            │
│  │ position    │     │ window monitors │     │             │            │
│  └─────────────┘     └─────────────────┘     └─────────────┘            │
│                              │                                           │
│                              ▼                                           │
│                       ┌─────────────┐                                    │
│                       │ Scenery     │                                    │
│                       │ Window      │                                    │
│                       ├─────────────┤                                    │
│                       │ LatMonitor  │                                    │
│                       │ LonMonitor  │                                    │
│                       │ 3-window    │                                    │
│                       │ tracking    │                                    │
│                       └──────┬──────┘                                    │
│                              │                                           │
│                     ┌────────┴────────┐                                  │
│                     ▼                 ▼                                  │
│              ┌─────────────┐   ┌─────────────┐                          │
│              │SceneTracker │   │  GeoIndex   │                          │
│              │(observations)│   │(spatial DB) │                          │
│              └─────────────┘   └─────────────┘                          │
└───────────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│      DdsClient      │────▶│    JobExecutor       │────▶│   CacheLayer     │
│  (submit prefetch   │     │  (generate tiles)    │     │  (memory + disk) │
│   jobs)             │     │                      │     │                  │
└─────────────────────┘     └──────────────────────┘     └──────────────────┘
```

### Module Structure

```
xearthlayer/src/prefetch/
├── mod.rs                       # Module exports
├── strategy.rs                  # Prefetcher trait (run loop interface)
├── load_monitor.rs              # FuseLoadMonitor trait
├── circuit_breaker.rs           # CircuitBreaker (pause on X-Plane load)
├── adaptive/
│   ├── mod.rs                   # Adaptive module exports
│   ├── coordinator/
│   │   ├── core.rs              # AdaptivePrefetchCoordinator
│   │   ├── runner.rs            # Prefetcher impl (async loop)
│   │   └── constants.rs         # Timing constants
│   ├── strategy.rs              # AdaptivePrefetchStrategy trait, PrefetchPlan
│   ├── ground_strategy.rs       # GroundStrategy (ring-based)
│   ├── boundary_strategy.rs     # BoundaryStrategy (row/column from monitors) [NEW]
│   ├── scenery_window.rs        # SceneryWindow (3-window model) [NEW]
│   ├── boundary_monitor.rs      # BoundaryMonitor (position vs edge) [NEW]
│   ├── phase_detector.rs        # PhaseDetector (Ground/Transition/Cruise)
│   ├── transition_throttle.rs   # TransitionThrottle (takeoff ramp-up)
│   ├── config.rs                # AdaptivePrefetchConfig
│   └── calibration/             # PerformanceCalibration, RollingCalibrator
```

### Removed Components

| Component | File | Reason |
|-----------|------|--------|
| `CruiseStrategy` | `cruise_strategy.rs` | Replaced by `BoundaryStrategy` |
| `BandCalculator` | `band_calculator.rs` | Band model replaced by boundary prediction |
| `BoundaryPrioritizer` | `boundary_prioritizer.rs` | Boundary logic absorbed into `SceneryWindow` monitors |
| `TurnDetector` | `turn_detector.rs` | No longer needed; boundary monitors handle turns naturally |

---

## Three-Window Model

The prefetch system maintains three logical windows that drive all prefetch decisions:

### Window Definitions

```
Direction of flight →

┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   ┌───────────────────────────────┐                                 │
│   │   X-Plane Window              │   ┌───────────────────┐        │
│   │   (inferred from SceneTracker)│   │  Target Window     │        │
│   │                               │   │  (what X-Plane     │        │
│   │          ✈ aircraft           │───▶  will load next)   │        │
│   │                               │   │                    │        │
│   │                               │   │  THIS is what we   │        │
│   └───────────────────────────────┘   │  prefetch          │        │
│                                       └───────────────────┘        │
│   ┌─────────────────────────────────────────────────────────┐      │
│   │   XEL Window                                             │      │
│   │   (what we've already prefetched / cached)               │      │
│   │   Should always CONTAIN the Target Window                │      │
│   └─────────────────────────────────────────────────────────┘      │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

| Window | Source of Truth | Purpose |
|--------|-----------------|---------|
| **X-Plane Window** | SceneTracker (`loaded_regions()`, `loaded_bounds()`) + retention inference with buffer | What X-Plane currently holds in memory (inferred) |
| **XEL Window** | GeoIndex `PrefetchedRegion` layer | What XEL has confirmed as cached (DSF region granularity) |
| **Target Window** | Computed by boundary monitors | What X-Plane will load next — the next row/column at the approaching window edge |

### The Prefetch Decision

```
Prefetch work = Target Window \ XEL Window

If empty → idle (we're ahead of X-Plane)
If non-empty → expand to DDS tiles, filter, submit
```

### Region States (GeoIndex PrefetchedRegion layer)

Each DSF region in the XEL Window has one of four states:

| State | Meaning | Prefetch Behaviour |
|-------|---------|-------------------|
| *absent* | Never evaluated | Include in target diff → submit |
| `InProgress` | Tiles submitted, awaiting completion | Skip bulk submission (prevents duplicate work) |
| `Prefetched` | All tiles confirmed cached | Exclude from target diff entirely |
| `NoCoverage` | No scenery data in this DSF region | Exclude from target diff, skip silently |

This follows a **two-phase commit** pattern:
1. **Reserve**: Region marked `InProgress` when tiles are submitted (prevents duplicate bulk submissions)
2. **Confirm**: Region marked `Prefetched` when tiles are confirmed in cache
3. **Timeout**: If `InProgress` exceeds `stale_region_timeout`, reverts to *absent* for re-evaluation

### Retention Inference

The X-Plane Window is inferred, not observed directly. We use a buffer zone to handle uncertainty:

```
Window (derived from initial load): 6 tall × 8 wide
Buffer: 1° on each side

Total retained area: 8 rows × 10 columns (window + buffer)
Centered on: aircraft position (sliding forward with flight)

Regions inside retained area → RetainedRegion in GeoIndex
Regions previously retained but now outside → evicted from GeoIndex
```

All retention decisions are logged at `DEBUG` level for flight test analysis.

---

## SceneryWindow

The `SceneryWindow` is the core computational model. It derives window dimensions, tracks retention, and predicts boundary crossings via two independent monitors.

### State Machine

```

  Uninitialized ──▶ Assumed ──▶ Measuring ──▶ Ready
       │               │            │            │
  (no telemetry)  (telemetry     (bounds       (window derived
                   but no FUSE    growing,      from observation,
                   activity,      waiting for   normal operation)
                   using          stability)         │
                   defaults)                    ◀────┘
                                              (re-derive on
                                               world rebuild)
```

| State | Condition | Window Source |
|-------|-----------|--------------|
| `Uninitialized` | No telemetry, no FUSE data | No predictions |
| `Assumed` | Telemetry present, no FUSE activity (ocean/sparse start) | Default dimensions (configurable, default 6×8) |
| `Measuring` | FUSE activity detected, bounds still growing | No predictions (initial load in progress) |
| `Ready` | Bounds stable for 2+ seconds (consecutive quiet checks) | Derived from observed `loaded_bounds()` |

### Window Derivation

During X-Plane's initial scene load, the SceneTracker accumulates all requested tiles. When the bounds stop growing (stable for 2 consecutive checks ~1s apart):

```rust
window_rows = loaded_bounds.height().ceil() as usize  // e.g., 6
window_cols = loaded_bounds.width().ceil() as usize    // e.g., 8
```

The aircraft is at the center of this initial load, giving us the complete window dimensions.

### World Rebuild Detection

If a burst arrives where tiles cover >50% of the current window area AND are roughly centered on the aircraft (not at an edge), this indicates X-Plane is rebuilding the world (teleport, rendering settings change). Response:

1. Clear XEL Window (all `PrefetchedRegion` entries)
2. Clear X-Plane Window (all `RetainedRegion` entries)
3. Reset to `Measuring` state
4. Re-derive window dimensions from the new load

### Dual Boundary Monitors

Two independent monitors, one per axis. Each compares the aircraft's position on its axis to the window edges on that axis:

```
SceneryWindow
    ├── LatitudeMonitor
    │     Watches: aircraft latitude vs. window north/south edges
    │     Triggers: ROW load prediction
    │
    └── LongitudeMonitor
          Watches: aircraft longitude vs. window east/west edges
          Triggers: COLUMN load prediction
```

#### Monitor Logic

```rust
pub struct BoundaryMonitor {
    axis: BoundaryAxis,        // Latitude or Longitude
    window_min: f64,           // e.g., lat 47.0 (south edge)
    window_max: f64,           // e.g., lat 53.0 (north edge)
    trigger_distance: f64,     // e.g., 3.0° from edge
}
```

Per-cycle check (position-only, no track dependency):

```
LatitudeMonitor:
    aircraft_lat = 52.55
    window north edge = 53.0
    distance to north edge = 0.45°

    0.45° < trigger_distance (3.0°)? YES
    → Predict: ROW load at lat 53, 54, 55 (3 deep north)

LongitudeMonitor:
    aircraft_lon = 7.2
    window east edge = 11.0
    distance to east edge = 3.8°

    3.8° < trigger_distance (3.0°)? NO
    → No prediction this cycle
```

#### Urgency

```
urgency = 1.0 - (distance_to_edge / trigger_distance)

distance = 0.45°, threshold = 1.5° → urgency = 0.70 (getting close)
distance = 0.15°, threshold = 1.5° → urgency = 0.90 (imminent)
distance = 3.80°, threshold = 1.5° → urgency < 0   (not triggered)
```

#### Window Edge Updates

Window edges update when X-Plane is observed loading the predicted row/column (via SceneTracker's `loaded_regions()` expanding). This keeps the model grounded in observation.

#### Turn Handling

No explicit turn detection. The monitors handle turns naturally:

```
Before turn (heading north):
    LatMonitor: 0.3° from north edge → triggered
    LonMonitor: 3.5° from east edge → not triggered

After turn (heading east):
    LatMonitor: distance to north edge stops shrinking → drops below urgency
    LonMonitor: distance to east edge shrinking → triggers
```

The transition is smooth and automatic. No stabilisation pause, no wasted cycles.

#### Track as Optional Enrichment

The monitors are position-only. However, track can optionally be used for:

- **Direction disambiguation**: If the aircraft is near both north and south edges (small window), track indicates which edge matters
- **Pre-warming**: If far from all edges but track indicates approach, start low-priority prefetch early

This is XEL's advantage over X-Plane — we know the direction of travel before the position-only trigger fires.

### Interface

```rust
pub struct SceneryWindow {
    state: WindowState,
    window_size: Option<(usize, usize)>,
    lat_monitor: BoundaryMonitor,
    lon_monitor: BoundaryMonitor,
    buffer: u8,
    scene_tracker: Arc<dyn SceneTracker>,
    geo_index: Arc<GeoIndex>,
}

impl SceneryWindow {
    /// Called each coordinator cycle. Returns boundary predictions if ready.
    pub fn update(
        &mut self,
        position: (f64, f64),
    ) -> Vec<BoundaryCrossing>;

    /// Optionally refine predictions using track (enrichment, not required).
    pub fn refine_with_track(
        &self,
        predictions: &mut Vec<BoundaryCrossing>,
        track: f64,
    );

    /// Current window dimensions, if derived.
    pub fn window_size(&self) -> Option<(usize, usize)>;

    /// Whether the window model is ready for predictions.
    pub fn is_ready(&self) -> bool;
}
```

---

## BoundaryStrategy

Replaces `CruiseStrategy`. Takes boundary predictions from `SceneryWindow` and produces a concrete list of DDS tiles to prefetch.

### Prediction Data

```rust
pub struct BoundaryCrossing {
    /// Which axis is being crossed
    pub axis: BoundaryAxis,        // Latitude or Longitude
    /// The DSF coordinate of the first row/column to load
    pub dsf_coord: i16,            // e.g., lat 50 or lon 5
    /// How urgent (0.0 = distant, 1.0 = imminent)
    pub urgency: f64,
    /// How many DSF tiles deep to load (typically 3)
    pub depth: u8,
}

pub enum BoundaryAxis {
    Latitude,   // crossing triggers a ROW load
    Longitude,  // crossing triggers a COLUMN load
}
```

### Algorithm

```
Input:  Vec<BoundaryCrossing> from SceneryWindow (sorted by urgency)
        XelWindow (GeoIndex PrefetchedRegion layer)
Output: PrefetchPlan { tiles: Vec<TileCoord>, estimated_time }

For each BoundaryCrossing:
    1. Determine DSF regions in the row/column

       ROW (lat crossing at lat 50, window width lon 3..10):
         Depth 0: [(50,3), (50,4), (50,5), ..., (50,10)]  ← most urgent
         Depth 1: [(51,3), (51,4), (51,5), ..., (51,10)]
         Depth 2: [(52,3), (52,4), (52,5), ..., (52,10)]
         = 24 DSF regions

       COLUMN (lon crossing at lon 11, window height lat 47..52):
         Depth 0: [(47,11), (48,11), (49,11), ..., (52,11)]  ← most urgent
         Depth 1: [(47,12), (48,12), (49,12), ..., (52,12)]
         Depth 2: [(47,13), (48,13), (49,13), ..., (52,13)]
         = 18 DSF regions

    2. Check SceneryIndex coverage for each region
       Regions with no scenery → mark NoCoverage in GeoIndex, skip

    3. Remove regions already in XEL Window (set difference)
       InProgress or Prefetched → skip
       NoCoverage → skip
       Absent → include

    4. For each remaining DSF region, get DDS tiles:
       a. SceneTracker history (most accurate — matches X-Plane's zoom/coverage)
       b. SceneryIndex query (for unseen regions)
       c. Fallback 4×4 grid at zoom 14 (last resort)

    5. Apply four-tier filter:
       Local tracking → Memory cache → Patch exclusion → Disk existence

    6. Order tiles:
       Primary: urgency rank (depth 0 before depth 1 before depth 2)
       Secondary: distance from aircraft position

    7. Mark submitted regions as InProgress in GeoIndex

    8. Append to PrefetchPlan
```

### Diagonal Flight

When both monitors trigger, both predictions appear in the list sorted by urgency. The more urgent axis goes first:

```
LatMonitor: urgency 0.85 (approaching north edge)
LonMonitor: urgency 0.45 (approaching east edge)

Plan order:
  1. Lat row tiles (depth 0, 1, 2) — urgent
  2. Lon column tiles (depth 0, 1, 2) — less urgent

If backpressure reduces batch size, the most urgent axis is served first.
```

### Interface

```rust
impl AdaptivePrefetchStrategy for BoundaryStrategy {
    fn calculate_prefetch(
        &self,
        position: (f64, f64),
        predictions: &[BoundaryCrossing],
        xel_window: &XelWindow,
        cached_tiles: &HashSet<TileCoord>,
    ) -> Option<PrefetchPlan>;
}
```

---

## Performance Calibration

### Calibration Phase

During X-Plane's initial scenery load, the system measures tile generation throughput:

```rust
pub struct PerformanceCalibration {
    /// Tiles generated per second (sustained)
    pub throughput_tiles_per_sec: f64,
    /// Average time to generate one tile (milliseconds)
    pub avg_tile_generation_ms: u64,
    /// Standard deviation of generation time
    pub tile_generation_stddev_ms: u64,
    /// Confidence level (0.0 - 1.0) based on sample size
    pub confidence: f64,
    /// Recommended strategy based on throughput
    pub recommended_strategy: StrategyMode,
    /// Timestamp of calibration
    pub calibrated_at: Instant,
    /// Baseline throughput (for degradation detection)
    pub baseline_throughput: f64,
}

pub enum StrategyMode {
    Aggressive,      // > 30 tiles/sec: high-confidence prefetch
    Opportunistic,   // 10-30 tiles/sec: moderate prefetch
    Disabled,        // < 10 tiles/sec: skip prefetch
}
```

### Rolling Recalibration

Flight conditions change (sparse oceanic → photogrammetry-heavy metro). The system adapts:

- Every 15 minutes: update throughput from sliding 5-minute window
- Degradation: observed < 70% of baseline → downgrade mode
- Recovery: observed > 90% of baseline → upgrade mode

---

## Integration Points

### SceneTracker Integration

The `SceneryWindow` reads from the `SceneTracker` to:

1. **Derive window dimensions**: `loaded_bounds()` during initial load
2. **Track loaded regions**: `loaded_regions()` to update X-Plane Window
3. **Detect bursts**: `subscribe_bursts()` for window derivation stability check

The SceneTracker remains unchanged — it is a pure observer. The `SceneryWindow` is the interpretive layer.

### GeoIndex Integration

The `GeoIndex` stores spatial state via type-keyed layers:

| Layer | Key | Value | Updated By |
|-------|-----|-------|------------|
| `PatchCoverage` (existing) | DSF region | patch name | OrthoUnionIndex (startup) |
| `RetainedRegion` (new) | DSF region | `()` | SceneryWindow (each cycle) |
| `PrefetchedRegion` (new) | DSF region | `RegionState` | BoundaryStrategy (on submit/complete) |

### DdsClient Integration

Prefetch jobs are submitted through the existing `DdsClient` trait with `Priority::Prefetch`. The priority queue ensures ON_DEMAND requests (FUSE) always jump ahead.

### Executor Backpressure

The coordinator checks `executor_load()` each cycle:

- Load > 80%: defer entire cycle
- Load > 50%: submit reduced batch (50%)
- Load < 50%: full submission

### TransitionThrottle

The existing `TransitionThrottle` ramps prefetch from 25% to 100% over 30 seconds after takeoff, preventing competition with X-Plane's post-takeoff scene loading. Unchanged.

### Four-Tier Filter Chain

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Prefetch Filter Chain                            │
│                                                                         │
│  1. LOCAL TRACKING (HashSet) - O(1)                                     │
│     Skip tiles submitted this session                                   │
│                                                                         │
│  2. MEMORY CACHE (async) - moka LRU query                              │
│     Skip tiles already generated                                        │
│                                                                         │
│  3. PATCHED REGION EXCLUSION (GeoIndex PatchCoverage)                   │
│     Skip tiles in DSF regions owned by scenery patches                  │
│                                                                         │
│  4. DISK EXISTENCE (filesystem probe) - slow                            │
│     Skip tiles from installed packages and XEL cache                    │
│     Checks: ZL, BI, GO2, GO filename patterns                          │
│                                                                         │
│  Only tiles passing ALL four filters are submitted for download         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Configuration

### Configuration Options

```ini
[prefetch]
# Master enable/disable for the prefetch system
# Values: true, false
# Default: true
enabled = true

# Strategy mode selection
# Values:
#   auto          - Select based on calibration (recommended)
#   aggressive    - Always use high-confidence prefetch
#   opportunistic - Moderate prefetch
#   disabled      - Never prefetch
# Default: auto
mode = auto

# Automatically disable prefetch if system performance is too low
# Values: auto, never
# Default: auto
low_performance_killswitch = auto

# Maximum tiles per prefetch cycle
# Caps tiles submitted in a single prefetch operation
# Range: 500 - 10000
# Default: 3000
max_tiles_per_cycle = 3000

# Ground strategy ring radius (degrees)
# Radius of prefetch ring when aircraft is on ground
# Range: 0.5 - 2.0
# Default: 1.0
ground_ring_radius = 1.0

# ═══════════════════════════════════════════════════════════════════════════
# BOUNDARY-DRIVEN PREFETCH SETTINGS
# ═══════════════════════════════════════════════════════════════════════════
# These control the boundary monitor behaviour during cruise flight.
# Defaults are based on empirical research from 5 flight tests (10.5+ hours)
# and match X-Plane 12's observed scenery loading patterns.

# Distance from window edge to start prefetching (degrees)
# When the aircraft is within this distance of the X-Plane window edge,
# the boundary monitor triggers prefetch for the next row/column.
# Range: 0.5 - 3.0
# Default: 3.0
trigger_distance = 3.0

# DSF tiles deep per row/column load
# Matches X-Plane's observed 3-deep strip loading pattern.
# Range: 1 - 5
# Default: 3
load_depth = 3

# Buffer around inferred X-Plane window (degrees)
# Adds hysteresis to retention inference. Regions outside window + buffer
# are considered evicted by X-Plane.
# Range: 0 - 3
# Default: 1
window_buffer = 1

# Seconds before InProgress region reverts to absent
# Safety timeout for the two-phase commit. If tiles haven't confirmed
# as cached within this time, the region is re-evaluated.
# Range: 30 - 600
# Default: 120
stale_region_timeout = 120

# Fallback window dimensions when no FUSE data available
# Used when starting in uncovered area (ocean, polar regions).
# Once X-Plane loads scenery, these are replaced by observed dimensions.
# Default: 6 rows, 8 columns (based on whitepaper observations)
default_window_rows = 6
default_window_cols = 8
```

### Deprecated Settings

The following settings from v1.x are deprecated and should be added to `DEPRECATED_KEYS` in `config/upgrade.rs`:

| Setting | Replacement | Reason |
|---------|-------------|--------|
| `trigger_position` | `trigger_distance` | Position within DSF replaced by distance from window edge |
| `lead_distance` | `load_depth` | Static lead replaced by dynamic boundary prediction |
| `band_width` | (derived from window) | Derived from observed window dimensions |
| `track_stability_threshold` | (removed) | No TurnDetector — monitors handle turns naturally |
| `turn_threshold` | (removed) | No TurnDetector |
| `track_stability_duration` | (removed) | No TurnDetector |
| `strategy` | (removed) | No longer selecting between ground/cruise/flight_plan — coordinator auto-selects |
| `time_budget_margin` | (removed) | Time budget replaced by boundary urgency |

### Calibration Thresholds

```ini
[prefetch.calibration]
aggressive_threshold = 30     # tiles/sec for aggressive mode
opportunistic_threshold = 10  # tiles/sec for opportunistic mode
sample_duration = 60          # seconds to measure during initial load
```

---

## Error Handling & Edge Cases

### Window Not Yet Derived

During `Uninitialized` or `Measuring` states, the boundary monitors have no window edges. The coordinator falls through to:
- `GroundStrategy` if on the ground
- Skip prefetch if airborne (TransitionThrottle handles takeoff ramp)

### Ocean / Sparse Regions

When the aircraft flies over areas with no XEL scenery coverage:

1. **FUSE activity drops** — X-Plane has nothing to request
2. **Telemetry continues** — we know the aircraft is still flying
3. **Monitors continue** — tracking position vs. window edges
4. **SceneryIndex consulted for target regions** — if target regions ahead have coverage, prefetch them NOW even though the aircraft is over uncovered area
5. **Uncovered target regions** → marked `NoCoverage`, skipped silently

The critical transition is **no-coverage → coverage**: the monitors see the window edge approaching covered regions and prefetch them before the aircraft arrives.

### Starting in No-Coverage Area

If the aircraft spawns over ocean/polar with no scenery:
1. No FUSE activity → cannot derive window dimensions
2. Telemetry present → enter `Assumed` state with default window (6×8)
3. Monitors operate with assumed dimensions
4. When aircraft enters coverage and X-Plane loads → transition to `Measuring` → `Ready`
5. Observed dimensions replace assumed dimensions

### Teleport / World Rebuild

Detection: burst covers >50% of current window area AND is centered on aircraft (not at an edge).

Response:
1. Clear XEL Window and X-Plane Window from GeoIndex
2. Reset to `Measuring` state
3. Re-derive window dimensions from new load

### Aircraft Stationary in Cruise

Orbiting or circling: aircraft doesn't approach any edge → monitors don't trigger → idle. This is correct — no prefetch needed, the area is already covered.

### Loss of Telemetry

Existing staleness check (10s threshold). Monitors hold last known state. No special handling.

### Partial Tile Failure

If some tiles in a submitted region fail:
- Region stays `InProgress`
- After `stale_region_timeout` (120s), reverts to *absent*
- Next evaluation cycle: target diff finds the region, re-submits
- Four-tier filter handles individual tile dedup (only failed tiles re-submitted)

---

## Testing Strategy

### Unit Tests

| Component | Test Focus |
|-----------|-----------|
| `BoundaryMonitor` | Trigger at correct distance; no trigger when far; urgency calculation; both edges (north/south or east/west) |
| `SceneryWindow` | Window derivation from mock bounds; stability check; retention inference with buffer; world rebuild detection; `Assumed` → `Measuring` → `Ready` transitions |
| `BoundaryStrategy` | Row generation for lat crossing; column generation for lon crossing; diagonal (both monitors); set difference with XEL Window; depth ordering; `NoCoverage` handling |
| `RegionState` | Two-phase commit: absent → InProgress → Prefetched; timeout revert; NoCoverage |
| `GeoIndex layers` | RetainedRegion add/remove; PrefetchedRegion state transitions |

### Integration Tests

| Scenario | Validates |
|----------|-----------|
| Straight flight north through 3 DSF boundaries | Rows prefetched in correct order, XEL Window grows |
| Diagonal flight (NE) | Both monitors trigger, interleaved by urgency |
| Turn from north to east | Lat monitor stops, lon monitor starts, no pause |
| Scene start → initial load → first prefetch | Window derivation, `Measuring` → `Ready` |
| Ocean → land transition | `NoCoverage` regions skipped, covered regions prefetched ahead |
| Start over ocean with telemetry | `Assumed` state, default dimensions, transition on coverage |
| Partial failure + timeout | Region reverts, re-evaluated, only failed tiles re-submitted |
| Teleport | World rebuild detected, state reset, re-derive |

### Flight Test Validation

Replay flight 5 (LFLL diagonal orbit) log data through the new system and verify:
- Prefetch targets match DSF regions where FUSE cache misses actually occurred
- No more "prefetch idle 90% of cycles"
- Target Window regions overlap with actual X-Plane loading events
- Both row and column loads are predicted before X-Plane triggers them

---

## References

- [X-Plane Scenery Loading Whitepaper v1.1](xplane-scenery-loading-whitepaper.md) — Empirical loading behaviour research
- [Flight Test Data](../../tests/flight-data/) — Compressed flight logs
- [Job Executor Design](job-executor-design.md) — Daemon architecture, job/task traits
- [GeoIndex Design](geo-index-design.md) — Geospatial reference database

---

**Document Version History**

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | January 2026 | Initial design based on flight test research |
| 1.1 | January 2026 | Added rolling recalibration, priority-based backpressure, design considerations |
| 2.0 | March 2026 | Major revision: replaced band-based cruise strategy with boundary-driven model. Three-window architecture (X-Plane / XEL / Target). Dual boundary monitors (lat/lon). Removed CruiseStrategy, BandCalculator, BoundaryPrioritizer, TurnDetector. Added SceneryWindow with window derivation, retention inference, and `Assumed` state for no-coverage starts. Two-phase commit for region tracking. GeoIndex extended with RetainedRegion and PrefetchedRegion layers. Updated configuration (deprecated 7 settings, added 6 new). Based on whitepaper v1.1 findings from 5 flights / 10.5+ hours of data. |
