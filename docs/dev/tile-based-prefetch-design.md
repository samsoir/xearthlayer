# Tile-Based Prefetch Strategy Design

## Status

**Superseded** — The observation-based concepts from this document (burst detection, tile prediction from loaded set, heading inference) have been realised in the [Adaptive Prefetch Design v2.0](adaptive-prefetch-design.md) as the boundary-driven model with `SceneryWindow`, dual boundary monitors, and the three-window architecture. This document is retained for historical reference.

## Problem Statement

The current prefetching strategies (`RadialPrefetcher`, `HeadingAwarePrefetcher`) use geometric models (circular rings, forward cones) to predict which DDS textures to prefetch. However, analysis of X-Plane 12's actual loading behavior reveals a fundamentally different pattern:

1. **X-Plane loads whole 1° × 1° tiles**, not individual textures
2. **Loading occurs in bursts**, followed by quiet periods of 1-2+ minutes
3. **The loaded area forms a diamond/rhombus** shape aligned with aircraft heading
4. **Tiles are loaded in rows/columns**, not scattered patterns

The current approach wastes resources prefetching textures that may not be needed while missing textures that will be requested.

## Evidence

Screenshots from X-Plane 12 (January 2026) show:

- Loaded scenery forms a **heading-aligned diamond** shape
- Clear **1° grid boundaries** visible in the loaded area
- **~8-10 tiles** loaded in each direction from aircraft position
- At 60°N latitude, this represents ~480-600nm N-S, ~240-300nm E-W

Key observation: The diamond's edges align perfectly with tile boundaries, confirming X-Plane loads **entire DSF tiles**, not partial coverage.

## Goals

1. **Align with X-Plane's behavior**: Prefetch at the tile level, not individual textures
2. **Exploit quiet periods**: Prefetch aggressively when X-Plane isn't loading
3. **Predict next tiles**: Use heading/movement to anticipate X-Plane's next load burst
4. **Preserve flexibility**: Maintain strategy abstraction for future X-Plane changes
5. **Graceful degradation**: Work without telemetry by inferring from load patterns

## Non-Goals

- Replacing existing prefetchers (they remain available via configuration)
- Modifying X-Plane's loading behavior
- Supporting partial tile prefetching

## Design

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Prefetch Strategy Selection                      │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  config.ini: prefetch.strategy = "tile_based"                       │
│                                                                     │
│                        ┌──────────────┐                             │
│                        │  Prefetcher  │ ◄── Trait (existing)        │
│                        │    trait     │                             │
│                        └──────┬───────┘                             │
│                               │                                     │
│          ┌────────────────────┼────────────────────┐                │
│          │                    │                    │                │
│          ▼                    ▼                    ▼                │
│  ┌───────────────┐   ┌───────────────┐   ┌───────────────┐         │
│  │    Radial     │   │ HeadingAware  │   │  TileBased    │         │
│  │  Prefetcher   │   │  Prefetcher   │   │  Prefetcher   │ ◄── NEW │
│  │               │   │               │   │               │         │
│  └───────────────┘   └───────────────┘   └───────────────┘         │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Component Design

#### 1. TileBurstTracker

Monitors FUSE activity to detect X-Plane's loading patterns.

```rust
/// Tracks X-Plane's tile loading activity and detects burst boundaries.
pub struct TileBurstTracker {
    /// Tiles accessed in the current burst
    current_burst_tiles: HashSet<TileCoord>,

    /// Tiles from the previous burst (for movement analysis)
    previous_burst_tiles: HashSet<TileCoord>,

    /// Timestamp of last FUSE DDS request
    last_request_time: Instant,

    /// Duration of quiet required to end a burst
    quiet_threshold: Duration,

    /// Whether we're currently in a quiet period
    in_quiet_period: bool,
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct TileCoord {
    pub lat: i32,  // Floor of latitude (e.g., 60 for 60.5°N)
    pub lon: i32,  // Floor of longitude (e.g., -146 for -145.5°W)
}

impl TileBurstTracker {
    /// Record a DDS request, extracting tile coordinates from the path
    pub fn record_request(&mut self, dds_path: &Path);

    /// Check if we're in a quiet period (no requests for quiet_threshold)
    pub fn is_quiet(&self) -> bool;

    /// Get tiles loaded in the most recent burst
    pub fn current_tiles(&self) -> &HashSet<TileCoord>;

    /// Get tiles from the previous burst
    pub fn previous_tiles(&self) -> &HashSet<TileCoord>;

    /// Call when quiet period detected to rotate burst state
    pub fn finalize_burst(&mut self);

    /// Infer heading from tile expansion between bursts
    pub fn infer_heading(&self) -> Option<InferredHeading>;
}

pub struct InferredHeading {
    /// Primary direction of expansion
    pub direction: Direction,
    /// Confidence based on tile delta analysis
    pub confidence: f32,
}

pub enum Direction {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}
```

#### 2. TilePredictor

Predicts which tiles X-Plane will load next.

```rust
/// Predicts the next tiles X-Plane will request based on current state.
pub struct TilePredictor {
    /// Number of tile rows to prefetch ahead on each axis
    rows_ahead: u32,
}

impl TilePredictor {
    /// Predict next tiles to prefetch.
    ///
    /// Always predicts tiles on BOTH the latitude and longitude axes,
    /// since aircraft rarely fly perfect cardinal headings.
    ///
    /// # Arguments
    /// * `loaded_tiles` - Tiles X-Plane has currently loaded
    /// * `heading` - Aircraft heading (from telemetry or inferred)
    ///
    /// # Returns
    /// Vec of tile coordinates to prefetch, ordered by priority
    pub fn predict_next_tiles(
        &self,
        loaded_tiles: &HashSet<TileCoord>,
        heading: Option<f64>,
    ) -> Vec<TileCoord>;
}
```

**Prediction Algorithm:**

```
1. Find the bounding box of currently loaded tiles
2. Determine the "forward" edges based on heading:
   - Heading available: Use heading to identify forward quadrant
   - No heading: Use all edges (conservative approach)
3. For BOTH axes (latitude AND longitude):
   - Identify the forward edge tiles
   - Add `rows_ahead` rows of tiles beyond that edge
4. Return unique tile set, prioritized by:
   - Distance from aircraft (if position known)
   - Direction alignment with heading (if known)
```

**Example (heading ~045° / NE):**

```
Current loaded tiles (X):          Predicted tiles (P):

        lon -147  -146  -145           lon -147  -146  -145  -144
       ┌──────────────────────        ┌────────────────────────────
lat 62 │                              │  P      P      P      P    ◄─ N+1 row
lat 61 │   X      X      X            │  X      X      X      P    ◄─ E+1 col
lat 60 │   X      X      X            │  X      X      X      P
lat 59 │   X      X      X            │  X      X      X      P
```

#### 3. TileBasedPrefetcher

Orchestrates the tile-based prefetching strategy.

```rust
/// Tile-based prefetcher that aligns with X-Plane's actual loading behavior.
pub struct TileBasedPrefetcher {
    /// Tracks X-Plane's burst loading patterns
    burst_tracker: TileBurstTracker,

    /// Predicts next tiles based on movement
    predictor: TilePredictor,

    /// Index of available ortho tiles and their DDS files
    ortho_index: Arc<OrthoUnionIndex>,

    /// Pipeline for prefetching DDS files
    pipeline: Arc<DdsHandler>,

    /// Telemetry receiver (optional, for precise heading)
    telemetry: Option<TelemetryReceiver>,

    /// Tiles currently being prefetched
    active_prefetch_tiles: HashSet<TileCoord>,

    /// Cancellation token for stopping prefetch on burst
    cancel_token: CancellationToken,
}

impl Prefetcher for TileBasedPrefetcher {
    async fn run(&mut self) -> Result<()> {
        loop {
            // Phase 1: Wait for quiet period
            self.wait_for_quiet().await;

            // Phase 2: Get heading (telemetry or inferred)
            let heading = self.get_heading();

            // Phase 3: Predict next tiles
            let next_tiles = self.predictor.predict_next_tiles(
                self.burst_tracker.current_tiles(),
                heading,
            );

            // Phase 4: Prefetch all DDS files in predicted tiles
            // No throttling - use full bandwidth during quiet period
            for tile in next_tiles {
                if self.burst_tracker.burst_detected() {
                    // X-Plane started loading - stop immediately
                    break;
                }

                self.prefetch_tile(tile).await;
            }

            // Phase 5: Finalize burst tracking
            self.burst_tracker.finalize_burst();
        }
    }

    /// Prefetch all DDS files within a 1° tile
    async fn prefetch_tile(&self, tile: TileCoord) {
        let dds_files = self.ortho_index.dds_files_in_tile(tile.lat, tile.lon);

        // Prefetch all files in parallel (no artificial limits)
        let futures: Vec<_> = dds_files
            .into_iter()
            .map(|path| self.pipeline.prefetch(path))
            .collect();

        // Use select to allow cancellation on burst detection
        tokio::select! {
            _ = futures::future::join_all(futures) => {}
            _ = self.cancel_token.cancelled() => {}
        }
    }
}
```

### State Model

The system maintains a virtual model of tile states:

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Tile State Model                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐           │
│  │  Available  │────►│  Predicted  │────►│ Prefetching │           │
│  │             │     │             │     │             │           │
│  │ (OrthoIndex)│     │ (Predictor) │     │ (Pipeline)  │           │
│  └─────────────┘     └─────────────┘     └──────┬──────┘           │
│                                                 │                   │
│                                                 ▼                   │
│                      ┌─────────────┐     ┌─────────────┐           │
│                      │   Loaded    │◄────│   Cached    │           │
│                      │  (X-Plane)  │     │  (Memory/   │           │
│                      │             │     │   Disk)     │           │
│                      └─────────────┘     └─────────────┘           │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

| State | Description | Tracked By |
|-------|-------------|------------|
| **Available** | Tiles we have ortho packages for | `OrthoUnionIndex` |
| **Predicted** | Tiles we expect X-Plane to load next | `TilePredictor` |
| **Prefetching** | Tiles currently being prefetched | `TileBasedPrefetcher` |
| **Cached** | DDS files ready in memory/disk cache | `MemoryCache`, `DiskCache` |
| **Loaded** | Tiles X-Plane has requested | `TileBurstTracker` |

### Configuration

```ini
[prefetch]
# Enable/disable prefetching
enabled = true

# Strategy selection: "radial" | "heading_aware" | "tile_based"
strategy = tile_based

# Tile-based strategy settings
[prefetch.tile_based]
# Seconds of quiet before considering a burst complete
burst_quiet_threshold = 15

# Number of tile rows to prefetch ahead on each axis
rows_ahead = 1
```

### OrthoUnionIndex Extension

The `OrthoUnionIndex` needs a new method to enumerate DDS files within a tile:

```rust
impl OrthoUnionIndex {
    /// Returns all DDS file paths within a 1° tile.
    ///
    /// # Arguments
    /// * `lat` - Floor of latitude (e.g., 60 for any point 60.0-60.999°N)
    /// * `lon` - Floor of longitude (e.g., -146 for any point -146.0 to -145.001°W)
    ///
    /// # Returns
    /// Vec of virtual paths to DDS files that XEL can generate for this tile
    pub fn dds_files_in_tile(&self, lat: i32, lon: i32) -> Vec<PathBuf>;
}
```

**Implementation consideration**: This may require extending the index to track terrain files by tile coordinate, or scanning the terrain directory structure on demand.

### Burst Detection Integration

The `TileBurstTracker` integrates with the existing FUSE monitoring:

```rust
// In Fuse3OrthoUnionFS or DdsHandler
impl DdsRequestor for ... {
    fn request_dds(&self, path: &Path) -> ... {
        // Notify burst tracker of request
        if let Some(tracker) = &self.burst_tracker {
            tracker.record_request(path);
        }

        // Process request...
    }
}
```

### Telemetry Fallback

When telemetry is unavailable, heading is inferred from tile loading patterns:

```rust
impl TileBurstTracker {
    pub fn infer_heading(&self) -> Option<InferredHeading> {
        let prev = &self.previous_burst_tiles;
        let curr = &self.current_burst_tiles;

        if prev.is_empty() || curr.is_empty() {
            return None;
        }

        // Find tiles added in current burst
        let new_tiles: HashSet<_> = curr.difference(prev).collect();

        // Analyze which edge expanded
        let prev_bounds = bounding_box(prev);
        let curr_bounds = bounding_box(curr);

        let north_expansion = curr_bounds.max_lat - prev_bounds.max_lat;
        let south_expansion = prev_bounds.min_lat - curr_bounds.min_lat;
        let east_expansion = curr_bounds.max_lon - prev_bounds.max_lon;
        let west_expansion = prev_bounds.min_lon - curr_bounds.min_lon;

        // Determine primary direction from expansion
        // ... (return Direction with confidence)
    }
}
```

## Behavioral Characteristics

### During X-Plane Burst

```
X-Plane: Loading tiles rapidly
XEL:     Serving requests on-demand
         Prefetcher PAUSED (no competition for bandwidth)
         Burst tracker recording loaded tiles
```

### During Quiet Period

```
X-Plane: Idle (rendering, flying)
XEL:     Prefetcher ACTIVE
         - Predicts next tiles
         - Prefetches ALL DDS files in predicted tiles
         - Uses FULL bandwidth (no throttling)
         - Monitors for burst restart
```

### On Burst Restart

```
X-Plane: Starts loading new tiles
XEL:     Prefetcher STOPS immediately
         - Cancels in-flight prefetch requests
         - Returns bandwidth to on-demand serving
         - Begins tracking new burst
```

## Implementation Plan

### Phase 1: Foundation

1. Add `TileCoord` type and utilities
2. Implement `TileBurstTracker` with quiet period detection
3. Add FUSE integration to feed requests to tracker
4. Add configuration keys for tile-based strategy

### Phase 2: Prediction

1. Implement `TilePredictor` with both-axes prediction
2. Add heading inference from burst patterns
3. Integrate with existing `TelemetryListener`

### Phase 3: OrthoUnionIndex Extension

1. Analyze current index structure for tile-based queries
2. Implement `dds_files_in_tile()` method
3. Consider caching or pre-computing tile → DDS mappings

### Phase 4: Prefetcher Implementation

1. Implement `TileBasedPrefetcher` with `Prefetcher` trait
2. Add cancellation support for burst interruption
3. Integrate with existing pipeline (no throttling)

### Phase 5: Integration

1. Add strategy selection to configuration
2. Wire up in service initialization
3. Add metrics/logging for prefetch effectiveness

## Success Criteria

- [ ] Prefetcher correctly identifies tile boundaries from DDS paths
- [ ] Burst detection triggers within 1 second of X-Plane starting to load
- [ ] Quiet period detection works with 15-second threshold
- [ ] Both latitude and longitude rows are prefetched
- [ ] Prefetch stops immediately when burst detected
- [ ] Works without telemetry (heading inference)
- [ ] Works with telemetry (precise heading)
- [ ] Existing prefetchers remain functional via config

## Future Considerations

1. **X-Plane 13**: Laminar Research is reimplementing scenery loading. The strategy abstraction allows us to adapt without major refactoring.

2. **Adaptive thresholds**: The quiet threshold could be learned from observed X-Plane behavior rather than hardcoded.

3. **Tile priority within burst**: If bandwidth is limited, prioritize tiles closest to aircraft or most aligned with heading.

4. **Multi-zoom support**: Different zoom levels may have different prefetch priorities.

## References

- [Predictive Caching Design](predictive-caching.md) - Original prefetch design
- [Heading-Aware Prefetch Design](heading-aware-prefetch-design.md) - Cone-based approach
- [Consolidated Mounting Design](consolidated-mounting-design.md) - OrthoUnionIndex architecture
- X-Plane 12 map screenshots (January 2026) - Evidence of tile-based loading
