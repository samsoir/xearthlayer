# Adaptive Tile-Based Prefetch System

**Technical Design Document**

**Project**: XEarthLayer
**Date**: January 2026
**Version**: 1.1
**Status**: Design Phase

---

## Table of Contents

1. [Overview](#overview)
2. [Goals and Non-Goals](#goals-and-non-goals)
3. [Background Research](#background-research)
4. [System Architecture](#system-architecture)
5. [Performance Calibration](#performance-calibration)
6. [Strategy Pattern](#strategy-pattern)
7. [Adaptive Algorithm](#adaptive-algorithm)
8. [Integration Points](#integration-points)
9. [Implementation Plan](#implementation-plan)
10. [Testing Strategy](#testing-strategy)

---

## Overview

### Purpose

The Adaptive Prefetch System predicts which scenery tiles X-Plane will request next and pre-loads them before X-Plane needs them. This eliminates loading stutters during flight by ensuring tiles are already in memory or on disk when X-Plane requests them.

### Key Innovation

Unlike simple distance-based prefetch, this system:

1. **Adapts to system performance** - Adjusts prefetch depth based on measured tile generation throughput
2. **Uses multiple strategies** - Different algorithms for ground, cruise, and flight plan phases
3. **Matches X-Plane's behavior** - Prefetches complete bands, not individual tiles, matching how X-Plane actually loads scenery

### Design Principles

- **Complete bands or nothing**: Partial prefetch is wasteful; commit to entire bands
- **Performance-aware**: Don't prefetch if the connection can't complete before X-Plane needs it
- **Non-interfering**: Pause prefetch when X-Plane is actively loading (circuit breaker)
- **Strategy-based**: Clean separation of prefetch logic by flight phase

---

## Goals and Non-Goals

### Goals

1. **Reduce scenery loading stutters** during cruise flight by having tiles pre-cached
2. **Self-calibrate** based on actual system and network performance
3. **Support multiple flight phases** with appropriate prefetch strategies
4. **Integrate cleanly** with existing XEarthLayer architecture (circuit breaker, job executor, cache)
5. **Minimize wasted work** by only prefetching tiles X-Plane will actually request

### Non-Goals

1. **Solve extremely slow connections** - If throughput is too low, prefetch won't help
2. **Predict turns** - React to heading changes, don't try to anticipate them
3. **Flight plan parsing** - Future enhancement, not in initial implementation
4. **Prewarm replacement** - Prewarm (loading entire regions) remains separate from prefetch (loading ahead during flight)

---

## Background Research

This design is based on empirical research documented in:

- **[X-Plane 12 Scenery Loading Behavior](xplane-scenery-loading-whitepaper.md)** - White paper on X-Plane's loading patterns
- **[Prefetch Flight Test Plan](prefetch-flight-test-plan.md)** - Raw flight test data and analysis

### Key Findings Informing Design

| Finding | Design Impact |
|---------|---------------|
| X-Plane triggers loading at 0.6° into DSF tile | Trigger prefetch at 0.3-0.5° to complete before X-Plane |
| X-Plane loads 2-3° ahead | Match this lead distance in prefetch depth |
| X-Plane loads complete bands (2-4 DSF tiles wide) | Prefetch entire bands, not individual tiles |
| Diagonal flight loads both lat and lon bands | Handle both directions for diagonal headings |
| Speed doesn't affect trigger position | Use position-based triggers, not time-based |
| Turn adaptation takes 20-40 seconds | Recalculate bands after heading stabilizes |
| Initial load is 12° squared (4-5 min) | Use this phase for performance calibration |

### Test Flight Data

| Flight | Route | Key Learning |
|--------|-------|--------------|
| EDDH → EDDF | Southbound | Band loading pattern, 0.6° trigger position |
| EDDH → EKCH | Northeast diagonal | Both lat/lon bands load simultaneously |
| EDDH → LFMN | Southbound with turns | Turn adaptation timing (20-40s) |
| KJFK → EGLL | High-speed transatlantic | Speed independence, ocean behavior |

---

## System Architecture

### Component Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        AdaptivePrefetchCoordinator                          │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │ • Monitors flight phase transitions                                   │  │
│  │ • Holds PerformanceCalibration data                                   │  │
│  │ • Selects active PrefetchStrategy                                     │  │
│  │ • Submits prefetch jobs to DdsClient                                  │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│         │                    │                      │                       │
│         ▼                    ▼                      ▼                       │
│  ┌─────────────┐     ┌─────────────┐        ┌─────────────┐                 │
│  │GroundStrategy│    │CruiseStrategy│       │(Future)     │                 │
│  ├─────────────┤     ├─────────────┤        │FlightPlan   │                 │
│  │Ring around  │     │Bands ahead  │        │Strategy     │                 │
│  │position     │     │of heading   │        │             │                 │
│  └─────────────┘     └─────────────┘        └──────────────┘                │
└─────────────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│    CircuitBreaker   │────▶│      DdsClient      │────▶│  JobExecutor     │
│  (pause on X-Plane  │     │  (submit prefetch   │     │  (generate tiles)│
│   loading)          │     │   jobs)             │     │                  │
└─────────────────────┘     └─────────────────────┘     └──────────────────┘
         │                                                       │
         ▼                                                       ▼
┌─────────────────────┐                               ┌──────────────────┐
│   SceneryIndex      │                               │   CacheLayer     │
│  (lookup tiles in   │                               │  (memory + disk) │
│   next band)        │                               │                  │
└─────────────────────┘                               └──────────────────┘
```

### Module Structure

```
xearthlayer/src/prefetch/
├── mod.rs                    # Module exports
├── coordinator.rs            # AdaptivePrefetchCoordinator
├── calibration.rs            # PerformanceCalibration
├── strategy/
│   ├── mod.rs               # PrefetchStrategy trait
│   ├── ground.rs            # GroundStrategy
│   └── cruise.rs            # CruiseStrategy
├── band_calculator.rs        # Calculate tiles in a band
└── phase_detector.rs         # Detect ground/cruise/approach phases

# Future (not in initial implementation):
# └── strategy/flight_plan.rs  # FlightPlanStrategy
```

---

## Performance Calibration

### Calibration Phase

During X-Plane's initial 12° × 12° scenery load, the system measures:

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

    /// Baseline throughput from initial calibration (for degradation detection)
    pub baseline_throughput: f64,
}

pub enum StrategyMode {
    /// High throughput: Can prefetch at 0.3° trigger
    Aggressive,
    /// Medium throughput: Prefetch on circuit breaker close
    Opportunistic,
    /// Low throughput: Skip prefetch (won't complete in time)
    Disabled,
}
```

### Calibration Thresholds

| Metric | Aggressive | Opportunistic | Disabled |
|--------|------------|---------------|----------|
| Throughput | > 30 tiles/sec | 10-30 tiles/sec | < 10 tiles/sec |
| Band completion time | < 60% of window | < 100% of window | > 100% of window |

### Calculation

```
Given:
  - Typical band size: ~3,000 tiles
  - Time to next DSF boundary: varies by speed
  - Throughput: measured during calibration

Band completion time = 3,000 / throughput
Time available = distance_to_boundary / ground_speed

If band_completion_time < time_available * 0.6:
    → AGGRESSIVE (plenty of time)
Else if band_completion_time < time_available:
    → OPPORTUNISTIC (start early, should complete)
Else:
    → DISABLED (won't complete, skip prefetch)
```

### Rolling Recalibration

Initial calibration happens during X-Plane's 12° load, but flight conditions change (e.g., flying from sparse oceanic tiles into photogrammetry-heavy metro areas). The system performs **rolling recalibration** to adapt:

```
ROLLING RECALIBRATION
┌─────────────────────────────────────────────────────────────────┐
│ Periodic Recalibration:                                         │
│   • Every 15 minutes: Update throughput metrics                 │
│   • Uses sliding window of last 5 minutes of tile generation    │
│                                                                 │
│ Degradation Detection:                                          │
│   • If observed throughput < 70% of baseline → downgrade mode   │
│   • AGGRESSIVE → OPPORTUNISTIC → DISABLED                       │
│   • Prevents over-aggressive prefetch when flying into          │
│     complex scenery areas                                       │
│                                                                 │
│ Recovery:                                                       │
│   • If throughput recovers to > 90% of baseline → upgrade mode  │
│   • DISABLED → OPPORTUNISTIC → AGGRESSIVE                       │
│   • Allows system to adapt when flying back to simpler areas    │
└─────────────────────────────────────────────────────────────────┘
```

---

## Strategy Pattern

### Trait Definition

```rust
/// Strategy for calculating which tiles to prefetch
pub trait PrefetchStrategy: Send + Sync {
    /// Calculate the next batch of tiles to prefetch
    fn calculate_prefetch(
        &self,
        position: &AircraftPosition,
        heading: f64,
        calibration: &PerformanceCalibration,
        scenery_index: &SceneryIndex,
        already_cached: &HashSet<TileCoord>,
    ) -> PrefetchPlan;

    /// Human-readable name for logging
    fn name(&self) -> &'static str;

    /// Whether this strategy should be active given current conditions
    fn is_applicable(&self, phase: &FlightPhase) -> bool;
}

pub struct PrefetchPlan {
    /// Tiles to prefetch, in priority order
    pub tiles: Vec<TileCoord>,

    /// Estimated time to complete (milliseconds)
    pub estimated_completion_ms: u64,

    /// Strategy that generated this plan
    pub strategy: &'static str,
}
```

### Ground Strategy

**Purpose**: Prepare for any departure direction while aircraft is on the ground.

**Algorithm**:
```
GROUND STRATEGY
┌─────────────────────────────────────────────────────────────────┐
│ Condition: GS < 40kt AND AGL < 20ft                             │
│                                                                 │
│ Shape: 1° ring around current position                          │
│                                                                 │
│        N (+1° lat)                                              │
│           ╱╲                                                    │
│      W ◄── ✈️ ──► E  (±1° lon)                                  │
│           ╲╱                                                    │
│        S (-1° lat)                                              │
│                                                                 │
│ Excludes: Tiles X-Plane already loaded (12° × 12° area)         │
│ Priority: Low (yield to FUSE requests)                          │
└─────────────────────────────────────────────────────────────────┘
```

### Cruise Strategy

**Purpose**: Load scenery ahead of the aircraft based on current track.

**Algorithm**:
```
CRUISE STRATEGY
┌─────────────────────────────────────────────────────────────────┐
│ Condition: GS > 40kt OR AGL > 20ft                              │
│            (supports rotorcraft and experimental aircraft)      │
│                                                                 │
│ 1. Determine track quadrant (track, not heading - includes wind)│
│    - Cardinal (N/S/E/W): Single band direction                  │
│    - Diagonal (NE/SE/SW/NW): Both lat AND lon bands             │
│                                                                 │
│ 2. Calculate band extent:                                       │
│    - Depth: 2-3° ahead (matching X-Plane's lead distance)       │
│    - Width: 3-4 DSF tiles perpendicular to travel               │
│                                                                 │
│ 3. Query scenery index for tiles in band                        │
│                                                                 │
│ 4. Filter out already-cached tiles                              │
│                                                                 │
│ 5. Order by distance from aircraft (closest first)              │
│                                                                 │
│ Trigger: Position-based (0.3-0.5°) or circuit breaker close     │
│ Priority: Medium                                                │
└─────────────────────────────────────────────────────────────────┘
```

### Turn Detection

**Purpose**: Detect when aircraft has completed a turn and track has stabilized.

**Algorithm**:
```
TURN DETECTION
┌─────────────────────────────────────────────────────────────────┐
│ Use TRACK (ground path), not HEADING (nose direction)           │
│ Track includes wind compensation = actual ground vector         │
│                                                                 │
│ Track Stabilized When:                                          │
│   • Track deviation < ±5° for 10 consecutive seconds            │
│                                                                 │
│ On Turn Detected:                                               │
│   1. Mark prefetch bands as stale                               │
│   2. Wait for track to stabilize (above criteria)               │
│   3. Recalculate bands for new track direction                  │
│   4. Resume prefetch with new band geometry                     │
│                                                                 │
│ Turn Threshold: Track change > 15° from last stable track       │
└─────────────────────────────────────────────────────────────────┘
```

### Edge Cases

| Scenario | Handling | Notes |
|----------|----------|-------|
| **180° reversal** | Cruise strategy handles naturally | New track = opposite direction, bands recalculate |
| **Holding pattern** | Typically within loaded scenery | Standard holds are 1-3nm legs; scenery usually cached |
| **Slow rotorcraft** | Use AGL > 20ft trigger | Helicopters at 50kt still get cruise strategy |
| **Circling approach** | Track changes trigger recalculation | Each stabilized leg gets appropriate bands |
| **Strong crosswind** | Track-based (not heading) handles this | Prefetch follows actual ground path |

### Flight Plan Strategy (Future)

**Purpose**: Pre-load entire route when flight plan is available.

**Algorithm**:
```
FLIGHT PLAN STRATEGY (Future Implementation)
┌─────────────────────────────────────────────────────────────────┐
│ Condition: Valid flight plan loaded                             │
│                                                                 │
│ 1. Parse waypoints from flight plan                             │
│                                                                 │
│ 2. Calculate corridor along route:                              │
│    - 5nm either side of route                                   │
│    - All DSF tiles intersecting corridor                        │
│                                                                 │
│ 3. Pre-load tiles starting from current position                │
│    along route                                                  │
│                                                                 │
│ Priority: High (known route > predicted heading)                │
└─────────────────────────────────────────────────────────────────┘
```

### Strategy Selection State Machine

```
┌──────────┐      airborne (GS>40 OR AGL>20)    ┌──────────┐
│  GROUND  │ ─────────────────────────────────► │  CRUISE  │
│          │                                    │          │
└──────────┘ ◄───────────────────────────────── └──────────┘
              on ground (GS<40 AND AGL<20)           │
                                                     │
                                                     ▼
                                              ┌───────────┐
                                              │ DISABLED  │
                                              │ (too slow)│
                                              └───────────┘
```

**Note**: Flight Plan strategy is a future enhancement and is not included in the initial implementation. When implemented, it will integrate as a higher-priority strategy that overrides Cruise when a valid flight plan is loaded.

---

## Adaptive Algorithm

### Core Loop

```rust
impl AdaptivePrefetchCoordinator {
    pub async fn run(&mut self, mut shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,

                // Wait for circuit breaker to close (X-Plane done loading)
                _ = self.circuit_breaker.wait_for_close() => {
                    self.on_circuit_breaker_close().await;
                }

                // Or wait for position trigger
                _ = self.position_trigger.wait() => {
                    self.on_position_trigger().await;
                }
            }
        }
    }

    async fn on_circuit_breaker_close(&mut self) {
        // Opportunistic trigger: X-Plane finished loading
        if self.calibration.recommended_strategy == StrategyMode::Opportunistic {
            self.execute_prefetch().await;
        }
    }

    async fn on_position_trigger(&mut self) {
        // Position-based trigger: 0.3° into DSF tile
        if self.calibration.recommended_strategy == StrategyMode::Aggressive {
            self.execute_prefetch().await;
        }
    }

    async fn execute_prefetch(&mut self) {
        // 1. Get current flight phase
        let phase = self.phase_detector.current_phase();

        // 2. Select appropriate strategy
        let strategy = self.select_strategy(&phase);

        // 3. Calculate prefetch plan
        let plan = strategy.calculate_prefetch(
            &self.position,
            self.heading,
            &self.calibration,
            &self.scenery_index,
            &self.cached_tiles,
        );

        // 4. Validate we can complete in time
        if !self.can_complete_in_time(&plan) {
            tracing::debug!("Skipping prefetch: insufficient time");
            return;
        }

        // 5. Submit jobs to executor
        for tile in plan.tiles {
            self.dds_client.request_tile(tile, Priority::Prefetch).await;
        }
    }
}
```

### Time Budget Calculation

```rust
fn can_complete_in_time(&self, plan: &PrefetchPlan) -> bool {
    // Calculate time until X-Plane's 0.6° trigger
    let distance_to_trigger = self.distance_to_dsf_boundary()
        + 0.6; // X-Plane's trigger position in next tile

    let time_available_secs = distance_to_trigger
        / self.ground_speed_deg_per_sec();

    let time_required_secs = plan.estimated_completion_ms as f64 / 1000.0;

    // Apply safety margin (70%)
    time_required_secs <= time_available_secs * 0.7
}
```

---

## Integration Points

### Circuit Breaker Integration

The existing `CircuitBreaker` component detects when X-Plane is actively loading scenery (>50 requests/sec). The prefetch system:

1. **Pauses** when circuit breaker opens (X-Plane loading)
2. **Resumes** when circuit breaker closes (X-Plane idle)
3. **Uses breaker close** as opportunistic prefetch trigger

### DdsClient Integration

Prefetch jobs are submitted through the existing `DdsClient` trait:

```rust
// Prefetch jobs use lower priority than FUSE requests
self.dds_client.request_tile(
    tile,
    Priority::Prefetch(distance_from_aircraft as u32),
).await;
```

#### Priority-Based Backpressure

The `JobExecutor` uses priority ordering (`ON_DEMAND > PREFETCH > HOUSEKEEPING`) which provides natural backpressure:

- When X-Plane requests tiles (ON_DEMAND), they jump the queue ahead of pending prefetch jobs
- Prefetch jobs effectively "pause" while ON_DEMAND requests are being served
- No explicit cancellation needed—priority inversion handles contention naturally
- When circuit breaker opens (X-Plane actively loading), new prefetch submissions pause
- Pending prefetch jobs remain queued but stay behind any ON_DEMAND requests

This design avoids complex job cancellation logic while ensuring X-Plane's requests are never delayed by prefetch work.

### Scenery Index Integration

The `SceneryIndex` provides tile lookup for band calculation:

```rust
// Get all tiles in next latitude band
let tiles = scenery_index.tiles_in_dsf(
    lat_floor..lat_floor + 3,  // 3° ahead
    lon_floor - 2..lon_floor + 2,  // 4° wide
);
```

### Metrics Integration

New metrics for prefetch monitoring:

```rust
// Prefetch performance metrics
prefetch_tiles_submitted: Counter,
prefetch_tiles_completed: Counter,
prefetch_time_budget_remaining: Gauge,
prefetch_strategy_active: Gauge,  // 0=disabled, 1=ground, 2=cruise, 3=fpl
calibration_throughput: Gauge,
```

### Configuration

Prefetching is configured in the `[prefetch]` section of `~/.xearthlayer/config.ini`.

#### Configuration Options

```ini
[prefetch]
# Master enable/disable for the prefetch system
# Values: true, false
# Default: true
enabled = true

# Strategy mode selection
# Values:
#   auto          - Select based on calibration (recommended)
#   aggressive    - Always use position-based trigger (fast connections)
#   opportunistic - Always use circuit breaker trigger (moderate connections)
#   disabled      - Never prefetch (manual override)
# Default: auto
mode = auto

# Automatically disable prefetch if system performance is too low
# Values:
#   auto  - Disable if throughput < threshold during calibration
#   never - Always attempt prefetch regardless of performance
# Default: auto
low_performance_killswitch = auto

# Strategy selection (which prefetch algorithm to use)
# Values:
#   auto        - Automatically select based on flight phase (recommended)
#   ground      - Force ground strategy (ring around position)
#   cruise      - Force cruise strategy (bands ahead of heading)
#   flight_plan - Force flight plan strategy (along route, requires FPL)
# Default: auto
#
# Note: Forcing a strategy overrides the automatic phase detection.
# This is not recommended for normal use - the 'auto' mode handles
# transitions between ground and cruise automatically. Only use this
# if you have a specific reason to lock to one strategy.
strategy = auto

# ═══════════════════════════════════════════════════════════════════════════
# ADVANCED TUNING OPTIONS - DANGER ZONE
# ═══════════════════════════════════════════════════════════════════════════
# The options below control the internal behavior of the prefetch algorithm.
# These defaults are based on empirical research from flight testing and
# match X-Plane 12's observed scenery loading patterns.
#
# WARNING: Do not modify these values unless you fully understand the
# prefetch system and have a specific reason to change them. Incorrect
# values can cause:
#   - Prefetch completing too late (no benefit)
#   - Prefetch interfering with X-Plane's loading (worse performance)
#   - Excessive resource usage (memory/network)
#
# If you're experiencing issues, try adjusting 'mode' above first.
# ═══════════════════════════════════════════════════════════════════════════

# Position trigger threshold (degrees into DSF tile)
# When mode=aggressive, prefetch triggers at this position
# Range: 0.1 - 0.5 (must be < X-Plane's 0.6 trigger)
# Default: 0.35
# trigger_position = 0.35

# Lead distance (degrees ahead to prefetch)
# How far ahead of aircraft to prefetch tiles
# Range: 1 - 4
# Default: 2
# lead_distance = 2

# Band width (DSF tiles perpendicular to travel)
# Width of prefetch band on each side of flight path
# Range: 1 - 4
# Default: 2 (meaning ±2 DSF tiles = 5 tiles wide total)
# band_width = 2

# Maximum tiles per prefetch cycle
# Caps tiles submitted in a single prefetch operation
# Range: 500 - 10000
# Default: 3000
# max_tiles_per_cycle = 3000

# Ground strategy ring radius (degrees)
# Radius of prefetch ring when aircraft is on ground
# Range: 0.5 - 2.0
# Default: 1.0
# ground_ring_radius = 1.0

# Time budget safety margin (percentage as decimal)
# Only prefetch if estimated time < available time × margin
# Range: 0.5 - 0.9
# Default: 0.7 (complete with 30% time buffer)
# time_budget_margin = 0.7
```

#### Calibration Thresholds

These thresholds determine mode selection when `mode = auto`:

```ini
[prefetch.calibration]
# Minimum throughput for aggressive mode (tiles/sec)
# Above this threshold: use position-based trigger
# Default: 30
aggressive_threshold = 30

# Minimum throughput for opportunistic mode (tiles/sec)
# Above this threshold: use circuit breaker trigger
# Below this threshold: disable prefetch (if killswitch=auto)
# Default: 10
opportunistic_threshold = 10

# Calibration sample duration (seconds)
# How long to measure throughput during initial load
# Longer = more accurate, but delays flight start
# Range: 30 - 300
# Default: 60
sample_duration = 60
```

#### Configuration Precedence

1. **Manual override**: `mode = disabled` always disables prefetch
2. **Performance killswitch**: If `low_performance_killswitch = auto` and throughput < `opportunistic_threshold`, prefetch disabled
3. **Calibration**: If `mode = auto`, calibration determines aggressive vs opportunistic
4. **User preference**: Explicit `mode = aggressive` or `mode = opportunistic` overrides calibration

#### Recommended Configurations

**High-performance system (fast internet, NVMe)**:
```ini
[prefetch]
enabled = true
mode = auto
lead_distance = 3
band_width = 3
max_tiles_per_cycle = 5000
```

**Conservative (moderate internet, SSD)**:
```ini
[prefetch]
enabled = true
mode = opportunistic
lead_distance = 2
band_width = 2
max_tiles_per_cycle = 2000
time_budget_margin = 0.8
```

**Minimal (slow internet, want to avoid overhead)**:
```ini
[prefetch]
enabled = true
mode = opportunistic
lead_distance = 1
band_width = 1
max_tiles_per_cycle = 1000
low_performance_killswitch = auto
```

**Disabled (debugging or testing)**:
```ini
[prefetch]
enabled = false
```

#### Rust Configuration Struct

```rust
#[derive(Debug, Clone)]
pub struct PrefetchConfig {
    pub enabled: bool,
    pub mode: PrefetchMode,
    pub strategy: PrefetchStrategy,
    pub low_performance_killswitch: KillswitchMode,
    pub trigger_position: f64,
    pub lead_distance: u8,
    pub band_width: u8,
    pub max_tiles_per_cycle: u32,
    pub ground_ring_radius: f64,
    pub time_budget_margin: f64,
    pub calibration: CalibrationConfig,
}

#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    pub aggressive_threshold: f64,
    pub opportunistic_threshold: f64,
    pub sample_duration_secs: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrefetchMode {
    /// Select trigger mode based on calibration
    Auto,
    /// Position-based trigger (0.3° into DSF tile)
    Aggressive,
    /// Circuit breaker trigger (when X-Plane finishes loading)
    Opportunistic,
    /// Prefetch disabled
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrefetchStrategy {
    /// Automatically select based on flight phase
    Auto,
    /// Force ground strategy (ring around position)
    Ground,
    /// Force cruise strategy (bands ahead of heading)
    Cruise,
    /// Force flight plan strategy (along route)
    FlightPlan,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KillswitchMode {
    Auto,
    Never,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: PrefetchMode::Auto,
            strategy: PrefetchStrategy::Auto,
            low_performance_killswitch: KillswitchMode::Auto,
            trigger_position: 0.35,
            lead_distance: 2,
            band_width: 2,
            max_tiles_per_cycle: 3000,
            ground_ring_radius: 1.0,
            time_budget_margin: 0.7,
            calibration: CalibrationConfig::default(),
        }
    }
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            aggressive_threshold: 30.0,
            opportunistic_threshold: 10.0,
            sample_duration_secs: 60,
        }
    }
}
```

---

## Design Considerations

### Memory Pressure

Large prefetch batches (default 3,000 tiles) could theoretically cause memory pressure. However, the existing architecture provides adequate protection:

1. **Memory cache is configurable**: Users can set `cache.memory_size` based on available RAM (e.g., 2GB default, up to 16GB+ on high-end systems)
2. **LRU eviction is always active**: The `CacheLayer`'s memory provider uses moka-based LRU eviction, preventing unbounded growth
3. **Disk cache is primary target**: Prefetched tiles are written to disk cache, which is the main goal—disk retrieval is still much faster than network download
4. **Tiles flow through, not accumulate**: Generated tiles are served and evicted based on access patterns, not queued indefinitely

On modern systems (32GB+ RAM), running X-Plane alongside XEarthLayer with aggressive prefetch has not caused OOM issues in testing. For constrained systems, the `max_tiles_per_cycle` tuning option can reduce batch sizes.

### Network Limitations

The prefetch system has limited control over low-level network behavior:

1. **Application-level prioritization works**: ON_DEMAND requests jump the queue ahead of prefetch requests
2. **Connection limiting is enforced**: HTTP concurrency capped at `min(cpus*16, 256)` connections
3. **Timeouts prevent stalls**: 10-second timeout per tile request

**Known limitation**: The system cannot set TCP-level priority flags from userspace Rust. On extremely slow or congested connections, prefetch traffic may contribute to buffer bloat, potentially adding latency to other network traffic. The `low_performance_killswitch` option automatically disables prefetch when throughput is too low to be effective, which also addresses this concern.

---

## Implementation Plan

### Phase 1: Performance Calibration

**Goal**: Implement calibration during initial load

**Tasks**:
1. Add `PerformanceCalibration` struct
2. Instrument initial load to measure throughput
3. Calculate strategy mode based on throughput
4. Log calibration results

**Files**:
- `xearthlayer/src/prefetch/calibration.rs` (new)

### Phase 2: Cruise Strategy

**Goal**: Implement basic heading-based prefetch

**Tasks**:
1. Implement `PrefetchStrategy` trait
2. Implement `CruiseStrategy` with band calculation
3. Integrate with circuit breaker for opportunistic trigger
4. Add position-based trigger (0.3° into DSF tile)

**Files**:
- `xearthlayer/src/prefetch/strategy/mod.rs` (new)
- `xearthlayer/src/prefetch/strategy/cruise.rs` (new)
- `xearthlayer/src/prefetch/band_calculator.rs` (new)

### Phase 3: Ground Strategy

**Goal**: Implement ring-based prefetch for ground operations

**Tasks**:
1. Implement `GroundStrategy`
2. Implement `PhaseDetector` for ground/cruise detection
3. Add strategy switching logic

**Files**:
- `xearthlayer/src/prefetch/strategy/ground.rs` (new)
- `xearthlayer/src/prefetch/phase_detector.rs` (new)

### Phase 4: Adaptive Coordinator

**Goal**: Tie everything together with adaptive logic

**Tasks**:
1. Implement `AdaptivePrefetchCoordinator`
2. Add time budget validation
3. Integrate with existing prefetch system
4. Add metrics

**Files**:
- `xearthlayer/src/prefetch/coordinator.rs` (new)

### Phase 5: Testing and Tuning

**Goal**: Validate with real flights

**Tasks**:
1. Test with slow network simulation
2. Test strategy transitions (ground → cruise)
3. Tune thresholds based on real-world data
4. Performance benchmarking

---

## Testing Strategy

### Unit Tests

- Band calculation accuracy
- Strategy selection logic
- Time budget calculations
- Heading quadrant detection

### Integration Tests

- Calibration during simulated initial load
- Circuit breaker integration
- DdsClient job submission
- Strategy switching

### Flight Tests

| Test | Purpose |
|------|---------|
| Ground taxi | Verify ground strategy ring loading |
| Straight cruise | Verify cruise strategy band loading |
| Heading change | Verify strategy recalculation after turn |
| Ocean crossing | Verify coastal prefetch during ocean flight |
| Slow network | Verify DISABLED mode with throttled connection |

### Metrics to Monitor

- Prefetch hit rate (tiles prefetched before X-Plane request)
- Time budget utilization
- Strategy distribution
- Calibration accuracy

---

## References

- [X-Plane Scenery Loading Whitepaper](xplane-scenery-loading-whitepaper.md)
- [Prefetch Flight Test Plan](prefetch-flight-test-plan.md)
- [Job Executor Design](job-executor-design.md)
- [Predictive Caching Design](predictive-caching.md)

---

**Document Version History**

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | January 2026 | Initial design based on flight test research |
| 1.1 | January 2026 | Added rolling recalibration, priority-based backpressure, design considerations |
