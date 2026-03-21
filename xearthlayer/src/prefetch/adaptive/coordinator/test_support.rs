//! Shared test mocks and helpers for coordinator tests.
//!
//! This module consolidates mock implementations and factory functions used
//! across the coordinator test suite, eliminating duplication of mock structs
//! like `StableBoundsTracker`, `BackpressureMockClient`, etc.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::coord::TileCoord;
use crate::executor::{DdsClient, DdsClientError, Priority};
use crate::prefetch::adaptive::calibration::{PerformanceCalibration, StrategyMode};
use crate::prefetch::adaptive::strategy::PrefetchPlan;
use crate::prefetch::state::AircraftState;
use crate::prefetch::SceneryIndex;
use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};

// ─────────────────────────────────────────────────────────────────────────────
// Factory helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Create a standard test calibration with opportunistic mode.
pub(crate) fn test_calibration() -> PerformanceCalibration {
    PerformanceCalibration {
        throughput_tiles_per_sec: 25.0,
        avg_tile_generation_ms: 40,
        tile_generation_stddev_ms: 10,
        confidence: 0.9,
        recommended_strategy: StrategyMode::Opportunistic,
        calibrated_at: Instant::now(),
        baseline_throughput: 25.0,
        sample_count: 100,
    }
}

/// Create a prefetch plan with the given number of sequential tiles.
pub(crate) fn test_plan(tile_count: usize) -> PrefetchPlan {
    let tiles: Vec<TileCoord> = (0..tile_count)
        .map(|i| TileCoord {
            row: 100 + i as u32,
            col: 200,
            zoom: 14,
        })
        .collect();
    let cal = test_calibration();
    PrefetchPlan::with_tiles(tiles, &cal, "test", 0, tile_count)
}

/// Create an aircraft state at ground conditions (low speed, low AGL).
pub(crate) fn ground_state(lat: f64, lon: f64) -> AircraftState {
    const HEADING_DEG: f32 = 90.0;
    const GROUND_SPEED_KT: f32 = 10.0;
    const AGL_FT: f32 = 5.0;
    AircraftState::new(lat, lon, HEADING_DEG, GROUND_SPEED_KT, AGL_FT)
}

/// Build patched regions covering a radius around a DSF tile.
///
/// Returns a HashSet covering `(center +/- radius)` in both lat and lon,
/// ensuring all ground strategy ring tiles are captured.
pub(crate) fn patched_region_area(
    center_lat: i32,
    center_lon: i32,
    radius: i32,
) -> HashSet<(i32, i32)> {
    let mut regions = HashSet::new();
    for lat in (center_lat - radius)..=(center_lat + radius) {
        for lon in (center_lon - radius)..=(center_lon + radius) {
            regions.insert((lat, lon));
        }
    }
    regions
}

/// Helper to create a SceneryIndex with tiles at a specific chunk zoom level.
pub(crate) fn make_scenery_index(lat: i32, lon: i32, chunk_zoom: u8) -> Arc<SceneryIndex> {
    use crate::coord::{to_tile_coords, CHUNKS_PER_TILE_SIDE, CHUNK_ZOOM_OFFSET};
    use crate::prefetch::scenery_index::{SceneryIndexConfig, SceneryTile};

    let index = SceneryIndex::new(SceneryIndexConfig::default());
    let tile_zoom = chunk_zoom - CHUNK_ZOOM_OFFSET;

    for lat_step in 0..4u32 {
        for lon_step in 0..4u32 {
            let sample_lat = lat as f64 + (lat_step as f64 * 0.25) + 0.125;
            let sample_lon = lon as f64 + (lon_step as f64 * 0.25) + 0.125;
            if let Ok(coord) = to_tile_coords(sample_lat, sample_lon, tile_zoom) {
                index.add_tile(SceneryTile {
                    row: coord.row * CHUNKS_PER_TILE_SIDE,
                    col: coord.col * CHUNKS_PER_TILE_SIDE,
                    chunk_zoom,
                    lat: sample_lat as f32,
                    lon: sample_lon as f32,
                    is_sea: false,
                });
            }
        }
    }

    Arc::new(index)
}

// ─────────────────────────────────────────────────────────────────────────────
// SceneTracker mocks
// ─────────────────────────────────────────────────────────────────────────────

/// Mock SceneTracker that returns no data for all queries.
pub(crate) struct DummyTracker;

impl SceneTracker for DummyTracker {
    fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
        HashSet::new()
    }
    fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
        false
    }
    fn total_requests(&self) -> u64 {
        0
    }
    fn loaded_regions(&self) -> HashSet<GeoRegion> {
        HashSet::new()
    }
    fn is_region_loaded(&self, _region: &GeoRegion) -> bool {
        false
    }
    fn loaded_bounds(&self) -> Option<GeoBounds> {
        None
    }
}

/// Mock SceneTracker that returns configurable stable bounds.
///
/// Used to drive the SceneryWindow into Ready state for boundary tests.
pub(crate) struct StableBoundsTracker {
    bounds: GeoBounds,
}

impl StableBoundsTracker {
    /// Create a tracker with the default bounds used by most coordinator tests.
    pub(crate) fn default_bounds() -> Self {
        Self {
            bounds: GeoBounds {
                min_lat: 47.0,
                max_lat: 53.0,
                min_lon: 3.0,
                max_lon: 11.0,
            },
        }
    }

    /// Create a tracker with custom bounds.
    pub(crate) fn with_bounds(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> Self {
        Self {
            bounds: GeoBounds {
                min_lat,
                max_lat,
                min_lon,
                max_lon,
            },
        }
    }
}

impl SceneTracker for StableBoundsTracker {
    fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
        HashSet::new()
    }
    fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
        false
    }
    fn total_requests(&self) -> u64 {
        0
    }
    fn loaded_regions(&self) -> HashSet<GeoRegion> {
        HashSet::new()
    }
    fn is_region_loaded(&self, _region: &GeoRegion) -> bool {
        false
    }
    fn loaded_bounds(&self) -> Option<GeoBounds> {
        Some(self.bounds)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DdsClient mocks
// ─────────────────────────────────────────────────────────────────────────────

/// Mock DDS client with configurable executor load and submission tracking.
pub(crate) struct BackpressureMockClient {
    /// Simulated executor load (0.0 to 1.0).
    pressure: f64,
    submitted: AtomicUsize,
    /// If Some, return ChannelFull after this many successful submits.
    fail_after: Option<usize>,
}

impl BackpressureMockClient {
    pub(crate) fn new(pressure: f64) -> Self {
        Self {
            pressure,
            submitted: AtomicUsize::new(0),
            fail_after: None,
        }
    }

    pub(crate) fn with_fail_after(mut self, n: usize) -> Self {
        self.fail_after = Some(n);
        self
    }

    pub(crate) fn submitted_count(&self) -> usize {
        self.submitted.load(Ordering::Relaxed)
    }
}

impl DdsClient for BackpressureMockClient {
    fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
        let count = self.submitted.load(Ordering::Relaxed);
        if let Some(limit) = self.fail_after {
            if count >= limit {
                return Err(DdsClientError::ChannelFull);
            }
        }
        self.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn request_dds(
        &self,
        _tile: TileCoord,
        _cancellation: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn request_dds_with_options(
        &self,
        _tile: TileCoord,
        _priority: Priority,
        _origin: RequestOrigin,
        _cancellation: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn executor_load(&self) -> f64 {
        self.pressure
    }
}

/// Mock DdsClient that accepts N submissions, then returns ChannelFull.
pub(crate) struct CapLimitedDdsClient {
    limit: usize,
    submitted: AtomicUsize,
}

impl CapLimitedDdsClient {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            submitted: AtomicUsize::new(0),
        }
    }

    /// Reset the counter to allow another batch of submissions.
    pub(crate) fn reset(&self) {
        self.submitted.store(0, Ordering::SeqCst);
    }
}

impl DdsClient for CapLimitedDdsClient {
    fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
        let prev = self.submitted.fetch_add(1, Ordering::SeqCst);
        if prev >= self.limit {
            // Undo the increment since we're rejecting
            self.submitted.fetch_sub(1, Ordering::SeqCst);
            Err(DdsClientError::ChannelFull)
        } else {
            Ok(())
        }
    }

    fn request_dds(
        &self,
        _tile: TileCoord,
        _cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn request_dds_with_options(
        &self,
        _tile: TileCoord,
        _priority: Priority,
        _origin: RequestOrigin,
        _cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn is_connected(&self) -> bool {
        true
    }
}

/// Mock DdsClient that reports high executor load (above defer threshold).
pub(crate) struct HighLoadDdsClient;

impl DdsClient for HighLoadDdsClient {
    fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
        Ok(())
    }

    fn request_dds(
        &self,
        _tile: TileCoord,
        _cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn request_dds_with_options(
        &self,
        _tile: TileCoord,
        _priority: Priority,
        _origin: RequestOrigin,
        _cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<DdsResponse> {
        let (_, rx) = tokio::sync::oneshot::channel();
        rx
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn executor_load(&self) -> f64 {
        0.95 // Above BACKPRESSURE_DEFER_THRESHOLD (0.8)
    }
}
