//! Ground operations prefetch strategy.
//!
//! Implements ring-based prefetching for ground operations (taxi, parking),
//! loading scenery tiles in a 1° ring around the perimeter of X-Plane's
//! already-loaded area.
//!
//! # X-Plane's Loaded Area
//!
//! When X-Plane spawns, it loads a rectangular area of scenery around the
//! aircraft. At ~44°N latitude, this is approximately 4° wide × 3° tall
//! (landscape orientation). The exact dimensions vary by latitude.
//!
//! # Ring Calculation
//!
//! Rather than hardcoding dimensions, this strategy calculates the ring
//! based on the bounding box of tiles that have actually been loaded
//! (passed via the `loaded_bounds` or derived from cached tiles).
//!
//! ```text
//! X-Plane loaded area:        1° ring around perimeter:
//! ┌───┬───┬───┬───┐              N N N N N N
//! │   │   │   │   │           W              E
//! ├───┼───┼───┼───┤           W              E
//! │   │ ✈ │   │   │           W              E
//! ├───┼───┼───┼───┤           W              E
//! │   │   │   │   │              S S S S S S
//! └───┴───┴───┴───┘
//!     (4° × 3°)                    (18 tiles)
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use crate::coord::TileCoord;
use crate::prefetch::SceneryIndex;

use super::calibration::PerformanceCalibration;
use super::config::AdaptivePrefetchConfig;
use super::phase_detector::FlightPhase;
use super::strategy::{AdaptivePrefetchStrategy, PrefetchPlan, PrefetchPlanMetadata};
use crate::prefetch::tile_based::DsfTileCoord;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Width of the prefetch ring in degrees (1° = 1 DSF tile).
const RING_WIDTH_DEG: i32 = 1;

/// Default loaded area latitude extent (degrees) when bounds not provided.
/// This is an estimate; actual bounds should be derived from loaded tiles.
const DEFAULT_LOADED_LAT_EXTENT: i32 = 3;

/// Default loaded area longitude extent (degrees) when bounds not provided.
/// This is an estimate; actual bounds should be derived from loaded tiles.
const DEFAULT_LOADED_LON_EXTENT: i32 = 4;

// ─────────────────────────────────────────────────────────────────────────────
// LoadedAreaBounds
// ─────────────────────────────────────────────────────────────────────────────

/// Bounding box of X-Plane's loaded scenery area.
///
/// Represents the rectangular area of DSF tiles that X-Plane has loaded.
/// The ring is calculated around this bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedAreaBounds {
    /// Minimum (southernmost) DSF latitude.
    pub lat_min: i32,
    /// Maximum (northernmost) DSF latitude.
    pub lat_max: i32,
    /// Minimum (westernmost) DSF longitude.
    pub lon_min: i32,
    /// Maximum (easternmost) DSF longitude.
    pub lon_max: i32,
}

impl LoadedAreaBounds {
    /// Create bounds from explicit values.
    pub fn new(lat_min: i32, lat_max: i32, lon_min: i32, lon_max: i32) -> Self {
        Self {
            lat_min,
            lat_max,
            lon_min,
            lon_max,
        }
    }

    /// Create default bounds centered on a position.
    ///
    /// Uses default extent estimates. For accurate bounds, derive from
    /// actual loaded tiles using `from_dsf_tiles()`.
    pub fn default_centered_on(lat: f64, lon: f64) -> Self {
        let center_lat = lat.floor() as i32;
        let center_lon = lon.floor() as i32;

        // For extent N, we need exactly N tiles: max - min = N - 1
        // To center: bias toward positive for even extents
        let half_lat = (DEFAULT_LOADED_LAT_EXTENT - 1) / 2;
        let half_lon = (DEFAULT_LOADED_LON_EXTENT - 1) / 2;

        let lat_min = center_lat - half_lat;
        let lon_min = center_lon - half_lon;

        Self {
            lat_min,
            lat_max: lat_min + DEFAULT_LOADED_LAT_EXTENT - 1,
            lon_min,
            lon_max: lon_min + DEFAULT_LOADED_LON_EXTENT - 1,
        }
    }

    /// Derive bounds from a set of loaded DSF tiles.
    ///
    /// This is the preferred method as it uses the actual loaded area.
    pub fn from_dsf_tiles(tiles: &[DsfTileCoord]) -> Option<Self> {
        if tiles.is_empty() {
            return None;
        }

        let mut lat_min = i32::MAX;
        let mut lat_max = i32::MIN;
        let mut lon_min = i32::MAX;
        let mut lon_max = i32::MIN;

        for tile in tiles {
            lat_min = lat_min.min(tile.lat);
            lat_max = lat_max.max(tile.lat);
            lon_min = lon_min.min(tile.lon);
            lon_max = lon_max.max(tile.lon);
        }

        Some(Self {
            lat_min,
            lat_max,
            lon_min,
            lon_max,
        })
    }

    /// Derive bounds from cached DDS tiles by converting to DSF coordinates.
    pub fn from_dds_tiles(tiles: &HashSet<TileCoord>) -> Option<Self> {
        if tiles.is_empty() {
            return None;
        }

        let dsf_tiles: Vec<DsfTileCoord> = tiles
            .iter()
            .map(|t| {
                let (lat, lon) = t.to_lat_lon();
                DsfTileCoord::from_lat_lon(lat, lon)
            })
            .collect();

        Self::from_dsf_tiles(&dsf_tiles)
    }

    /// Get the width in degrees (longitude extent).
    pub fn width(&self) -> i32 {
        self.lon_max - self.lon_min + 1
    }

    /// Get the height in degrees (latitude extent).
    pub fn height(&self) -> i32 {
        self.lat_max - self.lat_min + 1
    }

    /// Check if a DSF tile is inside the bounds.
    pub fn contains(&self, tile: &DsfTileCoord) -> bool {
        tile.lat >= self.lat_min
            && tile.lat <= self.lat_max
            && tile.lon >= self.lon_min
            && tile.lon <= self.lon_max
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GroundStrategy
// ─────────────────────────────────────────────────────────────────────────────

/// Ground operations prefetch strategy.
///
/// Calculates a 1° ring of DSF tiles around the perimeter of X-Plane's
/// loaded scenery area. Tiles within the loaded area are excluded.
pub struct GroundStrategy {
    /// Scenery index for looking up DDS tiles within DSF tiles.
    scenery_index: Option<Arc<SceneryIndex>>,

    /// Maximum tiles per prefetch cycle.
    max_tiles: u32,

    /// Explicit loaded area bounds (if known).
    /// When `None`, bounds are derived from cached tiles or defaults.
    loaded_bounds: Option<LoadedAreaBounds>,
}

impl std::fmt::Debug for GroundStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroundStrategy")
            .field("has_scenery_index", &self.scenery_index.is_some())
            .field("max_tiles", &self.max_tiles)
            .field("loaded_bounds", &self.loaded_bounds)
            .finish()
    }
}

impl GroundStrategy {
    /// Create a new ground strategy with the given configuration.
    pub fn new(config: &AdaptivePrefetchConfig) -> Self {
        Self {
            scenery_index: None,
            max_tiles: config.max_tiles_per_cycle,
            loaded_bounds: None,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(&AdaptivePrefetchConfig::default())
    }

    /// Set the scenery index for accurate tile lookup.
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.scenery_index = Some(index);
        self
    }

    /// Set explicit loaded area bounds.
    ///
    /// Use this when you know the exact bounds of X-Plane's loaded area.
    pub fn with_loaded_bounds(mut self, bounds: LoadedAreaBounds) -> Self {
        self.loaded_bounds = Some(bounds);
        self
    }

    /// Calculate the 1° ring of DSF tiles around the loaded area.
    ///
    /// Returns (tiles, bounds, bounds_source) where:
    /// - tiles: DSF tiles in the ring, ordered by distance from center
    /// - bounds: The loaded area bounds used for calculation
    /// - bounds_source: How bounds were determined ("explicit", "default")
    fn calculate_ring(
        &self,
        position: (f64, f64),
        _cached: &HashSet<TileCoord>,
    ) -> (Vec<DsfTileCoord>, LoadedAreaBounds, &'static str) {
        let (lat, lon) = position;

        // Determine loaded area bounds
        // NOTE: We do NOT derive bounds from cached tiles because that set includes
        // prefetched tiles, which would cause the bounds to grow exponentially!
        // Until we have proper X-Plane loaded area tracking (via DdsAccessEvent),
        // we use explicit bounds or position-centered defaults.
        let (bounds, bounds_source) = if let Some(explicit) = self.loaded_bounds {
            (explicit, "explicit")
        } else {
            (LoadedAreaBounds::default_centered_on(lat, lon), "default")
        };

        let mut ring_tiles = Vec::new();

        // North edge: tiles just above the loaded area
        let north_lat = bounds.lat_max + RING_WIDTH_DEG;
        for tile_lon in (bounds.lon_min - RING_WIDTH_DEG)..=(bounds.lon_max + RING_WIDTH_DEG) {
            ring_tiles.push(DsfTileCoord::new(north_lat, tile_lon));
        }

        // South edge: tiles just below the loaded area
        let south_lat = bounds.lat_min - RING_WIDTH_DEG;
        for tile_lon in (bounds.lon_min - RING_WIDTH_DEG)..=(bounds.lon_max + RING_WIDTH_DEG) {
            ring_tiles.push(DsfTileCoord::new(south_lat, tile_lon));
        }

        // East edge: tiles just right of the loaded area (excluding corners)
        let east_lon = bounds.lon_max + RING_WIDTH_DEG;
        for tile_lat in bounds.lat_min..=bounds.lat_max {
            ring_tiles.push(DsfTileCoord::new(tile_lat, east_lon));
        }

        // West edge: tiles just left of the loaded area (excluding corners)
        let west_lon = bounds.lon_min - RING_WIDTH_DEG;
        for tile_lat in bounds.lat_min..=bounds.lat_max {
            ring_tiles.push(DsfTileCoord::new(tile_lat, west_lon));
        }

        // Remove duplicates (corners are included in both N/S edges)
        ring_tiles.sort_by_key(|t| (t.lat, t.lon));
        ring_tiles.dedup();

        // Sort by distance from aircraft
        ring_tiles.sort_by(|a, b| {
            let (ca_lat, ca_lon) = a.center();
            let (cb_lat, cb_lon) = b.center();
            let dist_a = (ca_lat - lat).powi(2) + (ca_lon - lon).powi(2);
            let dist_b = (cb_lat - lat).powi(2) + (cb_lon - lon).powi(2);
            dist_a
                .partial_cmp(&dist_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        (ring_tiles, bounds, bounds_source)
    }

    /// Get DDS tiles for a DSF tile from the scenery index or fallback generation.
    ///
    /// If a scenery index is available, queries it for tiles in the DSF.
    /// Otherwise, generates a grid of tiles covering the DSF tile at zoom 14.
    fn get_dds_tiles_in_dsf(&self, dsf: &DsfTileCoord) -> Vec<TileCoord> {
        if let Some(ref index) = self.scenery_index {
            let (center_lat, center_lon) = dsf.center();
            // 1° DSF tile ≈ 60nm at equator, use 45nm radius to cover the tile
            let tiles = index.tiles_near(center_lat, center_lon, 45.0);
            let result: Vec<TileCoord> = tiles.iter().map(|t| t.to_tile_coord()).collect();

            if !result.is_empty() {
                return result;
            }
            // Fall through to generate tiles if scenery index had no tiles
        }

        // Fallback: Generate tiles covering the DSF tile at zoom 14
        // This allows prefetching without pre-installed scenery packages
        self.generate_tiles_for_dsf(dsf)
    }

    /// Generate DDS tile coordinates covering a DSF tile.
    ///
    /// Used as fallback when no scenery index is available or when
    /// the scenery index has no tiles for this DSF.
    fn generate_tiles_for_dsf(&self, dsf: &DsfTileCoord) -> Vec<TileCoord> {
        use crate::coord::to_tile_coords;

        const ZOOM: u8 = 14;

        // DSF tiles are 1° × 1°. Generate tiles at grid points.
        let lat_min = dsf.lat as f64;
        let lon_min = dsf.lon as f64;

        let mut tiles = Vec::new();

        // Sample a 4x4 grid within the DSF tile
        for lat_step in 0..4 {
            for lon_step in 0..4 {
                let lat = lat_min + (lat_step as f64 + 0.5) * 0.25;
                let lon = lon_min + (lon_step as f64 + 0.5) * 0.25;

                if let Ok(coord) = to_tile_coords(lat, lon, ZOOM) {
                    tiles.push(coord);
                }
            }
        }

        // Remove duplicates (nearby points may map to same tile)
        tiles.sort_by_key(|t| (t.row, t.col));
        tiles.dedup();

        tiles
    }

    /// Check if a scenery index is available.
    pub fn has_scenery_index(&self) -> bool {
        self.scenery_index.is_some()
    }
}

impl AdaptivePrefetchStrategy for GroundStrategy {
    fn calculate_prefetch(
        &self,
        position: (f64, f64),
        _track: f64, // Ignored for ground operations
        calibration: &PerformanceCalibration,
        already_cached: &HashSet<TileCoord>,
    ) -> PrefetchPlan {
        // Get DSF tiles in the ring along with bounds metadata
        let (dsf_tiles, bounds, bounds_source) = self.calculate_ring(position, already_cached);
        let dsf_tile_count = dsf_tiles.len();

        if dsf_tiles.is_empty() {
            return PrefetchPlan::empty(self.name());
        }

        // Collect all DDS tiles from the DSF tiles
        let mut all_tiles: Vec<TileCoord> = Vec::new();
        for dsf in &dsf_tiles {
            let dds_tiles = self.get_dds_tiles_in_dsf(dsf);
            all_tiles.extend(dds_tiles);
        }

        let total_considered = all_tiles.len();

        // Remove duplicates
        all_tiles.sort_by_key(|t| (t.row, t.col, t.zoom));
        all_tiles.dedup();

        // Filter out already-cached tiles
        let tiles_before_filter = all_tiles.len();
        all_tiles.retain(|t| !already_cached.contains(t));
        let skipped_cached = tiles_before_filter - all_tiles.len();

        // Sort by distance from aircraft position
        let (lat, lon) = position;
        crate::coord::sort_tiles_by_distance(&mut all_tiles, lat, lon);

        // Limit to max tiles
        if all_tiles.len() > self.max_tiles as usize {
            all_tiles.truncate(self.max_tiles as usize);
        }

        // Create metadata for coordinator logging/UI
        let metadata = PrefetchPlanMetadata::ground(
            bounds_source,
            dsf_tile_count,
            (
                bounds.lat_min,
                bounds.lat_max,
                bounds.lon_min,
                bounds.lon_max,
            ),
        );

        PrefetchPlan::with_tiles_and_metadata(
            all_tiles,
            calibration,
            self.name(),
            skipped_cached,
            total_considered,
            metadata,
        )
    }

    fn name(&self) -> &'static str {
        "ground"
    }

    fn is_applicable(&self, phase: &FlightPhase) -> bool {
        *phase == FlightPhase::Ground
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn test_calibration() -> PerformanceCalibration {
        PerformanceCalibration {
            throughput_tiles_per_sec: 20.0,
            avg_tile_generation_ms: 50,
            tile_generation_stddev_ms: 10,
            confidence: 0.9,
            recommended_strategy: super::super::calibration::StrategyMode::Opportunistic,
            calibrated_at: Instant::now(),
            baseline_throughput: 20.0,
            sample_count: 100,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // LoadedAreaBounds tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_bounds_default_centered() {
        let bounds = LoadedAreaBounds::default_centered_on(53.5, 9.5);

        // Default is 3 lat × 4 lon, centered on (53, 9)
        assert_eq!(bounds.height(), DEFAULT_LOADED_LAT_EXTENT);
        assert_eq!(bounds.width(), DEFAULT_LOADED_LON_EXTENT);

        // Should be roughly centered
        let center_lat = (bounds.lat_min + bounds.lat_max) / 2;
        let center_lon = (bounds.lon_min + bounds.lon_max) / 2;
        assert_eq!(center_lat, 53);
        assert_eq!(center_lon, 9);
    }

    #[test]
    fn test_bounds_from_dsf_tiles() {
        let tiles = vec![
            DsfTileCoord::new(52, 8),
            DsfTileCoord::new(52, 9),
            DsfTileCoord::new(52, 10),
            DsfTileCoord::new(53, 8),
            DsfTileCoord::new(53, 9),
            DsfTileCoord::new(53, 10),
            DsfTileCoord::new(54, 8),
            DsfTileCoord::new(54, 9),
            DsfTileCoord::new(54, 10),
        ];

        let bounds = LoadedAreaBounds::from_dsf_tiles(&tiles).unwrap();

        assert_eq!(bounds.lat_min, 52);
        assert_eq!(bounds.lat_max, 54);
        assert_eq!(bounds.lon_min, 8);
        assert_eq!(bounds.lon_max, 10);
        assert_eq!(bounds.height(), 3);
        assert_eq!(bounds.width(), 3);
    }

    #[test]
    fn test_bounds_contains() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);

        assert!(bounds.contains(&DsfTileCoord::new(53, 9)));
        assert!(bounds.contains(&DsfTileCoord::new(52, 8))); // Corner
        assert!(bounds.contains(&DsfTileCoord::new(54, 11))); // Corner

        assert!(!bounds.contains(&DsfTileCoord::new(51, 9))); // South
        assert!(!bounds.contains(&DsfTileCoord::new(55, 9))); // North
        assert!(!bounds.contains(&DsfTileCoord::new(53, 7))); // West
        assert!(!bounds.contains(&DsfTileCoord::new(53, 12))); // East
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Ring calculation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_ring_tile_count_3x4_area() {
        // For a 3×4 loaded area, the ring should have:
        // - North edge: 4+2 = 6 tiles (width + 2 corners)
        // - South edge: 4+2 = 6 tiles (width + 2 corners)
        // - East edge: 3 tiles (height, corners excluded)
        // - West edge: 3 tiles (height, corners excluded)
        // Total: 6 + 6 + 3 + 3 = 18 tiles

        let bounds = LoadedAreaBounds::new(52, 54, 8, 11); // 3 lat × 4 lon
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let (ring, returned_bounds, source) = strategy.calculate_ring((53.5, 9.5), &HashSet::new());

        assert_eq!(
            ring.len(),
            18,
            "Ring should have exactly 18 tiles for 3×4 area"
        );
        assert_eq!(returned_bounds, bounds);
        assert_eq!(source, "explicit");
    }

    #[test]
    fn test_ring_excludes_loaded_area() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let (ring, _, _) = strategy.calculate_ring((53.5, 9.5), &HashSet::new());

        // No tile in the ring should be inside the loaded area
        for tile in &ring {
            assert!(
                !bounds.contains(tile),
                "Ring tile {:?} should not be inside loaded area",
                tile
            );
        }
    }

    #[test]
    fn test_ring_surrounds_loaded_area() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let (ring, _, _) = strategy.calculate_ring((53.5, 9.5), &HashSet::new());

        // Should have tiles in all four directions
        let has_north = ring.iter().any(|t| t.lat > bounds.lat_max);
        let has_south = ring.iter().any(|t| t.lat < bounds.lat_min);
        let has_east = ring.iter().any(|t| t.lon > bounds.lon_max);
        let has_west = ring.iter().any(|t| t.lon < bounds.lon_min);

        assert!(has_north, "Ring should have northern tiles");
        assert!(has_south, "Ring should have southern tiles");
        assert!(has_east, "Ring should have eastern tiles");
        assert!(has_west, "Ring should have western tiles");
    }

    #[test]
    fn test_ring_sorted_by_distance() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let (ring, _, _) = strategy.calculate_ring((53.5, 9.5), &HashSet::new());

        let mut prev_dist = 0.0;
        for tile in &ring {
            let (clat, clon) = tile.center();
            let dist = (clat - 53.5_f64).powi(2) + (clon - 9.5_f64).powi(2);
            assert!(
                dist >= prev_dist - 0.001,
                "Ring tiles should be sorted by distance"
            );
            prev_dist = dist;
        }
    }

    #[test]
    fn test_ring_bounds_source_default() {
        let strategy = GroundStrategy::with_defaults();

        // With no explicit bounds and no cached tiles, should use default
        let (_, _, source) = strategy.calculate_ring((53.5, 9.5), &HashSet::new());
        assert_eq!(source, "default");
    }

    /// Regression test: Bounds must NOT expand based on cached tiles.
    ///
    /// Bug: cached_tiles includes prefetched tiles, which caused the "loaded area"
    /// bounds to expand to cover the entire globe (lat -85 to 84, lon spanning 160°).
    /// This resulted in 670 DSF tiles instead of 18, and 56,000+ tiles being considered.
    ///
    /// Fix: Bounds are always position-centered defaults, never derived from cached tiles.
    #[test]
    fn test_ring_bounds_ignore_cached_tiles_regression() {
        use crate::coord::to_tile_coords;

        let strategy = GroundStrategy::with_defaults();
        let position = (51.47, -0.46); // EGLL

        // Simulate cached tiles spanning a HUGE area (what the bug produced)
        let mut cached_tiles_global = HashSet::new();

        // Add tiles from vastly different locations (simulating prefetch gone wrong)
        // Melbourne, Australia
        if let Ok(tile) = to_tile_coords(-37.67, 144.85, 14) {
            cached_tiles_global.insert(tile);
        }
        // Tokyo, Japan
        if let Ok(tile) = to_tile_coords(35.68, 139.69, 14) {
            cached_tiles_global.insert(tile);
        }
        // New York, USA
        if let Ok(tile) = to_tile_coords(40.71, -74.01, 14) {
            cached_tiles_global.insert(tile);
        }
        // London (near position)
        if let Ok(tile) = to_tile_coords(51.5, -0.1, 14) {
            cached_tiles_global.insert(tile);
        }

        // Calculate ring with globally-distributed cached tiles
        let (ring_with_global_cache, bounds_global, source_global) =
            strategy.calculate_ring(position, &cached_tiles_global);

        // Calculate ring with empty cache
        let (ring_empty_cache, bounds_empty, source_empty) =
            strategy.calculate_ring(position, &HashSet::new());

        // CRITICAL: Both should produce the same bounds (position-centered defaults)
        assert_eq!(
            bounds_global, bounds_empty,
            "Bounds must NOT be affected by cached tiles! \
             Got {:?} with global cache vs {:?} with empty cache",
            bounds_global, bounds_empty
        );

        // Both should use "default" source (not "cached_tiles")
        assert_eq!(source_global, "default");
        assert_eq!(source_empty, "default");

        // Ring size should be the same (~18 tiles for default 3×4 area)
        assert_eq!(
            ring_with_global_cache.len(),
            ring_empty_cache.len(),
            "Ring size must not change based on cached tiles"
        );

        // Sanity check: ring should be small (not 670 DSF tiles!)
        assert!(
            ring_with_global_cache.len() <= 30,
            "Ring has {} tiles, expected ~18 for default bounds",
            ring_with_global_cache.len()
        );

        // Bounds should be centered on position, not spanning the globe
        assert!(
            bounds_global.lat_max - bounds_global.lat_min < 10,
            "Lat extent {} is too large (should be ~3°)",
            bounds_global.lat_max - bounds_global.lat_min
        );
        assert!(
            bounds_global.lon_max - bounds_global.lon_min < 10,
            "Lon extent {} is too large (should be ~4°)",
            bounds_global.lon_max - bounds_global.lon_min
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Strategy tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_ground_strategy_creation() {
        let strategy = GroundStrategy::with_defaults();
        assert_eq!(strategy.name(), "ground");
        assert!(strategy.is_applicable(&FlightPhase::Ground));
        assert!(!strategy.is_applicable(&FlightPhase::Cruise));
    }

    #[test]
    fn test_ground_strategy_ignores_track() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let cached = HashSet::new();

        // Different tracks should produce identical DSF ring (track is ignored)
        let (ring1, _, _) = strategy.calculate_ring((53.5, 9.5), &cached);
        let (ring2, _, _) = strategy.calculate_ring((53.5, 9.5), &cached);

        assert_eq!(ring1.len(), ring2.len());
        for (t1, t2) in ring1.iter().zip(ring2.iter()) {
            assert_eq!(t1, t2);
        }
    }

    #[test]
    fn test_ground_strategy_without_scenery_index_generates_fallback() {
        let bounds = LoadedAreaBounds::new(52, 54, 8, 11);
        let strategy = GroundStrategy::with_defaults().with_loaded_bounds(bounds);

        let calibration = test_calibration();
        let cached = HashSet::new();

        // Without scenery index, should generate fallback tiles
        let plan = strategy.calculate_prefetch((53.5, 9.5), 0.0, &calibration, &cached);

        // Fallback tile generation should produce tiles
        assert!(!plan.is_empty(), "Should generate fallback tiles");
        assert!(plan.total_considered > 0);
    }

    #[test]
    fn test_constants_are_reasonable() {
        // Verify constants match expected values
        assert_eq!(RING_WIDTH_DEG, 1, "Ring width should be 1°");
        assert_eq!(
            DEFAULT_LOADED_LAT_EXTENT, 3,
            "Default lat extent should be 3°"
        );
        assert_eq!(
            DEFAULT_LOADED_LON_EXTENT, 4,
            "Default lon extent should be 4°"
        );
    }
}
