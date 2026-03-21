//! Sliding prefetch box for cruise-phase tile prefetching.
//!
//! Computes a heading-biased rectangle around the aircraft position,
//! enumerates the DSF regions (1°×1°) it covers, and filters out
//! regions already tracked in the GeoIndex.
//!
//! # Proportional Bias Model
//!
//! The box extent is constant (default 9° per axis). The bias within
//! that extent slides proportionally with the track heading:
//!
//! ```text
//! forward_fraction = 0.5 + (max_bias - 0.5) × |component|
//! ```
//!
//! At cardinal headings (000°, 090°, etc.) the primary axis gets full
//! bias (80/20 at max_bias=0.8) while the perpendicular axis is
//! symmetric (50/50). At diagonals (045°) both axes share equal bias.

use tracing::debug;

use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion, RetainedRegion};

/// A heading-aware prefetch region around the aircraft.
///
/// The box has a fixed total extent per axis but biases the distribution
/// forward in the direction of travel. The bias slides smoothly with
/// heading — no binary thresholds or abrupt switching.
///
/// X-Plane loads a ~6×6 DSF area around the aircraft. The default 9°
/// extent covers this with 1.5° overlap on all sides, ensuring tiles
/// are prefetched before X-Plane crosses into the next DSF region.
#[derive(Debug, Clone)]
pub struct PrefetchBox {
    /// Total extent per axis in degrees.
    extent: f64,
    /// Maximum forward bias fraction (0.5 = symmetric, 0.8 = 80/20).
    max_bias: f64,
}

impl PrefetchBox {
    /// Create a new prefetch box.
    ///
    /// - `extent`: total degrees per axis (default 9.0, min 7.0)
    /// - `max_bias`: maximum forward fraction (default 0.8, range 0.5-0.9)
    pub fn new(extent: f64, max_bias: f64) -> Self {
        Self { extent, max_bias }
    }

    /// Compute all DSF regions within the heading-biased box.
    pub fn regions(&self, lat: f64, lon: f64, track: f64) -> Vec<DsfRegion> {
        let (lat_min, lat_max, lon_min, lon_max) = self.bounds(lat, lon, track);

        let dsf_lat_min = lat_min.floor() as i32;
        let dsf_lat_max = (lat_max - 1e-9).floor() as i32;
        let dsf_lon_min = lon_min.floor() as i32;
        let dsf_lon_max = (lon_max - 1e-9).floor() as i32;

        let capacity =
            ((dsf_lat_max - dsf_lat_min + 1) * (dsf_lon_max - dsf_lon_min + 1)).max(0) as usize;
        let mut result = Vec::with_capacity(capacity);

        for lat_i in dsf_lat_min..=dsf_lat_max {
            for lon_i in dsf_lon_min..=dsf_lon_max {
                result.push(DsfRegion::new(lat_i, lon_i));
            }
        }

        result
    }

    /// Compute DSF regions in the box that are NOT already tracked in GeoIndex.
    ///
    /// Filters out regions with any `PrefetchedRegion` state (InProgress,
    /// Prefetched, or NoCoverage).
    pub fn new_regions(
        &self,
        lat: f64,
        lon: f64,
        track: f64,
        geo_index: &GeoIndex,
    ) -> Vec<DsfRegion> {
        self.regions(lat, lon, track)
            .into_iter()
            .filter(|r| PrefetchedRegion::should_prefetch(geo_index, r))
            .collect()
    }

    /// Update retained regions in GeoIndex based on the prefetch box bounds.
    ///
    /// All DSF regions within the box (+ buffer) are marked as retained.
    /// Regions outside are evicted. This ensures the retention area covers
    /// the full prefetch box, preventing `evict_non_retained()` from removing
    /// regions that were just prefetched.
    pub fn update_retention(
        &self,
        lat: f64,
        lon: f64,
        track: f64,
        buffer: i32,
        geo_index: &GeoIndex,
    ) {
        let (lat_min, lat_max, lon_min, lon_max) = self.bounds(lat, lon, track);

        let dsf_lat_min = lat_min.floor() as i32 - buffer;
        let dsf_lat_max = (lat_max - 1e-9).floor() as i32 + buffer;
        let dsf_lon_min = lon_min.floor() as i32 - buffer;
        let dsf_lon_max = (lon_max - 1e-9).floor() as i32 + buffer;

        // Add all regions in the retained area
        for lat_i in dsf_lat_min..=dsf_lat_max {
            for lon_i in dsf_lon_min..=dsf_lon_max {
                let region = DsfRegion::new(lat_i, lon_i);
                if !geo_index.contains::<RetainedRegion>(&region) {
                    debug!(lat = lat_i, lon = lon_i, "retention: adding region");
                    geo_index.insert::<RetainedRegion>(region, RetainedRegion);
                }
            }
        }

        // Evict regions outside the retained area
        let retained_regions = geo_index.regions::<RetainedRegion>();
        for region in retained_regions {
            if region.lat < dsf_lat_min
                || region.lat > dsf_lat_max
                || region.lon < dsf_lon_min
                || region.lon > dsf_lon_max
            {
                debug!(
                    lat = region.lat,
                    lon = region.lon,
                    "retention: evicting region"
                );
                geo_index.remove::<RetainedRegion>(&region);
            }
        }
    }

    /// Compute the geographic bounds of the box.
    ///
    /// Returns `(lat_min, lat_max, lon_min, lon_max)`.
    ///
    /// The bias slides proportionally with heading:
    /// - `forward_fraction = 0.5 + (max_bias - 0.5) × |component|`
    /// - At cardinal headings: primary axis 80/20, perpendicular 50/50
    /// - At diagonals: both axes ~71/29
    /// - Total extent per axis is always constant
    pub fn bounds(&self, lat: f64, lon: f64, track: f64) -> (f64, f64, f64, f64) {
        let track_rad = track.to_radians();
        let lat_component = track_rad.cos(); // positive = north
        let lon_component = track_rad.sin(); // positive = east

        // Proportional forward fraction: 0.5 (symmetric) to max_bias (fully biased)
        let lat_fwd_frac = 0.5 + (self.max_bias - 0.5) * lat_component.abs();
        let lon_fwd_frac = 0.5 + (self.max_bias - 0.5) * lon_component.abs();

        // Apply direction: forward fraction goes in the direction of travel
        let (lat_min, lat_max) = if lat_component >= 0.0 {
            // Moving north (or due east/west): bias north
            (
                lat - self.extent * (1.0 - lat_fwd_frac),
                lat + self.extent * lat_fwd_frac,
            )
        } else {
            // Moving south: bias south
            (
                lat - self.extent * lat_fwd_frac,
                lat + self.extent * (1.0 - lat_fwd_frac),
            )
        };

        let (lon_min, lon_max) = if lon_component >= 0.0 {
            // Moving east (or due north/south): bias east
            (
                lon - self.extent * (1.0 - lon_fwd_frac),
                lon + self.extent * lon_fwd_frac,
            )
        } else {
            // Moving west: bias west
            (
                lon - self.extent * lon_fwd_frac,
                lon + self.extent * (1.0 - lon_fwd_frac),
            )
        };

        (lat_min, lat_max, lon_min, lon_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Proportional bias tests ─────────────────────────────────────────

    #[test]
    fn test_proportional_due_north() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(48.0, 15.0, 0.0);

        // Lat: 80/20 bias north. Ahead = 9 * 0.8 = 7.2, behind = 9 * 0.2 = 1.8
        assert!(
            (lat_max - 48.0 - 7.2).abs() < 0.01,
            "North should be 7.2° ahead, got {}",
            lat_max - 48.0
        );
        assert!(
            (48.0 - lat_min - 1.8).abs() < 0.01,
            "South should be 1.8° behind, got {}",
            48.0 - lat_min
        );

        // Lon: symmetric = 9 * 0.5 = 4.5 each side
        assert!((lon_max - 15.0 - 4.5).abs() < 0.01, "East should be 4.5°");
        assert!((15.0 - lon_min - 4.5).abs() < 0.01, "West should be 4.5°");
    }

    #[test]
    fn test_proportional_due_east() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (lat_min, lat_max, _lon_min, lon_max) = pbox.bounds(48.0, 15.0, 90.0);

        // Lat: symmetric (cos(90°) ≈ 0)
        let lat_north = lat_max - 48.0;
        let lat_south = 48.0 - lat_min;
        assert!(
            (lat_north - 4.5).abs() < 0.01,
            "Lat north should be 4.5°, got {}",
            lat_north
        );
        assert!(
            (lat_south - 4.5).abs() < 0.01,
            "Lat south should be 4.5°, got {}",
            lat_south
        );

        // Lon: 80/20 bias east
        let lon_east = lon_max - 15.0;
        assert!(
            (lon_east - 7.2).abs() < 0.01,
            "East should be 7.2° ahead, got {}",
            lon_east
        );
    }

    #[test]
    fn test_proportional_due_south() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (lat_min, lat_max, _, _) = pbox.bounds(48.0, 15.0, 180.0);

        // Lat: 80/20 bias south
        let lat_south = 48.0 - lat_min;
        let lat_north = lat_max - 48.0;
        assert!(
            (lat_south - 7.2).abs() < 0.01,
            "South should be 7.2° ahead, got {}",
            lat_south
        );
        assert!(
            (lat_north - 1.8).abs() < 0.01,
            "North should be 1.8° behind, got {}",
            lat_north
        );
    }

    #[test]
    fn test_proportional_due_west() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (_, _, lon_min, lon_max) = pbox.bounds(48.0, 15.0, 270.0);

        // Lon: 80/20 bias west
        let lon_west = 15.0 - lon_min;
        let lon_east = lon_max - 15.0;
        assert!(
            (lon_west - 7.2).abs() < 0.01,
            "West should be 7.2° ahead, got {}",
            lon_west
        );
        assert!(
            (lon_east - 1.8).abs() < 0.01,
            "East should be 1.8° behind, got {}",
            lon_east
        );
    }

    #[test]
    fn test_proportional_northeast_45() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (_lat_min, lat_max, _lon_min, lon_max) = pbox.bounds(48.0, 15.0, 45.0);

        // At 45°: |cos(45)| = |sin(45)| = 0.707
        // forward_frac = 0.5 + 0.3 * 0.707 = 0.712
        let lat_ahead = lat_max - 48.0;
        let lon_ahead = lon_max - 15.0;
        assert!(
            (lat_ahead - 6.41).abs() < 0.1,
            "Lat ahead should be ~6.4°, got {}",
            lat_ahead
        );
        assert!(
            (lon_ahead - 6.41).abs() < 0.1,
            "Lon ahead should be ~6.4°, got {}",
            lon_ahead
        );

        // Both axes should have equal bias at 45°
        assert!((lat_ahead - lon_ahead).abs() < 0.01, "Equal bias at 45°");
    }

    // ─── Total extent invariant ──────────────────────────────────────────

    #[test]
    fn test_total_extent_constant_at_all_headings() {
        let pbox = PrefetchBox::new(9.0, 0.8);

        for track in [
            0.0, 30.0, 45.0, 60.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0,
        ] {
            let (lat_min, lat_max, lon_min, lon_max) = pbox.bounds(48.0, 15.0, track);
            let lat_extent = lat_max - lat_min;
            let lon_extent = lon_max - lon_min;
            assert!(
                (lat_extent - 9.0).abs() < 0.01,
                "Lat extent should be 9.0 at track {}°, got {}",
                track,
                lat_extent
            );
            assert!(
                (lon_extent - 9.0).abs() < 0.01,
                "Lon extent should be 9.0 at track {}°, got {}",
                track,
                lon_extent
            );
        }
    }

    // ─── Southern hemisphere ─────────────────────────────────────────────

    #[test]
    fn test_proportional_southern_hemisphere() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let (lat_min, lat_max, _lon_min, lon_max) = pbox.bounds(-24.0, 134.0, 90.0);

        // Due east in Australia: lat symmetric, lon biased east
        let lat_extent = lat_max - lat_min;
        assert!((lat_extent - 9.0).abs() < 0.01, "Lat extent should be 9.0");

        let lon_ahead = lon_max - 134.0;
        assert!(
            (lon_ahead - 7.2).abs() < 0.01,
            "East should be 7.2° ahead, got {}",
            lon_ahead
        );

        // Negative lat regions
        assert!(lat_min < -24.0, "Min lat should be south of aircraft");
    }

    // ─── Region enumeration ──────────────────────────────────────────────

    #[test]
    fn test_region_count_at_9_extent() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        let regions = pbox.regions(48.0, 15.0, 0.0);

        // 9° extent → 10 DSF tiles per axis (box spans partial tiles at edges) → ~100 regions
        assert!(
            regions.len() >= 81 && regions.len() <= 110,
            "Should have ~100 regions at 9° extent, got {}",
            regions.len()
        );
    }

    #[test]
    fn test_regions_include_ahead_and_behind() {
        let pbox = PrefetchBox::new(9.0, 0.8);
        // Due east at (48.0, 15.0): lon ahead = 7.2°, behind = 1.8°
        let regions = pbox.regions(48.0, 15.0, 90.0);

        // Should include far east (ahead)
        let has_far_east = regions.iter().any(|r| r.lon == 21);
        assert!(has_far_east, "Should include lon=21 (~7° east ahead)");

        // Should include near west (behind)
        let has_near_west = regions.iter().any(|r| r.lon == 14);
        assert!(has_near_west, "Should include lon=14 (behind)");

        // Should NOT include far west (beyond behind margin)
        let has_far_west = regions.iter().any(|r| r.lon == 11);
        assert!(
            !has_far_west,
            "Should not include lon=11 (beyond 1.8° behind)"
        );
    }

    // ─── GeoIndex filtering ──────────────────────────────────────────────

    #[test]
    fn test_new_regions_filters_already_tracked() {
        use crate::geo_index::GeoIndex;

        let pbox = PrefetchBox::new(9.0, 0.8);
        let geo_index = GeoIndex::new();

        let tracked = DsfRegion::new(48, 14);
        geo_index.insert::<PrefetchedRegion>(tracked, PrefetchedRegion::in_progress());

        let new = pbox.new_regions(48.0, 15.0, 270.0, &geo_index);

        assert!(
            !new.contains(&tracked),
            "Should exclude already-tracked region"
        );
        assert!(!new.is_empty(), "Should have untracked regions");
    }

    // ─── Retention ───────────────────────────────────────────────────────

    #[test]
    fn test_retention_covers_prefetch_box_bounds() {
        use crate::geo_index::GeoIndex;

        let pbox = PrefetchBox::new(9.0, 0.8);
        let geo_index = GeoIndex::new();

        pbox.update_retention(48.0, 15.0, 270.0, 1, &geo_index);

        // All regions in the box should be retained
        let all_box_regions = pbox.regions(48.0, 15.0, 270.0);
        for region in &all_box_regions {
            assert!(
                geo_index.contains::<RetainedRegion>(region),
                "Region ({}, {}) in prefetch box should be retained",
                region.lat,
                region.lon
            );
        }
    }

    #[test]
    fn test_retention_does_not_evict_prefetched_regions() {
        use crate::geo_index::GeoIndex;
        use crate::prefetch::adaptive::boundary_strategy::BoundaryStrategy;

        let pbox = PrefetchBox::new(9.0, 0.8);
        let geo_index = GeoIndex::new();

        let region = DsfRegion::new(48, 12);
        BoundaryStrategy::new().mark_in_progress(&region, &geo_index);
        assert!(geo_index.contains::<PrefetchedRegion>(&region));

        pbox.update_retention(48.0, 15.0, 270.0, 1, &geo_index);

        assert!(
            geo_index.contains::<RetainedRegion>(&region),
            "InProgress region in box should be retained"
        );

        BoundaryStrategy::evict_non_retained(&geo_index);

        assert!(
            geo_index.contains::<PrefetchedRegion>(&region),
            "InProgress region should survive eviction when retention covers the box"
        );
    }

    // ─── Custom extent / bias ────────────────────────────────────────────

    #[test]
    fn test_custom_extent_7() {
        let pbox = PrefetchBox::new(7.0, 0.8);
        let (lat_min, lat_max, _, _) = pbox.bounds(48.0, 15.0, 0.0);

        let extent = lat_max - lat_min;
        assert!((extent - 7.0).abs() < 0.01, "Extent should be 7.0");
    }

    #[test]
    fn test_symmetric_bias_0_5() {
        let pbox = PrefetchBox::new(9.0, 0.5);
        let (lat_min, lat_max, _lon_min, _lon_max) = pbox.bounds(48.0, 15.0, 0.0);

        // With max_bias=0.5, all headings should be symmetric
        let lat_north = lat_max - 48.0;
        let lat_south = 48.0 - lat_min;
        assert!(
            (lat_north - lat_south).abs() < 0.01,
            "Should be symmetric with max_bias=0.5"
        );
    }
}
