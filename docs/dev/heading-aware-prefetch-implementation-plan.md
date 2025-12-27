# Heading-Aware Prefetch Implementation Plan

## Overview

This document provides a step-by-step implementation plan for the heading-aware
prefetch system described in `heading-aware-prefetch-design.md`. The plan is
organized into phases that can be implemented incrementally, with each phase
delivering testable functionality.

**Estimated effort:** 3-5 days
**Branch:** `feature/heading-aware-prefetch`
**Related design:** `docs/dev/heading-aware-prefetch-design.md`

---

## Phase 1: Core Infrastructure (Day 1)

### 1.1 Create Prefetch Types Module

**File:** `xearthlayer/src/prefetch/types.rs` (new)

```rust
// Types shared across prefetch implementations

pub struct AircraftState {
    pub position: (f64, f64),      // (lat, lon)
    pub heading: f32,              // degrees true
    pub ground_speed_kt: f32,
    pub timestamp: Instant,
}

pub struct TileCoord {
    pub zoom: u8,
    pub row: u32,
    pub col: u32,
}

pub enum TurnState {
    Straight,
    Turning { rate: f32, direction: TurnDirection },
}

pub enum TurnDirection {
    Left,
    Right,
}

pub struct PrefetchTile {
    pub coord: TileCoord,
    pub priority: u32,  // Lower = higher priority
    pub zone: PrefetchZone,
}

pub enum PrefetchZone {
    ForwardCenter,
    ForwardEdge,
    LateralBuffer,
    RearBuffer,
}
```

**Tasks:**
- [ ] Create the types module
- [ ] Add `mod types;` to `prefetch/mod.rs`
- [ ] Write unit tests for type conversions

### 1.2 Create Coordinate Utilities Module

**File:** `xearthlayer/src/prefetch/coordinates.rs` (new)

Implement the coordinate mathematics from design Appendix A:

```rust
/// Convert lat/lon to tile coordinates at given zoom level
pub fn lat_lon_to_tile(lat: f64, lon: f64, zoom: u8) -> TileCoord

/// Convert tile coordinates to lat/lon (center of tile)
pub fn tile_to_lat_lon(row: u32, col: u32, zoom: u8) -> (f64, f64)

/// Calculate tile size in nautical miles at given latitude
pub fn tile_size_nm_at_lat(lat: f64, zoom: u8) -> f64

/// Project a position along a heading for a given distance
pub fn project_position(
    start: (f64, f64),
    heading_deg: f32,
    distance_nm: f32,
) -> (f64, f64)

/// Calculate bearing from one position to another
pub fn bearing_between(from: (f64, f64), to: (f64, f64)) -> f32

/// Calculate distance in nm between two positions
pub fn distance_nm(from: (f64, f64), to: (f64, f64)) -> f32
```

**Tasks:**
- [ ] Implement coordinate conversion functions
- [ ] Implement projection functions
- [ ] Write comprehensive unit tests with known values
- [ ] Test at various latitudes (equator, mid-lat, polar)

### 1.3 Create Configuration Structures

**File:** `xearthlayer/src/prefetch/config.rs` (new)

```rust
pub struct HeadingAwarePrefetchConfig {
    // Cone parameters
    pub cone_half_angle: f32,           // default: 30.0
    pub min_lookahead_nm: f32,          // default: 5.0
    pub max_lookahead_nm: f32,          // default: 15.0
    pub lookahead_time_secs: f32,       // default: 60.0

    // Buffer parameters
    pub lateral_buffer_angle: f32,      // default: 45.0
    pub lateral_buffer_depth: u8,       // default: 3
    pub rear_buffer_tiles: u8,          // default: 3

    // Turn detection
    pub turn_rate_threshold: f32,       // default: 1.0 deg/sec
    pub turn_widening_factor: f32,      // default: 1.5
    pub turn_hold_time_secs: f32,       // default: 10.0

    // General
    pub zoom: u8,                       // default: 14
    pub max_tiles_per_cycle: usize,     // default: 100
    pub cycle_interval_ms: u64,         // default: 1000
    pub attempt_ttl_secs: u64,          // default: 60
}

pub struct FuseInferenceConfig {
    pub max_request_age_secs: u64,      // default: 30
    pub min_requests_for_inference: usize, // default: 10
    pub confidence_threshold: f32,       // default: 0.5
    pub wide_cone_multiplier: f32,      // default: 1.5
}

impl Default for HeadingAwarePrefetchConfig { ... }
impl Default for FuseInferenceConfig { ... }
```

**Tasks:**
- [ ] Define configuration structures with defaults
- [ ] Add to main config file parsing (optional for phase 1)
- [ ] Write tests for default values

---

## Phase 2: Heading-Aware Cone Generator (Day 1-2)

### 2.1 Create Cone Generator

**File:** `xearthlayer/src/prefetch/cone.rs` (new)

```rust
pub struct ConeGenerator {
    config: HeadingAwarePrefetchConfig,
}

impl ConeGenerator {
    pub fn new(config: HeadingAwarePrefetchConfig) -> Self;

    /// Calculate lookahead distance based on ground speed
    pub fn calculate_lookahead_nm(&self, ground_speed_kt: f32) -> f32;

    /// Generate tiles in a forward cone
    pub fn generate_cone_tiles(
        &self,
        position: (f64, f64),
        heading: f32,
        lookahead_nm: f32,
    ) -> Vec<PrefetchTile>;

    /// Fill the cone (not just edge rays)
    fn fill_cone(&self, center: TileCoord, heading: f32,
                 depth: i32, half_angle: f32) -> Vec<TileCoord>;

    /// Calculate priority for a tile
    fn calculate_priority(&self, tile: TileCoord,
                          center: TileCoord, heading: f32) -> u32;
}
```

**Tasks:**
- [ ] Implement lookahead calculation
- [ ] Implement cone ray generation
- [ ] Implement cone fill algorithm
- [ ] Implement priority scoring
- [ ] Write unit tests with visual verification
- [ ] Test at different headings (0°, 90°, 180°, 270°, 45°)

### 2.2 Create Buffer Generator

**File:** `xearthlayer/src/prefetch/buffer.rs` (new)

```rust
pub struct BufferGenerator {
    config: HeadingAwarePrefetchConfig,
}

impl BufferGenerator {
    /// Generate lateral buffer tiles
    pub fn generate_lateral_buffer(
        &self,
        center: TileCoord,
        heading: f32,
        cone_half_angle: f32,
    ) -> Vec<PrefetchTile>;

    /// Generate rear buffer tiles
    pub fn generate_rear_buffer(
        &self,
        center: TileCoord,
        heading: f32,
    ) -> Vec<PrefetchTile>;

    /// Combine cone and buffer tiles with deduplication
    pub fn merge_tiles(
        cone: Vec<PrefetchTile>,
        lateral: Vec<PrefetchTile>,
        rear: Vec<PrefetchTile>,
    ) -> Vec<PrefetchTile>;
}
```

**Tasks:**
- [ ] Implement lateral buffer generation
- [ ] Implement rear buffer generation
- [ ] Implement tile merging with priority preservation
- [ ] Write unit tests

---

## Phase 3: Turn Detection (Day 2)

### 3.1 Create Turn Detector

**File:** `xearthlayer/src/prefetch/turn.rs` (new)

```rust
pub struct TurnDetector {
    config: HeadingAwarePrefetchConfig,
    last_heading: f32,
    last_update: Instant,
    smoothed_turn_rate: f32,
    turn_state: TurnState,
    turn_end_time: Option<Instant>,
}

impl TurnDetector {
    pub fn new(config: HeadingAwarePrefetchConfig) -> Self;

    /// Update with new heading, return current turn state
    pub fn update(&mut self, heading: f32) -> TurnState;

    /// Get current effective cone parameters (may be widened during turn)
    pub fn get_effective_cone_params(&self) -> ConeParameters;

    /// Check if we're in post-turn hold period
    fn in_turn_hold(&self) -> bool;
}

pub struct ConeParameters {
    pub half_angle: f32,
    pub center_offset: f32,  // Bias into turn direction
}
```

**Tasks:**
- [ ] Implement heading delta calculation with wrap handling
- [ ] Implement exponential smoothing
- [ ] Implement turn state transitions
- [ ] Implement post-turn hold timer
- [ ] Implement cone parameter adjustment
- [ ] Write unit tests simulating various turn profiles

---

## Phase 4: FUSE Request Analyzer (Day 2-3)

### 4.1 Create Request Analyzer

**File:** `xearthlayer/src/prefetch/fuse_analyzer.rs` (new)

```rust
pub struct FuseRequestAnalyzer {
    config: FuseInferenceConfig,
    recent_requests: VecDeque<(TileCoord, Instant)>,
    inferred_state: Option<InferredAircraftState>,
}

pub struct InferredAircraftState {
    pub position: (f64, f64),
    pub heading: f32,
    pub ground_speed_kt: f32,
    pub confidence: f32,
    pub last_update: Instant,
}

impl FuseRequestAnalyzer {
    pub fn new(config: FuseInferenceConfig) -> Self;

    /// Record a tile request from FUSE
    pub fn record_request(&mut self, tile: TileCoord);

    /// Get current inferred state (if available)
    pub fn get_inferred_state(&self) -> Option<&InferredAircraftState>;

    /// Prune old requests
    fn prune_old_requests(&mut self);

    /// Update inference from request patterns
    fn update_inference(&mut self);

    /// Find centroid of request cluster
    fn find_centroid(&self) -> (i64, i64);

    /// Find leading edge of recent requests
    fn find_leading_edge(&self) -> Vec<&TileCoord>;
}
```

**Tasks:**
- [ ] Implement request recording and pruning
- [ ] Implement centroid calculation
- [ ] Implement leading edge detection
- [ ] Implement heading inference
- [ ] Implement speed estimation
- [ ] Implement confidence calculation
- [ ] Write unit tests with simulated request patterns

### 4.2 Create FUSE Integration Hook

**File:** Modify `xearthlayer/src/fuse/fuse3/passthrough.rs`

Add a notification channel for tile requests:

```rust
// Add to Fuse3PassthroughFS struct
prefetch_notifier: Option<tokio::sync::mpsc::UnboundedSender<TileCoord>>,

// Add method to set notifier
pub fn set_prefetch_notifier(&mut self, tx: UnboundedSender<TileCoord>);

// In read() method, add notification
if let Some(ref notifier) = self.prefetch_notifier {
    if let Some(tile) = self.parse_dds_to_tile(&filename) {
        let _ = notifier.send(tile);
    }
}
```

**Tasks:**
- [ ] Add notification channel to FUSE filesystem
- [ ] Parse DDS filename to tile coordinates
- [ ] Send non-blocking notifications
- [ ] Write integration test

---

## Phase 5: Hybrid Prefetcher (Day 3)

### 5.1 Create Hybrid Prefetcher

**File:** `xearthlayer/src/prefetch/hybrid.rs` (new)

```rust
pub struct HybridPrefetcher {
    config: HeadingAwarePrefetchConfig,
    fuse_config: FuseInferenceConfig,

    // Components
    cone_generator: ConeGenerator,
    buffer_generator: BufferGenerator,
    turn_detector: TurnDetector,
    fuse_analyzer: FuseRequestAnalyzer,

    // State
    telemetry_state: Option<AircraftState>,
    last_known_position: Option<(f64, f64)>,
    current_mode: PrefetchMode,

    // Tile tracking
    pending_tiles: HashSet<TileCoord>,
    failed_attempts: HashMap<TileCoord, Instant>,
}

pub enum PrefetchMode {
    Telemetry,
    FuseInference,
    Radial,
}

impl Prefetcher for HybridPrefetcher {
    async fn prefetch_cycle(&mut self, cache: Arc<dyn TileCache>) -> PrefetchResult;
}

impl HybridPrefetcher {
    pub fn new(config: HeadingAwarePrefetchConfig,
               fuse_config: FuseInferenceConfig) -> Self;

    /// Update from telemetry (XGPS2)
    pub fn update_telemetry(&mut self, state: AircraftState);

    /// Record FUSE request for inference
    pub fn record_fuse_request(&mut self, tile: TileCoord);

    /// Select best available mode
    fn select_mode(&self) -> PrefetchMode;

    /// Generate tiles based on current mode
    fn generate_tiles(&mut self) -> Vec<PrefetchTile>;

    /// Filter tiles that are already cached or recently failed
    fn filter_tiles(&self, tiles: Vec<PrefetchTile>,
                    cache: &dyn TileCache) -> Vec<PrefetchTile>;
}
```

**Tasks:**
- [ ] Implement mode selection logic
- [ ] Implement telemetry mode tile generation
- [ ] Implement FUSE inference mode (wider cone)
- [ ] Implement radial fallback mode
- [ ] Implement tile filtering
- [ ] Implement the Prefetcher trait
- [ ] Write unit tests for each mode
- [ ] Write integration tests for mode transitions

### 5.2 Create Prefetch Coordinator

**File:** `xearthlayer/src/prefetch/coordinator.rs` (new)

Coordinates between telemetry listener, FUSE analyzer, and prefetcher:

```rust
pub struct PrefetchCoordinator {
    prefetcher: HybridPrefetcher,
    telemetry_rx: mpsc::Receiver<AircraftState>,
    fuse_rx: mpsc::UnboundedReceiver<TileCoord>,
    cache: Arc<dyn TileCache>,
    pipeline: Arc<DdsHandler>,
    shutdown: CancellationToken,
}

impl PrefetchCoordinator {
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(
            Duration::from_millis(self.prefetcher.config.cycle_interval_ms)
        );

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,

                Some(telem) = self.telemetry_rx.recv() => {
                    self.prefetcher.update_telemetry(telem);
                }

                Some(tile) = self.fuse_rx.recv() => {
                    self.prefetcher.record_fuse_request(tile);
                }

                _ = interval.tick() => {
                    self.run_prefetch_cycle().await;
                }
            }
        }
    }

    async fn run_prefetch_cycle(&mut self) {
        let result = self.prefetcher.prefetch_cycle(self.cache.clone()).await;
        // Log metrics, handle errors
    }
}
```

**Tasks:**
- [ ] Implement coordinator run loop
- [ ] Handle telemetry updates
- [ ] Handle FUSE notifications
- [ ] Run prefetch cycles on interval
- [ ] Add metrics/logging
- [ ] Write integration tests

---

## Phase 6: Integration (Day 3-4)

### 6.1 Update Module Structure

**File:** `xearthlayer/src/prefetch/mod.rs`

```rust
mod buffer;
mod cone;
mod config;
mod coordinates;
mod coordinator;
mod fuse_analyzer;
mod hybrid;
mod turn;
mod types;

// Re-export public API
pub use config::{HeadingAwarePrefetchConfig, FuseInferenceConfig};
pub use coordinator::PrefetchCoordinator;
pub use hybrid::{HybridPrefetcher, PrefetchMode};
pub use types::{AircraftState, TileCoord, PrefetchTile};

// Keep existing for backwards compatibility
pub use radial::RadialPrefetcher;
pub use strategy::Prefetcher;
```

**Tasks:**
- [ ] Update mod.rs with new modules
- [ ] Ensure backwards compatibility with RadialPrefetcher
- [ ] Update public API exports

### 6.2 Wire into Run Command

**File:** Modify `xearthlayer-cli/src/commands/run.rs`

```rust
// Add CLI flag for prefetch strategy
#[arg(long, default_value = "hybrid")]
prefetch_strategy: PrefetchStrategy,

enum PrefetchStrategy {
    Radial,   // Current behavior
    Hybrid,   // New heading-aware with fallback
    Off,      // Disable prefetch
}

// In run() function, create appropriate prefetcher
let prefetcher: Box<dyn Prefetcher> = match args.prefetch_strategy {
    PrefetchStrategy::Radial => Box::new(RadialPrefetcher::new(...)),
    PrefetchStrategy::Hybrid => Box::new(HybridPrefetcher::new(...)),
    PrefetchStrategy::Off => return Ok(()), // Skip prefetch setup
};
```

**Tasks:**
- [ ] Add CLI argument for prefetch strategy
- [ ] Create appropriate prefetcher based on selection
- [ ] Wire FUSE notifier to coordinator
- [ ] Start coordinator task
- [ ] Test with all three strategies

### 6.3 Add Configuration File Support

**File:** Modify `xearthlayer/src/config/file.rs`

Add `[prefetch]` section:

```ini
[prefetch]
; Strategy: radial, hybrid, off
strategy = hybrid

; Cone parameters
cone_half_angle = 30
min_lookahead_nm = 5.0
max_lookahead_nm = 15.0
lookahead_time_secs = 60

; Buffer parameters
lateral_buffer_angle = 45
lateral_buffer_depth = 3
rear_buffer_tiles = 3

; Turn detection
turn_rate_threshold = 1.0
turn_widening_factor = 1.5

; Cycle timing
cycle_interval_ms = 1000
max_tiles_per_cycle = 100
```

**Tasks:**
- [ ] Add PrefetchSettings to config structures
- [ ] Parse [prefetch] section
- [ ] Add ConfigKey entries for prefetch settings
- [ ] Update config generation
- [ ] Write tests for config parsing

---

## Phase 7: Testing (Day 4)

### 7.1 Unit Tests

Create comprehensive unit tests for each module:

| Module | Test File | Key Tests |
|--------|-----------|-----------|
| coordinates | `coordinates_test.rs` | Lat/lon conversion, projection accuracy |
| cone | `cone_test.rs` | Tile counts, coverage at various headings |
| buffer | `buffer_test.rs` | Lateral/rear coverage |
| turn | `turn_test.rs` | Turn detection, smoothing, hold timer |
| fuse_analyzer | `fuse_analyzer_test.rs` | Inference accuracy, confidence |
| hybrid | `hybrid_test.rs` | Mode selection, tile generation |

**Tasks:**
- [ ] Write unit tests for coordinates module
- [ ] Write unit tests for cone generator
- [ ] Write unit tests for buffer generator
- [ ] Write unit tests for turn detector
- [ ] Write unit tests for FUSE analyzer
- [ ] Write unit tests for hybrid prefetcher
- [ ] Achieve >80% code coverage

### 7.2 Integration Tests

**File:** `xearthlayer/tests/prefetch_integration.rs`

```rust
#[tokio::test]
async fn test_telemetry_mode_prefetch() { ... }

#[tokio::test]
async fn test_fuse_inference_mode() { ... }

#[tokio::test]
async fn test_mode_fallback_chain() { ... }

#[tokio::test]
async fn test_turn_detection_widens_cone() { ... }

#[tokio::test]
async fn test_tile_priority_ordering() { ... }
```

**Tasks:**
- [ ] Write integration test for telemetry mode
- [ ] Write integration test for FUSE inference
- [ ] Write integration test for mode transitions
- [ ] Write integration test for turn handling
- [ ] Test with mock cache and pipeline

### 7.3 Manual Flight Testing

Create a test checklist for manual verification:

```markdown
## Flight Test Checklist

### Setup
- [ ] Clear disk cache
- [ ] Set memory cache to 3 GB
- [ ] Enable prefetch logging

### Test 1: Straight Flight (Telemetry)
- [ ] Enable XGPS2 in X-Plane
- [ ] Fly straight for 50nm
- [ ] Verify: No stutter
- [ ] Verify: Cache hit rate >90%
- [ ] Verify: Logs show "Telemetry mode"

### Test 2: Turning Flight
- [ ] Perform 90° turn
- [ ] Verify: Cone widens during turn
- [ ] Verify: No stutter during turn
- [ ] Verify: Logs show turn detection

### Test 3: FUSE Inference (No Telemetry)
- [ ] Disable XGPS2 in X-Plane
- [ ] Fly straight for 25nm
- [ ] Verify: Mode falls back to FuseInference
- [ ] Verify: Reduced but acceptable stuttering

### Test 4: Radial Fallback
- [ ] Start from cold (no prior requests)
- [ ] Verify: Mode starts as Radial
- [ ] Verify: Transitions to FuseInference after requests

### Test 5: Long-haul
- [ ] Fly 2-hour flight
- [ ] Verify: Stable memory usage
- [ ] Verify: Consistent performance
```

---

## Phase 8: Documentation and Cleanup (Day 5)

### 8.1 Update User Documentation

**File:** `docs/configuration.md`

Add prefetch configuration section with:
- Available strategies
- Recommended settings by flight type
- Memory cache sizing guidance
- Troubleshooting tips

### 8.2 Update Developer Documentation

**File:** `docs/dev/prefetch-architecture.md` (new)

Document:
- Module structure
- Data flow
- Extension points
- Performance considerations

### 8.3 Update CLAUDE.md

Add prefetch module to the architecture section and module dependencies.

### 8.4 Code Cleanup

- [ ] Remove any `#[allow(dead_code)]` from new modules
- [ ] Run `cargo clippy` and fix warnings
- [ ] Run `cargo fmt`
- [ ] Update CHANGELOG.md

---

## File Summary

### New Files (13)
```
xearthlayer/src/prefetch/
├── buffer.rs
├── cone.rs
├── config.rs
├── coordinates.rs
├── coordinator.rs
├── fuse_analyzer.rs
├── hybrid.rs
├── turn.rs
└── types.rs

xearthlayer/tests/
└── prefetch_integration.rs

docs/dev/
└── prefetch-architecture.md
```

### Modified Files (6)
```
xearthlayer/src/prefetch/mod.rs
xearthlayer/src/fuse/fuse3/passthrough.rs
xearthlayer/src/config/file.rs
xearthlayer/src/config/keys.rs
xearthlayer-cli/src/commands/run.rs
docs/configuration.md
```

---

## Dependencies

No new crate dependencies required. Uses existing:
- `tokio` for async coordination
- `tokio_util::sync::CancellationToken` for shutdown
- `dashmap` for concurrent tile tracking (if needed)

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Breaking existing RadialPrefetcher | Keep as default, hybrid is opt-in |
| FUSE notification overhead | Non-blocking channel, fire-and-forget |
| Memory pressure from tile tracking | Bounded HashSet with TTL eviction |
| Turn detection jitter | Exponential smoothing + threshold |

---

## Success Criteria

- [ ] All unit tests pass
- [ ] All integration tests pass
- [ ] `make verify` passes
- [ ] Manual flight test checklist complete
- [ ] Cache hit rate >85% in telemetry mode
- [ ] Cache hit rate >70% in FUSE inference mode
- [ ] No regressions in radial mode
- [ ] Documentation updated
