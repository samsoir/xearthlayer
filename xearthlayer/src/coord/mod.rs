//! Coordinate conversion module
//!
//! Provides conversions between geographic coordinates (latitude/longitude)
//! and Web Mercator tile/chunk coordinates used by satellite imagery providers.

mod types;

pub use types::{ChunkCoord, CoordError, TileCoord, MAX_LAT, MAX_ZOOM, MIN_LAT, MIN_LON, MIN_ZOOM};

use std::f64::consts::PI;

/// Converts geographic coordinates to tile coordinates.
///
/// # Arguments
///
/// * `lat` - Latitude in degrees (-85.05112878 to 85.05112878)
/// * `lon` - Longitude in degrees (-180.0 to 180.0)
/// * `zoom` - Zoom level (0 to 18)
///
/// # Returns
///
/// A `Result` containing the tile coordinates or an error if inputs are invalid.
#[inline]
pub fn to_tile_coords(lat: f64, lon: f64, zoom: u8) -> Result<TileCoord, CoordError> {
    // Validate inputs
    if !(MIN_LAT..=MAX_LAT).contains(&lat) {
        return Err(CoordError::InvalidLatitude(lat));
    }
    if !(MIN_LON..=180.0).contains(&lon) {
        return Err(CoordError::InvalidLongitude(lon));
    }
    if zoom > MAX_ZOOM {
        return Err(CoordError::InvalidZoom(zoom));
    }

    // Calculate number of tiles at this zoom level
    let n = 2.0_f64.powi(zoom as i32);

    // Convert longitude to tile X coordinate
    let col = ((lon + 180.0) / 360.0 * n) as u32;

    // Convert latitude to tile Y coordinate using Web Mercator projection
    let lat_rad = lat * PI / 180.0;
    let row = ((1.0 - lat_rad.tan().asinh() / PI) / 2.0 * n) as u32;

    Ok(TileCoord { row, col, zoom })
}

/// Converts tile coordinates back to geographic coordinates.
///
/// Returns the latitude/longitude of the tile's northwest corner.
#[inline]
pub fn tile_to_lat_lon(tile: &TileCoord) -> (f64, f64) {
    let n = 2.0_f64.powi(tile.zoom as i32);

    // Convert tile X coordinate to longitude
    let lon = tile.col as f64 / n * 360.0 - 180.0;

    // Convert tile Y coordinate to latitude using inverse Web Mercator
    let y = tile.row as f64 / n;
    let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
    let lat = lat_rad * 180.0 / PI;

    (lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_york_city_at_zoom_16() {
        // New York City: 40.7128°N, 74.0060°W
        let result = to_tile_coords(40.7128, -74.0060, 16);
        assert!(result.is_ok(), "Valid coordinates should not error");

        let tile = result.unwrap();
        assert_eq!(tile.row, 24640);
        assert_eq!(tile.col, 19295);
        assert_eq!(tile.zoom, 16);
    }

    #[test]
    fn test_invalid_latitude() {
        let result = to_tile_coords(90.0, 0.0, 10);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CoordError::InvalidLatitude(_)
        ));
    }

    #[test]
    fn test_tile_to_lat_lon_northwest_corner() {
        // Tile should return its northwest corner coordinates
        let tile = TileCoord {
            row: 24640,
            col: 19295,
            zoom: 16,
        };

        let (lat, lon) = tile_to_lat_lon(&tile);

        // Should be close to NYC but not exact (northwest corner of tile)
        assert!(
            (lat - 40.713).abs() < 0.01,
            "Latitude should be close to 40.713"
        );
        assert!(
            (lon - (-74.007)).abs() < 0.01,
            "Longitude should be close to -74.007"
        );
    }

    #[test]
    fn test_tile_to_lat_lon_at_equator() {
        // Tile at equator, prime meridian
        let tile = TileCoord {
            row: 512,
            col: 512,
            zoom: 10,
        };

        let (lat, lon) = tile_to_lat_lon(&tile);

        // At zoom 10, tile 512,512 should be near 0,0
        assert!(lat.abs() < 1.0, "Should be near equator");
        assert!(lon.abs() < 1.0, "Should be near prime meridian");
    }

    #[test]
    fn test_roundtrip_conversion() {
        // Convert lat/lon → tile → lat/lon should give similar coordinates
        let original_lat = 40.7128;
        let original_lon = -74.0060;
        let zoom = 16;

        // Forward conversion
        let tile = to_tile_coords(original_lat, original_lon, zoom).unwrap();

        // Reverse conversion
        let (converted_lat, converted_lon) = tile_to_lat_lon(&tile);

        // Should be close (within tile precision)
        // At zoom 16, each tile is ~1.2km, so tolerance should be small
        assert!(
            (converted_lat - original_lat).abs() < 0.01,
            "Latitude should roundtrip within 0.01 degrees"
        );
        assert!(
            (converted_lon - original_lon).abs() < 0.01,
            "Longitude should roundtrip within 0.01 degrees"
        );
    }

    #[test]
    fn test_roundtrip_at_different_zooms() {
        let lat = 51.5074; // London
        let lon = -0.1278;

        for zoom in [0, 5, 10, 15, 18] {
            let tile = to_tile_coords(lat, lon, zoom).unwrap();
            let (converted_lat, converted_lon) = tile_to_lat_lon(&tile);

            // Tolerance is the size of one tile at this zoom level
            // Since tile_to_lat_lon returns northwest corner, we need full tile tolerance
            let tile_size_degrees = 360.0 / (2.0_f64.powi(zoom as i32));

            assert!(
                (converted_lat - lat).abs() < tile_size_degrees,
                "Zoom {}: lat diff {} exceeds tile size {}",
                zoom,
                (converted_lat - lat).abs(),
                tile_size_degrees
            );
            assert!(
                (converted_lon - lon).abs() < tile_size_degrees,
                "Zoom {}: lon diff {} exceeds tile size {}",
                zoom,
                (converted_lon - lon).abs(),
                tile_size_degrees
            );
        }
    }
}
