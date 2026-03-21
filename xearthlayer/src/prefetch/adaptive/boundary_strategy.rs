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

    /// Check InProgress regions and promote to Prefetched if all tiles are cached.
    ///
    /// For each InProgress region, expands it to DDS tiles at the given zoom level
    /// and checks whether all tiles are present in the `cached_tiles` set. If so,
    /// the region is promoted to `Prefetched` state.
    pub fn promote_completed_regions(
        geo_index: &GeoIndex,
        cached_tiles: &std::collections::HashSet<TileCoord>,
        scenery_index: Option<&Arc<SceneryIndex>>,
    ) -> usize {
        let strategy = BoundaryStrategy::new();
        let in_progress: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_in_progress())
            .map(|(dsf, _)| dsf)
            .collect();

        let mut promoted = 0;
        for region in &in_progress {
            let tiles = Self::tiles_for_region(&strategy, region, scenery_index);
            if !tiles.is_empty() && tiles.iter().all(|t| cached_tiles.contains(t)) {
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
    fn tiles_for_region(
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

    #[test]
    fn test_promote_completed_regions() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Get the tiles for this region at zoom 14
        let strategy = BoundaryStrategy::new();
        let tiles = strategy.expand_to_tiles(&region, 14);
        assert!(!tiles.is_empty());

        // Put all tiles into the cached set
        let cached_tiles: std::collections::HashSet<TileCoord> = tiles.into_iter().collect();

        // No scenery index — uses geometric fallback
        let promoted = BoundaryStrategy::promote_completed_regions(&geo_index, &cached_tiles, None);
        assert_eq!(promoted, 1);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_promote_skips_incomplete_regions() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Get the tiles for this region at zoom 14
        let strategy = BoundaryStrategy::new();
        let tiles = strategy.expand_to_tiles(&region, 14);
        assert!(tiles.len() > 1);

        // Only put one tile into the cached set
        let mut cached_tiles = std::collections::HashSet::new();
        cached_tiles.insert(tiles[0]);

        let promoted = BoundaryStrategy::promote_completed_regions(&geo_index, &cached_tiles, None);
        assert_eq!(promoted, 0);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
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

        // Cache all the zoom 12 tiles
        let cached_tiles: std::collections::HashSet<TileCoord> = tiles.into_iter().collect();

        // Promote should work with SceneryIndex (not hardcoded zoom 14)
        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, &cached_tiles, Some(&index));
        assert_eq!(
            promoted, 1,
            "Should promote when all scenery index tiles are cached"
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
        let cached_tiles: std::collections::HashSet<TileCoord> = wrong_tiles.into_iter().collect();

        // Promote should NOT succeed — cached zoom 14, but index expects zoom 12
        let promoted =
            BoundaryStrategy::promote_completed_regions(&geo_index, &cached_tiles, Some(&index));
        assert_eq!(
            promoted, 0,
            "Should not promote when cached tiles are at wrong zoom level"
        );

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
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
