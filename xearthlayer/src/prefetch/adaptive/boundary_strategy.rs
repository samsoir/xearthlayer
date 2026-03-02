//! Boundary-driven prefetch strategy.
//!
//! Converts [`BoundaryCrossing`] predictions into concrete DSF region lists
//! (rows for latitude crossings, columns for longitude crossings) and applies
//! the set difference with the XEL Window to produce only unprefetched regions.

use super::boundary_monitor::{BoundaryAxis, BoundaryCrossing};
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
}

impl Default for BoundaryStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
