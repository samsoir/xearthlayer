# Sliding Prefetch Box Design

**Issue:** #86 (scenery window drift) — revised approach after flight testing
**Date:** 2026-03-17
**Status:** Draft

## Problem

The boundary-monitor approach to prefetching has a fundamental timing issue: the trigger distance required for lead time (≥1.5°) conflicts with the dead zone required for a fixed-size window (trigger < half-window). Flight testing over dense European coverage (LOWW→LPPT) showed:

- X-Plane loads tiles **3.3–3.7° ahead** of the aircraft in the direction of travel
- X-Plane loads **2 DSF columns/rows beyond its current edge** when approaching a boundary
- The boundary crossing fires when the aircraft is ~1° from the window edge
- By then, X-Plane has already requested tiles 2° beyond the edge — **our prefetch is too late**
- Result: 20-second freeze at DSF boundaries over dense European scenery

## Empirical Data

### LOWW→LPPT Flight (2026-03-17, heading ~260° WSW)

**X-Plane loading pattern (first FUSE miss per DSF region vs aircraft position):**

| Time | DSF Region | Aircraft Position | Ahead (lon) | Notes |
|------|-----------|-------------------|-------------|-------|
| 20:00:01 | +47+014 | 47.5°N, 15.6°E | 1.6° | Near aircraft |
| 20:01:20 | +46+012 | 47.5°N, 15.4°E | 3.4° | Far ahead |
| 20:06:59 | +46+011 | 47.3°N, 14.5°E | 3.5° | **THE FREEZE** |
| 20:11:40 | +47+010 | 47.1°N, 13.7°E | 3.7° | Far ahead |
| 20:17:49 | +45+009 | 46.9°N, 12.5°E | 3.5° | Consistent |

**Loading extent from aircraft:**

| Direction | Extent | Notes |
|-----------|--------|-------|
| Ahead (west) | 3.3–3.7° | Critical dimension, consistent |
| Behind (east) | ~0.5° | Minimal trailing |
| Left of track (south) | ~4° | Heading had south component |
| Right of track (north) | ~2° | Trailing side |

### Key Finding

X-Plane loads **2° of new tiles beyond its current boundary** when the aircraft is ~1° from that boundary. If we maintain tiles prefetched **3° ahead** of the aircraft, we always cover X-Plane's 2° look-ahead with 1° of margin:

```
Aircraft ──1°──▶ XP edge ──2°──▶ XP new load limit
Aircraft ────────3°────────────▶ Prefetch box edge
                                 ✓ Covered
```

## Design: Sliding Prefetch Box

### Concept

Replace the boundary-monitor/scenery-window model with a **sliding prefetch box** that moves with the aircraft every telemetry tick. Any DSF region that enters the box is prefetched. No trigger zones, no re-centering, no dead zones.

### Box Shape

The box is a rectangle in lat/lon space, biased in the direction of travel based on the aircraft's track heading.

**Per-axis rule:**
- If the aircraft has a forward component on an axis → **3° ahead, 1° behind**
- If the aircraft has no component on an axis → **symmetric (2° each side)**

The forward/behind determination uses the track heading decomposed into lat/lon:

```
lon_forward = -sin(track)   // positive = moving east
lat_forward = -cos(track)   // positive = moving north

// Latitude axis
if lat_forward > 0 (moving north):
    lat_min = aircraft_lat - 1°    (behind)
    lat_max = aircraft_lat + 3°    (ahead)
else if lat_forward < 0 (moving south):
    lat_min = aircraft_lat - 3°    (ahead)
    lat_max = aircraft_lat + 1°    (behind)
else (due east/west):
    lat_min = aircraft_lat - 2°    (symmetric)
    lat_max = aircraft_lat + 2°    (symmetric)

// Longitude axis (same logic with lon_forward)
if lon_forward > 0 (moving east):
    lon_min = aircraft_lon - 1°
    lon_max = aircraft_lon + 3°
else if lon_forward < 0 (moving west):
    lon_min = aircraft_lon - 3°
    lon_max = aircraft_lon + 1°
else (due north/south):
    lon_min = aircraft_lon - 2°
    lon_max = aircraft_lon + 2°
```

### Examples

**Heading 270° (due west):**
- `sin(270°) = -1` → lon: 3° west, 1° east
- `cos(270°) = 0` → lat: 2° each side
- Box: 4° lat × 4° lon, biased west

**Heading 225° (southwest):**
- `sin(225°) = -0.707` → lon: 3° west, 1° east
- `cos(225°) = -0.707` → lat: 3° south, 1° north
- Box: 4° lat × 4° lon, biased SW

**Heading 180° (due south):**
- `sin(180°) = 0` → lon: 2° each side
- `cos(180°) = -1` → lat: 3° south, 1° north
- Box: 4° lat × 4° lon, biased south

**Heading 260° (WSW — the LOWW→LPPT track):**
- `sin(260°) = -0.985` → lon: 3° west, 1° east
- `cos(260°) = -0.174` → lat: 3° south, 1° north (any component biases)
- Box: 4° lat × 4° lon, biased WSW

### Per-Tick Operation

Each telemetry tick (~0.5s):

1. Compute box from `(aircraft_lat, aircraft_lon, track_heading)`
2. Enumerate DSF regions that intersect the box
3. Filter out regions already prefetched or in-progress (via GeoIndex)
4. Filter out regions with no scenery coverage (patches, packages)
5. For new regions: expand to DDS tiles, submit to executor

### Throttling

- **Backpressure:** Same executor load check as current system — skip cycle if >80% utilization
- **Rate limiting:** `max_tiles_per_cycle` cap prevents flooding
- **Deduplication:** GeoIndex tracks `PrefetchedRegion` state (InProgress, Prefetched, NoCoverage)
- **Eviction:** Regions that leave the box are marked for GeoIndex cleanup

### Configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `prefetch.forward_margin` | 3.0 | Degrees ahead of aircraft in direction of travel |
| `prefetch.behind_margin` | 1.0 | Degrees behind aircraft |
| `prefetch.lateral_margin` | 2.0 | Degrees to each side when axis has no forward component |

### What This Replaces

- `SceneryWindow` state machine (Uninitialized → Assumed → Ready)
- `BoundaryMonitor` dual-axis edge detection
- `BoundaryCrossing` prediction and urgency scoring
- `center_on_position()` / `update_from_tracker()` sliding logic
- `trigger_distance` configuration
- `load_depth_lat` / `load_depth_lon` configuration

The boundary strategy (`BoundaryStrategy`) and region lifecycle (`PrefetchedRegion` states) remain — only the detection/triggering mechanism changes.

### What This Preserves

- `GeoIndex` for region state tracking
- `PrefetchedRegion` lifecycle (InProgress → Prefetched / NoCoverage)
- Region eviction when outside retained area
- `SceneryWindow.update_retention()` for retained region tracking
- `check_for_rebuild()` for teleport/settings change detection
- Four-tier filter pipeline (local → memory cache → patches → disk)
- Backpressure and executor load management

## Advantages Over Boundary Monitors

1. **No trigger timing problem** — box is always 3° ahead, not waiting for aircraft to approach an edge
2. **No dead zone constraint** — no re-centering needed, box just moves
3. **Heading-aware** — biases prefetch in direction of travel
4. **Simpler code** — no state machine, no dual monitors, no urgency scoring
5. **Continuous coverage** — every tick checks for new regions, not just at crossing events

## Design Decisions

1. **Ground phase:** Keep `GroundStrategy` ring-based approach for ground operations. The sliding box applies in cruise only. The phase detector manages this transition. In future the sliding box may subsume ground strategy, but maintaining separation justifies the orchestrator pattern and leaves room for additional strategies.

2. **Transition phase:** The box applies immediately at full size on entering cruise. The `TransitionThrottle` ramp independently controls submission rate (25% → 100% over 30s). Box size and submission rate are independent concerns.

3. **DSF-native math:** X-Plane loads by DSF integer boundaries, not by geographic distance. The box margins are in degrees, and region enumeration uses integer DSF coordinates:
   ```
   dsf_lat_min = floor(box_lat_min)
   dsf_lat_max = floor(box_lat_max)
   dsf_lon_min = floor(box_lon_min)
   dsf_lon_max = floor(box_lon_max)
   ```
   No cos(lat) correction — raw degrees match X-Plane's loading model.

4. **No component threshold:** Any non-zero heading component biases that axis to 3° ahead / 1° behind. Only exact cardinal headings (0°, 90°, 180°, 270°) produce symmetric 2°/2° on the perpendicular axis. At heading 265°, the small south component still biases lat 3° south / 1° north. Rationale: even a slight south component means the aircraft will eventually need those southern tiles. Prefetching a few extra tiles is negligible cost compared to a freeze from being too conservative. Simplicity over cleverness — the boundary monitor approach failed precisely because it was too sophisticated.
