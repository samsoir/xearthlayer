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
pub fn tile_to_lat_lon(_tile: &TileCoord) -> (f64, f64) {
    // TODO: Implement
    todo!()
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
}
