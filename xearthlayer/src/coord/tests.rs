//! Tests for coordinate conversion

use super::*;

#[test]
fn test_new_york_city_at_zoom_16() {
    // New York City: 40.7128°N, 74.0060°W
    // Expected tile coordinates calculated using known Web Mercator formula
    let result = to_tile_coords(40.7128, -74.0060, 16);
    assert!(result.is_ok(), "Valid coordinates should not error");

    let tile = result.unwrap();
    assert_eq!(
        tile.row, 24341,
        "NYC latitude should map to row 24341 at zoom 16"
    );
    assert_eq!(
        tile.col, 19298,
        "NYC longitude should map to col 19298 at zoom 16"
    );
    assert_eq!(tile.zoom, 16);
}

#[test]
fn test_london_at_zoom_10() {
    // London: 51.5074°N, 0.1278°W
    let result = to_tile_coords(51.5074, -0.1278, 10);
    assert!(result.is_ok());

    let tile = result.unwrap();
    assert_eq!(tile.row, 340);
    assert_eq!(tile.col, 511);
    assert_eq!(tile.zoom, 10);
}

#[test]
fn test_equator_prime_meridian() {
    // 0°N, 0°E should map to center of world
    let result = to_tile_coords(0.0, 0.0, 1);
    assert!(result.is_ok());

    let tile = result.unwrap();
    // At zoom 1: 2×2 tiles, center is at (1, 1)
    assert_eq!(tile.row, 1);
    assert_eq!(tile.col, 1);
}

#[test]
fn test_invalid_latitude_too_high() {
    let result = to_tile_coords(90.0, 0.0, 10);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        CoordError::InvalidLatitude(_)
    ));
}

#[test]
fn test_invalid_latitude_too_low() {
    let result = to_tile_coords(-90.0, 0.0, 10);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        CoordError::InvalidLatitude(_)
    ));
}

#[test]
fn test_invalid_longitude_too_high() {
    let result = to_tile_coords(0.0, 181.0, 10);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        CoordError::InvalidLongitude(_)
    ));
}

#[test]
fn test_invalid_longitude_too_low() {
    let result = to_tile_coords(0.0, -181.0, 10);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        CoordError::InvalidLongitude(_)
    ));
}

#[test]
fn test_invalid_zoom_too_high() {
    let result = to_tile_coords(0.0, 0.0, 19);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), CoordError::InvalidZoom(_)));
}

#[test]
fn test_max_valid_latitude() {
    // Web Mercator max latitude
    let result = to_tile_coords(85.05112878, 0.0, 10);
    assert!(result.is_ok());
}

#[test]
fn test_min_valid_latitude() {
    // Web Mercator min latitude
    let result = to_tile_coords(-85.05112878, 0.0, 10);
    assert!(result.is_ok());
}
