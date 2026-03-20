//! Core data types for the Scene Tracker.
//!
//! This module contains the empirical data model for tracking X-Plane's scenery requests.
//! Types here represent what was actually requested, not derived interpretations.

use std::collections::HashSet;
use std::time::Instant;

use crate::coord::{tile_to_lat_lon_center, TileCoord};
use crate::fuse::parse_dds_filename;

/// A DDS texture tile coordinate (empirical data - what X-Plane requested).
///
/// This represents the actual Web Mercator tile coordinates from DDS filename
/// parsing. Geographic information is derived from these coordinates, not stored.
///
/// # DDS Filename Format
///
/// X-Plane requests tiles in the format: `{row}_{col}_{maptype}{zoom}.dds`
/// Example: `100000_125184_BI18.dds` → row=100000, col=125184, zoom=18
///
/// # Relationship to Other Types
///
/// - Delegates coordinate math to [`crate::coord`] module
/// - Delegates filename parsing to [`crate::fuse::parse_dds_filename`]
/// - Semantically represents "what X-Plane requested" in the Scene Tracker domain
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DdsTileCoord {
    /// Web Mercator tile row (Y coordinate, 0 = north)
    pub row: u32,
    /// Web Mercator tile column (X coordinate, 0 = west)
    pub col: u32,
    /// Zoom level (e.g., 14, 16, 18)
    pub zoom: u8,
}

impl DdsTileCoord {
    /// Create a new DDS tile coordinate.
    pub fn new(row: u32, col: u32, zoom: u8) -> Self {
        Self { row, col, zoom }
    }

    /// Parse a DDS filename into tile coordinates.
    ///
    /// Delegates to [`crate::fuse::parse_dds_filename`] for robust parsing.
    ///
    /// # Expected Format
    ///
    /// `{row}_{col}_{maptype}{zoom}.dds`
    /// - `100000_125184_BI18.dds` (Bing, zoom 18)
    /// - `25264_10368_GO216.dds` (Google GO2, zoom 16)
    ///
    /// # Returns
    ///
    /// `None` if the filename doesn't match the expected format.
    pub fn from_dds_filename(filename: &str) -> Option<Self> {
        let parsed = parse_dds_filename(filename).ok()?;
        Some(Self {
            row: parsed.row,
            col: parsed.col,
            zoom: parsed.zoom,
        })
    }

    /// Derive the geographic center of this tile.
    ///
    /// Delegates to [`crate::coord::tile_to_lat_lon_center`] for Web Mercator → WGS84 conversion.
    ///
    /// # Returns
    ///
    /// A tuple of (latitude, longitude) in degrees.
    pub fn to_lat_lon(&self) -> (f64, f64) {
        let tile = TileCoord {
            row: self.row,
            col: self.col,
            zoom: self.zoom,
        };
        tile_to_lat_lon_center(&tile)
    }

    /// Derive which 1°×1° geographic region this tile falls within.
    ///
    /// The region is determined by flooring the tile's center coordinates.
    pub fn to_geo_region(&self) -> GeoRegion {
        let (lat, lon) = self.to_lat_lon();
        GeoRegion::from_lat_lon(lat, lon)
    }

    /// Convert to a [`TileCoord`] for use with the coord module.
    pub fn to_tile_coord(&self) -> TileCoord {
        TileCoord {
            row: self.row,
            col: self.col,
            zoom: self.zoom,
        }
    }
}

impl From<TileCoord> for DdsTileCoord {
    fn from(tile: TileCoord) -> Self {
        Self {
            row: tile.row,
            col: tile.col,
            zoom: tile.zoom,
        }
    }
}

impl From<DdsTileCoord> for TileCoord {
    fn from(dds: DdsTileCoord) -> Self {
        Self {
            row: dds.row,
            col: dds.col,
            zoom: dds.zoom,
        }
    }
}

impl std::fmt::Display for DdsTileCoord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display without map type (we don't store it)
        write!(f, "{}_{}@ZL{}", self.row, self.col, self.zoom)
    }
}

/// A 1°×1° geographic region (derived from tile coordinates, not stored).
///
/// Regions are identified by the floor of latitude and longitude.
/// For example, a position at (53.5, 9.7) falls in region (53, 9).
///
/// This type is derived from `DdsTileCoord`, never stored directly.
/// Use `DdsTileCoord::to_geo_region()` to calculate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GeoRegion {
    /// Floor of latitude (e.g., 53 for 53.5°N)
    pub lat: i32,
    /// Floor of longitude (e.g., 9 for 9.7°E)
    pub lon: i32,
}

impl GeoRegion {
    /// Create a new geographic region.
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    /// Create a region from floating-point coordinates.
    ///
    /// The region is determined by flooring both coordinates.
    pub fn from_lat_lon(lat: f64, lon: f64) -> Self {
        Self {
            lat: lat.floor() as i32,
            lon: lon.floor() as i32,
        }
    }

    /// Get the center point of this 1°×1° region.
    pub fn center(&self) -> (f64, f64) {
        (self.lat as f64 + 0.5, self.lon as f64 + 0.5)
    }
}

impl std::fmt::Display for GeoRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lat_hem = if self.lat >= 0 { 'N' } else { 'S' };
        let lon_hem = if self.lon >= 0 { 'E' } else { 'W' };
        write!(
            f,
            "{}{:02}{}{}",
            lat_hem,
            self.lat.abs(),
            lon_hem,
            self.lon.abs()
        )
    }
}

/// Geographic bounding box (derived from tile coordinates).
///
/// Represents the minimum bounding rectangle containing all requested tiles.
#[derive(Debug, Clone, Copy)]
pub struct GeoBounds {
    /// Minimum (southernmost) latitude
    pub min_lat: f64,
    /// Maximum (northernmost) latitude
    pub max_lat: f64,
    /// Minimum (westernmost) longitude
    pub min_lon: f64,
    /// Maximum (easternmost) longitude
    pub max_lon: f64,
}

impl GeoBounds {
    /// Create a new bounding box.
    pub fn new(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> Self {
        Self {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        }
    }

    /// Create a bounding box from a single point.
    pub fn from_point(lat: f64, lon: f64) -> Self {
        Self {
            min_lat: lat,
            max_lat: lat,
            min_lon: lon,
            max_lon: lon,
        }
    }

    /// Expand this bounding box to include a point.
    pub fn expand(&mut self, lat: f64, lon: f64) {
        self.min_lat = self.min_lat.min(lat);
        self.max_lat = self.max_lat.max(lat);
        self.min_lon = self.min_lon.min(lon);
        self.max_lon = self.max_lon.max(lon);
    }

    /// Get the center point of the bounds.
    ///
    /// This is used by the APT module to derive aircraft position
    /// when telemetry is unavailable.
    pub fn center(&self) -> (f64, f64) {
        (
            (self.min_lat + self.max_lat) / 2.0,
            (self.min_lon + self.max_lon) / 2.0,
        )
    }

    /// Get the width of the bounds in degrees.
    pub fn width(&self) -> f64 {
        self.max_lon - self.min_lon
    }

    /// Get the height of the bounds in degrees.
    pub fn height(&self) -> f64 {
        self.max_lat - self.min_lat
    }
}

/// Scene loading state - empirical model of X-Plane's requests.
///
/// This struct maintains the factual record of what X-Plane has requested.
/// All geographic interpretations should be derived from this data.
#[derive(Debug, Clone)]
pub struct SceneLoadingState {
    /// DDS tiles X-Plane has requested this session (empirical data).
    pub requested_tiles: HashSet<DdsTileCoord>,

    /// Timestamp of last tile access.
    pub last_activity: Instant,

    /// Total tile requests this session.
    pub total_requests: u64,
}

impl Default for SceneLoadingState {
    fn default() -> Self {
        Self {
            requested_tiles: HashSet::new(),
            last_activity: Instant::now(),
            total_requests: 0,
        }
    }
}

impl SceneLoadingState {
    /// Create a new empty scene loading state.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Event sent from FUSE to Scene Tracker (empirical data).
///
/// This represents a DDS file access that FUSE has processed.
/// The Scene Tracker receives these events to build its model.
#[derive(Debug, Clone)]
pub struct FuseAccessEvent {
    /// The DDS tile coordinates (parsed from filename).
    pub tile: DdsTileCoord,
    /// Timestamp of access.
    pub timestamp: Instant,
}

impl FuseAccessEvent {
    /// Create a new FUSE access event.
    pub fn new(tile: DdsTileCoord) -> Self {
        Self {
            tile,
            timestamp: Instant::now(),
        }
    }

    /// Create a new event with a specific timestamp.
    pub fn with_timestamp(tile: DdsTileCoord, timestamp: Instant) -> Self {
        Self { tile, timestamp }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod dds_tile_coord {
        use super::*;

        #[test]
        fn test_from_dds_filename_bing() {
            // Bing Maps format: row_col_BI{zoom}.dds
            let coord = DdsTileCoord::from_dds_filename("100000_125184_BI18.dds").unwrap();
            assert_eq!(coord.row, 100000);
            assert_eq!(coord.col, 125184);
            assert_eq!(coord.zoom, 18);
        }

        #[test]
        fn test_from_dds_filename_google_go2() {
            // Google GO2 format: row_col_GO2{zoom}.dds
            let coord = DdsTileCoord::from_dds_filename("25264_10368_GO216.dds").unwrap();
            assert_eq!(coord.row, 25264);
            assert_eq!(coord.col, 10368);
            assert_eq!(coord.zoom, 16);
        }

        #[test]
        fn test_from_dds_filename_zoom_14() {
            let coord = DdsTileCoord::from_dds_filename("5236_8652_BI14.dds").unwrap();
            assert_eq!(coord.row, 5236);
            assert_eq!(coord.col, 8652);
            assert_eq!(coord.zoom, 14);
        }

        #[test]
        fn test_from_dds_filename_with_path() {
            // Should work with path prefix
            let coord = DdsTileCoord::from_dds_filename("/path/to/textures/100000_125184_BI18.dds")
                .unwrap();
            assert_eq!(coord.row, 100000);
            assert_eq!(coord.col, 125184);
            assert_eq!(coord.zoom, 18);
        }

        #[test]
        fn test_from_dds_filename_invalid_format() {
            assert!(DdsTileCoord::from_dds_filename("invalid").is_none());
            assert!(DdsTileCoord::from_dds_filename("1309_2163.dds").is_none());
            assert!(DdsTileCoord::from_dds_filename("1309_2163_14.dds").is_none());
            assert!(DdsTileCoord::from_dds_filename("abc_def_BI14.dds").is_none());
        }

        #[test]
        fn test_display() {
            let coord = DdsTileCoord::new(100000, 125184, 18);
            // Display format shows row_col@ZL{zoom} (without map type)
            assert_eq!(format!("{}", coord), "100000_125184@ZL18");
        }

        #[test]
        fn test_to_lat_lon() {
            // Northern Europe tile at ZL14
            // Tile (5236, 8652, 14) is approximately 54.3°N, 10.0°E
            let coord = DdsTileCoord::new(5236, 8652, 14);
            let (lat, lon) = coord.to_lat_lon();

            // Northern Germany/Denmark area
            assert!(lat > 54.0 && lat < 55.0, "Expected lat ~54.3, got {}", lat);
            assert!(lon > 9.0 && lon < 11.0, "Expected lon ~10, got {}", lon);
        }

        #[test]
        fn test_to_geo_region() {
            let coord = DdsTileCoord::new(5236, 8652, 14);
            let region = coord.to_geo_region();

            // Should be in region 54N, 9E or 10E (northern Germany/Denmark area)
            assert_eq!(region.lat, 54, "Expected lat region 54");
            assert!(
                region.lon == 9 || region.lon == 10,
                "Expected lon region 9 or 10"
            );
        }

        #[test]
        fn test_hash_and_eq() {
            let coord1 = DdsTileCoord::new(100000, 125184, 18);
            let coord2 = DdsTileCoord::new(100000, 125184, 18);
            let coord3 = DdsTileCoord::new(100001, 125184, 18);

            assert_eq!(coord1, coord2);
            assert_ne!(coord1, coord3);

            let mut set = HashSet::new();
            set.insert(coord1);
            assert!(set.contains(&coord2));
            assert!(!set.contains(&coord3));
        }

        #[test]
        fn test_to_tile_coord_conversion() {
            let dds = DdsTileCoord::new(100000, 125184, 18);
            let tile: TileCoord = dds.into();
            assert_eq!(tile.row, 100000);
            assert_eq!(tile.col, 125184);
            assert_eq!(tile.zoom, 18);
        }

        #[test]
        fn test_from_tile_coord_conversion() {
            let tile = TileCoord {
                row: 100000,
                col: 125184,
                zoom: 18,
            };
            let dds: DdsTileCoord = tile.into();
            assert_eq!(dds.row, 100000);
            assert_eq!(dds.col, 125184);
            assert_eq!(dds.zoom, 18);
        }
    }

    mod geo_region {
        use super::*;

        #[test]
        fn test_from_lat_lon() {
            let region = GeoRegion::from_lat_lon(53.5, 9.7);
            assert_eq!(region.lat, 53);
            assert_eq!(region.lon, 9);
        }

        #[test]
        fn test_from_lat_lon_negative() {
            let region = GeoRegion::from_lat_lon(-33.9, -70.6); // Santiago, Chile
            assert_eq!(region.lat, -34);
            assert_eq!(region.lon, -71);
        }

        #[test]
        fn test_center() {
            let region = GeoRegion::new(53, 9);
            let (lat, lon) = region.center();
            assert!((lat - 53.5).abs() < 0.0001);
            assert!((lon - 9.5).abs() < 0.0001);
        }

        #[test]
        fn test_display_positive() {
            let region = GeoRegion::new(53, 9);
            assert_eq!(format!("{}", region), "N53E9");
        }

        #[test]
        fn test_display_negative() {
            let region = GeoRegion::new(-34, -71);
            assert_eq!(format!("{}", region), "S34W71");
        }

        #[test]
        fn test_hash_and_eq() {
            let region1 = GeoRegion::new(53, 9);
            let region2 = GeoRegion::new(53, 9);
            let region3 = GeoRegion::new(53, 10);

            assert_eq!(region1, region2);
            assert_ne!(region1, region3);

            let mut set = HashSet::new();
            set.insert(region1);
            assert!(set.contains(&region2));
            assert!(!set.contains(&region3));
        }
    }

    mod geo_bounds {
        use super::*;

        #[test]
        fn test_from_point() {
            let bounds = GeoBounds::from_point(53.5, 9.7);
            assert!((bounds.center().0 - 53.5).abs() < 0.0001);
            assert!((bounds.center().1 - 9.7).abs() < 0.0001);
        }

        #[test]
        fn test_expand() {
            let mut bounds = GeoBounds::from_point(53.5, 9.7);
            bounds.expand(54.0, 10.5);

            assert!((bounds.min_lat - 53.5).abs() < 0.0001);
            assert!((bounds.max_lat - 54.0).abs() < 0.0001);
            assert!((bounds.min_lon - 9.7).abs() < 0.0001);
            assert!((bounds.max_lon - 10.5).abs() < 0.0001);
        }

        #[test]
        fn test_center() {
            let bounds = GeoBounds::new(53.0, 54.0, 9.0, 11.0);
            let (lat, lon) = bounds.center();
            assert!((lat - 53.5).abs() < 0.0001);
            assert!((lon - 10.0).abs() < 0.0001);
        }

        #[test]
        fn test_width_and_height() {
            let bounds = GeoBounds::new(53.0, 54.0, 9.0, 11.0);
            assert!((bounds.width() - 2.0).abs() < 0.0001);
            assert!((bounds.height() - 1.0).abs() < 0.0001);
        }
    }

}
