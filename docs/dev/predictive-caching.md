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
                           │ UDP Port (configurable, default 49003)
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                   Telemetry Listener                             │
│         Parse packets → AircraftState { lat, lon, hdg, gs, alt } │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                   Tile Prediction Engine                         │
│  ┌─────────────────┐    ┌─────────────────┐                     │
│  │  Flight Cone    │    │  Radial Buffer  │                     │
│  │  (heading+speed)│    │  (current pos)  │                     │
│  └────────┬────────┘    └────────┬────────┘                     │
│           └──────────┬───────────┘                              │
│                      ▼                                          │
│           Prioritized Tile List                                 │
│     (cone tiles first, then radial, by distance)                │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                   Pre-fetch Scheduler                            │
│  - Filters already-cached tiles                                 │
│  - Yields to on-demand requests (lower priority)                │
│  - Submits to existing pipeline                                 │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│              Existing Pipeline (unchanged)                       │
│         Download → Assemble → Encode → Cache (Memory + Disk)    │
└─────────────────────────────────────────────────────────────────┘
```

### New Components

#### 1. Telemetry Listener (`xearthlayer/src/prefetch/listener.rs`)

Listens for X-Plane UDP broadcast packets and extracts aircraft state.

**X-Plane UDP Format**: X-Plane broadcasts flight data over UDP. The relevant data output indices are:
- Index 3: Speeds (kias, keas, ktas, ktgs) - ground speed in knots
- Index 17: Pitch, roll, magnetic heading, true heading
- Index 20: Latitude, longitude, altitude MSL

**Output**: `AircraftState` struct updated at ~1-2 Hz

```rust
pub struct AircraftState {
    pub latitude: f64,      // degrees
    pub longitude: f64,     // degrees
    pub altitude_msl: f32,  // feet
    pub heading_true: f32,  // degrees (0-360)
    pub ground_speed: f32,  // knots
    pub timestamp: Instant,
}
```

#### 2. Tile Prediction Engine (`xearthlayer/src/prediction/`)

Calculates which tiles should be pre-fetched based on aircraft state.

**Prediction Strategies**:

1. **Flight Cone**: Tiles ahead of the aircraft
   - Projects aircraft position forward based on heading and ground speed
   - Cone angle: ±45° from heading (configurable)
   - Cone distance: 180 seconds of flight time (configurable)
   - Tiles ordered by time-to-reach (closest first)

2. **Radial Buffer**: Tiles around current position
   - Catches lateral movement, orbits, unexpected turns
   - Radius: 3 tile widths (configurable)
   - Tiles ordered by distance from current position

**Priority Assignment**:
```
Priority 0 (highest): On-demand FUSE requests
Priority 1: Cone tiles (ordered by time-to-reach)
Priority 2: Radial tiles (ordered by distance)
```

#### 3. Pre-fetch Scheduler (`xearthlayer/src/prefetch/`)

Manages the flow of predicted tiles into the pipeline.

**Responsibilities**:
- Receive predicted tile list from prediction engine
- Filter out tiles already in memory or disk cache
- Submit tiles to pipeline with appropriate priority
- Respect pipeline concurrency limits
- Cancel stale predictions when aircraft state changes significantly

**Key Design Decision**: Pre-fetch requests use the **same pipeline** as on-demand requests. This ensures:
- Request coalescing works automatically (no duplicate work)
- Concurrency limits are respected
- Cache behavior is consistent
- No code duplication

### Modified Components

#### Pipeline Priority System

The existing pipeline needs priority support to ensure on-demand requests are never starved.

**Approach**: Add priority to request submission. On-demand requests get priority 0, pre-fetch requests get priority 1-2. The pipeline processes higher-priority requests first.

**Implementation Options**:
1. Priority queue for pending requests
2. Separate channels with priority polling
3. Semaphore reservation for on-demand requests

Decision: TBD during implementation

## Configuration

New configuration keys in `[prefetch]` section:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `prefetch.enabled` | bool | `true` | Enable/disable predictive caching |
| `prefetch.udp_port` | u16 | `49003` | X-Plane UDP broadcast port |
| `prefetch.cone_angle` | f32 | `45.0` | Half-angle of prediction cone (degrees) |
| `prefetch.cone_distance` | u32 | `180` | Look-ahead time (seconds) |
| `prefetch.radial_radius` | u32 | `3` | Radial buffer in tile widths |

Example `config.ini`:
```ini
[prefetch]
enabled = true
udp_port = 49003
cone_angle = 45
cone_distance = 180
radial_radius = 3
```

## X-Plane Setup

Users must enable UDP data output in X-Plane:

1. Settings → Data Output
2. Enable "Network via UDP"
3. Set destination IP to localhost (127.0.0.1) or machine running XEarthLayer
4. Set port (default 49003)
5. Enable data indices: 3 (speeds), 17 (orientation), 20 (position)

## Design Decisions

### DD-001: Reuse Existing Pipeline

**Decision**: Pre-fetch requests go through the same pipeline as on-demand requests.

**Rationale**:
- Request coalescing works automatically between pre-fetch and on-demand
- Consistent caching behavior (memory + disk)
- No code duplication
- Existing concurrency limits apply

**Trade-off**: Need to implement pipeline prioritization.

### DD-002: LRU Cache Eviction

**Decision**: Use standard LRU eviction, no reserved pre-fetch budget.

**Rationale**:
- Pre-fetched tiles are typically the freshest in cache
- Simplifies initial implementation
- LRU naturally keeps recently-predicted tiles

**Future Enhancement**: Consider reserved budget if eviction becomes problematic.

### DD-003: Same Zoom Level as On-Demand

**Decision**: Pre-fetch at same zoom level as on-demand requests (determined by scenery package).

**Rationale**:
- Simplifies implementation
- Avoids zoom-level mismatch issues
- Altitude-based zoom can be added later if needed

### DD-004: Disk Cache for Pre-fetched Tiles

**Decision**: Pre-fetched tiles are written to disk cache, same as on-demand.

**Rationale**:
- Consistent behavior
- Tiles persist across sessions
- User may fly same route again

## Future Enhancements

### FE-001: In-Memory Cache Index

Maintain a `HashSet<TileKey>` of all cached tiles (memory + disk) for O(1) lookup.

**Benefits**:
- Instant filtering of already-cached tiles from pre-fetch queue
- Avoid disk I/O for cache existence checks
- Updated on cache insert/evict

### FE-002: Flight Plan Awareness

Parse X-Plane flight plan data to pre-fetch along planned route.

**Benefits**:
- Long-range prediction for IFR flights
- Pre-fetch entire route before takeoff

### FE-003: Altitude-Based Zoom

Adjust pre-fetch zoom level based on altitude.

**Benefits**:
- Higher altitude = wider view = lower zoom levels
- Better resource utilization at cruise altitude

### FE-004: Adaptive Prediction

Learn from actual tile requests to improve prediction accuracy.

**Benefits**:
- Self-tuning cone angle and distance
- Account for user's flying style

## Implementation Phases

### Phase 1: Telemetry Listener
- UDP socket listener
- X-Plane packet parsing
- AircraftState struct and updates

### Phase 2: Tile Prediction Engine
- Coordinate conversion (lat/lon → tile coordinates)
- Flight cone calculation
- Radial buffer calculation
- Priority assignment

### Phase 3: Pipeline Priority Support
- Priority-aware request handling
- Ensure on-demand requests are never starved

### Phase 4: Pre-fetch Scheduler
- Cache existence filtering
- Request submission to pipeline
- Stale prediction cancellation

### Phase 5: Configuration and Integration
- Config keys and parsing
- CLI integration
- TUI metrics for pre-fetch activity

### Phase 6: Testing and Tuning
- Unit tests for prediction math
- Integration tests with mock telemetry
- Real-world testing and parameter tuning

## Testing Strategy

### Unit Tests
- Coordinate conversion accuracy
- Cone/radial tile calculation
- Priority ordering
- UDP packet parsing

### Integration Tests
- Mock telemetry → predicted tiles
- Pre-fetch → pipeline → cache flow
- Coalescing between pre-fetch and on-demand

### Manual Testing
- FPS monitoring with/without pre-fetch
- Various flight profiles (cruise, approach, pattern work)
- Different network/storage configurations

## Metrics

New TUI metrics to add:
- Pre-fetch queue depth
- Pre-fetch hits (tiles requested by sim that were pre-fetched)
- Pre-fetch waste (tiles pre-fetched but never used)
- Telemetry status (connected/disconnected)

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| UDP packet loss | Missing telemetry updates | Interpolate from last known state |
| Prediction misses | Wasted bandwidth | Tune cone angle, rely on radial buffer |
| Pipeline starvation | On-demand latency | Strict priority enforcement |
| Memory pressure | Cache thrashing | Monitor and tune, consider reserved budget |
| X-Plane config burden | User friction | Document setup, consider auto-detection |

## Open Questions

1. **Telemetry interpolation**: How to handle gaps in UDP data?
2. **Prediction update rate**: 1 Hz vs 2 Hz vs on-change?
3. **Stale prediction handling**: When to cancel in-flight pre-fetch requests?
4. **TUI integration**: How to display pre-fetch activity?

---

## Changelog

| Date | Author | Changes |
|------|--------|---------|
| 2025-12-21 | Claude | Initial design document |
