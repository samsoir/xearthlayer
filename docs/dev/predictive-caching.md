# Predictive Tile Caching

**Status**: Implemented (v0.2.7+, dual-zone architecture v0.2.9+, circuit breaker v0.2.11+)

## Overview

This document describes the design and implementation of predictive tile caching in XEarthLayer. The feature pre-fetches tiles ahead of the aircraft's position to reduce FPS drops when the simulator loads new scenery.

## Dual-Zone Prefetch Architecture

XEarthLayer v0.2.9+ uses a dual-zone prefetch system that targets the boundary around X-Plane's ~90nm loaded scenery:

![Dual-Zone Prefetch Diagram](prefetch-zones.svg)

```
                              Heading
                                 ↑
                           ┌─────────────┐
                          ╱   HEADING     ╲
                         ╱     CONE        ╲         ← 85-120nm forward
                        ╱   (60° wide)      ╲          (deep lookahead)
                       ╱                     ╲
          ┌───────────┴───────────────────────┴───────────┐
          │              RADIAL BUFFER (360°)             │ ← 85-100nm
          │  ┌───────────────────────────────────────┐    │   (all directions)
          │  │                                       │    │
          │  │       X-PLANE LOADED SCENERY          │    │
          │  │          (~90nm radius)               │    │
          │  │                                       │    │
          │  │              ✈ Aircraft               │    │
          │  │                                       │    │
          │  └───────────────────────────────────────┘    │
          └───────────────────────────────────────────────┘
```

**Key Design Principles:**

1. **Don't prefetch what X-Plane already has**: X-Plane maintains ~90nm of loaded tiles. Prefetching inside this zone is redundant.

2. **Radial Buffer (85-100nm, 360°)**: A 15nm-wide ring around the boundary catches unexpected turns, orbits, and lateral movements. Every direction is covered.

3. **Heading Cone (85-120nm, 60° forward)**: Extends 35nm along the flight path for deep lookahead. This is where smooth flight comes from.

4. **Both zones run every cycle**: The radial buffer and heading cone are combined each cycle, with duplicates removed via request coalescing.

5. **Intersection-based tile selection**: Tiles are included if *any part* intersects the prefetch zone, not just the center point. This prevents edge tiles from being missed.

## Problem Statement

When flying in X-Plane, the simulator requests tiles as the aircraft approaches new scenery areas. These on-demand requests cause noticeable FPS drops (e.g., 80fps → 25fps) because tile generation involves:
- HTTP downloads of 256 chunks per tile
- Image assembly (16×16 grid)
- DDS encoding with mipmap generation
- Cache writes

Even with fast internet and NVMe storage, the latency is perceptible. The solution is to predict which tiles will be needed and pre-fetch them before X-Plane requests them.

## Design Goals

1. **Reduce FPS drops**: Pre-cached tiles serve from memory cache with <10ms latency
2. **Predictive accuracy**: Use aircraft telemetry to anticipate tile needs
3. **Resource efficiency**: Don't starve on-demand requests or waste bandwidth
4. **Seamless integration**: Submit jobs to the shared job executor daemon
5. **Configurability**: Allow users to tune behavior for their setup
6. **Graceful degradation**: Work even without telemetry using FUSE request analysis

## Architecture

### Component Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                     X-Plane (UDP Broadcast)                      │
│                  Position, Heading, Speed, Alt                   │
└──────────────────────────┬──────────────────────────────────────┘
                           │ UDP Port (configurable, default 49002)
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                   Telemetry Listener                             │
│         Parse packets → AircraftState { lat, lon, hdg, gs, alt } │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│           HeadingAwarePrefetcher (Recommended - "auto")          │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  Graceful Degradation Chain:                                ││
│  │                                                             ││
│  │  ┌──────────┐ stale(>5s) ┌──────────────┐ low    ┌────────┐││
│  │  │Telemetry │──────────→│FUSE Inference│──────→ │ Radial │││
│  │  │ (precise)│           │(fuzzy margins)│ conf   │Fallback│││
│  │  └──────────┘           └──────────────┘        └────────┘││
│  │    ~50-80                  ~100-150               49 tiles ││
│  │  tiles/cycle              tiles/cycle            (7×7 grid)││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  SceneryIndex (Source of Truth)                             ││
│  │  - Reads .ter files for exact tile coordinates              ││
│  │  - Knows correct zoom level per tile from packages          ││
│  │  - Deprioritizes sea tiles (~33% of tiles)                  ││
│  └─────────────────────────────────────────────────────────────┘│
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│              Job Executor Daemon                                 │
│         DdsGenerateJob → Tasks: Download, Assemble, Encode, Cache│
└─────────────────────────────────────────────────────────────────┘
```

### Prefetcher Trait (Strategy Pattern)

The prefetch system uses a strategy pattern via the `Prefetcher` trait, enabling runtime selection of different prefetching algorithms:

```rust
pub trait Prefetcher: Send {
    /// Run the prefetcher, processing state updates until cancelled.
    fn run(
        self: Box<Self>,
        state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Get a human-readable name for this prefetcher strategy.
    fn name(&self) -> &'static str;

    /// Get a description of this prefetcher strategy.
    fn description(&self) -> &'static str;

    /// Get startup info string for logging.
    fn startup_info(&self) -> String;
}
```

**Available Strategies**:
- `HeadingAwarePrefetcher` (name: "heading-aware") - Direction-aware with graceful degradation (recommended)
- `RadialPrefetcher` (name: "radial") - Simple grid around position
- Strategy "auto" uses HeadingAwarePrefetcher with automatic fallback

### Components

#### 1. Telemetry Listener (`xearthlayer/src/prefetch/listener.rs`)

Listens for UDP broadcast packets and extracts aircraft state. Supports multiple protocols:

**X-Plane DATA Protocol** (port 49003):
- Index 3: Speeds (kias, keas, ktas, ktgs) - ground speed in knots
- Index 17: Pitch, roll, magnetic heading, true heading
- Index 20: Latitude, longitude, altitude MSL

**ForeFlight Protocol** (port 49002):
- XGPS2: Position, altitude, heading, ground speed
- XATT2: Attitude data (heading updates)

The listener auto-detects packet format based on prefix bytes.

**Output**: `AircraftState` struct updated at ~1-2 Hz

```rust
pub struct AircraftState {
    pub latitude: f64,      // degrees
    pub longitude: f64,     // degrees
    pub altitude: f32,      // feet MSL
    pub heading: f32,       // degrees (0-360, normalized)
    pub ground_speed: f32,  // knots
    pub updated_at: Instant,
}
```

#### 2. HeadingAwarePrefetcher (`xearthlayer/src/prefetch/heading_aware.rs`) - Recommended

Unified prefetcher with automatic mode selection based on available input:

**Input Modes**:
1. **Telemetry Mode**: Uses precise cone generator when UDP telemetry is available
2. **FUSE Inference Mode**: Uses dynamic envelope when telemetry is stale (>5s)
3. **Radial Fallback Mode**: Simple radius when no heading data is available

**Graceful Degradation**:
```
┌─────────────────┐   stale (>5s)   ┌──────────────────┐   low confidence   ┌────────────────┐
│   Telemetry     │ ───────────────▶│  FUSE Inference  │ ──────────────────▶│ Radial Fallback│
│  (precise)      │                 │  (fuzzy margins) │                    │   (no heading) │
└─────────────────┘                 └──────────────────┘                    └────────────────┘
     ~50-80                              ~100-150                               49 tiles
   tiles/cycle                         tiles/cycle                             (7×7 grid)
```

**Telemetry Mode Features**:
- **ConeGenerator**: Projects a cone ahead of aircraft based on heading
- **BufferGenerator**: Covers lateral and rear areas around aircraft
- **Turn Detection**: Widens cone during turns for better coverage
- **SceneryIndex**: Uses scenery package data for exact tile coordinates and zoom levels

**Configuration** (`HeadingAwarePrefetcherConfig`):
```rust
pub struct HeadingAwarePrefetcherConfig {
    pub heading: HeadingAwarePrefetchConfig,  // Cone/buffer settings
    pub fuse_inference: FuseInferenceConfig,  // FUSE fallback settings
    pub telemetry_stale_threshold: Duration,  // When to switch to FUSE (default: 5s)
    pub fuse_confidence_threshold: f32,       // Min confidence for FUSE mode (default: 0.3)
    pub radial_fallback_radius: u8,           // Radial grid size (default: 3 = 7×7)
}
```

#### 3. ConeGenerator (`xearthlayer/src/prefetch/cone.rs`)

Generates tiles in a cone-shaped region ahead of the aircraft.

**Algorithm**:
1. Calculate aircraft position in tile coordinates
2. Project a cone with configurable half-angle (default: 45°)
3. Generate tiles within inner-to-outer radius range (default: 85-95nm)
4. Assign priority based on distance and zone (center higher than edges)
5. Support turn widening: expand cone during detected turns

**Prefetch Zones**:
```rust
pub enum PrefetchZone {
    ForwardCenter,  // Highest priority - directly ahead
    ForwardEdge,    // Medium priority - cone edges
    Lateral,        // Lower priority - sides
    Rear,           // Lowest priority - behind aircraft
}
```

#### 4. BufferGenerator (`xearthlayer/src/prefetch/buffer.rs`)

Generates tiles in lateral and rear buffer zones for coverage during maneuvers.

**Coverage Areas**:
- Side buffers: 2-tile strips on each side
- Rear buffer: 2-tile strip behind aircraft
- Priorities lower than forward cone

#### 5. FUSE Request Analyzer (`xearthlayer/src/prefetch/inference.rs`)

Infers aircraft position and heading from FUSE file access patterns when telemetry is unavailable.

**Algorithm**:
1. Monitor DDS file requests from X-Plane
2. Track tile request patterns over time
3. Infer position as centroid of recent requests
4. Infer heading from request direction vector
5. Calculate confidence based on pattern consistency

**Confidence Calculation**:
- High confidence: Consistent linear request pattern
- Medium confidence: Clustered but directional
- Low confidence: Scattered or insufficient data

#### 6. SceneryIndex (`xearthlayer/src/prefetch/scenery_index.rs`)

Spatial index of tiles from installed scenery packages. Provides exact tile coordinates from .ter files instead of calculating them.

**Benefits**:
- **Exact zoom levels**: Reads actual zoom from DDS filenames in .ter files
- **Only real tiles**: Only prefetches tiles that exist in the package
- **Sea tile detection**: Deprioritizes sea tiles (~33% of typical package)
- **Fast lookup**: Grid-based spatial index for O(1) queries

**Building the Index**:
```rust
let index = SceneryIndex::with_defaults();
for pkg in &ortho_packages {
    match index.build_from_package(&pkg.path) {
        Ok(count) => println!("Indexed {} tiles from {}", count, pkg.region()),
        Err(e) => warn!("Failed to index {}: {}", pkg.region(), e),
    }
}
```

**Querying Tiles**:
```rust
let tiles = index.tiles_near(lat, lon, radius_nm);
// Returns Vec<SceneryTile> with exact coordinates and zoom levels
```

**Index Structure**:
```rust
pub struct SceneryTile {
    pub row: u32,        // Tile row at chunk_zoom
    pub col: u32,        // Tile column at chunk_zoom
    pub chunk_zoom: u8,  // Zoom level from filename (16 or 18)
    pub lat: f32,        // Center latitude from LOAD_CENTER
    pub lon: f32,        // Center longitude from LOAD_CENTER
    pub is_sea: bool,    // Detected from filepath (z_sea_water_*)
}
```

#### 7. SceneryIndex Cache (`xearthlayer/src/prefetch/scenery_cache.rs`)

The SceneryIndex is persisted to disk to avoid rebuilding on every launch. With EU + partial US (~445,000 tiles), initial indexing takes ~20 seconds. The cache reduces subsequent startups to ~200-500ms.

**Cache Location**: `~/.xearthlayer/scenery_index.cache`

**Cache Format**:
```
SCENERY INDEX CACHE
1                                          # format version
2                                          # number of packages
445053                                     # total tile count
12847                                      # sea tile count
eu_ortho  /path/to/package  1703980800  125000   # package metadata (name, path, mtime, file_count)
na_ortho  /path/to/package  1703980900  320053

94800  47888  18  44.50434  -114.22485  0  # tile data (row col zoom lat lon is_sea)
94801  47889  18  44.51234  -114.21485  1
...
```

**Cache Invalidation**:
The cache is invalidated when any of these conditions are detected:
- Package list changed (packages added or removed)
- Any terrain directory's modification time (mtime) changed
- Any terrain directory's file count changed

**Startup Flow**:
1. Check if cache file exists
2. Gather current package metadata (mtime, file counts)
3. Compare against cached package metadata
4. If valid → load tiles from cache (~200-500ms)
5. If invalid → rebuild from .ter files (~20s) and save new cache

**Performance**:
| Scenario | Time |
|----------|------|
| First launch (no cache) | ~20s (build) + ~200ms (save) |
| Subsequent launch (cache valid) | ~200-500ms (load + validate) |
| Launch after package update | ~20s (rebuild) + ~200ms (save) |

Cache file size: ~15-25 bytes per tile × 445,000 tiles ≈ **7-11 MB**

### Circuit Breaker (`xearthlayer/src/prefetch/circuit_breaker.rs`)

The circuit breaker automatically pauses prefetching when X-Plane is loading scenery. This prevents prefetch jobs from competing with on-demand requests during high-load periods.

**SOLID Architecture (v0.2.11):**

The circuit breaker follows SOLID principles with trait-based abstractions:

- **`FuseLoadMonitor`** (`load_monitor.rs`) - Interface for tracking FUSE request counts
  - `record_request()` - Called for each non-prefetch FUSE request
  - `total_requests()` - Returns total count for rate calculation
- **`SharedFuseLoadMonitor`** - Thread-safe implementation using `AtomicU64`
- **`PrefetchThrottler`** (`throttler.rs`) - Interface for throttle decisions
  - `should_throttle()` - Returns true if prefetching should pause
  - `state()` - Returns `ThrottleState` (Active, Paused, Resuming)
- **`CircuitBreaker`** - Implements `PrefetchThrottler` with state machine

This design eliminates ~127 lines of duplicated circuit breaker logic from prefetchers.

**How it works:**
1. The `FuseLoadMonitor` tracks FUSE-originated jobs (X-Plane requests, NOT prefetch requests)
2. When FUSE jobs/second exceeds threshold (default: 50.0) for sustained duration (default: 500ms), circuit opens
3. While open, prefetch cycles are skipped entirely
4. After load drops below threshold, circuit enters half-open state
5. If load stays low during cooloff period (default: 2s), circuit closes and prefetching resumes

**State Machine:**
```
Closed --[fuse_rate > threshold for open_duration]--> Open
Open --[fuse_rate < threshold]--> HalfOpen
HalfOpen --[half_open_duration elapsed + try_close()]--> Closed
HalfOpen --[fuse_rate > threshold]--> Open (reset)
```

**CRITICAL Design Decision:**
- Circuit breaker only monitors FUSE-originated jobs (`!is_prefetch`)
- This prevents self-fulfilling lockup where prefetch jobs trigger the breaker
- Only X-Plane's own scenery loading requests affect the circuit state
- Prefetchers depend on `PrefetchThrottler` trait (not concrete `CircuitBreaker`)

**TUI Display:**
| ThrottleState | TUI Shows |
|---------------|-----------|
| Active | "Active" (normal prefetching) |
| Paused | "Paused (high X-Plane load)" |
| Resuming | "Resuming..." |

### Timeout Mechanism

Prefetch requests include a 10-second timeout to prevent stuck jobs:

```rust
// Spawn timeout task to cancel the request if it takes too long
let cancellation_token = CancellationToken::new();
let timeout_token = cancellation_token.clone();

tokio::spawn(async move {
    tokio::time::sleep(PREFETCH_REQUEST_TIMEOUT).await;
    timeout_token.cancel();
});
```

This ensures that hung HTTP connections or slow provider responses don't block the prefetch queue indefinitely.

### Shared Memory Cache

The prefetcher shares the same `MemoryCacheAdapter` instance with the pipeline:

```rust
// In XEarthLayerService
if let Some(memory_cache) = service.memory_cache_adapter() {
    let prefetcher = PrefetcherBuilder::new()
        .memory_cache(memory_cache)
        .dds_handler(service.create_prefetch_handler())
        // ... other config
        .build();
}
```

This ensures prefetch cache checks are accurate - if a tile is in the pipeline's memory cache, the prefetcher will see it.

### Zoom Level Handling

XEarthLayer uses the **SceneryIndex** as the source of truth for which tiles to prefetch. The index reads `.ter` files from your scenery packages to determine:

- **Exact tile coordinates** from the package's terrain files
- **Correct zoom levels** as defined by the package (not hardcoded)
- **Tile type** (land vs sea) for priority ordering

This approach means no zoom level configuration is needed - the prefetcher automatically handles whatever zoom levels exist in your scenery packages.

## Configuration

Configuration keys in `[prefetch]` section:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable/disable predictive caching |
| `strategy` | string | `auto` | Strategy: `auto`, `heading-aware`, or `radial` |
| `udp_port` | u16 | `49002` | X-Plane UDP broadcast port |
| `inner_radius_nm` | f32 | `85.0` | Inner edge of prefetch zone (nautical miles) |
| `outer_radius_nm` | f32 | `180.0` | Outer edge of prefetch zone (nautical miles) |
| `cone_angle` | f32 | `80.0` | Half-angle of heading cone (degrees) |
| `radial_radius` | u8 | `120` | Radial prefetcher tile radius |
| `max_tiles_per_cycle` | usize | `200` | Max tiles to submit per cycle |
| `cycle_interval_ms` | u64 | `2000` | Interval between prefetch cycles (ms) |
| `circuit_breaker_threshold` | f64 | `50.0` | FUSE jobs/second to trip circuit breaker |
| `circuit_breaker_open_ms` | u64 | `500` | Sustained load duration (ms) to open circuit |
| `circuit_breaker_half_open_secs` | u64 | `2` | Cooloff time before closing circuit |

Example `config.ini`:
```ini
[prefetch]
enabled = true
strategy = auto
udp_port = 49002

# Zone boundaries (nautical miles)
inner_radius_nm = 85              # Start 5nm inside X-Plane's 90nm boundary
outer_radius_nm = 180             # Extended look-ahead coverage

# Heading-aware cone
cone_angle = 80                   # 160° total cone width

# Radial prefetcher (fallback when no telemetry)
radial_radius = 120               # Tile radius for radial prefetching

# Cycle limits
max_tiles_per_cycle = 200
cycle_interval_ms = 2000

# Circuit breaker (pause prefetch during X-Plane scenery loading)
circuit_breaker_threshold = 50.0        # FUSE jobs/sec to trip
circuit_breaker_open_ms = 500           # Duration to sustain before pause
circuit_breaker_half_open_secs = 2      # Cooloff before resume
```

## X-Plane Setup

Users must enable UDP data output in X-Plane:

**Option 1: ForeFlight Protocol (Recommended)**
1. Settings → Network
2. Enable "Send to ForeFlight"
3. XEarthLayer receives position/heading on UDP port 49002

**Option 2: DATA Protocol**
1. Settings → Data Output
2. Enable "Network via UDP"
3. Set destination IP to localhost (127.0.0.1) or machine running XEarthLayer
4. Set port to 49003
5. Enable data indices: 3 (speeds), 17 (orientation), 20 (position)

## CLI Usage

The prefetcher is automatically started with `xearthlayer run`:

```
Scenery index: 42548 tiles (28032 land, 14516 sea)
Prefetch system started (heading-aware, 45° cone, 85-95nm zone, zoom 14, UDP port 49002)
```

Dashboard shows real-time prefetch status:
```
Prefetch: Telemetry | 23/cycle | Cache: 156↑ TTL: 8⊘
```

Disable with `--no-prefetch` flag:
```bash
xearthlayer run --no-prefetch
```

### SceneryIndex Cache Management

Manage the SceneryIndex cache outside of `run` with dedicated CLI commands:

```bash
# Show cache status
xearthlayer scenery-index status
# Output:
# Scenery index cache: /home/user/.xearthlayer/scenery_index.cache
#   Status: Valid
#   Version: 1
#   Packages: 2
#     - EU (402505 files)
#     - NA (42548 files)
#   Total tiles: 445053
#   Land tiles: 246193
#   Sea tiles: 198860
#   File size: 16.9 MB

# Rebuild the index from installed packages
xearthlayer scenery-index update
# Output:
# Building scenery index from 2 packages...
#   Indexing EU... 402505 tiles
#   Indexing NA... 42548 tiles
# Saving cache...
# Done. 445053 tiles indexed (246193 land, 198860 sea)

# Delete the cache file (forces rebuild on next run)
xearthlayer scenery-index clear
```

These commands are useful for:
- Verifying cache status after package installation
- Manually rebuilding the index without running the full UI
- Troubleshooting cache invalidation issues

## Design Decisions

### DD-001: HeadingAware vs Radial Prefetching

**Decision**: Use HeadingAwarePrefetcher with graceful degradation as the default strategy.

**Rationale**:
- Telemetry mode provides precise, direction-aware prefetching
- FUSE inference provides fallback when telemetry is unavailable
- Radial fallback ensures basic functionality in all cases
- Single unified implementation reduces code complexity

### DD-002: Graceful Degradation Chain

**Decision**: Automatically degrade from Telemetry → FUSE Inference → Radial based on data availability.

**Rationale**:
- Users don't need to manually configure for their setup
- System adapts to changing conditions (e.g., UDP packet loss)
- Each mode provides appropriate coverage for its confidence level

### DD-003: SceneryIndex as Source of Truth

**Decision**: Use SceneryIndex to determine which tiles (and at which zoom levels) to prefetch.

**Rationale**:
- Scenery packages define reality - they contain specific tiles at specific zoom levels
- Configuration-based zoom level management was redundant
- SceneryIndex reads `.ter` files to know exact tile coordinates
- Eliminates need for zoom-level-specific configuration
- Tiles prioritized by distance (closer = more important)

### DD-004: Scenery-Aware Prefetch

**Decision**: Build index from .ter files to know exact tile coordinates.

**Rationale**:
- Coordinate calculation can produce tiles that don't exist
- Zoom levels vary by provider and region
- Sea tiles can be deprioritized (33% bandwidth savings)
- Index build is fast (~3s for 42k tiles)

### DD-005: Shared Memory Cache

**Decision**: Prefetcher uses the same memory cache adapter instance as the pipeline.

**Rationale**:
- Accurate cache hit detection
- No duplicate cache instances consuming memory
- Consistent behavior between prefetch and on-demand requests

### DD-006: Per-Request Timeout

**Decision**: Each prefetch request has a 10-second timeout via CancellationToken.

**Rationale**:
- Prevents stuck jobs from blocking the queue
- Matches FUSE on-demand request timeout
- Failed requests are TTL-tracked to prevent immediate retry

### DD-007: TTL Tracking

**Decision**: Recently-attempted tiles are skipped for 60 seconds.

**Rationale**:
- Prevents hammering provider with failed requests
- Reduces wasted bandwidth
- Allows transient failures to recover

### DD-008: Strategy Pattern

**Decision**: Use `Prefetcher` trait for strategy abstraction.

**Rationale**:
- Enables runtime strategy selection
- Allows A/B testing of different algorithms
- Clean separation of concerns
- Easy to add new strategies

## Metrics

TUI dashboard displays:
- Current mode (Telemetry/FUSE/Radial)
- Tiles submitted per cycle
- Cache hits (tiles already in memory)
- TTL skipped (recently attempted)
- Active zoom level

Log entries show detailed cycle information:
```
INFO Prefetch cycle complete mode="telemetry" generated=47 submitted=12 cache_hits=28 ttl_skipped=7
```

## Module Structure

```
xearthlayer/src/prefetch/
├── mod.rs              # Module exports
├── strategy.rs         # Prefetcher trait
├── heading_aware.rs    # HeadingAwarePrefetcher (recommended)
├── radial.rs           # RadialPrefetcher (ring-based, nautical mile annulus)
├── cone.rs             # ConeGenerator for forward prefetch
├── buffer.rs           # BufferGenerator for lateral/rear
├── intersection.rs     # Tile/zone intersection testing
├── inference.rs        # FUSE request analyzer
├── scenery_index.rs    # SceneryIndex for exact tile lookup
├── scenery_cache.rs    # Persistent cache for SceneryIndex
├── load_monitor.rs     # FuseLoadMonitor trait + SharedFuseLoadMonitor
├── throttler.rs        # PrefetchThrottler trait + ThrottleState
├── circuit_breaker.rs  # CircuitBreaker implements PrefetchThrottler
├── prewarm.rs          # Cold-start cache warming
├── listener.rs         # UDP telemetry listener
├── config.rs           # Configuration types
├── builder.rs          # PrefetcherBuilder
├── state.rs            # Shared status for dashboard (+ CircuitState display)
├── types.rs            # Common types (PrefetchTile, zones)
├── coordinates.rs      # Coordinate conversion utilities
├── condition.rs        # Prefetch conditions (speed thresholds)
├── predictor.rs        # Legacy predictor (deprecated)
├── scheduler.rs        # Legacy scheduler (deprecated)
└── error.rs            # Error types
```

## Performance Characteristics

| Metric | Telemetry Mode | FUSE Inference | Radial Fallback |
|--------|---------------|----------------|-----------------|
| Tiles/cycle | 50-80 | 100-150 | 49 |
| Accuracy | High | Medium | Low |
| Coverage | Direction-aware | Fuzzy envelope | Uniform grid |
| Turn handling | Cone widening | Envelope expansion | N/A |

## Future Enhancements

### FE-001: Adaptive Radius

Adjust prefetch radius based on ground speed - larger radius at higher speeds.

### FE-002: Flight Plan Integration

Parse X-Plane flight plan to pre-fetch entire route before takeoff.

### FE-003: Cache Visualization

Map view showing cached tiles, prefetch requests, and aircraft position.

### FE-004: Provider-Aware Throttling

Reduce prefetch rate when provider shows signs of throttling.

### FE-005: Incremental SceneryIndex Updates

Only re-scan packages whose `mtime` changed instead of rebuilding the entire index. When the cache is stale due to a single package update, scan only that package and merge results with cached tiles from unchanged packages.

### FE-006: Binary SceneryIndex Format

Switch from text-based format to bincode/MessagePack for faster cache load times. The current text format (~7-11 MB for 445k tiles) loads in ~200-500ms. Binary format could reduce this to ~50-100ms for very large worldwide tile sets.

### FE-007: Async SceneryIndex Cache I/O

Use `tokio::fs` for non-blocking cache read/write operations. Currently cache operations are synchronous and block the main thread briefly during startup. Async I/O would allow the dashboard to render immediately while cache loads in background.

---

## Changelog

| Date | Author | Changes |
|------|--------|---------|
| 2025-12-21 | Claude | Initial design document |
| 2025-12-22 | Claude | Implementation updates: ForeFlight protocol support, PrefetchCondition trait, MinimumSpeedCondition (30kt default) |
| 2025-12-23 | Claude | Major refactor: RadialPrefetcher as recommended strategy (49 tiles vs 21,000+), Prefetcher trait for strategy pattern, shared memory cache adapter, 10-second request timeout, TTL tracking for failed tiles |
| 2025-12-25 | Claude | HeadingAwarePrefetcher: Cone and buffer generators, turn detection, FUSE inference fallback, graceful degradation chain |
| 2025-12-26 | Claude | SceneryIndex: Parse .ter files for exact tile coordinates, sea tile detection, grid-based spatial index |
| 2025-12-28 | Claude | **Dual-zone architecture**: Radial buffer (85-100nm, 360°) + heading cone (85-120nm, 60° forward). New intersection.rs module for proper tile/zone intersection testing. Config keys: radial_outer_radius_nm, cone_outer_radius_nm, cone_half_angle. Three-pool CPU limiter gives prefetch guaranteed capacity. |
| 2025-12-28 | Claude | **SceneryIndex persistent cache**: New `scenery_cache.rs` module caches the SceneryIndex to disk (~7-11 MB for 445k tiles). First launch: ~20s build + save. Subsequent launches: ~200-500ms load. Cache invalidates on package changes (mtime, file count). Added `from_tiles()` and `all_tiles()` methods to SceneryIndex. Future enhancements: FE-005 (incremental updates), FE-006 (binary format), FE-007 (async I/O). |
| 2026-01-06 | Claude | **Ring-based radial prefetch + Circuit breaker**: (1) RadialPrefetcher now uses nautical-mile annulus (85-120nm) instead of tile-count grid. `radial_radius` deprecated. (2) Circuit breaker pauses prefetch when X-Plane is loading scenery (FUSE job rate > 5/sec). Three states: Closed (active), Open (paused), HalfOpen (resuming). Only counts FUSE-originated jobs to prevent self-fulfilling lockup. New config keys: `circuit_breaker_threshold`, `circuit_breaker_open_secs`, `circuit_breaker_half_open_secs`. Updated defaults: `cone_angle` 45°→80°, `outer_radius_nm` 95→120nm, `max_tiles_per_cycle` 50→200. |
