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

        // Split by underscore: row_col_typeZoom
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() != 3 {
            return None;
        }

        let chunk_row: u32 = parts[0].parse().ok()?;
        let chunk_col: u32 = parts[1].parse().ok()?;

        // Extract zoom from the third part (e.g., "BI16" -> 16)
        let zoom_part = parts[2];
        let zoom: u8 = zoom_part
            .chars()
            .skip_while(|c| c.is_alphabetic())
            .collect::<String>()
            .parse()
            .ok()?;

        // Convert chunk coordinates to geographic coordinates
        // Chunks are 256x256 pixels at chunk_zoom level
        // DDS tiles are 16x16 chunks, so tile_zoom = chunk_zoom - 4
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
    let n = 2.0_f64.powi(zoom as i32);

    // Calculate tile center (add 0.5 for center of tile)
    let lon = (chunk_col as f64 / n) * 360.0 - 180.0;

    // Web Mercator latitude calculation
    let lat_rad = std::f64::consts::PI * (1.0 - 2.0 * chunk_row as f64 / n);
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
}
