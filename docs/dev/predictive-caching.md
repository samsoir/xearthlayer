# Predictive Tile Caching

**Status**: Implemented

## Overview

This document describes the design and implementation of predictive tile caching in XEarthLayer. The feature pre-fetches tiles ahead of the aircraft's position to reduce FPS drops when the simulator loads new scenery.

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
4. **Seamless integration**: Reuse existing pipeline infrastructure
5. **Configurability**: Allow users to tune behavior for their setup

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
│                   Prefetcher (Strategy Pattern)                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  RadialPrefetcher (Recommended)                         │    │
│  │  - 7×7 tile grid around current position (49 tiles)     │    │
│  │  - Check memory cache before submitting                 │    │
│  │  - TTL tracking to avoid re-requesting failed tiles     │    │
│  │  - 10-second timeout per request                        │    │
│  └─────────────────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  PrefetchScheduler (Legacy)                             │    │
│  │  - Complex cone + radial prediction                     │    │
│  │  - Can generate 21,000+ tile requests per cycle         │    │
│  │  - Not recommended for production use                   │    │
│  └─────────────────────────────────────────────────────────┘    │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│              Existing Pipeline (unchanged)                       │
│         Download → Assemble → Encode → Cache (Memory + Disk)    │
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
}
```

**Available Strategies**:
- `RadialPrefetcher` (name: "radial") - Simple, cache-aware, recommended
- `PrefetchScheduler` (name: "flight-path") - Complex prediction, legacy

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

#### 2. Radial Prefetcher (`xearthlayer/src/prefetch/radial.rs`) - Recommended

Simple, cache-aware prefetching that maintains a buffer of tiles around the current position.

**Algorithm**:
1. Convert aircraft position to tile coordinates
2. Calculate all tiles within configured radius (default: 3 tiles = 7×7 grid = 49 tiles)
3. Check memory cache for each tile (shared with pipeline)
4. Skip tiles that were recently attempted (TTL tracking, default: 60 seconds)
5. Submit prefetch requests for missing tiles with 10-second timeout

**Key Features**:
- **Shared Memory Cache**: Uses the same `MemoryCacheAdapter` instance as the pipeline, ensuring accurate cache hit detection
- **TTL Tracking**: Tiles that fail are not re-requested for 60 seconds, preventing provider hammering
- **Request Timeout**: Each prefetch request has a 10-second timeout via `CancellationToken`
- **Rate Limiting**: Minimum 2 seconds between prefetch cycles
- **Movement Detection**: Only runs when aircraft moves to a new tile

**Configuration** (`RadialPrefetchConfig`):
```rust
pub struct RadialPrefetchConfig {
    pub radius: u8,           // Tile radius (default: 3 = 49 tiles)
    pub zoom: u8,             // Zoom level (default: 14)
    pub attempt_ttl: Duration, // Don't retry failed tiles for this long (default: 60s)
}
```

**Performance**:
- 49 tiles per cycle (vs 21,000+ with legacy scheduler)
- Typically 7-13 new requests per cycle (rest are cache hits or TTL-skipped)
- ~40-80% cache hit rate during continuous flight

#### 3. Legacy Scheduler (`xearthlayer/src/prefetch/scheduler.rs`)

Complex flight-path prediction with cone and radial calculations. Not recommended due to:
- Generates 21,000+ tile predictions per cycle
- Can overwhelm tile providers (causes throttling)
- Complex coordinate math with edge cases

Kept for reference and potential future optimization.

#### 4. Pre-fetch Condition (`xearthlayer/src/prefetch/condition.rs`)

Determines when prefetching should be active using dependency-injected condition logic.

**Implementations**:
- `MinimumSpeedCondition` - Activates prefetch above a threshold speed (default 30kt)
- `AlwaysActiveCondition` - Always allows prefetch (testing)
- `NeverActiveCondition` - Never allows prefetch (testing)

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
    let prefetcher: Box<dyn Prefetcher> = Box::new(
        RadialPrefetcher::new(memory_cache, dds_handler, config)
    );
}
```

This ensures prefetch cache checks are accurate - if a tile is in the pipeline's memory cache, the prefetcher will see it.

## Configuration

Configuration keys in `[prefetch]` section:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `prefetch.enabled` | bool | `true` | Enable/disable predictive caching |
| `prefetch.udp_port` | u16 | `49002` | X-Plane UDP broadcast port |
| `prefetch.cone_angle` | f32 | `45.0` | Half-angle of prediction cone (degrees) - legacy |
| `prefetch.cone_distance_nm` | f32 | `10.0` | Look-ahead distance in nautical miles - legacy |
| `prefetch.radial_radius_nm` | f32 | `5.0` | Radial buffer in nautical miles - legacy |

Example `config.ini`:
```ini
[prefetch]
enabled = true
udp_port = 49002
```

## X-Plane Setup

Users must enable UDP data output in X-Plane:

1. Settings → Data Output
2. Enable "Network via UDP"
3. Set destination IP to localhost (127.0.0.1) or machine running XEarthLayer
4. Set port (default 49002 for ForeFlight protocol)
5. Enable data indices: 3 (speeds), 17 (orientation), 20 (position)

Alternatively, enable ForeFlight broadcast which sends XGPS/XATT packets.

## CLI Usage

The prefetcher is automatically started with `xearthlayer run`:

```
Prefetch system started (radial, 3-tile radius, UDP port 49002)
```

Dashboard shows real-time prefetch status:
```
Prefetch: 7 submitted, 26 in-flight skipped, 49 predicted (cycle #701)
```

Disable with `--no-prefetch` flag:
```bash
xearthlayer run --no-prefetch
```

## Design Decisions

### DD-001: Radial vs Flight-Path Prediction

**Decision**: Use simple radial prefetching instead of complex flight-path prediction.

**Rationale**:
- Flight-path prediction generated 21,000+ tiles per cycle, overwhelming providers
- Radial approach generates only 49 tiles with high cache hit rate
- Works for all flight profiles (cruise, pattern work, orbits)
- Simpler code with fewer edge cases

### DD-002: Shared Memory Cache

**Decision**: Prefetcher uses the same memory cache adapter instance as the pipeline.

**Rationale**:
- Accurate cache hit detection
- No duplicate cache instances consuming memory
- Consistent behavior between prefetch and on-demand requests

### DD-003: Per-Request Timeout

**Decision**: Each prefetch request has a 10-second timeout via CancellationToken.

**Rationale**:
- Prevents stuck jobs from blocking the queue
- Matches FUSE on-demand request timeout
- Failed requests are TTL-tracked to prevent immediate retry

### DD-004: TTL Tracking

**Decision**: Recently-attempted tiles are skipped for 60 seconds.

**Rationale**:
- Prevents hammering provider with failed requests
- Reduces wasted bandwidth
- Allows transient failures to recover

### DD-005: Strategy Pattern

**Decision**: Use `Prefetcher` trait for strategy abstraction.

**Rationale**:
- Enables runtime strategy selection
- Allows A/B testing of different algorithms
- Clean separation of concerns
- Easy to add new strategies

## Metrics

TUI dashboard displays:
- Tiles predicted (total in radius)
- Tiles submitted (new requests this cycle)
- In-flight skipped (TTL or already processing)
- Cycle number
- Aircraft position

Log entries show detailed cycle information:
```
INFO Prefetch cycle complete lat="55.6292" lon="12.6734" tile_row=5131 tile_col=8768
     total=49 cache_hits=38 submitted=7 ttl_skipped=4
```

## Future Enhancements

### FE-001: Heading-Aware Prioritization

Prioritize tiles in the direction of flight within the radial grid.

### FE-002: Adaptive Radius

Adjust radius based on ground speed - larger radius at higher speeds.

### FE-003: Cache Visualization

Map view showing cached tiles, prefetch requests, and aircraft position.

### FE-004: Flight Plan Integration

Parse X-Plane flight plan to pre-fetch entire route before takeoff.

---

## Changelog

| Date | Author | Changes |
|------|--------|---------|
| 2025-12-21 | Claude | Initial design document |
| 2025-12-22 | Claude | Implementation updates: ForeFlight protocol support, PrefetchCondition trait, MinimumSpeedCondition (30kt default), removed redundant cache checking (DdsHandler handles it), heading normalization (0-360) |
| 2025-12-23 | Claude | Major refactor: RadialPrefetcher as recommended strategy (49 tiles vs 21,000+), Prefetcher trait for strategy pattern, shared memory cache adapter, 10-second request timeout, TTL tracking for failed tiles |
