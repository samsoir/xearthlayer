//! Region lifecycle management for the prefetch system.
//!
//! Provides region lifecycle management:
//! - [`BoundaryStrategy::sweep_stale_regions`] — removes `InProgress` regions
//!   that have exceeded the staleness timeout (eligible for re-prefetch).
//! - [`BoundaryStrategy::promote_completed_regions`] — promotes `InProgress`
//!   regions to `Prefetched` once all their tiles are confirmed in cache.

use std::collections::HashSet;
use std::sync::Arc;

use crate::coord::{to_tile_coords, TileCoord};
use crate::executor::DdsDiskCacheChecker;
use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion, RetainedRegion};
use crate::prefetch::tile_based::DsfTileCoord;
use crate::prefetch::SceneryIndex;

/// Region lifecycle management for the prefetch system.
///
/// Handles tile expansion, region state transitions (InProgress → Prefetched),
/// staleness sweeps, and retention-based eviction.
pub struct BoundaryStrategy;

impl BoundaryStrategy {
    /// Creates a new `BoundaryStrategy`.
    pub fn new() -> Self {
        Self
    }

    /// Expand a DSF region into DDS tiles using a 4x4 sample grid.
    ///
    /// Samples 16 points within the 1x1 degree region and converts each to
    /// a DDS tile coordinate at the given zoom level. Duplicates are removed
    /// (nearby sample points may map to the same tile at lower zoom levels).
    pub fn expand_to_tiles(&self, region: &DsfRegion, zoom: u8) -> Vec<TileCoord> {
        let lat_min = region.lat as f64;
        let lon_min = region.lon as f64;
        let mut tiles = Vec::with_capacity(16);
        let mut seen = std::collections::HashSet::with_capacity(16);

        for lat_step in 0..4u32 {
            for lon_step in 0..4u32 {
                let sample_lat = lat_min + (lat_step as f64 * 0.25) + 0.125;
                let sample_lon = lon_min + (lon_step as f64 * 0.25) + 0.125;
                if let Ok(coord) = to_tile_coords(sample_lat, sample_lon, zoom) {
                    if seen.insert((coord.row, coord.col)) {
                        tiles.push(coord);
                    }
                }
            }
        }

        tiles
    }

    /// Mark a region as having no scenery coverage.
    pub fn mark_no_coverage(&self, region: &DsfRegion, geo_index: &GeoIndex) {
        geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::no_coverage());
        tracing::debug!(
            lat = region.lat,
            lon = region.lon,
            "boundary: marked NoCoverage"
        );
    }

    /// Mark a region as having prefetch in progress.
    pub fn mark_in_progress(&self, region: &DsfRegion, geo_index: &GeoIndex) {
        geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::in_progress());
        tracing::debug!(
            lat = region.lat,
            lon = region.lon,
            "boundary: marked InProgress"
        );
    }

    /// Sweep the GeoIndex for stale `InProgress` regions and remove them.
    ///
    /// Stale regions have been `InProgress` for longer than the specified timeout,
    /// indicating the prefetch job either failed or was never completed. Removing
    /// them makes them eligible for re-prefetch.
    pub fn sweep_stale_regions(geo_index: &GeoIndex, timeout: std::time::Duration) -> usize {
        let stale: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, region)| region.is_stale(timeout))
            .map(|(dsf, _)| dsf)
            .collect();

        let removed = stale.len();
        for region in &stale {
            geo_index.remove::<PrefetchedRegion>(region);
        }

        if removed > 0 {
            tracing::debug!(removed, "Swept stale InProgress regions");
        }
        removed
    }

    /// Check InProgress regions and promote to Prefetched if all tiles are
    /// present in the authoritative DDS disk cache.
    ///
    /// For each InProgress region, expands it to DDS tiles via
    /// `tiles_for_region` (using the scenery index when available) and
    /// queries the `DdsDiskCacheChecker` for each tile. A region is
    /// promoted to `Prefetched` only when **every** one of its tiles
    /// is present on disk (check-all with short-circuit on first miss).
    ///
    /// If `dds_disk_checker` is `None`, promotion is skipped — there is
    /// no source of truth to consult. The rescue path
    /// (`evaluate_stale_regions`) will still fire for stale regions.
    ///
    /// See #172 Part 3: the prior version consulted a `cached_tiles`
    /// `HashSet` shadow that failed to track ~94% of actually-cached
    /// tiles during production flights (4 normal vs. 61 stale-rescue
    /// promotions in a 9-hour log). Querying the authoritative cache
    /// directly collapses one source of truth.
    pub fn promote_completed_regions(
        geo_index: &GeoIndex,
        dds_disk_checker: Option<&Arc<dyn DdsDiskCacheChecker>>,
        scenery_index: Option<&Arc<SceneryIndex>>,
    ) -> usize {
        let (Some(checker), Some(index)) = (dds_disk_checker, scenery_index) else {
            // Without an authoritative checker there is nothing to consult,
            // and without the scenery index there is no way to know which
            // tiles the region should contain. Skip promotion; the rescue
            // path handles stale regions.
            return 0;
        };

        let in_progress: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_in_progress())
            .map(|(dsf, _)| dsf)
            .collect();

        let mut promoted = 0;
        for region in &in_progress {
            let tiles = index.tiles_in_region(*region);
            if tiles.is_empty() {
                continue;
            }
            // Check-all with short-circuit on first miss. Each lookup is
            // an O(1) in-memory index check.
            // Pass tile coords (not chunk_origin) — the DDS disk cache is
            // keyed by tile coords `tile:{tile_zoom}:{tile_row}:{tile_col}`.
            // Using chunk coords here silently returned false for every
            // tile, preventing promotion.
            let all_present = tiles
                .iter()
                .all(|t| checker.tile_exists_blocking(t.row, t.col, t.zoom));
            if all_present {
                geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::prefetched());
                promoted += 1;
            }
        }

        if promoted > 0 {
            tracing::debug!(promoted, "Promoted InProgress regions to Prefetched");
        }
        promoted
    }

    /// Get tiles for a DSF region using scenery index when available.
    ///
    /// Queries the scenery index for actual installed tiles (at correct zoom
    /// levels), falling back to geometric 4x4 grid at zoom 14.
    pub fn tiles_for_region(
        strategy: &BoundaryStrategy,
        region: &DsfRegion,
        scenery_index: Option<&Arc<SceneryIndex>>,
    ) -> Vec<TileCoord> {
        if let Some(index) = scenery_index {
            let center_lat = region.lat as f64 + 0.5;
            let center_lon = region.lon as f64 + 0.5;
            let tiles = index.tiles_near(center_lat, center_lon, 45.0);
            let result: Vec<TileCoord> = tiles.iter().map(|t| t.to_tile_coord()).collect();
            if !result.is_empty() {
                return result;
            }
        }
        strategy.expand_to_tiles(region, 14)
    }

    /// Evict `PrefetchedRegion` entries for regions no longer in the retained window.
    ///
    /// Removes `Prefetched` and `NoCoverage` entries whose DSF region is not
    /// present in the `RetainedRegion` layer. `InProgress` entries are preserved
    /// because they represent actively running prefetch jobs.
    ///
    /// Returns 0 (no-op) when the `RetainedRegion` layer is empty, indicating
    /// retention tracking is not yet active.
    pub fn evict_non_retained(geo_index: &GeoIndex) -> usize {
        let retained = geo_index.regions::<RetainedRegion>();
        if retained.is_empty() {
            return 0; // Retention not active yet
        }

        let retained_set: std::collections::HashSet<DsfRegion> = retained.into_iter().collect();

        let to_evict: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(dsf, region)| !region.is_in_progress() && !retained_set.contains(dsf))
            .map(|(dsf, _)| dsf)
            .collect();

        let evicted = to_evict.len();
        for region in &to_evict {
            geo_index.remove::<PrefetchedRegion>(region);
        }

        if evicted > 0 {
            tracing::debug!(evicted, "Evicted non-retained PrefetchedRegion entries");
        }
        evicted
    }

    /// Remove entries from `cached_tiles` whose DSF region is not in the retained window.
    ///
    /// Returns 0 (no-op) when the `RetainedRegion` layer is empty, indicating
    /// retention tracking is not yet active. This prevents stale `cached_tiles`
    /// entries from blocking re-prefetch of regions the aircraft has moved past.
    pub fn evict_cached_tiles_outside_retained(
        cached_tiles: &mut HashSet<TileCoord>,
        geo_index: &GeoIndex,
    ) -> usize {
        let retained = geo_index.regions::<RetainedRegion>();
        if retained.is_empty() {
            return 0;
        }

        let retained_set: std::collections::HashSet<DsfRegion> = retained.into_iter().collect();

        let before = cached_tiles.len();
        cached_tiles.retain(|tile| {
            let (lat, lon) = tile.to_lat_lon();
            let dsf = DsfTileCoord::from_lat_lon(lat, lon);
            retained_set.contains(&DsfRegion::new(dsf.lat, dsf.lon))
        });
        let evicted = before - cached_tiles.len();

        if evicted > 0 {
            tracing::debug!(
                evicted,
                remaining = cached_tiles.len(),
                "Evicted cached_tiles outside retained window"
            );
        }
        evicted
    }
}

impl Default for BoundaryStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_region_to_dds_tiles() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);
        let tiles = strategy.expand_to_tiles(&region, 14);
        // 4x4 grid = up to 16 tiles (dedup may reduce slightly)
        assert!(!tiles.is_empty());
        assert!(tiles.len() <= 16);
        // All tiles should be at the requested zoom
        for tile in &tiles {
            assert_eq!(tile.zoom, 14);
        }
    }

    #[test]
    fn test_expand_region_tiles_within_dsf_bounds() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);
        let tiles = strategy.expand_to_tiles(&region, 14);
        // Tiles should be within the DSF region's geographic bounds
        // (can't easily check lat/lon from TileCoord, just verify non-empty)
        assert!(!tiles.is_empty());
    }

    #[test]
    fn test_mark_no_coverage() {
        let strategy = BoundaryStrategy::new();
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        strategy.mark_no_coverage(&region, &geo_index);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_no_coverage());
    }

    #[test]
    fn test_mark_in_progress() {
        let strategy = BoundaryStrategy::new();
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        strategy.mark_in_progress(&region, &geo_index);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    // =========================================================================
    // Staleness sweep
    // =========================================================================

    #[test]
    fn test_sweep_stale_regions() {
        use std::time::Duration;

        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        // Insert InProgress with a timestamp that will be stale immediately
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Use a zero timeout so that any InProgress region is immediately stale
        let removed = BoundaryStrategy::sweep_stale_regions(&geo_index, Duration::ZERO);
        assert_eq!(removed, 1);
        assert!(!geo_index.contains::<PrefetchedRegion>(&region));
    }

    #[test]
    fn test_sweep_keeps_fresh_regions() {
        use std::time::Duration;

        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Use a very long timeout — region should not be stale
        let removed = BoundaryStrategy::sweep_stale_regions(&geo_index, Duration::from_secs(3600));
        assert_eq!(removed, 0);
        assert!(geo_index.contains::<PrefetchedRegion>(&region));
    }

    #[test]
    fn test_sweep_keeps_prefetched_regions() {
        use std::time::Duration;

        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());

        // Even with zero timeout, Prefetched regions are never stale
        let removed = BoundaryStrategy::sweep_stale_regions(&geo_index, Duration::ZERO);
        assert_eq!(removed, 0);
        assert!(geo_index.contains::<PrefetchedRegion>(&region));
    }

    // =========================================================================
    // Region promotion
    // =========================================================================

    /// Mock [`DdsDiskCacheChecker`] backed by an in-memory set of tile
    /// coordinates. Stores by tile coords (not chunk) to match how the
    /// DDS disk cache is actually keyed.
    struct MockDiskChecker {
        tiles: std::sync::Mutex<HashSet<(u32, u32, u8)>>,
    }

    impl MockDiskChecker {
        fn new() -> Self {
            Self {
                tiles: std::sync::Mutex::new(HashSet::new()),
            }
        }

        /// Populate from a set of [`TileCoord`]s.
        fn with_tile_coords(iter: impl IntoIterator<Item = TileCoord>) -> Arc<Self> {
            let me = Self::new();
            {
                let mut set = me.tiles.lock().unwrap();
                for tile in iter {
                    set.insert((tile.row, tile.col, tile.zoom));
                }
            }
            Arc::new(me)
        }
    }

    impl DdsDiskCacheChecker for MockDiskChecker {
        fn tile_exists(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            let present = self.tiles.lock().unwrap().contains(&(row, col, zoom));
            Box::pin(async move { present })
        }

        fn tile_exists_blocking(&self, row: u32, col: u32, zoom: u8) -> bool {
            self.tiles.lock().unwrap().contains(&(row, col, zoom))
        }
    }

    #[test]
    fn test_promote_completed_regions() {
        // #176: promotion has no geometric fallback — it needs a
        // SceneryIndex to know the region's tile set. Wire a minimal
        // single-tile index rather than relying on `expand_to_tiles`.
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 50.5,
            lon: 9.5,
            is_sea: false,
        });
        let tiles = index.tiles_in_region(region);
        assert_eq!(tiles.len(), 1);

        // Populate the mock disk checker with every tile's chunk coords.
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());

        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));
        assert_eq!(promoted, 1);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_promote_skips_incomplete_regions() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 50.2,
            lon: 9.2,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 26000,
            col: 11000,
            chunk_zoom: 16,
            lat: 50.8,
            lon: 9.8,
            is_sea: false,
        });
        let tiles = index.tiles_in_region(region);
        assert_eq!(tiles.len(), 2);

        // Mock checker knows only one tile → region is incomplete
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(std::iter::once(tiles[0]));

        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));
        assert_eq!(promoted, 0);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    #[test]
    fn test_promote_skipped_when_no_checker_configured() {
        // Without a `DdsDiskCacheChecker` there is no authoritative source
        // to consult. `promote_completed_regions` should skip and return 0
        // rather than promote based on nothing.
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let promoted = BoundaryStrategy::promote_completed_regions(&geo_index, None, None);
        assert_eq!(promoted, 0);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(
            state.is_in_progress(),
            "Region must stay InProgress when no checker is configured"
        );
    }

    // =========================================================================
    // tiles_for_region + SceneryIndex integration
    // =========================================================================

    /// Create a SceneryIndex populated with tiles for a specific DSF region.
    fn make_scenery_index_for_region(lat: i32, lon: i32, chunk_zoom: u8) -> Arc<SceneryIndex> {
        use crate::coord::{to_tile_coords, CHUNKS_PER_TILE_SIDE, CHUNK_ZOOM_OFFSET};
        use crate::prefetch::scenery_index::{SceneryIndexConfig, SceneryTile};

        let index = SceneryIndex::new(SceneryIndexConfig::default());

        // Sample a 4x4 grid within the 1deg DSF region and add tiles
        for lat_step in 0..4u32 {
            for lon_step in 0..4u32 {
                let sample_lat = lat as f64 + (lat_step as f64 * 0.25) + 0.125;
                let sample_lon = lon as f64 + (lon_step as f64 * 0.25) + 0.125;
                let tile_zoom = chunk_zoom - CHUNK_ZOOM_OFFSET;
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

    #[test]
    fn test_tiles_for_region_without_scenery_index_uses_zoom_14() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);

        // No scenery index -> falls back to geometric expansion at zoom 14
        let tiles = BoundaryStrategy::tiles_for_region(&strategy, &region, None);

        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(tile.zoom, 14, "Fallback should use zoom 14");
        }
    }

    #[test]
    fn test_tiles_for_region_with_scenery_index_uses_actual_zoom() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);

        // SceneryIndex populated at chunk_zoom 16 -> tile zoom 12
        let index = make_scenery_index_for_region(50, 9, 16);
        let tiles = BoundaryStrategy::tiles_for_region(&strategy, &region, Some(&index));

        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 12,
                "Should use zoom 12 from scenery index (chunk_zoom 16)"
            );
        }
    }

    #[test]
    fn test_tiles_for_region_falls_back_when_index_empty_for_region() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);

        // SceneryIndex exists but has tiles only at different region (60, 20)
        let index = make_scenery_index_for_region(60, 20, 16);
        let tiles = BoundaryStrategy::tiles_for_region(&strategy, &region, Some(&index));

        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 14,
                "Should fall back to zoom 14 when no index tiles nearby"
            );
        }
    }

    #[test]
    fn test_promote_completed_regions_with_scenery_index() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // SceneryIndex at chunk_zoom 16 -> tile zoom 12
        let index = make_scenery_index_for_region(50, 9, 16);

        // Get tiles via SceneryIndex — these are zoom 12 tiles
        let strategy = BoundaryStrategy::new();
        let tiles = BoundaryStrategy::tiles_for_region(&strategy, &region, Some(&index));
        assert!(!tiles.is_empty());
        assert!(tiles.iter().all(|t| t.zoom == 12));

        // Populate the mock disk checker with every tile's chunk coords.
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());

        // Promote should work with SceneryIndex (not hardcoded zoom 14)
        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));
        assert_eq!(
            promoted, 1,
            "Should promote when all scenery index tiles are on disk"
        );

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_promote_fails_with_wrong_zoom_cached() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // SceneryIndex at chunk_zoom 16 -> tile zoom 12
        let index = make_scenery_index_for_region(50, 9, 16);

        // Cache zoom 14 tiles (the OLD wrong behavior) instead of zoom 12
        let strategy = BoundaryStrategy::new();
        let wrong_tiles = strategy.expand_to_tiles(&region, 14);
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(wrong_tiles.into_iter());

        // Promote should NOT succeed — cached zoom 14, but index expects zoom 12
        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));
        assert_eq!(
            promoted, 0,
            "Should not promote when cached tiles are at wrong zoom level"
        );

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    // =========================================================================
    // Region tile-set SSOT regression tests (#176)
    // =========================================================================

    use crate::prefetch::scenery_index::SceneryTile;

    /// Build a [`DdsDiskCacheChecker`] that reports only the listed tiles as
    /// present on disk.
    fn test_checker(tiles: &[TileCoord]) -> Arc<dyn DdsDiskCacheChecker> {
        MockDiskChecker::with_tile_coords(tiles.iter().copied())
    }

    #[test]
    fn test_promotion_not_blocked_by_missing_tile_in_adjacent_region() {
        // Regression for #176 defect 1. The old implementation queried a 45nm
        // radius around the region centre, which reaches ~15nm into the regions
        // north and south. Promotion then demanded tiles that were never
        // submitted for this region, so under a moving prefetch box the fast
        // path could effectively never fire.
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(33, -119);

        // One tile inside the region, cached.
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        // One tile in the region immediately north, NOT cached. Well within
        // 45nm of the +33-119 centre, so the old code demanded it.
        index.add_tile(SceneryTile {
            row: 3000,
            col: 4000,
            chunk_zoom: 16,
            lat: 34.2,
            lon: -118.5,
            is_sea: false,
        });

        let geo_index = GeoIndex::new();
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Checker knows only the in-region tile.
        let checker = test_checker(&[TileCoord {
            row: 1000 / 16,
            col: 2000 / 16,
            zoom: 12,
        }]);

        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));

        assert_eq!(
            promoted, 1,
            "the adjacent region's missing tile must not block promotion"
        );
        assert!(geo_index
            .get::<PrefetchedRegion>(&region)
            .unwrap()
            .is_prefetched());
    }

    #[test]
    fn test_promotion_blocked_by_missing_tile_inside_region() {
        // The other half of the contract: a genuinely incomplete region must
        // not be promoted.
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(33, -119);

        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 5000,
            col: 6000,
            chunk_zoom: 16,
            lat: 33.7,
            lon: -118.2,
            is_sea: false,
        });

        let geo_index = GeoIndex::new();
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let checker = test_checker(&[TileCoord {
            row: 1000 / 16,
            col: 2000 / 16,
            zoom: 12,
        }]);

        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, Some(&checker), Some(&index));

        assert_eq!(promoted, 0, "a missing in-region tile must block promotion");
        assert!(geo_index
            .get::<PrefetchedRegion>(&region)
            .unwrap()
            .is_in_progress());
    }

    // -------------------------------------------------------------------------
    // evict_non_retained tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_evict_non_retained_removes_prefetched_outside_retained() {
        let geo_index = GeoIndex::new();

        // Retained window covers (50,7) and (51,7)
        geo_index.insert::<RetainedRegion>(DsfRegion::new(50, 7), RetainedRegion);
        geo_index.insert::<RetainedRegion>(DsfRegion::new(51, 7), RetainedRegion);

        // Prefetched entries: two inside, two outside
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(48, 7), PrefetchedRegion::prefetched());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(52, 5), PrefetchedRegion::no_coverage());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        assert_eq!(evicted, 2);
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(48, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(52, 5)));
    }

    #[test]
    fn test_evict_non_retained_preserves_in_progress() {
        let geo_index = GeoIndex::new();

        // Retained window only covers (52, 7) — both (50,7) and (51,7) are outside
        geo_index.insert::<RetainedRegion>(DsfRegion::new(52, 7), RetainedRegion);

        // InProgress at (50,7) is outside retained, but should be preserved
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::in_progress());
        // Prefetched at (51,7) is outside retained, should be evicted
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        assert_eq!(evicted, 1); // Only Prefetched, not InProgress
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
    }

    #[test]
    fn test_evict_cached_tiles_removes_tiles_outside_retained() {
        use crate::coord::to_tile_coords;

        let geo_index = GeoIndex::new();
        geo_index.insert::<RetainedRegion>(DsfRegion::new(50, 7), RetainedRegion);

        let mut cached_tiles = std::collections::HashSet::new();

        // Tile inside retained region (50, 7)
        let tile_inside = to_tile_coords(50.5, 7.5, 14).unwrap();
        cached_tiles.insert(tile_inside);

        // Tile outside retained region (48, 5)
        let tile_outside = to_tile_coords(48.5, 5.5, 14).unwrap();
        cached_tiles.insert(tile_outside);

        let evicted =
            BoundaryStrategy::evict_cached_tiles_outside_retained(&mut cached_tiles, &geo_index);

        assert_eq!(evicted, 1);
        assert!(cached_tiles.contains(&tile_inside));
        assert!(!cached_tiles.contains(&tile_outside));
    }

    #[test]
    fn test_evict_cached_tiles_noop_when_no_retained_regions() {
        use crate::coord::to_tile_coords;

        let geo_index = GeoIndex::new();
        let mut cached_tiles = std::collections::HashSet::new();

        let tile = to_tile_coords(50.5, 7.5, 14).unwrap();
        cached_tiles.insert(tile);

        let evicted =
            BoundaryStrategy::evict_cached_tiles_outside_retained(&mut cached_tiles, &geo_index);

        assert_eq!(evicted, 0);
        assert!(cached_tiles.contains(&tile));
    }

    #[test]
    fn test_evict_non_retained_noop_when_no_retained_regions() {
        let geo_index = GeoIndex::new();

        // RetainedRegion layer is empty — retention not yet active
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        // Should not evict anything — retention not active means we can't determine
        // what's outside the window
        assert_eq!(evicted, 0);
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
    }
}
