//! DSF tile coordinate type for 1° × 1° X-Plane scenery tiles.
//!
//! X-Plane's scenery system is based on 1° × 1° DSF (Distribution Scenery Format)
//! tiles. This module provides a distinct coordinate type to avoid confusion with
//! Web Mercator DDS tile coordinates.
//!
//! # Coordinate System
//!
//! - `lat`: Floor of latitude (e.g., 60 for 60.5°N, -47 for -46.3°S)
//! - `lon`: Floor of longitude (e.g., -146 for -145.5°W, 12 for 12.7°E)
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::tile_based::DsfTileCoord;
//!
//! // Aircraft at N60.5, W145.3
//! let tile = DsfTileCoord::from_lat_lon(60.5, -145.3);
//! assert_eq!(tile.lat, 60);   // Floor of latitude
//! assert_eq!(tile.lon, -146); // Floor of longitude
//! ```

use std::path::Path;

/// Represents a 1° × 1° DSF tile in X-Plane's scenery system.
///
/// Named `DsfTileCoord` to distinguish from `TileCoord` (Web Mercator DDS tiles).
///
/// # Coordinate Convention
///
/// Coordinates follow X-Plane's DSF naming convention where the tile is named
/// after its southwest corner. For example, `+60-146` covers:
/// - Latitude: 60°N to 61°N
/// - Longitude: 146°W to 145°W
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct DsfTileCoord {
    /// Floor of latitude in degrees (e.g., 60 for 60.5°N).
    pub lat: i32,
    /// Floor of longitude in degrees (e.g., -146 for -145.5°W).
    pub lon: i32,
}

impl DsfTileCoord {
    /// Create a DSF tile coordinate from a tile's southwest corner coordinates.
    ///
    /// This is the direct constructor - use `from_lat_lon` for aircraft positions.
    pub const fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    /// Create from floating point coordinates (floors to tile boundary).
    ///
    /// Takes an aircraft's position and returns the DSF tile containing it.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tile = DsfTileCoord::from_lat_lon(60.5, -145.3);
    /// assert_eq!(tile.lat, 60);
    /// assert_eq!(tile.lon, -146);
    /// ```
    pub fn from_lat_lon(lat: f64, lon: f64) -> Self {
        Self {
            lat: lat.floor() as i32,
            lon: lon.floor() as i32,
        }
    }

    /// Create from a DDS texture filename.
    ///
    /// Parses filenames like `123456_789012_BI16.dds` to extract the DSF tile
    /// containing this texture. The row/col/zoom values are converted to
    /// lat/lon coordinates, then floored to DSF tile boundaries.
    ///
    /// # Returns
    ///
    /// `Some(DsfTileCoord)` if the filename is valid, `None` otherwise.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tile = DsfTileCoord::from_dds_path(Path::new("textures/12345_67890_BI16.dds"));
    /// ```
    pub fn from_dds_path(path: &Path) -> Option<Self> {
        let filename = path.file_name()?.to_str()?;
        Self::from_dds_filename(filename)
    }

    /// Parse a DDS filename (without path) to extract the DSF tile.
    ///
    /// Filename format: `{row}_{col}_{map_type}{zoom}.dds`
    /// Example: `123456_789012_BI16.dds`
    pub fn from_dds_filename(filename: &str) -> Option<Self> {
        // Remove .dds extension
        let stem = filename.strip_suffix(".dds")?;
        Self::parse_scenery_stem(stem)
    }

    /// Parse a DSF filename to extract the tile coordinate.
    ///
    /// DSF filenames follow the pattern `[+-]\d{2}[+-]\d{3}.dsf`.
    /// Examples: `+33-119.dsf` → `(33, -119)`, `-46+012.dsf` → `(-46, 12)`.
    pub fn from_dsf_filename(filename: &str) -> Option<Self> {
        let stem = filename.strip_suffix(".dsf")?;

        // Must be exactly 7 chars: [+-]DD[+-]DDD
        if stem.len() != 7 {
            return None;
        }

        let lat: i32 = stem[0..3].parse().ok()?;
        let lon: i32 = stem[3..7].parse().ok()?;
        Some(Self::new(lat, lon))
    }

    /// Parse a scenery filename (DDS, TER, etc.) to extract the DSF tile.
    ///
    /// Filename format: `{chunk_row}_{chunk_col}_{map_type}{zoom}.{ext}`
    /// Works with any extension — the coordinate parsing is the same.
    /// Also handles `_sea` suffix (e.g., `10000_5000_BI16_sea.ter`).
    pub fn from_scenery_filename(filename: &str) -> Option<Self> {
        // Strip any extension
        let stem = filename.rsplit_once('.')?.0;
        Self::parse_scenery_stem(stem)
    }

    /// Parse the stem of a scenery filename (without extension).
    ///
    /// Handles both `row_col_typeZoom` and `row_col_typeZoom_sea` formats.
    fn parse_scenery_stem(stem: &str) -> Option<Self> {
        // Handle _sea suffix
        let stem = stem.strip_suffix("_sea").unwrap_or(stem);

        // Split by underscore: row_col_typeZoom
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() != 3 {
            return None;
        }

        let chunk_row: u32 = parts[0].parse().ok()?;
        let chunk_col: u32 = parts[1].parse().ok()?;

        // Extract zoom from the last 2 digits of the third part.
        // Examples: "BI16" → 16, "GO218" → 18, "GO216" → 16
        // The map type prefix may include a trailing digit (GO2), so we
        // cannot simply skip alphabetic chars — we must take the last 2 digits.
        let zoom_part = parts[2];
        if zoom_part.len() < 3 {
            return None;
        }
        let zoom: u8 = zoom_part[zoom_part.len() - 2..].parse().ok()?;

        // Convert chunk coordinates to geographic coordinates
        let (lat, lon) = chunk_to_lat_lon(chunk_row, chunk_col, zoom);

        Some(Self::from_lat_lon(lat, lon))
    }

    /// Get the bounding box of this tile.
    ///
    /// # Returns
    ///
    /// Tuple of `(min_lat, max_lat, min_lon, max_lon)` in degrees.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tile = DsfTileCoord::new(60, -146);
    /// let (min_lat, max_lat, min_lon, max_lon) = tile.bounds();
    /// assert_eq!((min_lat, max_lat), (60.0, 61.0));
    /// assert_eq!((min_lon, max_lon), (-146.0, -145.0));
    /// ```
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let min_lat = self.lat as f64;
        let max_lat = (self.lat + 1) as f64;
        let min_lon = self.lon as f64;
        let max_lon = (self.lon + 1) as f64;
        (min_lat, max_lat, min_lon, max_lon)
    }

    /// Get the center point of this tile.
    ///
    /// # Returns
    ///
    /// Tuple of `(lat, lon)` in degrees.
    pub fn center(&self) -> (f64, f64) {
        (self.lat as f64 + 0.5, self.lon as f64 + 0.5)
    }

    /// Get adjacent tiles (N, S, E, W).
    ///
    /// Returns tiles immediately adjacent on the 4 cardinal directions.
    /// Coordinates are clamped to valid ranges (-90 to 89 for lat, -180 to 179 for lon).
    pub fn adjacent_cardinal(&self) -> [DsfTileCoord; 4] {
        [
            self.offset(1, 0),  // North
            self.offset(-1, 0), // South
            self.offset(0, 1),  // East
            self.offset(0, -1), // West
        ]
    }

    /// Get adjacent tiles including diagonals (N, NE, E, SE, S, SW, W, NW).
    ///
    /// Returns all 8 tiles surrounding this one.
    pub fn adjacent_all(&self) -> [DsfTileCoord; 8] {
        [
            self.offset(1, 0),   // N
            self.offset(1, 1),   // NE
            self.offset(0, 1),   // E
            self.offset(-1, 1),  // SE
            self.offset(-1, 0),  // S
            self.offset(-1, -1), // SW
            self.offset(0, -1),  // W
            self.offset(1, -1),  // NW
        ]
    }

    /// Get a tile offset from this one.
    ///
    /// Wraps longitude at ±180°, clamps latitude to valid range.
    pub fn offset(&self, lat_delta: i32, lon_delta: i32) -> DsfTileCoord {
        let new_lat = (self.lat + lat_delta).clamp(-90, 89);
        let mut new_lon = self.lon + lon_delta;

        // Wrap longitude
        if new_lon >= 180 {
            new_lon -= 360;
        } else if new_lon < -180 {
            new_lon += 360;
        }

        DsfTileCoord::new(new_lat, new_lon)
    }
}

impl std::fmt::Display for DsfTileCoord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // X-Plane DSF naming convention: +60-146 for N60, W146
        let lat_sign = if self.lat >= 0 { '+' } else { '-' };
        let lon_sign = if self.lon >= 0 { '+' } else { '-' };
        write!(
            f,
            "{}{:02}{}{:03}",
            lat_sign,
            self.lat.abs(),
            lon_sign,
            self.lon.abs()
        )
    }
}

/// Convert chunk (slippy map) coordinates to geographic coordinates.
///
/// Chunks use Web Mercator projection. This converts the tile center
/// to latitude/longitude.
fn chunk_to_lat_lon(chunk_row: u32, chunk_col: u32, zoom: u8) -> (f64, f64) {
    /// Offset to convert from cell top-left to cell center.
    ///
    /// Using the center rather than the corner ensures correct 1° DSF region
    /// assignment. At zoom 12 (DDS tiles), cell width is ~0.088° — the corner
    /// can fall in a different region than the center.
    const CELL_CENTER_OFFSET: f64 = 0.5;
    /// Full longitude range in degrees (Web Mercator covers -180° to +180°).
    const FULL_LONGITUDE_DEGREES: f64 = 360.0;
    /// Longitude offset to shift from [0°, 360°] → [-180°, +180°].
    const LONGITUDE_ORIGIN_OFFSET: f64 = 180.0;
    /// Web Mercator normalization factor: row/n maps to [0, 1], multiplied by
    /// this to map to the full [-π, +π] Mercator Y range.
    const MERCATOR_Y_SCALE: f64 = 2.0;

    let n = 2.0_f64.powi(zoom as i32);

    let col_center = chunk_col as f64 + CELL_CENTER_OFFSET;
    let lon = (col_center / n) * FULL_LONGITUDE_DEGREES - LONGITUDE_ORIGIN_OFFSET;

    // Inverse Web Mercator latitude (cell center)
    let row_center = chunk_row as f64 + CELL_CENTER_OFFSET;
    let lat_rad = std::f64::consts::PI * (1.0 - MERCATOR_Y_SCALE * row_center / n);
    let lat = lat_rad.sinh().atan().to_degrees();

    (lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_lat_lon_positive() {
        let tile = DsfTileCoord::from_lat_lon(60.5, 12.7);
        assert_eq!(tile.lat, 60);
        assert_eq!(tile.lon, 12);
    }

    #[test]
    fn test_from_lat_lon_negative() {
        let tile = DsfTileCoord::from_lat_lon(-46.3, -145.8);
        assert_eq!(tile.lat, -47); // Floor of -46.3 is -47
        assert_eq!(tile.lon, -146);
    }

    #[test]
    fn test_from_lat_lon_on_boundary() {
        let tile = DsfTileCoord::from_lat_lon(60.0, -146.0);
        assert_eq!(tile.lat, 60);
        assert_eq!(tile.lon, -146);
    }

    #[test]
    fn test_bounds() {
        let tile = DsfTileCoord::new(60, -146);
        let (min_lat, max_lat, min_lon, max_lon) = tile.bounds();
        assert!((min_lat - 60.0).abs() < 0.001);
        assert!((max_lat - 61.0).abs() < 0.001);
        assert!((min_lon - (-146.0)).abs() < 0.001);
        assert!((max_lon - (-145.0)).abs() < 0.001);
    }

    #[test]
    fn test_center() {
        let tile = DsfTileCoord::new(60, -146);
        let (lat, lon) = tile.center();
        assert!((lat - 60.5).abs() < 0.001);
        assert!((lon - (-145.5)).abs() < 0.001);
    }

    #[test]
    fn test_display() {
        let tile = DsfTileCoord::new(60, -146);
        assert_eq!(tile.to_string(), "+60-146");

        let tile2 = DsfTileCoord::new(-12, 5);
        assert_eq!(tile2.to_string(), "-12+005");
    }

    #[test]
    fn test_adjacent_cardinal() {
        let tile = DsfTileCoord::new(45, -122);
        let adj = tile.adjacent_cardinal();

        assert_eq!(adj[0], DsfTileCoord::new(46, -122)); // N
        assert_eq!(adj[1], DsfTileCoord::new(44, -122)); // S
        assert_eq!(adj[2], DsfTileCoord::new(45, -121)); // E
        assert_eq!(adj[3], DsfTileCoord::new(45, -123)); // W
    }

    #[test]
    fn test_adjacent_all_includes_corners() {
        let tile = DsfTileCoord::new(45, -122);
        let adj = tile.adjacent_all();

        // Check corners are included
        assert!(adj.contains(&DsfTileCoord::new(46, -121))); // NE
        assert!(adj.contains(&DsfTileCoord::new(46, -123))); // NW
        assert!(adj.contains(&DsfTileCoord::new(44, -121))); // SE
        assert!(adj.contains(&DsfTileCoord::new(44, -123))); // SW
    }

    #[test]
    fn test_offset_wrapping() {
        // Test longitude wrapping at 180°
        let tile = DsfTileCoord::new(0, 179);
        let wrapped = tile.offset(0, 2);
        assert_eq!(wrapped.lon, -179);

        // Test latitude clamping
        let north = DsfTileCoord::new(89, 0);
        let clamped = north.offset(5, 0);
        assert_eq!(clamped.lat, 89); // Clamped to max
    }

    #[test]
    fn test_from_dds_filename() {
        // Test parsing a typical DDS filename
        // Row 10000, Col 5000 at zoom 16 should map to some geographic location
        let tile = DsfTileCoord::from_dds_filename("10000_5000_BI16.dds");
        assert!(tile.is_some());
    }

    #[test]
    fn test_from_dds_filename_invalid() {
        assert!(DsfTileCoord::from_dds_filename("invalid.dds").is_none());
        assert!(DsfTileCoord::from_dds_filename("not_a_tile.txt").is_none());
        assert!(DsfTileCoord::from_dds_filename("").is_none());
    }

    #[test]
    fn test_from_dds_path() {
        let path = Path::new("textures/12345_67890_BI16.dds");
        let tile = DsfTileCoord::from_dds_path(path);
        assert!(tile.is_some());
    }

    #[test]
    fn test_equality_and_hashing() {
        use std::collections::HashSet;

        let t1 = DsfTileCoord::new(60, -146);
        let t2 = DsfTileCoord::new(60, -146);
        let t3 = DsfTileCoord::new(60, -145);

        assert_eq!(t1, t2);
        assert_ne!(t1, t3);

        let mut set = HashSet::new();
        set.insert(t1);
        assert!(set.contains(&t2));
        assert!(!set.contains(&t3));
    }

    // =========================================================================
    // from_dsf_filename tests (Issue #51)
    // =========================================================================

    #[test]
    fn test_from_dsf_filename_positive_coords() {
        let tile = DsfTileCoord::from_dsf_filename("+45+011.dsf");
        assert_eq!(tile, Some(DsfTileCoord::new(45, 11)));
    }

    #[test]
    fn test_from_dsf_filename_negative_coords() {
        let tile = DsfTileCoord::from_dsf_filename("-46+012.dsf");
        assert_eq!(tile, Some(DsfTileCoord::new(-46, 12)));
    }

    #[test]
    fn test_from_dsf_filename_mixed_signs() {
        let tile = DsfTileCoord::from_dsf_filename("+33-119.dsf");
        assert_eq!(tile, Some(DsfTileCoord::new(33, -119)));
    }

    #[test]
    fn test_from_dsf_filename_zero_coords() {
        let tile = DsfTileCoord::from_dsf_filename("+00+000.dsf");
        assert_eq!(tile, Some(DsfTileCoord::new(0, 0)));
    }

    #[test]
    fn test_from_dsf_filename_wrong_extension() {
        assert!(DsfTileCoord::from_dsf_filename("+33-119.ter").is_none());
    }

    #[test]
    fn test_from_dsf_filename_invalid_format() {
        assert!(DsfTileCoord::from_dsf_filename("invalid.dsf").is_none());
        assert!(DsfTileCoord::from_dsf_filename("").is_none());
        assert!(DsfTileCoord::from_dsf_filename("+33.dsf").is_none());
    }

    // =========================================================================
    // from_scenery_filename tests (Issue #51)
    // =========================================================================

    #[test]
    fn test_from_scenery_filename_dds() {
        let tile = DsfTileCoord::from_scenery_filename("10000_5000_BI16.dds");
        assert!(tile.is_some());
    }

    #[test]
    fn test_from_scenery_filename_ter() {
        let tile = DsfTileCoord::from_scenery_filename("10000_5000_BI16.ter");
        assert!(tile.is_some());
    }

    #[test]
    fn test_from_scenery_filename_sea_suffix() {
        let tile = DsfTileCoord::from_scenery_filename("10000_5000_BI16_sea.ter");
        assert!(tile.is_some());
    }

    #[test]
    fn test_from_scenery_filename_invalid() {
        assert!(DsfTileCoord::from_scenery_filename("invalid.dds").is_none());
        assert!(DsfTileCoord::from_scenery_filename("readme.txt").is_none());
        assert!(DsfTileCoord::from_scenery_filename("").is_none());
    }

    #[test]
    fn test_from_scenery_filename_matches_from_dds_filename() {
        // For .dds files, from_scenery_filename should produce same result as from_dds_filename
        let dds_result = DsfTileCoord::from_dds_filename("10000_5000_BI16.dds");
        let scenery_result = DsfTileCoord::from_scenery_filename("10000_5000_BI16.dds");
        assert_eq!(dds_result, scenery_result);
    }

    /// GO2 naming convention: `GO218` → map type "GO2", zoom 18.
    /// The trailing digit in "GO2" must not be consumed as part of the zoom.
    #[test]
    fn test_from_scenery_filename_go2_convention() {
        let bi = DsfTileCoord::from_scenery_filename("10000_5000_BI16.dds");
        let go2 = DsfTileCoord::from_scenery_filename("10000_5000_GO216.dds");
        // Both are the same chunk coordinates, so they should produce
        // the same DSF region regardless of map type prefix.
        assert_eq!(bi, go2, "BI16 and GO216 should resolve to same DSF region");
    }

    /// GO2 at zoom 18 should parse correctly.
    #[test]
    fn test_from_scenery_filename_go218() {
        use crate::coord::TileCoord;

        // Coordinates at zoom 18 for a known region
        let tc = crate::coord::to_tile_coords(33.5, -118.5, 18).unwrap();
        let filename = format!("{}_{}_GO218.dds", tc.row, tc.col);
        let dsf = DsfTileCoord::from_scenery_filename(&filename).unwrap();

        // Should map to region (33, -119) — same as TileCoord center
        let tile = TileCoord {
            row: tc.row,
            col: tc.col,
            zoom: 18,
        };
        let (lat, _lon) = tile.to_lat_lon();
        assert_eq!(
            dsf.lat,
            lat.floor() as i32,
            "GO218 should parse zoom as 18, not 218"
        );
    }

    /// Property-based test: all zoom levels and map type conventions must parse
    /// to the same DSF region for the same geographic point.
    ///
    /// Patches can use any zoom level (12-19) and any map type (BI, GO2, etc.).
    /// The zoom parser must handle all combinations correctly.
    #[test]
    fn test_scenery_filename_zoom_parsing_all_levels_and_types() {
        // Test coordinates at several geographic locations
        let test_points: &[(f64, f64)] = &[
            (33.5, -118.5), // Los Angeles
            (43.5, 6.5),    // Nice/LFMN
            (-33.9, 151.2), // Sydney
            (51.5, -0.1),   // London
        ];

        // Realistic zoom levels for ortho scenery
        let zoom_levels: &[u8] = &[12, 14, 16, 17, 18];

        // Map type prefixes: BI (Bing), GO2 (Google), USA (US-specific)
        let map_types: &[&str] = &["BI", "GO2", "USA"];

        for &(lat, lon) in test_points {
            let expected_lat = lat.floor() as i32;
            let expected_lon = lon.floor() as i32;

            for &zoom in zoom_levels {
                let tc = crate::coord::to_tile_coords(lat, lon, zoom).unwrap();

                for &map_type in map_types {
                    let filename = format!("{}_{}_{}{}", tc.row, tc.col, map_type, zoom);

                    // DDS extension
                    let dds_file = format!("{filename}.dds");
                    let dsf = DsfTileCoord::from_scenery_filename(&dds_file)
                        .unwrap_or_else(|| panic!("Failed to parse: {dds_file}"));
                    assert_eq!(
                        dsf.lat, expected_lat,
                        "Wrong lat for {dds_file}: got {}, expected {}",
                        dsf.lat, expected_lat
                    );
                    assert_eq!(
                        dsf.lon, expected_lon,
                        "Wrong lon for {dds_file}: got {}, expected {}",
                        dsf.lon, expected_lon
                    );

                    // TER extension (same coordinates)
                    let ter_file = format!("{filename}.ter");
                    let dsf_ter = DsfTileCoord::from_scenery_filename(&ter_file)
                        .unwrap_or_else(|| panic!("Failed to parse: {ter_file}"));
                    assert_eq!(
                        dsf, dsf_ter,
                        "DDS and TER should resolve same region: {dds_file} vs {ter_file}"
                    );

                    // _sea.ter suffix
                    let sea_file = format!("{filename}_sea.ter");
                    let dsf_sea = DsfTileCoord::from_scenery_filename(&sea_file)
                        .unwrap_or_else(|| panic!("Failed to parse: {sea_file}"));
                    assert_eq!(
                        dsf, dsf_sea,
                        "DDS and _sea.ter should resolve same region: {dds_file} vs {sea_file}"
                    );
                }
            }
        }
    }

    // =========================================================================
    // chunk_to_lat_lon center coordinate regression tests (Issue #51)
    // =========================================================================

    #[test]
    fn test_chunk_to_lat_lon_uses_center_not_top_left() {
        // chunk_to_lat_lon must use cell CENTER to determine the correct DSF region.
        // Using top-left corner causes tiles near 1° boundaries to be assigned to
        // the wrong region — the bug that caused DDS generation in patched regions.
        //
        // TileCoord::to_lat_lon() already uses center (+ 0.5). chunk_to_lat_lon
        // must agree with it.
        use crate::coord::TileCoord;

        // LFMN regression: this tile's top-left is at lon=5.977° (+005)
        // but its center is at lon=6.021° (+006) — the patched region.
        let lfmn_row: u32 = 1482;
        let lfmn_col: u32 = 2116;
        let dds_zoom: u8 = 12;

        let tile = TileCoord {
            row: lfmn_row,
            col: lfmn_col,
            zoom: dds_zoom,
        };
        let (expected_lat, expected_lon) = tile.to_lat_lon();
        let (actual_lat, actual_lon) = chunk_to_lat_lon(lfmn_row, lfmn_col, dds_zoom);

        assert!(
            (expected_lat - actual_lat).abs() < 0.001,
            "lat mismatch: TileCoord={expected_lat}, chunk_to_lat_lon={actual_lat}"
        );
        assert!(
            (expected_lon - actual_lon).abs() < 0.001,
            "lon mismatch: TileCoord={expected_lon}, chunk_to_lat_lon={actual_lon}"
        );
    }

    #[test]
    fn test_dds_filename_region_matches_tile_coord_region() {
        // The exact regression case from the LFMN live test.
        // DDS file 1482_2116_ZL12.dds should resolve to DSF region +44+006,
        // matching what TileCoord::to_dsf_tile_name() computes.
        use crate::coord::TileCoord;

        // LFMN boundary tile: top-left in +005, center in +006 (patched)
        let lfmn_row: u32 = 1482;
        let lfmn_col: u32 = 2116;
        let dds_zoom: u8 = 12;

        let tile = TileCoord {
            row: lfmn_row,
            col: lfmn_col,
            zoom: dds_zoom,
        };
        let expected_dsf = tile.to_dsf_tile_name(); // "+44+006"

        let dds_filename = format!("{lfmn_row}_{lfmn_col}_ZL{dds_zoom}.dds");
        let dsf = DsfTileCoord::from_scenery_filename(&dds_filename).unwrap();
        let actual_dsf = format!("{dsf}");

        assert_eq!(
            expected_dsf, actual_dsf,
            "DDS filename region should match TileCoord region"
        );
    }
}
