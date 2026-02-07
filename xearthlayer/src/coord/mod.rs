//! Coordinate conversion module
//!
//! Provides conversions between geographic coordinates (latitude/longitude)
//! and Web Mercator tile/chunk coordinates used by satellite imagery providers.

mod types;

pub use types::{
    ChunkCoord, CoordError, TileChunksIterator, TileCoord, MAX_LAT, MAX_ZOOM, MIN_LAT, MIN_LON,
    MIN_ZOOM,
};

/// Format a DSF-style directory name from integer lat/lon.
///
/// Produces names like `+33-119`, `-12+005`, `+00+000` following
/// X-Plane's DSF naming convention (southwest corner of 1°×1° tile).
#[inline]
pub fn format_dsf_name(lat: i32, lon: i32) -> String {
    let lat_sign = if lat >= 0 { '+' } else { '-' };
    let lon_sign = if lon >= 0 { '+' } else { '-' };
    format!("{}{:02}{}{:03}", lat_sign, lat.abs(), lon_sign, lon.abs())
}

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
    let max_coord = (n as u32).saturating_sub(1);

    // Convert longitude to tile X coordinate
    // Clamp to valid range to handle floating-point edge cases
    let col_f = (lon + 180.0) / 360.0 * n;
    let col = (col_f as u32).min(max_coord);

    // Convert latitude to tile Y coordinate using Web Mercator projection
    // Clamp to valid range to handle floating-point edge cases
    let lat_rad = lat * PI / 180.0;
    let row_f = (1.0 - lat_rad.tan().asinh() / PI) / 2.0 * n;
    let row = (row_f as u32).min(max_coord);

    Ok(TileCoord { row, col, zoom })
}

/// Converts geographic coordinates to chunk coordinates.
///
/// This directly converts lat/lon to a chunk within a tile, useful for
/// determining which specific 256×256 pixel chunk to download.
///
/// # Arguments
///
/// * `lat` - Latitude in degrees (-85.05112878 to 85.05112878)
/// * `lon` - Longitude in degrees (-180.0 to 180.0)
/// * `zoom` - Zoom level (0 to 18)
///
/// # Returns
///
/// A `Result` containing the chunk coordinates or an error if inputs are invalid.
#[inline]
pub fn to_chunk_coords(lat: f64, lon: f64, zoom: u8) -> Result<ChunkCoord, CoordError> {
    // First get the tile coordinates
    let tile = to_tile_coords(lat, lon, zoom)?;

    // Now we need to find the position within the tile
    // Each tile is divided into 16×16 chunks
    // We calculate at chunk resolution (zoom + 4 for 2^4 = 16)
    let chunk_zoom_offset = 4; // log2(16) = 4
    let chunk_zoom = zoom + chunk_zoom_offset;
    let n_chunks = 2.0_f64.powi(chunk_zoom as i32);
    let max_chunk_coord = (n_chunks as u32).saturating_sub(1);

    // Calculate chunk-level coordinates
    // Clamp to valid range to handle floating-point edge cases
    let chunk_col_f = (lon + 180.0) / 360.0 * n_chunks;
    let chunk_col_global = (chunk_col_f as u32).min(max_chunk_coord);

    let lat_rad = lat * PI / 180.0;
    let chunk_row_f = (1.0 - lat_rad.tan().asinh() / PI) / 2.0 * n_chunks;
    let chunk_row_global = (chunk_row_f as u32).min(max_chunk_coord);

    // Extract the chunk position within the tile (0-15)
    let chunk_row = (chunk_row_global % 16) as u8;
    let chunk_col = (chunk_col_global % 16) as u8;

    Ok(ChunkCoord {
        tile_row: tile.row,
        tile_col: tile.col,
        chunk_row,
        chunk_col,
        zoom,
    })
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

/// Converts tile coordinates to the geographic center of the tile.
///
/// Returns the latitude/longitude of the tile's center point.
/// This is useful for displaying human-readable coordinates.
///
/// # Example
///
/// ```
/// use xearthlayer::coord::{TileCoord, tile_to_lat_lon_center};
///
/// let tile = TileCoord { row: 100000, col: 125184, zoom: 18 };
/// let (lat, lon) = tile_to_lat_lon_center(&tile);
/// // Returns approximately (39.19, -8.07) - center of tile in Portugal
/// ```
#[inline]
pub fn tile_to_lat_lon_center(tile: &TileCoord) -> (f64, f64) {
    let n = 2.0_f64.powi(tile.zoom as i32);

    // Convert tile X coordinate to longitude (add 0.5 for center)
    let lon = (tile.col as f64 + 0.5) / n * 360.0 - 180.0;

    // Convert tile Y coordinate to latitude using inverse Web Mercator (add 0.5 for center)
    let y = (tile.row as f64 + 0.5) / n;
    let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
    let lat = lat_rad * 180.0 / PI;

    (lat, lon)
}

/// Converts tile coordinates to a Bing Maps quadkey.
///
/// Quadkeys are base-4 strings where each digit (0-3) represents which quadrant
/// the tile is in at each zoom level. This is Bing Maps' tile naming scheme.
///
/// # Arguments
///
/// * `tile` - The tile coordinates to convert
///
/// # Returns
///
/// A string of digits 0-3, with length equal to the zoom level.
/// Zoom 0 returns an empty string.
///
/// # Quadkey Encoding
///
/// Each digit represents a quadrant:
/// - 0 = top-left (northwest)
/// - 1 = top-right (northeast)
/// - 2 = bottom-left (southwest)
/// - 3 = bottom-right (southeast)
///
/// # Example
///
/// ```
/// use xearthlayer::coord::{TileCoord, tile_to_quadkey};
///
/// let tile = TileCoord { row: 5, col: 3, zoom: 3 };
/// assert_eq!(tile_to_quadkey(&tile), "213");
/// ```
#[inline]
pub fn tile_to_quadkey(tile: &TileCoord) -> String {
    let mut quadkey = String::with_capacity(tile.zoom as usize);

    // Build quadkey from most significant to least significant zoom level
    for i in (1..=tile.zoom).rev() {
        let mut digit = 0u8;
        let mask = 1u32 << (i - 1);

        // Check if column bit is set (east/west)
        if (tile.col & mask) != 0 {
            digit += 1;
        }

        // Check if row bit is set (north/south)
        if (tile.row & mask) != 0 {
            digit += 2;
        }

        quadkey.push((b'0' + digit) as char);
    }

    quadkey
}

/// Converts a Bing Maps quadkey to tile coordinates.
///
/// This is the inverse of `tile_to_quadkey`.
///
/// # Arguments
///
/// * `quadkey` - A string containing only digits 0-3
///
/// # Returns
///
/// A `Result` containing the tile coordinates or an error if the quadkey is invalid.
///
/// # Errors
///
/// Returns `CoordError::InvalidQuadkey` if:
/// - The quadkey contains characters other than '0'-'3'
/// - The quadkey length exceeds `MAX_ZOOM` (18)
///
/// # Example
///
/// ```
/// use xearthlayer::coord::{quadkey_to_tile, TileCoord};
///
/// let tile = quadkey_to_tile("213").unwrap();
/// assert_eq!(tile, TileCoord { row: 5, col: 3, zoom: 3 });
/// ```
#[inline]
pub fn quadkey_to_tile(quadkey: &str) -> Result<TileCoord, CoordError> {
    // Validate quadkey length
    if quadkey.len() > MAX_ZOOM as usize {
        return Err(CoordError::InvalidQuadkey(quadkey.to_string()));
    }

    let mut row = 0u32;
    let mut col = 0u32;

    // Parse quadkey from left to right (most significant to least significant)
    for c in quadkey.chars() {
        // Validate character is a digit 0-3
        let digit = match c {
            '0' => 0,
            '1' => 1,
            '2' => 2,
            '3' => 3,
            _ => return Err(CoordError::InvalidQuadkey(quadkey.to_string())),
        };

        // Shift existing coordinates left (multiply by 2)
        row <<= 1;
        col <<= 1;

        // Add bit based on digit
        if (digit & 1) != 0 {
            col |= 1; // East
        }
        if (digit & 2) != 0 {
            row |= 1; // South
        }
    }

    Ok(TileCoord {
        row,
        col,
        zoom: quadkey.len() as u8,
    })
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
    fn test_tile_to_lat_lon_center_europe() {
        // From AutoOrtho .ter file: 100000_125184_BI18.ter
        // LOAD_CENTER 39.18969 -8.07495
        // Note: AutoOrtho's LOAD_CENTER may use a slightly different calculation
        let tile = TileCoord {
            row: 100000,
            col: 125184,
            zoom: 18,
        };

        let (lat, lon) = tile_to_lat_lon_center(&tile);

        // Should be close to the LOAD_CENTER from the .ter file
        // At zoom 18, tile size is ~0.0014° so 0.02° tolerance is reasonable
        assert!(
            (lat - 39.18969).abs() < 0.02,
            "Latitude {} should be close to 39.18969",
            lat
        );
        assert!(
            (lon - (-8.07495)).abs() < 0.02,
            "Longitude {} should be close to -8.07495",
            lon
        );
    }

    #[test]
    fn test_tile_to_lat_lon_center_australia() {
        // From AutoOrtho .ter file: 169840_253472_BI18.ter
        // LOAD_CENTER -46.91275 168.10181
        let tile = TileCoord {
            row: 169840,
            col: 253472,
            zoom: 18,
        };

        let (lat, lon) = tile_to_lat_lon_center(&tile);

        // Should be close to the LOAD_CENTER from the .ter file
        assert!(
            (lat - (-46.91275)).abs() < 0.02,
            "Latitude {} should be close to -46.91275",
            lat
        );
        assert!(
            (lon - 168.10181).abs() < 0.02,
            "Longitude {} should be close to 168.10181",
            lon
        );
    }

    #[test]
    fn test_tile_to_lat_lon_center_asia() {
        // From AutoOrtho .ter file: 100000_222560_BI18.ter
        // LOAD_CENTER 39.18969 125.65063
        let tile = TileCoord {
            row: 100000,
            col: 222560,
            zoom: 18,
        };

        let (lat, lon) = tile_to_lat_lon_center(&tile);

        // Should be close to the LOAD_CENTER from the .ter file
        assert!(
            (lat - 39.18969).abs() < 0.02,
            "Latitude {} should be close to 39.18969",
            lat
        );
        assert!(
            (lon - 125.65063).abs() < 0.02,
            "Longitude {} should be close to 125.65063",
            lon
        );
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

    // Chunk coordinate tests
    #[test]
    fn test_to_chunk_coords_basic() {
        // NYC should map to a specific chunk
        let chunk = to_chunk_coords(40.7128, -74.0060, 16).unwrap();

        // Verify it's the correct tile
        assert_eq!(chunk.tile_row, 24640);
        assert_eq!(chunk.tile_col, 19295);
        assert_eq!(chunk.zoom, 16);

        // Chunk coords should be 0-15
        assert!(chunk.chunk_row < 16);
        assert!(chunk.chunk_col < 16);
    }

    #[test]
    fn test_chunk_to_global_coords() {
        // A chunk in tile (100, 200) at position (5, 7) within the tile
        let chunk = ChunkCoord {
            tile_row: 100,
            tile_col: 200,
            chunk_row: 5,
            chunk_col: 7,
            zoom: 10,
        };

        let (global_row, global_col, zoom) = chunk.to_global_coords();

        // Global coords should be: tile * 16 + chunk_offset
        assert_eq!(global_row, 100 * 16 + 5); // 1605
        assert_eq!(global_col, 200 * 16 + 7); // 3207
                                              // Zoom should be +4 from tile zoom (10 + 4 = 14)
        assert_eq!(zoom, 14);
    }

    #[test]
    fn test_chunk_at_tile_origin() {
        // Chunk at (0,0) within tile should have same global coords as tile*16
        let chunk = ChunkCoord {
            tile_row: 50,
            tile_col: 75,
            chunk_row: 0,
            chunk_col: 0,
            zoom: 12,
        };

        let (global_row, global_col, _) = chunk.to_global_coords();
        assert_eq!(global_row, 50 * 16);
        assert_eq!(global_col, 75 * 16);
    }

    #[test]
    fn test_chunk_at_tile_max() {
        // Chunk at (15,15) within tile (last chunk)
        let chunk = ChunkCoord {
            tile_row: 10,
            tile_col: 20,
            chunk_row: 15,
            chunk_col: 15,
            zoom: 8,
        };

        let (global_row, global_col, _) = chunk.to_global_coords();
        assert_eq!(global_row, 10 * 16 + 15); // 175
        assert_eq!(global_col, 20 * 16 + 15); // 335
    }

    #[test]
    fn test_tile_and_chunk_coords_consistent() {
        // Converting to tile and to chunk should give consistent results
        let lat = 51.5074;
        let lon = -0.1278;
        let zoom = 12;

        let tile = to_tile_coords(lat, lon, zoom).unwrap();
        let chunk = to_chunk_coords(lat, lon, zoom).unwrap();

        // Chunk's tile coords should match direct tile coords
        assert_eq!(chunk.tile_row, tile.row);
        assert_eq!(chunk.tile_col, tile.col);
        assert_eq!(chunk.zoom, tile.zoom);
    }

    // Tile chunks iterator tests
    #[test]
    fn test_tile_chunks_iterator_count() {
        // A tile should yield exactly 256 chunks (16×16)
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 12,
        };

        let chunks: Vec<_> = tile.chunks().collect();
        assert_eq!(chunks.len(), 256, "Tile should contain exactly 256 chunks");
    }

    #[test]
    fn test_tile_chunks_iterator_order() {
        // Chunks should be yielded in row-major order
        let tile = TileCoord {
            row: 50,
            col: 75,
            zoom: 10,
        };

        let mut chunks = tile.chunks();

        // First chunk should be (0, 0)
        let first = chunks.next().unwrap();
        assert_eq!(first.chunk_row, 0);
        assert_eq!(first.chunk_col, 0);

        // Second chunk should be (0, 1)
        let second = chunks.next().unwrap();
        assert_eq!(second.chunk_row, 0);
        assert_eq!(second.chunk_col, 1);

        // Skip to end of first row (chunk 15)
        for _ in 2..16 {
            chunks.next();
        }

        // 17th chunk should be (1, 0) - start of second row
        let row2_start = chunks.next().unwrap();
        assert_eq!(row2_start.chunk_row, 1);
        assert_eq!(row2_start.chunk_col, 0);
    }

    #[test]
    fn test_tile_chunks_all_belong_to_same_tile() {
        // All chunks should reference the same tile coordinates
        let tile = TileCoord {
            row: 123,
            col: 456,
            zoom: 15,
        };

        for chunk in tile.chunks() {
            assert_eq!(chunk.tile_row, tile.row);
            assert_eq!(chunk.tile_col, tile.col);
            assert_eq!(chunk.zoom, tile.zoom);
        }
    }

    #[test]
    fn test_tile_chunks_coordinates_in_range() {
        // All chunk coordinates should be 0-15
        let tile = TileCoord {
            row: 10,
            col: 20,
            zoom: 8,
        };

        for chunk in tile.chunks() {
            assert!(
                chunk.chunk_row < 16,
                "Chunk row {} should be less than 16",
                chunk.chunk_row
            );
            assert!(
                chunk.chunk_col < 16,
                "Chunk col {} should be less than 16",
                chunk.chunk_col
            );
        }
    }

    #[test]
    fn test_tile_chunks_no_duplicates() {
        // Each chunk position should appear exactly once
        let tile = TileCoord {
            row: 42,
            col: 84,
            zoom: 14,
        };

        let mut seen = std::collections::HashSet::new();
        for chunk in tile.chunks() {
            let key = (chunk.chunk_row, chunk.chunk_col);
            assert!(
                seen.insert(key),
                "Duplicate chunk at ({}, {})",
                chunk.chunk_row,
                chunk.chunk_col
            );
        }

        assert_eq!(seen.len(), 256, "Should have 256 unique chunks");
    }

    // Quadkey conversion tests
    #[test]
    fn test_tile_to_quadkey_zoom_0() {
        // Zoom 0 has only one tile, quadkey should be empty
        let tile = TileCoord {
            row: 0,
            col: 0,
            zoom: 0,
        };
        assert_eq!(tile_to_quadkey(&tile), "");
    }

    #[test]
    fn test_tile_to_quadkey_zoom_1() {
        // Zoom 1 has 2×2 tiles, quadkeys are "0", "1", "2", "3"
        assert_eq!(
            tile_to_quadkey(&TileCoord {
                row: 0,
                col: 0,
                zoom: 1
            }),
            "0"
        ); // top-left
        assert_eq!(
            tile_to_quadkey(&TileCoord {
                row: 0,
                col: 1,
                zoom: 1
            }),
            "1"
        ); // top-right
        assert_eq!(
            tile_to_quadkey(&TileCoord {
                row: 1,
                col: 0,
                zoom: 1
            }),
            "2"
        ); // bottom-left
        assert_eq!(
            tile_to_quadkey(&TileCoord {
                row: 1,
                col: 1,
                zoom: 1
            }),
            "3"
        ); // bottom-right
    }

    #[test]
    fn test_tile_to_quadkey_bing_example() {
        // Example from Bing Maps documentation
        // Tile (3, 5, 3) should be quadkey "213"
        let tile = TileCoord {
            row: 5,
            col: 3,
            zoom: 3,
        };
        assert_eq!(tile_to_quadkey(&tile), "213");
    }

    #[test]
    fn test_tile_to_quadkey_length() {
        // Quadkey length should equal zoom level
        for zoom in 0..=18 {
            let tile = TileCoord {
                row: 0,
                col: 0,
                zoom,
            };
            assert_eq!(
                tile_to_quadkey(&tile).len(),
                zoom as usize,
                "Quadkey length should match zoom level"
            );
        }
    }

    #[test]
    fn test_tile_to_quadkey_nyc() {
        // New York City tile at zoom 16
        let tile = TileCoord {
            row: 24640,
            col: 19295,
            zoom: 16,
        };
        let quadkey = tile_to_quadkey(&tile);
        assert_eq!(quadkey.len(), 16);
        // Quadkey should only contain digits 0-3
        assert!(quadkey.chars().all(|c| ('0'..='3').contains(&c)));
    }

    #[test]
    fn test_quadkey_to_tile_zoom_0() {
        // Empty quadkey should return zoom 0 tile at origin
        let tile = quadkey_to_tile("").unwrap();
        assert_eq!(tile.row, 0);
        assert_eq!(tile.col, 0);
        assert_eq!(tile.zoom, 0);
    }

    #[test]
    fn test_quadkey_to_tile_zoom_1() {
        // Single digit quadkeys should map to zoom 1 tiles
        assert_eq!(
            quadkey_to_tile("0").unwrap(),
            TileCoord {
                row: 0,
                col: 0,
                zoom: 1
            }
        ); // top-left
        assert_eq!(
            quadkey_to_tile("1").unwrap(),
            TileCoord {
                row: 0,
                col: 1,
                zoom: 1
            }
        ); // top-right
        assert_eq!(
            quadkey_to_tile("2").unwrap(),
            TileCoord {
                row: 1,
                col: 0,
                zoom: 1
            }
        ); // bottom-left
        assert_eq!(
            quadkey_to_tile("3").unwrap(),
            TileCoord {
                row: 1,
                col: 1,
                zoom: 1
            }
        ); // bottom-right
    }

    #[test]
    fn test_quadkey_to_tile_bing_example() {
        // Reverse of Bing example
        let tile = quadkey_to_tile("213").unwrap();
        assert_eq!(tile.row, 5);
        assert_eq!(tile.col, 3);
        assert_eq!(tile.zoom, 3);
    }

    #[test]
    fn test_quadkey_to_tile_invalid_characters() {
        // Quadkey with invalid characters should error
        assert!(quadkey_to_tile("012a").is_err());
        assert!(quadkey_to_tile("4").is_err());
        assert!(quadkey_to_tile("01-3").is_err());
    }

    #[test]
    fn test_quadkey_to_tile_too_long() {
        // Quadkey longer than MAX_ZOOM should error
        let long_quadkey = "0123012301230123012"; // 19 digits
        assert!(quadkey_to_tile(long_quadkey).is_err());
    }

    #[test]
    fn test_quadkey_roundtrip() {
        // Converting tile → quadkey → tile should be identity
        let original = TileCoord {
            row: 24640,
            col: 19295,
            zoom: 16,
        };
        let quadkey = tile_to_quadkey(&original);
        let converted = quadkey_to_tile(&quadkey).unwrap();
        assert_eq!(converted, original);
    }

    // Property-based tests using proptest
    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn test_roundtrip_property(
                lat in -85.05..85.05_f64,
                lon in -180.0..180.0_f64,
                zoom in 0u8..=18
            ) {
                // Convert to tile and back
                let tile = to_tile_coords(lat, lon, zoom)?;
                let (converted_lat, converted_lon) = tile_to_lat_lon(&tile);

                // Calculate expected precision at this zoom level
                let tile_size = 360.0 / (2.0_f64.powi(zoom as i32));

                // Converted coordinates should be within one tile of original
                prop_assert!(
                    (converted_lat - lat).abs() < tile_size,
                    "Latitude roundtrip failed: {} -> {} (diff: {}, tile_size: {})",
                    lat, converted_lat, (converted_lat - lat).abs(), tile_size
                );
                prop_assert!(
                    (converted_lon - lon).abs() < tile_size,
                    "Longitude roundtrip failed: {} -> {} (diff: {}, tile_size: {})",
                    lon, converted_lon, (converted_lon - lon).abs(), tile_size
                );
            }

            #[test]
            fn test_tile_coords_in_bounds(
                lat in -85.05..85.05_f64,
                lon in -180.0..180.0_f64,
                zoom in 0u8..=18
            ) {
                let tile = to_tile_coords(lat, lon, zoom)?;

                // Tile coordinates should be within valid range
                let max_tile = 2u32.pow(zoom as u32);
                prop_assert!(
                    tile.row < max_tile,
                    "Row {} exceeds maximum {} at zoom {}",
                    tile.row, max_tile, zoom
                );
                prop_assert!(
                    tile.col < max_tile,
                    "Col {} exceeds maximum {} at zoom {}",
                    tile.col, max_tile, zoom
                );
                prop_assert_eq!(tile.zoom, zoom);
            }

            #[test]
            fn test_longitude_monotonic(
                lat in 0.0..1.0_f64,
                lon1 in -180.0..-90.0_f64,
                lon2 in -90.0..0.0_f64,
                zoom in 10u8..=15
            ) {
                // For fixed latitude, increasing longitude should increase column
                let tile1 = to_tile_coords(lat, lon1, zoom)?;
                let tile2 = to_tile_coords(lat, lon2, zoom)?;

                prop_assert!(
                    tile1.col < tile2.col,
                    "Longitude not monotonic: lon {} (col {}) >= lon {} (col {})",
                    lon1, tile1.col, lon2, tile2.col
                );
            }

            #[test]
            fn test_tile_to_lat_lon_in_bounds(
                row_raw in 0u32..65536,
                col_raw in 0u32..65536,
                zoom in 0u8..=16
            ) {
                let max_coord = 2u32.pow(zoom as u32);
                // Constrain row/col to valid range for this zoom
                let row = row_raw % max_coord;
                let col = col_raw % max_coord;

                let tile = TileCoord { row, col, zoom };
                let (lat, lon) = tile_to_lat_lon(&tile);

                // Results should be in valid geographic bounds
                prop_assert!(
                    (MIN_LAT..=MAX_LAT).contains(&lat),
                    "Latitude {} out of bounds [{}, {}]",
                    lat, MIN_LAT, MAX_LAT
                );
                prop_assert!(
                    (-180.0..=180.0).contains(&lon),
                    "Longitude {} out of bounds [-180, 180]",
                    lon
                );
            }

            #[test]
            fn test_reject_invalid_latitude(
                lat in -90.0..-85.06_f64,
                lon in -180.0..180.0_f64,
                zoom in 0u8..=18
            ) {
                // Latitudes outside Web Mercator range should error
                let result = to_tile_coords(lat, lon, zoom);
                prop_assert!(result.is_err());
                prop_assert!(matches!(result.unwrap_err(), CoordError::InvalidLatitude(_)));
            }

            #[test]
            fn test_reject_invalid_longitude(
                lat in -85.0..85.0_f64,
                lon in 180.01..360.0_f64,
                zoom in 0u8..=18
            ) {
                // Longitudes outside valid range should error
                let result = to_tile_coords(lat, lon, zoom);
                prop_assert!(result.is_err());
                prop_assert!(matches!(result.unwrap_err(), CoordError::InvalidLongitude(_)));
            }

            // Chunk coordinate property tests
            #[test]
            fn test_chunk_coords_in_valid_range(
                lat in -85.05..85.05_f64,
                lon in -180.0..180.0_f64,
                zoom in 0u8..=18
            ) {
                // Chunk coordinates should always be 0-15
                let chunk = to_chunk_coords(lat, lon, zoom)?;

                prop_assert!(
                    chunk.chunk_row < 16,
                    "Chunk row {} should be < 16",
                    chunk.chunk_row
                );
                prop_assert!(
                    chunk.chunk_col < 16,
                    "Chunk col {} should be < 16",
                    chunk.chunk_col
                );
            }

            #[test]
            fn test_chunk_tile_coords_match_tile_conversion(
                lat in -85.05..85.05_f64,
                lon in -180.0..180.0_f64,
                zoom in 0u8..=18
            ) {
                // Chunk's tile coords should match direct tile conversion
                let tile = to_tile_coords(lat, lon, zoom)?;
                let chunk = to_chunk_coords(lat, lon, zoom)?;

                prop_assert_eq!(chunk.tile_row, tile.row);
                prop_assert_eq!(chunk.tile_col, tile.col);
                prop_assert_eq!(chunk.zoom, tile.zoom);
            }

            #[test]
            fn test_chunk_global_coords_calculation(
                tile_row in 0u32..1000,
                tile_col in 0u32..1000,
                chunk_row in 0u8..16,
                chunk_col in 0u8..16,
                zoom in 0u8..=18
            ) {
                // Global coords should be tile * 16 + chunk_offset
                let chunk = ChunkCoord {
                    tile_row,
                    tile_col,
                    chunk_row,
                    chunk_col,
                    zoom,
                };

                let (global_row, global_col, global_zoom) = chunk.to_global_coords();

                prop_assert_eq!(global_row, tile_row * 16 + chunk_row as u32);
                prop_assert_eq!(global_col, tile_col * 16 + chunk_col as u32);
                prop_assert_eq!(global_zoom, zoom + 4);
            }

            #[test]
            fn test_tile_chunks_iterator_yields_256(
                row in 0u32..1000,
                col in 0u32..1000,
                zoom in 0u8..=18
            ) {
                // Iterator should always yield exactly 256 chunks
                let tile = TileCoord { row, col, zoom };
                let count = tile.chunks().count();
                prop_assert_eq!(count, 256, "Tile should yield 256 chunks");
            }

            #[test]
            fn test_tile_chunks_iterator_all_valid(
                row in 0u32..1000,
                col in 0u32..1000,
                zoom in 0u8..=18
            ) {
                // All chunks from iterator should have valid coordinates
                let tile = TileCoord { row, col, zoom };

                for chunk in tile.chunks() {
                    prop_assert_eq!(chunk.tile_row, tile.row);
                    prop_assert_eq!(chunk.tile_col, tile.col);
                    prop_assert_eq!(chunk.zoom, tile.zoom);
                    prop_assert!(chunk.chunk_row < 16);
                    prop_assert!(chunk.chunk_col < 16);
                }
            }

            #[test]
            fn test_tile_chunks_iterator_no_duplicates(
                row in 0u32..100,
                col in 0u32..100,
                zoom in 0u8..=18
            ) {
                // Iterator should yield no duplicate chunk positions
                let tile = TileCoord { row, col, zoom };
                let mut seen = std::collections::HashSet::new();

                for chunk in tile.chunks() {
                    let key = (chunk.chunk_row, chunk.chunk_col);
                    prop_assert!(
                        seen.insert(key),
                        "Duplicate chunk at ({}, {})",
                        chunk.chunk_row,
                        chunk.chunk_col
                    );
                }

                prop_assert_eq!(seen.len(), 256);
            }

            // Quadkey property tests
            #[test]
            fn test_quadkey_roundtrip_property(
                row in 0u32..100000,
                col in 0u32..100000,
                zoom in 0u8..=18
            ) {
                // Constrain row/col to valid range for zoom level
                let max_coord = 2u32.pow(zoom as u32);
                let row = row % max_coord.max(1);
                let col = col % max_coord.max(1);

                let original = TileCoord { row, col, zoom };
                let quadkey = tile_to_quadkey(&original);
                let converted = quadkey_to_tile(&quadkey)?;

                prop_assert_eq!(converted, original,
                    "Roundtrip failed: {:?} -> {} -> {:?}", original, quadkey, converted);
            }

            #[test]
            fn test_quadkey_length_equals_zoom(
                row in 0u32..1000,
                col in 0u32..1000,
                zoom in 0u8..=18
            ) {
                // Quadkey length should always equal zoom level
                let max_coord = 2u32.pow(zoom as u32);
                let row = row % max_coord.max(1);
                let col = col % max_coord.max(1);

                let tile = TileCoord { row, col, zoom };
                let quadkey = tile_to_quadkey(&tile);

                prop_assert_eq!(quadkey.len(), zoom as usize,
                    "Quadkey length {} doesn't match zoom {}", quadkey.len(), zoom);
            }

            #[test]
            fn test_quadkey_only_valid_digits(
                row in 0u32..1000,
                col in 0u32..1000,
                zoom in 1u8..=18
            ) {
                // Quadkeys should only contain digits 0-3
                let max_coord = 2u32.pow(zoom as u32);
                let row = row % max_coord;
                let col = col % max_coord;

                let tile = TileCoord { row, col, zoom };
                let quadkey = tile_to_quadkey(&tile);

                for c in quadkey.chars() {
                    prop_assert!(
                        ('0'..='3').contains(&c),
                        "Quadkey {} contains invalid character '{}'", quadkey, c
                    );
                }
            }

            #[test]
            fn test_quadkey_deterministic(
                row in 0u32..1000,
                col in 0u32..1000,
                zoom in 0u8..=18
            ) {
                // Same tile should always produce same quadkey
                let max_coord = 2u32.pow(zoom as u32);
                let row = row % max_coord.max(1);
                let col = col % max_coord.max(1);

                let tile = TileCoord { row, col, zoom };
                let quadkey1 = tile_to_quadkey(&tile);
                let quadkey2 = tile_to_quadkey(&tile);

                prop_assert_eq!(quadkey1, quadkey2, "Quadkey generation not deterministic");
            }

            #[test]
            fn test_quadkey_to_tile_validates_length(
                quadkey_len in 19usize..30
            ) {
                // Quadkeys longer than MAX_ZOOM should be rejected
                let quadkey: String = std::iter::repeat_n('0', quadkey_len).collect();
                let result = quadkey_to_tile(&quadkey);

                prop_assert!(result.is_err(), "Should reject quadkey length {}", quadkey_len);
                prop_assert!(matches!(result.unwrap_err(), CoordError::InvalidQuadkey(_)));
            }

            #[test]
            fn test_quadkey_zoom_0_always_empty(
                _row in 0u32..100,
                _col in 0u32..100
            ) {
                // Zoom 0 tiles should always produce empty quadkey
                let tile = TileCoord { row: 0, col: 0, zoom: 0 };
                let quadkey = tile_to_quadkey(&tile);
                prop_assert_eq!(quadkey, "", "Zoom 0 should produce empty quadkey");
            }
        }
    }
}
