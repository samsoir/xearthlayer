//! Core types for zoom level overlap detection and deduplication.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

/// Minimum supported zoom level for overlap detection.
pub const MIN_ZOOM_LEVEL: u8 = 12;

/// Maximum supported zoom level for overlap detection.
pub const MAX_ZOOM_LEVEL: u8 = 19;

/// Reference to a terrain tile parsed from a `.ter` file.
#[derive(Debug, Clone, PartialEq)]
pub struct TileReference {
    /// Tile row coordinate (from DDS filename).
    pub row: u32,
    /// Tile column coordinate (from DDS filename).
    pub col: u32,
    /// Chunk zoom level (from DDS filename, e.g., 16 or 18).
    pub zoom: u8,
    /// Imagery provider code (e.g., "BI" for Bing, "GO2" for Google Go2).
    pub provider: String,
    /// Geographic center latitude from LOAD_CENTER.
    pub lat: f32,
    /// Geographic center longitude from LOAD_CENTER.
    pub lon: f32,
    /// Path to the `.ter` file.
    pub ter_path: PathBuf,
    /// Whether this is a sea/water tile.
    pub is_sea: bool,
}

impl TileReference {
    /// Calculate parent tile coordinates at a lower zoom level.
    ///
    /// Returns `None` if:
    /// - `target_zl` is >= current zoom
    /// - The zoom difference is odd (tiles don't align)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tile = TileReference { zoom: 18, row: 100032, col: 42688, .. };
    /// assert_eq!(tile.parent_at(16), Some((25008, 10672)));
    /// assert_eq!(tile.parent_at(14), Some((6252, 2668)));
    /// ```
    pub fn parent_at(&self, target_zl: u8) -> Option<(u32, u32)> {
        if target_zl >= self.zoom {
            return None; // Target must be lower zoom level
        }
        let zl_diff = self.zoom - target_zl;
        if !zl_diff.is_multiple_of(2) {
            return None; // Zoom levels must align (even difference)
        }
        let scale = 4u32.pow((zl_diff / 2) as u32);
        Some((self.row / scale, self.col / scale))
    }

    /// Check if this tile falls within a 1°×1° geographic cell.
    ///
    /// Used for targeted tile operations.
    pub fn in_cell(&self, lat: i32, lon: i32) -> bool {
        let tile_lat = self.lat.floor() as i32;
        let tile_lon = self.lon.floor() as i32;
        tile_lat == lat && tile_lon == lon
    }
}

/// A detected overlap between two tiles at different zoom levels.
#[derive(Debug, Clone)]
pub struct ZoomOverlap {
    /// Higher resolution tile (representative, for backward compatibility).
    pub higher_zl: TileReference,
    /// Lower resolution tile (parent).
    pub lower_zl: TileReference,
    /// All higher resolution tiles that overlap this parent.
    /// When coverage is Complete, this contains all children (e.g., 16 for ZL18→ZL16).
    /// When coverage is Partial, this contains only the existing children.
    pub all_higher_zl: Vec<TileReference>,
    /// Zoom level difference (e.g., 2 for ZL18→ZL16).
    pub zl_diff: u8,
    /// Whether the overlap is complete or partial.
    pub coverage: OverlapCoverage,
}

/// Type of overlap coverage between two tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapCoverage {
    /// Higher ZL tile completely covers the lower ZL tile's area.
    Complete,
    /// Higher ZL tile only partially covers the lower ZL tile's area.
    Partial,
}

/// Priority strategy for resolving overlaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZoomPriority {
    /// Keep highest zoom level, remove all lower (best visual quality).
    #[default]
    Highest,
    /// Keep lowest zoom level, remove all higher (smallest package size).
    Lowest,
    /// Keep tiles at the specified zoom level, remove others.
    Specific(u8),
}

impl fmt::Display for ZoomPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Highest => write!(f, "highest"),
            Self::Lowest => write!(f, "lowest"),
            Self::Specific(zl) => write!(f, "zl{}", zl),
        }
    }
}

impl ZoomPriority {
    /// Parse a priority string (e.g., "highest", "lowest", "zl16").
    pub fn parse(s: &str) -> Result<Self, DedupeError> {
        match s.to_lowercase().as_str() {
            "highest" => Ok(Self::Highest),
            "lowest" => Ok(Self::Lowest),
            s if s.starts_with("zl") => {
                let zl: u8 = s[2..]
                    .parse()
                    .map_err(|_| DedupeError::InvalidPriority(s.to_string()))?;
                if !(MIN_ZOOM_LEVEL..=MAX_ZOOM_LEVEL).contains(&zl) {
                    return Err(DedupeError::InvalidPriority(format!(
                        "zoom level must be between {} and {}, got {}",
                        MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL, zl
                    )));
                }
                Ok(Self::Specific(zl))
            }
            _ => Err(DedupeError::InvalidPriority(s.to_string())),
        }
    }
}

/// Filter options for deduplication operations.
#[derive(Debug, Clone, Default)]
pub struct DedupeFilter {
    /// Limit operation to tiles within this 1°×1° cell (lat, lon).
    pub tile: Option<TileCoord>,
}

impl DedupeFilter {
    /// Create a filter for a specific tile.
    pub fn for_tile(lat: i32, lon: i32) -> Self {
        Self {
            tile: Some(TileCoord { lat, lon }),
        }
    }

    /// Check if a tile reference passes this filter.
    pub fn matches(&self, tile: &TileReference) -> bool {
        match &self.tile {
            Some(coord) => tile.in_cell(coord.lat, coord.lon),
            None => true, // No filter, all tiles match
        }
    }
}

/// 1°×1° tile coordinate (matches X-Plane DSF tile boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileCoord {
    /// Latitude (integer degrees, -90 to 89).
    pub lat: i32,
    /// Longitude (integer degrees, -180 to 179).
    pub lon: i32,
}

impl TileCoord {
    /// Format as X-Plane tile name (e.g., "+37-118").
    pub fn to_xplane_name(&self) -> String {
        format!("{:+03}{:+04}", self.lat, self.lon)
    }

    /// Parse from "lat,lon" string format.
    pub fn parse(s: &str) -> Result<Self, DedupeError> {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() != 2 {
            return Err(DedupeError::InvalidTileCoord(
                "expected format: lat,lon (e.g., 37,-118)".to_string(),
            ));
        }
        let lat: i32 = parts[0].trim().parse().map_err(|_| {
            DedupeError::InvalidTileCoord(format!("invalid latitude: {}", parts[0]))
        })?;
        let lon: i32 = parts[1].trim().parse().map_err(|_| {
            DedupeError::InvalidTileCoord(format!("invalid longitude: {}", parts[1]))
        })?;

        if !(-90..=89).contains(&lat) {
            return Err(DedupeError::InvalidTileCoord(format!(
                "latitude must be between -90 and 89, got {}",
                lat
            )));
        }
        if !(-180..=179).contains(&lon) {
            return Err(DedupeError::InvalidTileCoord(format!(
                "longitude must be between -180 and 179, got {}",
                lon
            )));
        }

        Ok(Self { lat, lon })
    }
}

impl fmt::Display for TileCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.lat, self.lon)
    }
}

/// Result of a deduplication operation.
#[derive(Debug, Default)]
pub struct DedupeResult {
    /// Total tiles analyzed.
    pub tiles_analyzed: usize,
    /// Zoom levels present in the package.
    pub zoom_levels_present: Vec<u8>,
    /// Overlap counts by (higher_zl, lower_zl) pair.
    pub overlaps_by_pair: HashMap<(u8, u8), usize>,
    /// Tiles that were (or would be) removed.
    pub tiles_removed: Vec<TileReference>,
    /// Tiles that were preserved.
    pub tiles_preserved: Vec<TileReference>,
    /// Partial overlaps where no action was taken.
    pub partial_overlaps: Vec<ZoomOverlap>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// A missing tile needed to complete coverage.
#[derive(Debug, Clone)]
pub struct MissingTile {
    /// Row coordinate at the higher zoom level.
    pub row: u32,
    /// Column coordinate at the higher zoom level.
    pub col: u32,
    /// Zoom level of the missing tile.
    pub zoom: u8,
    /// Approximate latitude (center of tile).
    pub lat: f32,
    /// Approximate longitude (center of tile).
    pub lon: f32,
}

/// A gap in coverage where a lower ZL tile has incomplete higher ZL coverage.
#[derive(Debug, Clone)]
pub struct CoverageGap {
    /// The parent tile that has incomplete coverage.
    pub parent: TileReference,
    /// Existing higher ZL children.
    pub existing_children: Vec<TileReference>,
    /// Missing higher ZL tiles needed to complete coverage.
    pub missing_tiles: Vec<MissingTile>,
    /// Number of expected children (e.g., 16 for ZL16→ZL18).
    pub expected_count: u32,
}

impl CoverageGap {
    /// Calculate the percentage of coverage.
    pub fn coverage_percent(&self) -> f32 {
        (self.existing_children.len() as f32 / self.expected_count as f32) * 100.0
    }

    /// Get the number of missing tiles.
    pub fn missing_count(&self) -> usize {
        self.missing_tiles.len()
    }
}

/// Result of gap analysis.
#[derive(Debug, Default, Clone)]
pub struct GapAnalysisResult {
    /// Total tiles analyzed.
    pub tiles_analyzed: usize,
    /// Zoom levels present in the package.
    pub zoom_levels_present: Vec<u8>,
    /// Gaps found (partial overlaps that need filling).
    pub gaps: Vec<CoverageGap>,
    /// Total missing tiles across all gaps.
    pub total_missing_tiles: usize,
    /// Filter applied (if any).
    pub tile_filter: Option<(i32, i32)>,
}

impl DedupeResult {
    /// Get the total number of overlaps detected.
    pub fn total_overlaps(&self) -> usize {
        self.overlaps_by_pair.values().sum()
    }

    /// Check if any overlaps were detected.
    pub fn has_overlaps(&self) -> bool {
        self.total_overlaps() > 0
    }
}

/// Errors that can occur during deduplication.
#[derive(Debug, Clone)]
pub enum DedupeError {
    /// Invalid priority string.
    InvalidPriority(String),
    /// Invalid tile coordinate.
    InvalidTileCoord(String),
    /// IO error reading files.
    IoError(String),
    /// Failed to parse .ter file.
    ParseError(String),
    /// Package directory not found.
    PackageNotFound(PathBuf),
    /// Terrain directory not found.
    TerrainDirNotFound(PathBuf),
}

impl fmt::Display for DedupeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPriority(s) => write!(f, "invalid priority: {}", s),
            Self::InvalidTileCoord(s) => write!(f, "invalid tile coordinate: {}", s),
            Self::IoError(s) => write!(f, "IO error: {}", s),
            Self::ParseError(s) => write!(f, "parse error: {}", s),
            Self::PackageNotFound(p) => write!(f, "package not found: {}", p.display()),
            Self::TerrainDirNotFound(p) => {
                write!(f, "terrain directory not found: {}", p.display())
            }
        }
    }
}

impl std::error::Error for DedupeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_reference_parent_at() {
        let tile = TileReference {
            row: 100032,
            col: 42688,
            zoom: 18,
            provider: "BI".to_string(),
            lat: 39.15,
            lon: -121.36,
            ter_path: PathBuf::from("test.ter"),
            is_sea: false,
        };

        // ZL18 → ZL16 (scale 4)
        assert_eq!(tile.parent_at(16), Some((25008, 10672)));

        // ZL18 → ZL14 (scale 16)
        assert_eq!(tile.parent_at(14), Some((6252, 2668)));

        // ZL18 → ZL12 (scale 64)
        assert_eq!(tile.parent_at(12), Some((1563, 667)));

        // Invalid: target >= current
        assert_eq!(tile.parent_at(18), None);
        assert_eq!(tile.parent_at(20), None);

        // Invalid: odd difference
        assert_eq!(tile.parent_at(17), None);
        assert_eq!(tile.parent_at(15), None);
    }

    #[test]
    fn test_tile_reference_in_cell() {
        let tile = TileReference {
            row: 100032,
            col: 42688,
            zoom: 18,
            provider: "BI".to_string(),
            lat: 39.15,
            lon: -121.36,
            ter_path: PathBuf::from("test.ter"),
            is_sea: false,
        };

        assert!(tile.in_cell(39, -122));
        assert!(!tile.in_cell(38, -122));
        assert!(!tile.in_cell(39, -121));
    }

    #[test]
    fn test_zoom_priority_parse() {
        assert_eq!(
            ZoomPriority::parse("highest").unwrap(),
            ZoomPriority::Highest
        );
        assert_eq!(
            ZoomPriority::parse("HIGHEST").unwrap(),
            ZoomPriority::Highest
        );
        assert_eq!(ZoomPriority::parse("lowest").unwrap(), ZoomPriority::Lowest);
        assert_eq!(
            ZoomPriority::parse("zl16").unwrap(),
            ZoomPriority::Specific(16)
        );
        assert_eq!(
            ZoomPriority::parse("ZL18").unwrap(),
            ZoomPriority::Specific(18)
        );

        assert!(ZoomPriority::parse("invalid").is_err());
        assert!(ZoomPriority::parse("zl5").is_err()); // Below MIN_ZOOM_LEVEL
        assert!(ZoomPriority::parse("zl25").is_err()); // Above MAX_ZOOM_LEVEL
    }

    #[test]
    fn test_zoom_priority_display() {
        assert_eq!(ZoomPriority::Highest.to_string(), "highest");
        assert_eq!(ZoomPriority::Lowest.to_string(), "lowest");
        assert_eq!(ZoomPriority::Specific(16).to_string(), "zl16");
    }

    #[test]
    fn test_tile_coord_parse() {
        let coord = TileCoord::parse("37,-118").unwrap();
        assert_eq!(coord.lat, 37);
        assert_eq!(coord.lon, -118);

        let coord = TileCoord::parse("-33, 151").unwrap();
        assert_eq!(coord.lat, -33);
        assert_eq!(coord.lon, 151);

        assert!(TileCoord::parse("invalid").is_err());
        assert!(TileCoord::parse("37").is_err());
        assert!(TileCoord::parse("91,-118").is_err()); // Invalid lat
        assert!(TileCoord::parse("37,180").is_err()); // Invalid lon
    }

    #[test]
    fn test_tile_coord_to_xplane_name() {
        assert_eq!(TileCoord { lat: 37, lon: -118 }.to_xplane_name(), "+37-118");
        assert_eq!(TileCoord { lat: -33, lon: 151 }.to_xplane_name(), "-33+151");
        assert_eq!(TileCoord { lat: 0, lon: 0 }.to_xplane_name(), "+00+000");
    }

    #[test]
    fn test_dedupe_filter_matches() {
        let tile = TileReference {
            row: 100032,
            col: 42688,
            zoom: 18,
            provider: "BI".to_string(),
            lat: 39.15,
            lon: -121.36,
            ter_path: PathBuf::from("test.ter"),
            is_sea: false,
        };

        // No filter - all tiles match
        let filter = DedupeFilter::default();
        assert!(filter.matches(&tile));

        // Matching tile filter
        let filter = DedupeFilter::for_tile(39, -122);
        assert!(filter.matches(&tile));

        // Non-matching tile filter
        let filter = DedupeFilter::for_tile(40, -122);
        assert!(!filter.matches(&tile));
    }

    #[test]
    fn test_dedupe_result_total_overlaps() {
        let mut result = DedupeResult::default();
        assert_eq!(result.total_overlaps(), 0);
        assert!(!result.has_overlaps());

        result.overlaps_by_pair.insert((18, 16), 100);
        result.overlaps_by_pair.insert((18, 14), 25);
        result.overlaps_by_pair.insert((17, 16), 50);

        assert_eq!(result.total_overlaps(), 175);
        assert!(result.has_overlaps());
    }
}
