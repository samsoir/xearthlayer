# Aircraft Position & Telemetry Architecture

This document specifies the architecture for aircraft position tracking, scene loading analytics, and prefetch coordination in XEarthLayer.

## Overview & Philosophy

XEarthLayer requires knowledge of the aircraft's position and trajectory to efficiently stream scenery. This data can come from multiple sources with varying accuracy. The architecture separates concerns into three distinct modules that collaborate through well-defined interfaces.

### Design Principles

1. **Separation of Concerns**: Each module has a single responsibility
2. **Independence**: Modules operate independently - disabling one doesn't break others
3. **SOLID**: Dependencies are injected, interfaces are trait-based
4. **Observable**: Modules expose their state for consumers via broadcast + query APIs
5. **Testability**: Each module can be tested in isolation with mock dependencies

### Module Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              FUSE Layer                                     │
│                    (DDS access events - empirical data)                     │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │ unbounded channel (fire-and-forget)
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                     X-Plane Request Model (Module 3)                        │
│         Scene Tracker - maintains model of X-Plane's requests               │
│                                                                             │
│  • Stores DDS tile coordinates (empirical data)                             │
│  • Detects burst patterns (session init, boundary crossing)                 │
│  • Exposes loaded regions for queries                                       │
│  • Does NOT interpret - consumers derive meaning                            │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │ query API (loaded bounds, regions)
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│              Aircraft Position & Telemetry - APT (Module 1)                 │
│           Single source of truth for aircraft state                         │
│                                                                             │
│  Inputs:                                                                    │
│  • GPS Telemetry (XGPS2 UDP) - precise, authoritative                       │
│  • Scene Tracker data - derives position when no telemetry                  │
│                                                                             │
│  Outputs:                                                                   │
│  • Broadcast channel (position updates)                                     │
│  • Query API (position, vectors, telemetry status)                          │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │ broadcast + query
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                       Prefetch System (Module 2)                            │
│              Predicts and preloads scenery tiles                            │
│                                                                             │
│  • Observes APT for aircraft position/heading                               │
│  • Queries Scene Tracker for loaded regions (avoid re-prefetching)          │
│  • Submits prefetch jobs during quiet periods                               │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Module 1: Aircraft Position & Telemetry (APT)

### Responsibility

Provide a **single source of truth** for aircraft position and vector data, abstracting over multiple data sources with varying accuracy.

### Data Sources

| Source | Accuracy | Data Available | When Used |
|--------|----------|----------------|-----------|
| GPS Telemetry (XGPS2) | Precise (meters) | Position, heading, ground speed, altitude | When X-Plane broadcasts enabled |
| Scene Tracker Inference | ~1° (~60nm) | Position only (heading unreliable) | Fallback when no GPS |

### State Model

```rust
/// Telemetry connection status (binary - is X-Plane sending XGPS2?)
pub enum TelemetryStatus {
    /// Receiving XGPS2 UDP data from X-Plane
    Connected,
    /// Not receiving telemetry data
    Disconnected,
}

/// Source of the current position data
pub enum PositionSource {
    /// From GPS telemetry (XGPS2)
    Telemetry,
    /// Inferred from scene loading patterns
    SceneInference,
}

/// Position accuracy level
pub enum PositionAccuracy {
    /// Meter-level accuracy (GPS telemetry)
    High,
    /// ~1° (~60nm) accuracy (scene inference)
    Low,
}

/// Aircraft state snapshot
pub struct AircraftState {
    pub latitude: f64,
    pub longitude: f64,
    pub heading: f32,        // Degrees true, 0 if unknown
    pub ground_speed: f32,   // Knots, 0 if unknown
    pub altitude: f32,       // Feet MSL, 0 if unknown
    pub timestamp: Instant,
    pub source: PositionSource,
    pub accuracy: PositionAccuracy,
}

/// Complete APT status for consumers
pub struct AircraftPositionStatus {
    /// Current aircraft state (if any position data available)
    pub state: Option<AircraftState>,

    /// Is X-Plane sending telemetry? (independent of position source)
    pub telemetry_status: TelemetryStatus,

    /// Are heading/speed/altitude available? (only from telemetry)
    pub vectors_available: bool,
}
```

**Key distinction:**
- `TelemetryStatus` answers: "Is X-Plane broadcasting XGPS2?"
- `PositionSource` answers: "Where did this position come from?"
- `PositionAccuracy` answers: "How accurate is this position?"

These are independent. For example:
- Telemetry could be Connected but we're still using an older inferred position (brief gap)
- Telemetry could be Disconnected but we have a recent inferred position

### Public Interface

```rust
/// Trait for querying aircraft position (pull API)
pub trait AircraftPositionProvider: Send + Sync {
    /// Get complete aircraft position status
    fn status(&self) -> AircraftPositionStatus;

    /// Get current position if available (convenience method)
    fn position(&self) -> Option<(f64, f64)>;

    /// Get telemetry connection status
    fn telemetry_status(&self) -> TelemetryStatus;

    /// Get the source of current position data
    fn position_source(&self) -> Option<PositionSource>;

    /// Check if we have any position data
    fn has_position(&self) -> bool;

    /// Check if vector data (heading, speed, altitude) is available
    fn has_vectors(&self) -> bool;
}

/// Trait for subscribing to position updates (push API)
pub trait AircraftPositionBroadcaster: Send + Sync {
    /// Subscribe to position updates
    fn subscribe(&self) -> broadcast::Receiver<AircraftState>;
}
```

### Internal Components

1. **TelemetryReceiver**: Listens for XGPS2 UDP packets, parses aircraft state
2. **InferenceAdapter**: Subscribes to Scene Tracker for fallback position
3. **StateAggregator**: Merges inputs, selects best source, broadcasts updates

### Behavior

- When GPS telemetry is available, it takes precedence (authoritative)
- When GPS is unavailable, falls back to Scene Tracker inference
- GPS status reflects actual data source being used
- Broadcasts position updates on any change (rate-limited to avoid flooding)
- Query API always returns latest known state

---

## Module 2: Prefetch System

### Responsibility

Predict and preload scenery tiles of 1x1 deg web mercator based on aircraft position, heading, and scene loading patterns.

### Dependencies

- **APT Module**: Observer for position/heading data
- **Scene Tracker**: Query for loaded areas (to avoid re-prefetching)
- **DDS Client**: Submit prefetch jobs to executor

### State Model

```rust
pub struct PrefetchStatus {
    pub mode: PrefetchMode,
    pub stats: PrefetchStats,
    pub circuit_state: CircuitState,
}

pub enum PrefetchMode {
    /// Actively prefetching based on position/heading
    Active,
    /// Paused due to heavy X-Plane loading (circuit breaker open)
    Paused,
    /// Idle - no position data or nothing to prefetch
    Idle,
    /// Disabled by configuration
    Disabled,
}

pub struct PrefetchStats {
    pub cycles_completed: u64,
    pub tiles_submitted: u64,
    pub tiles_from_cache: u64,
    pub last_cycle_tiles: u64,
}
```

### Public Interface

```rust
/// Trait for querying prefetch status
pub trait PrefetchStatusProvider: Send + Sync {
    /// Get current prefetch status
    fn status(&self) -> PrefetchStatus;
}
```

### Behavior

- Observes APT for position updates
- Uses Scene Tracker to understand what X-Plane has loaded
- During quiet periods (no active X-Plane loading), predicts and prefetches
- Circuit breaker pauses prefetch during heavy X-Plane activity
- Operates independently - can be disabled without affecting APT
- Tracks state of the prefetch jobs allowing for progress and state display in UI and interactions (pause/resume/cancel)

---

## Module 3: X-Plane Request Model (Scene Tracker)

### Responsibility

Build and maintain a **mental model** of what scenery X-Plane has loaded, detect loading patterns, and provide data for position inference and prefetch prediction.

**Important boundary**: The Scene Tracker does NOT interpret or infer meaning from the data. It maintains a factual model of what X-Plane has requested. Interpretation (e.g., "the aircraft is probably here") is the responsibility of consumers like APT.

### Input

- **FUSE Events**: Unbounded channel of DDS file access events from FUSE layer

### What FUSE Provides vs What Scene Tracker Does

| FUSE Layer Responsibility | Scene Tracker Responsibility |
|---------------------------|------------------------------|
| Detect file access | Track which tiles have been accessed |
| Parse DDS path to extract tile coordinates | Aggregate tiles into a coherent model |
| Send event immediately (no blocking) | Detect temporal patterns (bursts) |
| No interpretation of meaning | Expose data for consumers to interpret |

### Data Model Philosophy

**Empirical data is the foundation. Inference is a calculation on top.**

- **Store**: What X-Plane actually requested (DDS tile coordinates)
- **Derive**: 1°×1° regions, position, bounds, etc. via calculation
- **Never assume**: Mapping between DDS tiles and geographic regions

This separation means:
- The model reflects reality
- Inference logic can evolve independently
- Multiple consumers can apply different inference strategies
- Testing is straightforward

### State Model

```rust
/// A DDS texture tile coordinate (empirical data - what X-Plane requested)
pub struct DdsTileCoord {
    pub row: u32,   // Web Mercator tile row
    pub col: u32,   // Web Mercator tile column
    pub zoom: u8,   // Zoom level (e.g., 14, 16)
}

impl DdsTileCoord {
    /// Derive the geographic center of this tile
    pub fn to_lat_lon(&self) -> (f64, f64) {
        // Web Mercator → WGS84 conversion
        let n = 2.0_f64.powi(self.zoom as i32);
        let lon = (self.col as f64 / n) * 360.0 - 180.0;
        let lat_rad = (std::f64::consts::PI * (1.0 - 2.0 * self.row as f64 / n)).sinh().atan();
        (lat_rad.to_degrees(), lon)
    }

    /// Derive which 1°×1° region this tile falls within
    pub fn to_geo_region(&self) -> GeoRegion {
        let (lat, lon) = self.to_lat_lon();
        GeoRegion {
            lat: lat.floor() as i32,
            lon: lon.floor() as i32,
        }
    }
}

/// A 1°×1° geographic region (derived, not stored)
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub struct GeoRegion {
    pub lat: i32,  // Floor of latitude (e.g., 53 for 53.5°)
    pub lon: i32,  // Floor of longitude (e.g., 9 for 9.7°)
}

/// Scene loading state - empirical model of X-Plane's requests
pub struct SceneLoadingState {
    /// DDS tiles X-Plane has requested this session (empirical data)
    pub requested_tiles: HashSet<DdsTileCoord>,

    /// Tiles requested in the current/most recent burst
    pub current_burst_tiles: Vec<DdsTileCoord>,

    /// Is a loading burst currently in progress?
    pub burst_active: bool,

    /// Timestamp of last tile access
    pub last_activity: Instant,

    /// Timestamp when current burst started (if active)
    pub burst_started: Option<Instant>,

    /// Total tile requests this session
    pub total_requests: u64,
}

/// A completed loading burst (for event broadcasting)
pub struct LoadingBurst {
    pub tiles: Vec<DdsTileCoord>,
    pub started: Instant,
    pub ended: Instant,
}

/// Burst classification (determined by consumers, not Scene Tracker)
pub enum BurstType {
    /// Large burst at session start
    SessionInit,
    /// Burst when aircraft crosses region boundary
    BoundaryCrossing,
    /// Small burst (texture refresh, etc.)
    Minor,
}
```

### Public Interface

```rust
/// Trait for querying scene loading state (pull API)
pub trait SceneTracker: Send + Sync {
    /// Get current loading state snapshot
    fn state(&self) -> SceneLoadingState;

    /// Get all DDS tiles X-Plane has requested this session (empirical data)
    fn requested_tiles(&self) -> HashSet<DdsTileCoord>;

    /// Check if a specific DDS tile has been requested
    fn is_tile_requested(&self, tile: &DdsTileCoord) -> bool;

    /// Check if X-Plane is currently in a loading burst
    fn is_burst_active(&self) -> bool;

    /// Get tiles from the current/most recent burst
    fn current_burst_tiles(&self) -> Vec<DdsTileCoord>;

    // === Derived queries (calculated from empirical data) ===

    /// Derive which 1°×1° regions have been loaded (calculated, not stored)
    fn loaded_regions(&self) -> HashSet<GeoRegion>;

    /// Check if a 1°×1° region has any requested tiles
    fn is_region_loaded(&self, region: &GeoRegion) -> bool;

    /// Derive the geographic bounding box of all requested tiles
    fn loaded_bounds(&self) -> Option<GeoBounds>;
}

/// Geographic bounding box (derived from tile coordinates)
pub struct GeoBounds {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
}

impl GeoBounds {
    /// Get the center point of the bounds
    pub fn center(&self) -> (f64, f64) {
        (
            (self.min_lat + self.max_lat) / 2.0,
            (self.min_lon + self.max_lon) / 2.0,
        )
    }
}

/// Trait for subscribing to scene loading events (push API)
pub trait SceneTrackerEvents: Send + Sync {
    /// Subscribe to completed loading bursts
    fn subscribe_bursts(&self) -> broadcast::Receiver<LoadingBurst>;

    /// Subscribe to individual tile access events (high volume)
    fn subscribe_tile_access(&self) -> broadcast::Receiver<DdsTileCoord>;
}
```

### Consumer Responsibilities

The Scene Tracker provides empirical data. Consumers derive meaning from it:

| Consumer | What They Do With Scene Tracker Data |
|----------|--------------------------------------|
| **APT Module** | Derives position from center of loaded bounds (fallback) |
| **Prefetch System** | Queries loaded regions to avoid re-prefetching, predicts next regions |
| **Dashboard** | Could display loaded area visualization |

### Position Inference (APT's Responsibility)

APT uses Scene Tracker data to derive position when telemetry is unavailable:

```rust
// In APT module, NOT Scene Tracker
fn derive_position_from_scene(tracker: &dyn SceneTracker) -> Option<AircraftState> {
    let bounds = tracker.loaded_bounds()?;

    // Center of loaded area
    let (lat, lon) = bounds.center();

    Some(AircraftState {
        latitude: lat,
        longitude: lon,
        heading: 0.0,        // Cannot derive from scene data
        ground_speed: 0.0,   // Cannot derive from scene data
        altitude: 0.0,       // Cannot derive from scene data
        timestamp: Instant::now(),
        source: PositionSource::SceneInference,
        accuracy: PositionAccuracy::Low,  // ~1° error margin
    })
}
```

**Note**: Heading could potentially be derived from burst patterns (direction of new tile requests), but this is speculative and would require tracking request order over time. This is a potential future enhancement.

### Burst Detection Logic

```rust
// Configuration
const BURST_QUIET_THRESHOLD: Duration = Duration::from_millis(500);
const BURST_MIN_TILES: usize = 3;

// A burst ends when no tile requests for BURST_QUIET_THRESHOLD
// A burst is significant if it contains >= BURST_MIN_TILES
```

### FUSE Integration (Informer Pattern)

```rust
/// Event sent from FUSE to Scene Tracker (empirical data)
pub struct FuseAccessEvent {
    /// The DDS tile coordinates (parsed from filename)
    pub tile: DdsTileCoord,

    /// Timestamp of access
    pub timestamp: Instant,
}
```

**Priority Order in FUSE Layer:**

1. **Return DDS resource to X-Plane** (critical path - X-Plane is waiting)
2. **Inform Scene Tracker** (secondary - async, fire-and-forget)

The informing step should NEVER delay the DDS response. Implementation approach:

```rust
// In FUSE read handler (pseudocode)
async fn handle_dds_read(&self, path: &str) -> Result<DdsData> {
    // 1. CRITICAL PATH: Get/generate the DDS resource
    let dds_data = self.get_or_generate_dds(path).await?;

    // 2. SECONDARY: Inform Scene Tracker (fire-and-forget, non-blocking)
    if let Some(tile_coord) = parse_dds_path(path) {
        // try_send on unbounded channel - never blocks
        let _ = self.scene_tracker_tx.send(FuseAccessEvent {
            tile: tile_coord,
            timestamp: Instant::now(),
        });
    }

    // 3. Return to X-Plane immediately
    Ok(dds_data)
}
```

**Key points:**
- Parsing the filename is cheap (string manipulation)
- Channel send is non-blocking (unbounded channel)
- Even if parsing fails, DDS delivery continues
- Scene Tracker processes events asynchronously in its own task

```
// FUSE layer responsibility:
// 1. Handle DDS read request → return resource (PRIORITY)
// 2. Parse filename to extract row, col, zoom
// 3. Fire-and-forget send to Scene Tracker channel
// 4. Does NOT wait for Scene Tracker acknowledgment
// 5. Does NOT interpret meaning - just reports what was requested

// Scene Tracker responsibility (separate async task):
// 1. Receives events from channel at its own pace
// 2. Stores DdsTileCoord in requested_tiles set
// 3. Manages burst state (active/ended based on timing)
// 4. Broadcasts events to subscribers
// 5. Does NOT interpret meaning - just maintains the model
```

### Why Unbounded Channel?

| Consideration | Decision |
|---------------|----------|
| FUSE must not block | Unbounded = no backpressure |
| Event loss unacceptable | Unbounded = guaranteed delivery |
| Memory concern? | Bounded by session: ~10K-50K tiles typical |
| What if tracker is slow? | Events queue, processed eventually |

---

## Data Flow Diagram

```
                                    X-Plane
                                       │
                                       │ UDP (XGPS2)
                                       ▼
                              ┌─────────────────┐
                              │ TelemetryReceiver│
                              └────────┬────────┘
                                       │ AircraftState (precise)
        ┌──────────────────────────────┼──────────────────────────────┐
        │                              │                              │
        │                              ▼                              │
        │              ┌───────────────────────────────┐              │
        │              │    Aircraft Position &        │              │
        │              │    Telemetry (APT)            │              │
        │              │                               │              │
        │              │  ┌─────────────────────────┐  │              │
        │   derive     │  │    StateAggregator      │  │  broadcast   │
        │   position   │  │  (selects best source)  │  │  + query     │
        │      ┌───────┼──│                         │──┼───────┐      │
        │      │       │  └─────────────────────────┘  │       │      │
        │      │       └───────────────────────────────┘       │      │
        │      │                                               │      │
        │      │                                               ▼      │
        │      │                                    ┌──────────────────┴──┐
        │      │                                    │  Dashboard (TUI)    │
        │      │                                    │  • Telemetry status │
        │      │                                    │  • Position display │
        │      │                                    └─────────────────────┘
        │      │
        │      │       query loaded regions
        ▼      │       ┌───────────────────────────────────────┐
┌──────────────┴───────────┐                     ┌─────────────┴───────────┐
│  X-Plane Request Model   │                     │    Prefetch System      │
│  (Scene Tracker)         │                     │                         │
│                          │◄────────────────────│  • Observes APT         │
│  • Stores DDS tile coords│  query loaded       │  • Queries Scene Tracker│
│  • Detects burst patterns│  regions            │  • Predicts next regions│
│  • Exposes empirical data│                     │  • Submits prefetch jobs│
│                          │                     │                         │
└──────────────┬───────────┘                     └─────────────────────────┘
               ▲
               │ unbounded channel (DdsTileCoord events)
               │
┌──────────────┴───────────┐
│      FUSE Layer          │
│  (parses DDS filenames)  │
│  (sends tile coords)     │
└──────────────────────────┘
```

**Key principle**: Data flows up from empirical observations (FUSE) through the Scene Tracker, which APT can query for derived position when telemetry is unavailable. The Prefetch System observes APT for position and queries Scene Tracker for loaded regions.

---

## Current Code Mapping

### Existing Components → New Architecture

| Current Code | New Location | Changes Needed |
|--------------|--------------|----------------|
| `TelemetryListener` | APT Module | Rename to TelemetryReceiver, add broadcast |
| `SharedPrefetchStatus` | Split | APT state + Prefetch stats become separate |
| `TileBasedPrefetcher.BurstTracker` | Scene Tracker | Extract to standalone module |
| `DdsAccessEvent` | Scene Tracker | Change `dsf_tile` to `DdsTileCoord` |
| `DsfTileCoord` | Remove/Replace | Use `DdsTileCoord` (empirical) + `GeoRegion` (derived) |
| `handle_telemetry_update()` | APT Module | Remove from prefetcher |
| `update_inferred_position()` | APT derives from Scene Tracker | Clean interface |

### Files to Create

```
xearthlayer/src/
├── aircraft/                    # New APT module
│   ├── mod.rs                   # Module exports
│   ├── provider.rs              # AircraftPositionProvider trait + impl
│   ├── telemetry.rs             # TelemetryReceiver (from listener.rs)
│   ├── state.rs                 # AircraftState, TelemetryStatus, PositionSource
│   └── aggregator.rs            # Combines telemetry + scene inference
│
├── scene_tracker/               # New Scene Tracker module
│   ├── mod.rs                   # Module exports
│   ├── tracker.rs               # SceneTracker trait + impl
│   ├── model.rs                 # DdsTileCoord, GeoRegion, SceneLoadingState
│   ├── burst.rs                 # Burst detection logic
│   └── coords.rs                # Coordinate conversion (DDS → geographic)
│
├── prefetch/                    # Refactored prefetch module
│   ├── ...                      # Existing prefetch code
│   ├── status.rs                # PrefetchStatus (stats only, no aircraft)
│   └── apt_observer.rs          # APT observer integration
```

### Files to Modify

| File | Changes |
|------|---------|
| `fuse/fuse3/ortho_union_fs.rs` | Send `DdsTileCoord` events to Scene Tracker |
| `fuse/fuse3/shared.rs` | Parse DDS filename to `DdsTileCoord` |
| `prefetch/tile_based/prefetcher.rs` | Remove aircraft state, observe APT, query Scene Tracker |
| `prefetch/state.rs` | Remove aircraft state, keep prefetch stats only |
| `ui/dashboard/` | Read from APT for position, Prefetch for stats |
| `commands/run.rs` | Wire new modules, update startup sequence |

### Types to Remove/Replace

| Current Type | Replacement | Reason |
|--------------|-------------|--------|
| `DsfTileCoord` | `GeoRegion` | DSF tiles are derived, not empirical data |
| `GpsStatus` (in prefetch) | `TelemetryStatus` + `PositionSource` | Separate concerns |
| `SharedPrefetchStatus.aircraft` | `AircraftPositionProvider` | Different module |

---

## Implementation Phases

### Phase 1: Scene Tracker Module
**Goal**: Extract X-Plane request tracking into standalone module

1. Create `scene_tracker/` module structure
2. Extract `BurstTracker` from tile-based prefetcher
3. Create `SceneTracker` trait and implementation
4. Wire FUSE → Scene Tracker unbounded channel
5. Add position inference from loaded tiles
6. Tests for burst detection and inference

### Phase 2: Aircraft Position & Telemetry Module
**Goal**: Create unified position provider

1. Create `aircraft/` module structure
2. Move `TelemetryListener` → `TelemetryReceiver`
3. Create `AircraftPositionProvider` trait
4. Implement state aggregator (GPS + inference fallback)
5. Add broadcast channel for updates
6. Wire Scene Tracker as inference source
7. Tests for source selection and broadcasting

### Phase 3: Dashboard Integration
**Goal**: Display position from APT module

1. Update dashboard to use APT provider
2. Remove dependency on `SharedPrefetchStatus` for position
3. GPS status shows actual APT status
4. Position display works regardless of prefetch state
5. Verify position updates when telemetry received

### Phase 4: Prefetch System Refactor
**Goal**: Prefetch becomes observer of APT

1. Remove aircraft state handling from prefetcher
2. Add APT observer for position/heading
3. Query Scene Tracker for loaded tiles
4. Update `PrefetchStatus` to exclude aircraft state
5. Clean separation of concerns complete

### Phase 5: Cleanup & Documentation
**Goal**: Remove dead code, update docs

1. Remove old `SharedPrefetchStatus` aircraft fields
2. Remove duplicate state handling
3. Update CLAUDE.md with new architecture
4. Update user documentation if needed

---

## Testing Strategy

### Unit Tests

- **APT**: Source selection (GPS vs inference), broadcast behavior
- **Scene Tracker**: Burst detection, tile tracking, inference accuracy
- **Prefetch**: Observer integration, circuit breaker with new APIs

### Integration Tests

- **FUSE → Scene Tracker → APT**: Position inference flow
- **Telemetry → APT → Dashboard**: GPS status display
- **APT → Prefetch**: Observer notification

### Manual Testing

- Start with GPS disabled, verify "Inferred" status and position
- Enable GPS, verify "Connected" status and precise position
- Disable prefetch, verify position still displays
- Heavy scene loading, verify burst detection

---

## Open Questions

1. **Rate limiting**: How often should APT broadcast position updates? Every GPS packet (~2Hz) or rate-limited?

2. **Historical data**: Should Scene Tracker keep history of loaded tiles across sessions for cache warming?

3. **Confidence decay**: Should position confidence decay over time if no updates received?

4. **Multiple telemetry sources**: Future support for other position sources (e.g., SimConnect on Windows)?

---

## Appendix: Channel Types

| Connection | Channel Type | Rationale |
|------------|--------------|-----------|
| FUSE → Scene Tracker | `mpsc::unbounded` | Critical events, must not drop |
| APT → Consumers | `broadcast` | Multiple subscribers, ok to lag |
| APT query | Direct method call | Synchronous current-state queries |
| Scene Tracker → APT | `watch` | Latest value only, position inference |
