//! Tile intersection testing for prefetch zone calculations.
//!
//! This module provides utilities for determining whether tiles intersect
//! with geographic zones (radial buffers, heading cones, etc.). It accounts
//! for tile size variation across zoom levels.
//!
//! # Key Concept: Intersection vs Center
//!
//! A tile's **center** may be outside a zone while its **edge** is inside.
//! These utilities test for **intersection** - any overlap between tile and zone.
//!
//! ```text
//!     ┌─────────────┐
//!     │  Tile       │
//!     │   ●────────────── Center at 125nm (outside 120nm zone)
//!     │             │
//!     └─────┬───────┘
//!           │
//!           └────────────── Edge at 119nm (inside 120nm zone!)
//! ```

use super::scenery_index::SceneryTile;

/// Estimated half-sizes for tiles at different zoom levels (in nautical miles).
///
/// These are conservative estimates that ensure we don't miss any intersecting tiles.
/// Based on tile size calculations at mid-latitudes (~45°).
pub fn tile_half_size_nm(zoom: u8) -> f32 {
    match zoom {
        z if z <= 11 => 8.0, // ~15nm tiles at ZL11 and below
        12 => 6.0,           // ~10-12nm tiles at ZL12
        13 => 3.0,           // ~5nm tiles at ZL13
        14 => 1.5,           // ~2.5nm tiles at ZL14
        15 => 0.75,          // ~1.2nm tiles at ZL15
        _ => 0.5,            // <1nm tiles at ZL16+
    }
}

/// Geographic bounds of a tile for intersection testing.
#[derive(Debug, Clone, Copy)]
pub struct TileBounds {
    /// Distance from reference point to tile center (nm)
    pub center_distance_nm: f32,
    /// Half-size of tile (nm) - radius from center to edge
    pub half_size_nm: f32,
    /// Bearing from reference point to tile center (degrees, 0=North)
    pub bearing_deg: f32,
    /// Angular half-extent of tile as seen from reference (degrees)
    pub angular_half_extent_deg: f32,
}

impl TileBounds {
    /// Calculate bounds for a scenery tile relative to a reference position.
    ///
    /// # Arguments
    ///
    /// * `tile` - The scenery tile to calculate bounds for
    /// * `ref_lat` - Reference latitude (typically aircraft position)
    /// * `ref_lon` - Reference longitude
    pub fn from_scenery_tile(tile: &SceneryTile, ref_lat: f64, ref_lon: f64) -> Self {
        let ref_lat = ref_lat as f32;
        let ref_lon = ref_lon as f32;

        // Calculate distance and bearing to tile center
        let lat_diff = (tile.lat - ref_lat) * 60.0; // North positive, nm
        let lon_diff = (tile.lon - ref_lon) * 60.0 * ref_lat.to_radians().cos(); // East positive, nm
        let center_distance_nm = (lat_diff * lat_diff + lon_diff * lon_diff).sqrt();

        // Bearing (0° = North, 90° = East)
        let bearing_deg = lon_diff.atan2(lat_diff).to_degrees();
        let bearing_deg = if bearing_deg < 0.0 {
            bearing_deg + 360.0
        } else {
            bearing_deg
        };

        // Tile half-size based on zoom
        let half_size_nm = tile_half_size_nm(tile.tile_zoom());

        // Angular extent of tile (wider at close range)
        let angular_half_extent_deg = if center_distance_nm > 0.1 {
            (half_size_nm / center_distance_nm).atan().to_degrees()
        } else {
            90.0 // Very close tiles cover a wide angle
        };

        Self {
            center_distance_nm,
            half_size_nm,
            bearing_deg,
            angular_half_extent_deg,
        }
    }

    /// Inner edge distance (closest point of tile to reference).
    #[inline]
    pub fn inner_edge_nm(&self) -> f32 {
        (self.center_distance_nm - self.half_size_nm).max(0.0)
    }

    /// Outer edge distance (farthest point of tile from reference).
    #[inline]
    pub fn outer_edge_nm(&self) -> f32 {
        self.center_distance_nm + self.half_size_nm
    }

    /// Check if tile intersects an annular zone (ring between inner and outer radius).
    ///
    /// # Arguments
    ///
    /// * `inner_radius_nm` - Inner boundary of the zone
    /// * `outer_radius_nm` - Outer boundary of the zone
    pub fn intersects_annulus(&self, inner_radius_nm: f32, outer_radius_nm: f32) -> bool {
        // Tile intersects if its inner edge <= outer boundary AND outer edge >= inner boundary
        self.inner_edge_nm() <= outer_radius_nm && self.outer_edge_nm() >= inner_radius_nm
    }

    /// Check if tile intersects a heading cone.
    ///
    /// # Arguments
    ///
    /// * `heading_deg` - Aircraft heading (degrees, 0=North)
    /// * `half_angle_deg` - Half-angle of the cone (e.g., 30° for a 60° cone)
    /// * `inner_radius_nm` - Inner boundary of cone
    /// * `outer_radius_nm` - Outer boundary of cone
    pub fn intersects_cone(
        &self,
        heading_deg: f32,
        half_angle_deg: f32,
        inner_radius_nm: f32,
        outer_radius_nm: f32,
    ) -> bool {
        // First check radial distance
        if !self.intersects_annulus(inner_radius_nm, outer_radius_nm) {
            return false;
        }

        // Calculate angle difference from heading
        let mut angle_diff = (self.bearing_deg - heading_deg).abs();
        if angle_diff > 180.0 {
            angle_diff = 360.0 - angle_diff;
        }

        // Account for tile angular extent (tile edge might be inside cone even if center is outside)
        let effective_angle_diff = (angle_diff - self.angular_half_extent_deg).max(0.0);

        effective_angle_diff <= half_angle_deg
    }

    /// Get the angle difference from a given heading (degrees).
    ///
    /// Returns value in range [0, 180].
    pub fn angle_from_heading(&self, heading_deg: f32) -> f32 {
        let mut diff = (self.bearing_deg - heading_deg).abs();
        if diff > 180.0 {
            diff = 360.0 - diff;
        }
        diff
    }
}

/// Result of zone intersection testing for a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneIntersection {
    /// Tile does not intersect any prefetch zone.
    None,
    /// Tile intersects the radial buffer zone only.
    RadialBuffer,
    /// Tile intersects the heading cone (forward edge).
    ConeEdge,
    /// Tile intersects the heading cone (forward center - highest priority).
    ConeCenter,
}

impl ZoneIntersection {
    /// Check if tile intersects any zone.
    pub fn intersects(&self) -> bool {
        !matches!(self, ZoneIntersection::None)
    }

    /// Get base priority for this zone (lower = higher priority).
    pub fn base_priority(&self) -> u32 {
        match self {
            ZoneIntersection::ConeCenter => 0,
            ZoneIntersection::ConeEdge => 50,
            ZoneIntersection::RadialBuffer => 100,
            ZoneIntersection::None => u32::MAX,
        }
    }
}

/// Test which prefetch zone(s) a tile intersects.
///
/// # Arguments
///
/// * `bounds` - Pre-calculated tile bounds
/// * `heading_deg` - Aircraft heading (degrees)
/// * `cone_half_angle_deg` - Half-angle of heading cone
/// * `inner_radius_nm` - Inner boundary (start of prefetch zone)
/// * `radial_outer_nm` - Outer boundary of radial buffer
/// * `cone_outer_nm` - Outer boundary of heading cone
pub fn test_zone_intersection(
    bounds: &TileBounds,
    heading_deg: f32,
    cone_half_angle_deg: f32,
    inner_radius_nm: f32,
    radial_outer_nm: f32,
    cone_outer_nm: f32,
) -> ZoneIntersection {
    // Check if completely inside X-Plane's loaded zone
    if bounds.outer_edge_nm() < inner_radius_nm {
        return ZoneIntersection::None;
    }

    // Check heading cone first (higher priority)
    if bounds.intersects_cone(
        heading_deg,
        cone_half_angle_deg,
        inner_radius_nm,
        cone_outer_nm,
    ) {
        let angle_diff = bounds.angle_from_heading(heading_deg);
        if angle_diff < cone_half_angle_deg / 2.0 {
            return ZoneIntersection::ConeCenter;
        } else {
            return ZoneIntersection::ConeEdge;
        }
    }

    // Check radial buffer
    if bounds.intersects_annulus(inner_radius_nm, radial_outer_nm) {
        return ZoneIntersection::RadialBuffer;
    }

    ZoneIntersection::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn make_tile(lat: f32, lon: f32, chunk_zoom: u8) -> SceneryTile {
        SceneryTile {
            row: 0,
            col: 0,
            chunk_zoom,
            lat,
            lon,
            is_sea: false,
        }
    }

    #[test]
    fn test_tile_half_size_decreases_with_zoom() {
        assert!(tile_half_size_nm(12) > tile_half_size_nm(14));
        assert!(tile_half_size_nm(14) > tile_half_size_nm(16));
    }

    #[test]
    fn test_bounds_inner_outer_edge() {
        // Tile at 100nm with 5nm half-size
        let bounds = TileBounds {
            center_distance_nm: 100.0,
            half_size_nm: 5.0,
            bearing_deg: 0.0,
            angular_half_extent_deg: 2.86, // atan(5/100) degrees
        };

        assert_eq!(bounds.inner_edge_nm(), 95.0);
        assert_eq!(bounds.outer_edge_nm(), 105.0);
    }

    #[test]
    fn test_intersects_annulus_inside() {
        let bounds = TileBounds {
            center_distance_nm: 90.0,
            half_size_nm: 5.0,
            bearing_deg: 0.0,
            angular_half_extent_deg: 3.0,
        };

        // 85-95nm edge should intersect 85-100nm zone
        assert!(bounds.intersects_annulus(85.0, 100.0));
    }

    #[test]
    fn test_intersects_annulus_outside() {
        let bounds = TileBounds {
            center_distance_nm: 70.0,
            half_size_nm: 5.0,
            bearing_deg: 0.0,
            angular_half_extent_deg: 3.0,
        };

        // 65-75nm edge should NOT intersect 85-100nm zone
        assert!(!bounds.intersects_annulus(85.0, 100.0));
    }

    #[test]
    fn test_intersects_annulus_edge_case() {
        // Tile center at 80nm with 6nm half-size
        // Outer edge at 86nm - should intersect 85nm boundary
        let bounds = TileBounds {
            center_distance_nm: 80.0,
            half_size_nm: 6.0,
            bearing_deg: 0.0,
            angular_half_extent_deg: 4.0,
        };

        assert!(bounds.intersects_annulus(85.0, 100.0));
    }

    #[test]
    fn test_intersects_cone_in_center() {
        // Tile directly ahead (bearing = heading)
        let bounds = TileBounds {
            center_distance_nm: 100.0,
            half_size_nm: 3.0,
            bearing_deg: 45.0, // Same as heading
            angular_half_extent_deg: 1.7,
        };

        assert!(bounds.intersects_cone(45.0, 30.0, 85.0, 120.0));
    }

    #[test]
    fn test_intersects_cone_outside_angle() {
        // Tile 90 degrees off heading
        let bounds = TileBounds {
            center_distance_nm: 100.0,
            half_size_nm: 3.0,
            bearing_deg: 135.0, // 90° off from heading 45
            angular_half_extent_deg: 1.7,
        };

        assert!(!bounds.intersects_cone(45.0, 30.0, 85.0, 120.0));
    }

    #[test]
    fn test_zone_intersection_cone_center() {
        let bounds = TileBounds {
            center_distance_nm: 100.0,
            half_size_nm: 3.0,
            bearing_deg: 45.0,
            angular_half_extent_deg: 1.7,
        };

        let result = test_zone_intersection(&bounds, 45.0, 30.0, 85.0, 100.0, 120.0);
        assert_eq!(result, ZoneIntersection::ConeCenter);
    }

    #[test]
    fn test_zone_intersection_radial_only() {
        // Tile behind aircraft (180° off heading) but within radial zone
        let bounds = TileBounds {
            center_distance_nm: 92.0,
            half_size_nm: 3.0,
            bearing_deg: 225.0, // Behind aircraft heading 45°
            angular_half_extent_deg: 1.8,
        };

        let result = test_zone_intersection(&bounds, 45.0, 30.0, 85.0, 100.0, 120.0);
        assert_eq!(result, ZoneIntersection::RadialBuffer);
    }

    #[test]
    fn test_zone_intersection_none() {
        // Tile inside X-Plane's loaded zone
        let bounds = TileBounds {
            center_distance_nm: 70.0,
            half_size_nm: 5.0,
            bearing_deg: 45.0,
            angular_half_extent_deg: 4.0,
        };

        let result = test_zone_intersection(&bounds, 45.0, 30.0, 85.0, 100.0, 120.0);
        assert_eq!(result, ZoneIntersection::None);
    }
}
