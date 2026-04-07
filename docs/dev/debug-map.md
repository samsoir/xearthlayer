# Debug Map

A live browser-based map for visualising XEarthLayer's prefetch state in real time. Feature-gated behind `debug-map` — not included in standard builds.

## Quick Start

```bash
# Build with debug map + GPU encoding (release mode)
make debug-build

# Run (starts debug map server alongside normal operation)
make debug-run

# Or manually:
cargo build --release --features debug-map
cargo run --release --features debug-map -- run
```

Open `http://localhost:8087` in a browser.

## What It Shows

### Map Layers

| Layer | Visual | Description |
|-------|--------|-------------|
| **Aircraft** | Blue dot + heading line | Current position and track from X-Plane Web API |
| **Prefetch Box** | Dashed blue rectangle | The sliding prefetch box bounds (heading-biased) |
| **DSF Regions** | Coloured 1°×1° squares | State of each DSF region — see colour key below |
| **DDS Tiles** | Small coloured squares (zoom 8+) | Individual tile status visible when zoomed in |

### Region Colours

| Colour | State | Meaning |
|--------|-------|---------|
| **Green** | Prefetched | Tiles were ready before X-Plane asked — prefetch working |
| **Yellow** | In Progress | Prefetch has submitted tiles, awaiting completion |
| **Red** | FUSE Loaded | X-Plane had to wait — tiles generated on-demand (prefetch missed) |
| **Orange** | Mixed | Some tiles prefetched, some loaded on-demand |
| **Grey** | No Coverage | No scenery data exists for this region |
| **Purple** | Patched | Region owned by a scenery patch (custom mesh/elevation) |
| **Blue outline** | Retained | In the retention window (tracked for eviction management) |

### DDS Tile Colours (zoom 8+)

When zoomed in, individual DDS tiles (~0.25° at ZL12) appear:

| Colour | Meaning |
|--------|---------|
| **Red** | FUSE on-demand generated — X-Plane waited for this tile |
| **Light green** | FUSE cache hit — was prefetched, served from cache |
| **Dark green** | Prefetch generated — tile created ahead of X-Plane |

### Stats Panel (top right)

- **Sim state**: NORMAL, PAUSED, LOADING, REPLAY, GROUND
- **Position**: Aircraft lat/lon, track, ground speed, altitude
- **Regions**: Total tracked, prefetched count, in-progress count
- **Tiles Submitted**: Total prefetch tiles submitted this session
- **Mode**: Current prefetch strategy mode

### Map Controls

- **Drag** to pan (disables auto-follow)
- **Double-click** to re-enable auto-follow on aircraft
- **Scroll** to zoom
- **Hover** over a region/tile for tooltip with details (DSF coordinates, hit/miss counts)

## Interpreting the Map

### Healthy Prefetch

A well-functioning prefetch system shows:
- **Green regions ahead** of the aircraft in the direction of travel
- **The prefetch box (dashed blue) covering green regions** beyond the aircraft
- **No red regions** appearing at the leading edge

### Prefetch Problems

| Pattern | Problem | Likely Cause |
|---------|---------|-------------|
| Red at leading edge | X-Plane outran prefetch | Box too small or tiles generating too slowly |
| Large red core on ground | Initial load uncached | Normal — X-Plane loads before telemetry connects |
| Orange everywhere | Prefetch too slow | Tiles not completing before X-Plane requests them |
| Green behind, red ahead | Wrong bias direction | Track heading not matching actual direction of travel |
| Box inside red area | Box undersized | `forward_margin`/`behind_margin` too small |

### Cold Start Behaviour

On first load at an airport:
1. **Red core appears** — X-Plane loads scenery before XEL has position data
2. **Orange ring forms** — ground strategy starts, overlaps with X-Plane's ongoing load
3. **Green ring expands outward** — ground strategy gets ahead of X-Plane

This is expected. The red core is the unavoidable cold start. The green ring shows prefetch working once telemetry connects.

## Architecture

```
Executor Daemon ──record_with_tile()──▶ TileActivityTracker (global)
                                              │
                                              ▼
Axum Server ◀──collect_snapshot()──── DebugMapState
  GET /           (every 2s)              │
  GET /api/state                          ├─ SharedAircraftPosition
                                          ├─ SharedSimState
                                          ├─ GeoIndex
                                          ├─ SharedPrefetchStatus
                                          └─ TileActivityTracker
```

- **Port**: 8087 (hardcoded, different from X-Plane Web API on 8086)
- **Poll interval**: 2 seconds (JavaScript `setInterval`)
- **Feature gate**: `#[cfg(feature = "debug-map")]` — all code excluded from standard builds
- **Server starts**: Inside `ServiceOrchestrator::initialize_services()` after all services are ready

## Configuration

No configuration needed. The debug map server starts automatically when the binary is built with `--features debug-map`.

The port (8087) is currently hardcoded. If it conflicts, modify `DEFAULT_DEBUG_MAP_PORT` in `debug_map/server.rs`.
