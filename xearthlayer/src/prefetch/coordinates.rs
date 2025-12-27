//! Coordinate utilities for heading-aware prefetching.
//!
//! This module provides navigation mathematics for calculating tile coverage
//! based on aircraft position, heading, and speed. Functions use the WGS-84
//! ellipsoid approximation for distance calculations.
//!
//! # Coordinate System
//!
//! - Latitude: degrees north (-90 to 90)
//! - Longitude: degrees east (-180 to 180)
//! - Heading: degrees true (0-360, 0=north, 90=east)
//! - Distance: nautical miles (1 nm = 1852 meters)

use std::f64::consts::PI;

use crate::coord::{to_tile_coords, TileCoord};

// Re-export tile_to_lat_lon_center for use in other prefetch modules
pub use crate::coord::tile_to_lat_lon_center;

/// Earth's radius in nautical miles.
const EARTH_RADIUS_NM: f64 = 3440.065;

/// Degrees to radians conversion factor.
const DEG_TO_RAD: f64 = PI / 180.0;

/// Radians to degrees conversion factor.
const RAD_TO_DEG: f64 = 180.0 / PI;

/// Calculate the approximate size of a tile in nautical miles.
///
/// Tile size varies with latitude due to the Mercator projection.
/// At the equator, tiles are largest; they shrink toward the poles.
///
/// # Arguments
///
/// * `lat` - Latitude in degrees
/// * `zoom` - Zoom level (0-18)
///
/// # Returns
///
/// Approximate tile width/height in nautical miles.
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::coordinates::tile_size_nm_at_lat;
///
/// // At mid-latitudes (45°) at zoom 14
/// let size = tile_size_nm_at_lat(45.0, 14);
/// assert!(size > 0.5 && size < 1.5); // ~0.93nm
/// ```
pub fn tile_size_nm_at_lat(lat: f64, zoom: u8) -> f64 {
    // Earth's circumference in nautical miles
    let earth_circumference_nm = 2.0 * PI * EARTH_RADIUS_NM;

    // At zoom level Z, there are 2^Z tiles spanning 360 degrees
    let tiles_at_zoom = 2.0_f64.powi(zoom as i32);

    // Size at equator
    let size_at_equator = earth_circumference_nm / tiles_at_zoom;

    // Adjust for latitude (tiles shrink toward poles in Mercator)
    let lat_rad = lat.abs() * DEG_TO_RAD;
    size_at_equator * lat_rad.cos()
}

/// Project a position along a heading for a given distance.
///
/// Uses spherical earth approximation (great circle) for accurate results
/// over short to medium distances (up to ~100nm).
///
/// # Arguments
///
/// * `start` - Starting position as (latitude, longitude) in degrees
/// * `heading_deg` - True heading in degrees (0-360)
/// * `distance_nm` - Distance to project in nautical miles
///
/// # Returns
///
/// New position as (latitude, longitude) in degrees.
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::coordinates::project_position;
///
/// // Project 10nm north from the equator
/// let (lat, lon) = project_position((0.0, 0.0), 0.0, 10.0);
/// assert!((lat - 0.167).abs() < 0.01); // ~10 arcminutes north
/// assert!(lon.abs() < 0.001);
/// ```
pub fn project_position(start: (f64, f64), heading_deg: f32, distance_nm: f32) -> (f64, f64) {
    let (lat1, lon1) = start;
    let lat1_rad = lat1 * DEG_TO_RAD;
    let lon1_rad = lon1 * DEG_TO_RAD;
    let heading_rad = (heading_deg as f64) * DEG_TO_RAD;
    let angular_distance = (distance_nm as f64) / EARTH_RADIUS_NM;

    // Spherical law of cosines for destination point
    let sin_lat1 = lat1_rad.sin();
    let cos_lat1 = lat1_rad.cos();
    let sin_d = angular_distance.sin();
    let cos_d = angular_distance.cos();

    let lat2_rad = (sin_lat1 * cos_d + cos_lat1 * sin_d * heading_rad.cos()).asin();

    let lon2_rad =
        lon1_rad + (heading_rad.sin() * sin_d * cos_lat1).atan2(cos_d - sin_lat1 * lat2_rad.sin());

    let lat2 = lat2_rad * RAD_TO_DEG;
    let mut lon2 = lon2_rad * RAD_TO_DEG;

    // Normalize longitude to -180..180
    if lon2 > 180.0 {
        lon2 -= 360.0;
    } else if lon2 < -180.0 {
        lon2 += 360.0;
    }

    // Clamp latitude to valid Web Mercator range
    let lat2 = lat2.clamp(-85.05112878, 85.05112878);

    (lat2, lon2)
}

/// Calculate the initial bearing from one position to another.
///
/// Returns the forward azimuth (direction to travel from start to end).
/// Uses the spherical earth model.
///
/// # Arguments
///
/// * `from` - Starting position as (latitude, longitude) in degrees
/// * `to` - Ending position as (latitude, longitude) in degrees
///
/// # Returns
///
/// Bearing in degrees (0-360, 0=north, 90=east).
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::coordinates::bearing_between;
///
/// // Bearing from origin to a point due east
/// let bearing = bearing_between((0.0, 0.0), (0.0, 1.0));
/// assert!((bearing - 90.0).abs() < 0.1);
/// ```
pub fn bearing_between(from: (f64, f64), to: (f64, f64)) -> f32 {
    let (lat1, lon1) = from;
    let (lat2, lon2) = to;

    let lat1_rad = lat1 * DEG_TO_RAD;
    let lat2_rad = lat2 * DEG_TO_RAD;
    let delta_lon = (lon2 - lon1) * DEG_TO_RAD;

    let x = delta_lon.cos() * lat2_rad.cos();
    let y = lat1_rad.cos() * lat2_rad.sin() - lat1_rad.sin() * x;
    let x = delta_lon.sin() * lat2_rad.cos();

    let bearing_rad = x.atan2(y);
    let mut bearing_deg = bearing_rad * RAD_TO_DEG;

    // Normalize to 0-360
    if bearing_deg < 0.0 {
        bearing_deg += 360.0;
    }

    bearing_deg as f32
}

/// Calculate the great-circle distance between two positions.
///
/// Uses the haversine formula for accuracy over short distances.
///
/// # Arguments
///
/// * `from` - First position as (latitude, longitude) in degrees
/// * `to` - Second position as (latitude, longitude) in degrees
///
/// # Returns
///
/// Distance in nautical miles.
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::coordinates::distance_nm;
///
/// // Distance from equator, prime meridian to 1 degree north
/// let dist = distance_nm((0.0, 0.0), (1.0, 0.0));
/// assert!((dist - 60.0).abs() < 0.5); // 1 degree = ~60nm
/// ```
pub fn distance_nm(from: (f64, f64), to: (f64, f64)) -> f32 {
    let (lat1, lon1) = from;
    let (lat2, lon2) = to;

    let lat1_rad = lat1 * DEG_TO_RAD;
    let lat2_rad = lat2 * DEG_TO_RAD;
    let delta_lat = (lat2 - lat1) * DEG_TO_RAD;
    let delta_lon = (lon2 - lon1) * DEG_TO_RAD;

    // Haversine formula
    let a = (delta_lat / 2.0).sin().powi(2)
        + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();

    (EARTH_RADIUS_NM * c) as f32
}

/// Calculate the angular difference between two tiles from a reference position.
///
/// This is used for determining if a tile is within the forward cone.
///
/// # Arguments
///
/// * `reference` - Reference position as (latitude, longitude)
/// * `heading` - Aircraft heading in degrees
/// * `tile` - Tile to check
///
/// # Returns
///
/// Angle in degrees from the heading to the tile center.
/// Positive values are to the right, negative to the left.
pub fn angle_to_tile(reference: (f64, f64), heading: f32, tile: &TileCoord) -> f32 {
    let (tile_lat, tile_lon) = tile_to_lat_lon_center(tile);
    let bearing = bearing_between(reference, (tile_lat, tile_lon));

    // Calculate difference, handling wrap-around
    let mut diff = bearing - heading;
    if diff > 180.0 {
        diff -= 360.0;
    } else if diff < -180.0 {
        diff += 360.0;
    }

    diff
}

/// Calculate the distance from a position to the center of a tile.
///
/// # Arguments
///
/// * `position` - Reference position as (latitude, longitude)
/// * `tile` - Tile to measure distance to
///
/// # Returns
///
/// Distance in nautical miles to the tile center.
pub fn distance_to_tile(position: (f64, f64), tile: &TileCoord) -> f32 {
    let (tile_lat, tile_lon) = tile_to_lat_lon_center(tile);
    distance_nm(position, (tile_lat, tile_lon))
}

/// Convert a position to tile coordinates at the specified zoom level.
///
/// This is a convenience wrapper around `crate::coord::to_tile_coords`.
///
/// # Arguments
///
/// * `lat` - Latitude in degrees
/// * `lon` - Longitude in degrees
/// * `zoom` - Zoom level
///
/// # Returns
///
/// Tile coordinates, or None if the position is invalid.
pub fn position_to_tile(lat: f64, lon: f64, zoom: u8) -> Option<TileCoord> {
    to_tile_coords(lat, lon, zoom).ok()
}

/// Calculate tiles along a ray from a starting tile in a given direction.
///
/// Used for generating the forward cone edge rays.
///
/// # Arguments
///
/// * `start_pos` - Starting geographic position
/// * `heading` - Direction of the ray in degrees
/// * `max_distance_nm` - Maximum distance to trace
/// * `zoom` - Zoom level for tile coordinates
///
/// # Returns
///
/// Vector of unique tiles along the ray, ordered by distance.
pub fn tiles_along_ray(
    start_pos: (f64, f64),
    heading: f32,
    max_distance_nm: f32,
    zoom: u8,
) -> Vec<TileCoord> {
    tiles_along_ray_bounded(start_pos, heading, 0.0, max_distance_nm, zoom)
}

/// Calculate tiles along a ray within a bounded distance range.
///
/// Similar to `tiles_along_ray`, but starts tracing from `min_distance_nm`
/// instead of from the start position. This is used for annular cone generation
/// where tiles close to the aircraft (within X-Plane's preload zone) should
/// be excluded.
///
/// # Arguments
///
/// * `start_pos` - Starting geographic position (aircraft location)
/// * `heading` - Direction of the ray in degrees
/// * `min_distance_nm` - Minimum distance to start tracing (inner radius)
/// * `max_distance_nm` - Maximum distance to trace (outer radius)
/// * `zoom` - Zoom level for tile coordinates
///
/// # Returns
///
/// Vector of unique tiles along the ray between min and max distance,
/// ordered by distance from start.
pub fn tiles_along_ray_bounded(
    start_pos: (f64, f64),
    heading: f32,
    min_distance_nm: f32,
    max_distance_nm: f32,
    zoom: u8,
) -> Vec<TileCoord> {
    let mut tiles = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Sample points along the ray at intervals smaller than tile size
    let sample_interval_nm = tile_size_nm_at_lat(start_pos.0, zoom) as f32 * 0.5;

    // Start from min_distance, stepping by sample_interval
    let start_sample = (min_distance_nm / sample_interval_nm).floor() as usize;
    let num_samples = (max_distance_nm / sample_interval_nm).ceil() as usize;

    for i in start_sample..=num_samples {
        let distance = i as f32 * sample_interval_nm;
        if distance > max_distance_nm {
            break;
        }
        if distance < min_distance_nm {
            continue;
        }

        let (lat, lon) = project_position(start_pos, heading, distance);
        if let Some(tile) = position_to_tile(lat, lon, zoom) {
            let key = (tile.row, tile.col);
            if seen.insert(key) {
                tiles.push(tile);
            }
        }
    }

    tiles
}

/// Normalize a heading to the range [0, 360) degrees.
///
/// Handles negative headings and values >= 360 by wrapping appropriately.
///
/// # Arguments
///
/// * `heading` - Heading in degrees (can be any value)
///
/// # Returns
///
/// Heading normalized to [0, 360) degrees.
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::coordinates::normalize_heading;
///
/// assert_eq!(normalize_heading(0.0), 0.0);
/// assert_eq!(normalize_heading(360.0), 0.0);
/// assert_eq!(normalize_heading(-90.0), 270.0);
/// assert_eq!(normalize_heading(450.0), 90.0);
/// ```
pub fn normalize_heading(heading: f32) -> f32 {
    let mut h = heading % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== tile_size_nm_at_lat tests ====================

    #[test]
    fn test_tile_size_at_equator() {
        // At equator, tile size should be maximum for a given zoom
        let size_equator = tile_size_nm_at_lat(0.0, 14);
        let size_45deg = tile_size_nm_at_lat(45.0, 14);

        assert!(
            size_equator > size_45deg,
            "Equator tiles should be larger than mid-latitude tiles"
        );
    }

    #[test]
    fn test_tile_size_symmetry() {
        // North and south latitudes should have same tile size
        let size_north = tile_size_nm_at_lat(45.0, 14);
        let size_south = tile_size_nm_at_lat(-45.0, 14);

        assert!(
            (size_north - size_south).abs() < 0.001,
            "Tile sizes should be symmetric"
        );
    }

    #[test]
    fn test_tile_size_zoom_relationship() {
        // Higher zoom = smaller tiles
        let size_z12 = tile_size_nm_at_lat(45.0, 12);
        let size_z14 = tile_size_nm_at_lat(45.0, 14);

        assert!(
            size_z12 > size_z14 * 3.0,
            "Z12 tiles should be ~4x larger than Z14"
        );
    }

    #[test]
    fn test_tile_size_realistic_values() {
        // At 45° latitude, zoom 14 tile calculation:
        // Earth circumference ≈ 21,600 nm
        // At zoom 14: 2^14 = 16,384 tiles
        // Size at equator: 21,600 / 16,384 ≈ 1.32nm
        // At 45°: cos(45°) ≈ 0.707
        // Size at 45°: 1.32 × 0.707 ≈ 0.93nm
        let size = tile_size_nm_at_lat(45.0, 14);
        assert!(size > 0.5 && size < 1.5, "Expected ~0.93nm, got {}", size);
    }

    // ==================== project_position tests ====================

    #[test]
    fn test_project_north() {
        // Project 60nm north from equator (should be ~1 degree)
        let (lat, lon) = project_position((0.0, 0.0), 0.0, 60.0);

        assert!((lat - 1.0).abs() < 0.02, "Expected ~1°N, got {}", lat);
        assert!(lon.abs() < 0.001, "Longitude should be unchanged");
    }

    #[test]
    fn test_project_east() {
        // Project 60nm east from equator
        let (lat, lon) = project_position((0.0, 0.0), 90.0, 60.0);

        assert!(lat.abs() < 0.02, "Latitude should be unchanged");
        assert!((lon - 1.0).abs() < 0.02, "Expected ~1°E, got {}", lon);
    }

    #[test]
    fn test_project_zero_distance() {
        let start = (45.5, -122.7);
        let (lat, lon) = project_position(start, 123.0, 0.0);

        assert!((lat - start.0).abs() < 0.0001);
        assert!((lon - start.1).abs() < 0.0001);
    }

    #[test]
    fn test_project_longitude_wrap() {
        // Project east from near 180° should wrap to negative
        let (lat, lon) = project_position((0.0, 179.0), 90.0, 120.0);

        assert!(lat.abs() < 0.1, "Latitude should be near equator");
        assert!(lon < 0.0, "Should wrap to negative longitude: {}", lon);
    }

    // ==================== bearing_between tests ====================

    #[test]
    fn test_bearing_north() {
        let bearing = bearing_between((0.0, 0.0), (1.0, 0.0));
        assert!(
            bearing.abs() < 1.0 || (bearing - 360.0).abs() < 1.0,
            "Due north should be ~0°, got {}",
            bearing
        );
    }

    #[test]
    fn test_bearing_east() {
        let bearing = bearing_between((0.0, 0.0), (0.0, 1.0));
        assert!(
            (bearing - 90.0).abs() < 1.0,
            "Due east should be ~90°, got {}",
            bearing
        );
    }

    #[test]
    fn test_bearing_south() {
        let bearing = bearing_between((1.0, 0.0), (0.0, 0.0));
        assert!(
            (bearing - 180.0).abs() < 1.0,
            "Due south should be ~180°, got {}",
            bearing
        );
    }

    #[test]
    fn test_bearing_west() {
        let bearing = bearing_between((0.0, 0.0), (0.0, -1.0));
        assert!(
            (bearing - 270.0).abs() < 1.0,
            "Due west should be ~270°, got {}",
            bearing
        );
    }

    // ==================== distance_nm tests ====================

    #[test]
    fn test_distance_one_degree_latitude() {
        // 1 degree of latitude is approximately 60 nautical miles
        let dist = distance_nm((0.0, 0.0), (1.0, 0.0));
        assert!(
            (dist - 60.0).abs() < 1.0,
            "1° lat should be ~60nm, got {}",
            dist
        );
    }

    #[test]
    fn test_distance_zero() {
        let dist = distance_nm((45.0, -122.0), (45.0, -122.0));
        assert!(dist.abs() < 0.001, "Same point should have zero distance");
    }

    #[test]
    fn test_distance_symmetry() {
        let a = (45.0, -122.0);
        let b = (46.0, -121.0);

        let dist_ab = distance_nm(a, b);
        let dist_ba = distance_nm(b, a);

        assert!(
            (dist_ab - dist_ba).abs() < 0.001,
            "Distance should be symmetric"
        );
    }

    #[test]
    fn test_distance_toulouse_to_paris() {
        // LFBO (Toulouse) to LFPG (Paris) is approximately 310nm
        let toulouse = (43.6, 1.4);
        let paris = (49.0, 2.5);
        let dist = distance_nm(toulouse, paris);

        assert!((dist - 325.0).abs() < 20.0, "Expected ~325nm, got {}", dist);
    }

    // ==================== angle_to_tile tests ====================

    #[test]
    fn test_angle_directly_ahead() {
        // Create a tile directly north of the reference
        let reference = (45.0, -122.0);
        if let Some(tile) = position_to_tile(45.1, -122.0, 14) {
            let angle = angle_to_tile(reference, 0.0, &tile);
            assert!(
                angle.abs() < 10.0,
                "Tile ahead should have angle ~0, got {}",
                angle
            );
        }
    }

    #[test]
    fn test_angle_to_right() {
        // Tile to the east, heading north
        let reference = (45.0, -122.0);
        if let Some(tile) = position_to_tile(45.0, -121.9, 14) {
            let angle = angle_to_tile(reference, 0.0, &tile);
            assert!(
                angle > 0.0 && angle < 100.0,
                "Tile to right should have positive angle, got {}",
                angle
            );
        }
    }

    #[test]
    fn test_angle_to_left() {
        // Tile to the west, heading north
        let reference = (45.0, -122.0);
        if let Some(tile) = position_to_tile(45.0, -122.1, 14) {
            let angle = angle_to_tile(reference, 0.0, &tile);
            assert!(
                angle < 0.0 && angle > -100.0,
                "Tile to left should have negative angle, got {}",
                angle
            );
        }
    }

    // ==================== tiles_along_ray tests ====================

    #[test]
    fn test_tiles_along_ray_includes_start() {
        let start = (45.0, -122.0);
        let tiles = tiles_along_ray(start, 0.0, 5.0, 14);

        assert!(!tiles.is_empty(), "Should find at least one tile");

        // First tile should contain the start position
        let start_tile = position_to_tile(start.0, start.1, 14).unwrap();
        assert_eq!(
            tiles[0], start_tile,
            "First tile should be at start position"
        );
    }

    #[test]
    fn test_tiles_along_ray_ordered_by_distance() {
        let start = (45.0, -122.0);
        let tiles = tiles_along_ray(start, 0.0, 10.0, 14);

        // Tiles should be ordered by increasing row (going north = decreasing row)
        // Actually, going north means decreasing row numbers in Web Mercator
        for i in 1..tiles.len() {
            let dist_prev = distance_to_tile(start, &tiles[i - 1]);
            let dist_curr = distance_to_tile(start, &tiles[i]);
            assert!(
                dist_curr >= dist_prev - 0.1, // Allow small tolerance for tile center vs edge
                "Tiles should be ordered by distance"
            );
        }
    }

    #[test]
    fn test_tiles_along_ray_no_duplicates() {
        let start = (45.0, -122.0);
        let tiles = tiles_along_ray(start, 45.0, 15.0, 14);

        let mut seen = std::collections::HashSet::new();
        for tile in &tiles {
            let key = (tile.row, tile.col);
            assert!(seen.insert(key), "Duplicate tile found: {:?}", tile);
        }
    }

    // ==================== tiles_along_ray_bounded tests ====================

    #[test]
    fn test_tiles_along_ray_bounded_excludes_inner() {
        let start = (45.0, -122.0);
        let inner_radius = 2.0; // nm
        let outer_radius = 8.0; // nm

        let tiles = tiles_along_ray_bounded(start, 0.0, inner_radius, outer_radius, 14);

        // All tiles should be beyond the inner radius
        for tile in &tiles {
            let dist = distance_to_tile(start, tile);
            assert!(
                dist >= inner_radius - 0.5, // Allow small tolerance for tile size
                "Tile at distance {} should be >= inner radius {}",
                dist,
                inner_radius
            );
        }
    }

    #[test]
    fn test_tiles_along_ray_bounded_respects_outer() {
        let start = (45.0, -122.0);
        let inner_radius = 2.0;
        let outer_radius = 6.0;

        let tiles = tiles_along_ray_bounded(start, 0.0, inner_radius, outer_radius, 14);

        // All tiles should be within the outer radius (allowing for tile size)
        let tile_size = tile_size_nm_at_lat(start.0, 14) as f32;
        for tile in &tiles {
            let dist = distance_to_tile(start, tile);
            assert!(
                dist <= outer_radius + tile_size,
                "Tile at distance {} should be <= outer radius {} + tile_size {}",
                dist,
                outer_radius,
                tile_size
            );
        }
    }

    #[test]
    fn test_tiles_along_ray_bounded_fewer_than_unbounded() {
        let start = (45.0, -122.0);
        let inner_radius = 3.0;
        let outer_radius = 10.0;

        let bounded = tiles_along_ray_bounded(start, 0.0, inner_radius, outer_radius, 14);
        let unbounded = tiles_along_ray(start, 0.0, outer_radius, 14);

        // Bounded should have fewer tiles than unbounded
        assert!(
            bounded.len() < unbounded.len(),
            "Bounded ({}) should have fewer tiles than unbounded ({})",
            bounded.len(),
            unbounded.len()
        );
    }

    #[test]
    fn test_tiles_along_ray_bounded_zero_inner_equals_unbounded() {
        let start = (45.0, -122.0);
        let outer_radius = 8.0;

        let bounded = tiles_along_ray_bounded(start, 45.0, 0.0, outer_radius, 14);
        let unbounded = tiles_along_ray(start, 45.0, outer_radius, 14);

        // With zero inner radius, should be equivalent
        assert_eq!(
            bounded.len(),
            unbounded.len(),
            "Zero inner radius should equal unbounded"
        );
    }

    // ==================== roundtrip tests ====================

    #[test]
    fn test_project_and_distance_consistency() {
        // Project a known distance, then measure - should match
        let start = (45.0, -122.0);
        let distance = 20.0; // nm
        let heading = 45.0;

        let end = project_position(start, heading, distance);
        let measured = distance_nm(start, end);

        assert!(
            (measured - distance).abs() < 0.5,
            "Projected {} nm but measured {} nm",
            distance,
            measured
        );
    }

    #[test]
    fn test_project_and_bearing_consistency() {
        // Project along a heading, then calculate bearing back - should be opposite
        let start = (45.0, -122.0);
        let heading = 60.0_f32;

        let end = project_position(start, heading, 50.0);
        let bearing = bearing_between(start, end);

        let diff = (bearing - heading).abs();
        assert!(
            diff < 2.0 || (360.0 - diff) < 2.0,
            "Expected bearing ~{}, got {}",
            heading,
            bearing
        );
    }

    // ==================== normalize_heading tests ====================

    #[test]
    fn test_normalize_heading_valid_range() {
        assert_eq!(normalize_heading(0.0), 0.0);
        assert_eq!(normalize_heading(90.0), 90.0);
        assert_eq!(normalize_heading(180.0), 180.0);
        assert_eq!(normalize_heading(270.0), 270.0);
    }

    #[test]
    fn test_normalize_heading_negative() {
        assert!((normalize_heading(-1.0) - 359.0).abs() < 0.001);
        assert!((normalize_heading(-90.0) - 270.0).abs() < 0.001);
        assert!((normalize_heading(-180.0) - 180.0).abs() < 0.001);
        assert!((normalize_heading(-270.0) - 90.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_heading_overflow() {
        assert!((normalize_heading(360.0) - 0.0).abs() < 0.001);
        assert!((normalize_heading(361.0) - 1.0).abs() < 0.001);
        assert!((normalize_heading(450.0) - 90.0).abs() < 0.001);
        assert!((normalize_heading(720.0) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_heading_always_in_range() {
        // Test a range of values
        for h in [
            -720, -360, -180, -90, -1, 0, 1, 90, 180, 270, 359, 360, 450, 720,
        ] {
            let normalized = normalize_heading(h as f32);
            assert!(
                (0.0..360.0).contains(&normalized),
                "normalize_heading({}) = {} is not in [0, 360)",
                h,
                normalized
            );
        }
    }
}
