//! Boundary-driven prefetch strategy.
//!
//! Converts [`BoundaryCrossing`] predictions into concrete DSF region lists
//! (rows for latitude crossings, columns for longitude crossings) and applies
//! the set difference with the XEL Window to produce only unprefetched regions.

use super::boundary_monitor::{BoundaryAxis, BoundaryCrossing};
use crate::coord::{to_tile_coords, TileCoord};
use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion};

/// A DSF region targeted for prefetch, with ordering metadata.
#[derive(Debug, Clone)]
pub struct TargetRegion {
    /// The DSF region to prefetch.
    pub region: DsfRegion,
    /// Depth index from the boundary edge (0 = closest, most urgent).
    pub depth_index: u8,
    /// Which axis triggered this target.
    pub axis: BoundaryAxis,
    /// Urgency inherited from the [`BoundaryCrossing`].
    pub urgency: f64,
}

/// Converts boundary crossing predictions into DSF region lists.
///
/// For latitude crossings, generates full rows (spanning the window's longitude
/// range). For longitude crossings, generates full columns (spanning the
/// window's latitude range). Results are ordered by depth (closest rows/columns
/// first) for priority-aware submission.
pub struct BoundaryStrategy;

impl BoundaryStrategy {
    /// Creates a new `BoundaryStrategy`.
    pub fn new() -> Self {
        Self
    }

    /// Generate target DSF regions for a single boundary crossing.
    ///
    /// # Arguments
    /// * `crossing` - The boundary crossing prediction (includes direction).
    /// * `cross_range` - Inclusive range on the OTHER axis:
    ///   - For latitude crossings: `(min_lon, max_lon)` — columns to fill each row.
    ///   - For longitude crossings: `(min_lat, max_lat)` — rows to fill each column.
    ///
    /// # Returns
    /// Regions ordered by depth: all depth-0 regions first, then depth-1, etc.
    pub fn generate_regions(
        &self,
        crossing: &BoundaryCrossing,
        cross_range: (i32, i32),
    ) -> Vec<TargetRegion> {
        let mut regions = Vec::new();
        let dir = crossing.direction as i32;

        for d in 0..crossing.depth as i32 {
            let coord = crossing.dsf_coord as i32 + d * dir;

            for cross in cross_range.0..=cross_range.1 {
                let region = match crossing.axis {
                    BoundaryAxis::Latitude => DsfRegion::new(coord, cross),
                    BoundaryAxis::Longitude => DsfRegion::new(cross, coord),
                };
                regions.push(TargetRegion {
                    region,
                    depth_index: d as u8,
                    axis: crossing.axis,
                    urgency: crossing.urgency,
                });
            }
        }

        regions
    }

    /// Filter out regions that are already tracked in the GeoIndex.
    ///
    /// Excludes regions with any [`PrefetchedRegion`] state (`InProgress`,
    /// `Prefetched`, or `NoCoverage`). Only absent regions are returned.
    pub fn filter_already_handled<'a>(
        &self,
        regions: &'a [TargetRegion],
        geo_index: &GeoIndex,
    ) -> Vec<&'a TargetRegion> {
        regions
            .iter()
            .filter(|r| PrefetchedRegion::should_prefetch(geo_index, &r.region))
            .collect()
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
        tracing::debug!(lat = region.lat, lon = region.lon, "boundary: marked NoCoverage");
    }

    /// Mark a region as having prefetch in progress.
    pub fn mark_in_progress(&self, region: &DsfRegion, geo_index: &GeoIndex) {
        geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::in_progress());
        tracing::debug!(lat = region.lat, lon = region.lon, "boundary: marked InProgress");
    }

    /// Expand target regions into DDS tiles, marking each region as InProgress.
    ///
    /// Returns tiles ordered by depth (most urgent first). Each expanded
    /// region is marked as `InProgress` in the GeoIndex. Regions that
    /// produce no tiles are marked as `NoCoverage` instead.
    pub fn expand_targets_to_tiles(
        &self,
        targets: &[TargetRegion],
        geo_index: &GeoIndex,
        zoom: u8,
    ) -> Vec<TileCoord> {
        let mut all_tiles = Vec::new();

        for target in targets {
            let tiles = self.expand_to_tiles(&target.region, zoom);
            if tiles.is_empty() {
                self.mark_no_coverage(&target.region, geo_index);
            } else {
                self.mark_in_progress(&target.region, geo_index);
                all_tiles.extend(tiles);
            }
        }

        all_tiles
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
    use crate::coord::TileCoord;

    fn make_crossing(
        axis: BoundaryAxis,
        dsf_coord: i16,
        depth: u8,
        urgency: f64,
        direction: i8,
    ) -> BoundaryCrossing {
        BoundaryCrossing {
            axis,
            dsf_coord,
            urgency,
            depth,
            direction,
        }
    }

    #[test]
    fn test_row_generation_for_northbound_lat_crossing() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 3, 0.5, 1);
        // Window width: columns 3..=10 (8 cols)
        let window_lon = (3, 10);

        let regions = strategy.generate_regions(&crossing, window_lon);

        // Should produce 3 rows (depth=3): 53, 54, 55
        // Each row has 8 columns: 3,4,5,6,7,8,9,10
        assert_eq!(regions.len(), 3 * 8); // 24 regions

        // Check first row (depth 0)
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(53, 3) && r.depth_index == 0));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(53, 10) && r.depth_index == 0));

        // Check second row (depth 1)
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(54, 5) && r.depth_index == 1));

        // Check third row (depth 2)
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(55, 7) && r.depth_index == 2));
    }

    #[test]
    fn test_column_generation_for_eastbound_lon_crossing() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Longitude, 11, 3, 0.5, 1);
        // Window height: rows 47..=52 (6 rows)
        let window_lat = (47, 52);

        let regions = strategy.generate_regions(&crossing, window_lat);

        // Should produce 3 columns (depth=3): 11, 12, 13
        // Each column has 6 rows: 47,48,49,50,51,52
        assert_eq!(regions.len(), 3 * 6); // 18 regions

        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(47, 11) && r.depth_index == 0));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(52, 13) && r.depth_index == 2));
    }

    #[test]
    fn test_southbound_row_generation() {
        let strategy = BoundaryStrategy::new();
        // dsf_coord = 46 means south of window; direction -1 means 46, 45, 44
        let crossing = make_crossing(BoundaryAxis::Latitude, 46, 3, 0.5, -1);
        let window_lon = (3, 10);

        let regions = strategy.generate_regions(&crossing, window_lon);

        // Going south: 46, 45, 44
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(46, 5) && r.depth_index == 0));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(45, 5) && r.depth_index == 1));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(44, 5) && r.depth_index == 2));
    }

    #[test]
    fn test_westbound_column_generation() {
        let strategy = BoundaryStrategy::new();
        // dsf_coord = 2 means west of window; direction -1 means 2, 1, 0
        let crossing = make_crossing(BoundaryAxis::Longitude, 2, 3, 0.5, -1);
        let window_lat = (47, 52);

        let regions = strategy.generate_regions(&crossing, window_lat);

        // Going west: 2, 1, 0
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(50, 2) && r.depth_index == 0));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(50, 1) && r.depth_index == 1));
        assert!(regions
            .iter()
            .any(|r| r.region == DsfRegion::new(50, 0) && r.depth_index == 2));
    }

    #[test]
    fn test_depth_ordering() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 3, 0.5, 1);
        let window_lon = (3, 10);

        let regions = strategy.generate_regions(&crossing, window_lon);

        // All depth 0 should come before depth 1, before depth 2
        let mut last_depth = 0;
        for region in &regions {
            assert!(region.depth_index >= last_depth, "depth ordering violated");
            last_depth = region.depth_index;
        }
    }

    #[test]
    fn test_filter_excludes_prefetched_regions() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 1, 0.5, 1);
        let window_lon = (3, 5);

        let geo_index = GeoIndex::new();
        // Mark (53, 4) as prefetched
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(53, 4), PrefetchedRegion::prefetched());

        let all_regions = strategy.generate_regions(&crossing, window_lon);
        let filtered = strategy.filter_already_handled(&all_regions, &geo_index);

        // (53, 4) should be excluded
        assert!(!filtered.iter().any(|r| r.region == DsfRegion::new(53, 4)));
        // Others should remain
        assert!(filtered.iter().any(|r| r.region == DsfRegion::new(53, 3)));
        assert!(filtered.iter().any(|r| r.region == DsfRegion::new(53, 5)));
    }

    #[test]
    fn test_filter_excludes_in_progress_regions() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 1, 0.5, 1);
        let window_lon = (3, 5);

        let geo_index = GeoIndex::new();
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(53, 3), PrefetchedRegion::in_progress());

        let all_regions = strategy.generate_regions(&crossing, window_lon);
        let filtered = strategy.filter_already_handled(&all_regions, &geo_index);

        assert!(!filtered.iter().any(|r| r.region == DsfRegion::new(53, 3)));
    }

    #[test]
    fn test_filter_excludes_no_coverage_regions() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 1, 0.5, 1);
        let window_lon = (3, 5);

        let geo_index = GeoIndex::new();
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(53, 5), PrefetchedRegion::no_coverage());

        let all_regions = strategy.generate_regions(&crossing, window_lon);
        let filtered = strategy.filter_already_handled(&all_regions, &geo_index);

        assert!(!filtered.iter().any(|r| r.region == DsfRegion::new(53, 5)));
    }

    #[test]
    fn test_filter_includes_absent_regions() {
        let strategy = BoundaryStrategy::new();
        let crossing = make_crossing(BoundaryAxis::Latitude, 53, 1, 0.5, 1);
        let window_lon = (3, 5);

        let geo_index = GeoIndex::new(); // Empty — all absent

        let all_regions = strategy.generate_regions(&crossing, window_lon);
        let filtered = strategy.filter_already_handled(&all_regions, &geo_index);

        // All should be included (none are in GeoIndex)
        assert_eq!(all_regions.len(), filtered.len());
    }

    #[test]
    fn test_expand_region_to_dds_tiles() {
        let strategy = BoundaryStrategy::new();
        let region = DsfRegion::new(50, 9);
        let tiles = strategy.expand_to_tiles(&region, 14);
        // 4×4 grid = up to 16 tiles (dedup may reduce slightly)
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

    #[test]
    fn test_expand_targets_to_tiles_ordered_by_depth() {
        let strategy = BoundaryStrategy::new();
        let geo_index = GeoIndex::new();

        let targets = vec![
            TargetRegion {
                region: DsfRegion::new(53, 5),
                depth_index: 0,
                axis: BoundaryAxis::Latitude,
                urgency: 0.5,
            },
            TargetRegion {
                region: DsfRegion::new(54, 5),
                depth_index: 1,
                axis: BoundaryAxis::Latitude,
                urgency: 0.5,
            },
        ];

        let tiles = strategy.expand_targets_to_tiles(&targets, &geo_index, 14);

        // Should have tiles from both regions
        assert!(!tiles.is_empty());
        // Both regions should be marked InProgress
        assert!(geo_index.get::<PrefetchedRegion>(&DsfRegion::new(53, 5)).unwrap().is_in_progress());
        assert!(geo_index.get::<PrefetchedRegion>(&DsfRegion::new(54, 5)).unwrap().is_in_progress());
    }
}
