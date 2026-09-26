//! Tile types for X-Plane 12 scenery processing.
//!
//! These types represent the structure of X-Plane 12 scenery tiles,
//! which consist of DSF terrain mesh, terrain definitions, and textures.

use std::path::PathBuf;

/// Information about a single X-Plane 12 scenery tile.
///
/// A tile corresponds to a 1×1 degree geographic area and contains:
/// - DSF files with terrain mesh data
/// - Terrain (.ter) files defining surface properties
/// - Texture files (masks for water/shorelines, DDS textures)
#[derive(Debug, Clone)]
pub struct TileInfo {
    /// Tile identifier (e.g., "+37-118").
    ///
    /// This follows X-Plane's naming convention where the sign and digits
    /// represent the southwest corner of the tile in latitude/longitude.
    pub id: String,

    /// Path to the source tile directory.
    pub path: PathBuf,

    /// Latitude of the tile's southwest corner.
    pub latitude: i32,

    /// Longitude of the tile's southwest corner.
    pub longitude: i32,

    /// DSF files in this tile.
    ///
    /// DSF (Distribution Scenery Format) files contain the terrain mesh,
    /// object placements, and other scenery data.
    pub dsf_files: Vec<PathBuf>,

    /// Terrain definition files (.ter) in this tile.
    ///
    /// These files define how terrain textures are applied and
    /// reference the texture files to use.
    pub ter_files: Vec<PathBuf>,

    /// Mask files (water/sea masks) in this tile.
    ///
    /// PNG files that define water boundaries and shorelines.
    /// These are kept because they're needed for proper water rendering.
    pub mask_files: Vec<PathBuf>,

    /// DDS texture files in this tile.
    ///
    /// These are typically skipped during processing because XEarthLayer
    /// generates them on-demand from satellite imagery.
    pub dds_files: Vec<PathBuf>,
}

impl TileInfo {
    /// Create a new tile info with the given ID and coordinates.
    pub fn new(
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        latitude: i32,
        longitude: i32,
    ) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            latitude,
            longitude,
            dsf_files: Vec::new(),
            ter_files: Vec::new(),
            mask_files: Vec::new(),
            dds_files: Vec::new(),
        }
    }

    /// Returns the total number of files in this tile (excluding DDS).
    pub fn file_count(&self) -> usize {
        self.dsf_files.len() + self.ter_files.len() + self.mask_files.len()
    }

    /// Returns the number of DDS files that will be skipped.
    pub fn dds_count(&self) -> usize {
        self.dds_files.len()
    }
}

/// Summary of processed tiles.
#[derive(Debug, Clone, Default)]
pub struct ProcessSummary {
    /// Total number of tiles processed.
    pub tile_count: usize,

    /// Total DSF files copied.
    pub dsf_count: usize,

    /// Sum of DSF sizes as read from the source.
    pub dsf_raw_bytes: u64,

    /// Sum of DSF sizes as written to the package (7z containers).
    pub dsf_stored_bytes: u64,

    /// DSF files that were already 7z at the source and were copied unchanged.
    pub dsf_precompressed: usize,

    /// Total terrain (.ter) files copied.
    pub ter_count: usize,

    /// Total mask files copied.
    pub mask_count: usize,

    /// Total DDS files that were skipped.
    pub dds_skipped: usize,

    /// Warnings generated during processing.
    pub warnings: Vec<TileWarning>,
}

impl ProcessSummary {
    /// Create a new empty summary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if any warnings were generated.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Add a warning to the summary.
    pub fn add_warning(&mut self, tile_id: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(TileWarning {
            tile_id: tile_id.into(),
            message: message.into(),
        });
    }
}

/// Warning about a tile during processing.
///
/// Warnings indicate non-fatal issues that don't prevent processing
/// but may indicate problems with the source data.
#[derive(Debug, Clone)]
pub struct TileWarning {
    /// Tile identifier (e.g., "+37-118").
    pub tile_id: String,

    /// Warning message describing the issue.
    pub message: String,
}

impl TileWarning {
    /// Create a new tile warning.
    pub fn new(tile_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tile_id: tile_id.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_info_new() {
        let tile = TileInfo::new("+37-118", "/test/path", 37, -118);
        assert_eq!(tile.id, "+37-118");
        assert_eq!(tile.latitude, 37);
        assert_eq!(tile.longitude, -118);
        assert!(tile.dsf_files.is_empty());
    }

    #[test]
    fn test_tile_info_file_count() {
        let mut tile = TileInfo::new("+37-118", "/test", 37, -118);
        tile.dsf_files.push(PathBuf::from("test.dsf"));
        tile.ter_files.push(PathBuf::from("test.ter"));
        tile.ter_files.push(PathBuf::from("test2.ter"));
        tile.mask_files.push(PathBuf::from("test_sea.png"));

        assert_eq!(tile.file_count(), 4);
    }

    #[test]
    fn test_tile_info_dds_count() {
        let mut tile = TileInfo::new("+37-118", "/test", 37, -118);
        tile.dds_files.push(PathBuf::from("test.dds"));
        tile.dds_files.push(PathBuf::from("test2.dds"));

        assert_eq!(tile.dds_count(), 2);
    }

    #[test]
    fn test_tile_info_clone() {
        let tile = TileInfo::new("+37-118", "/test", 37, -118);
        let cloned = tile.clone();
        assert_eq!(tile.id, cloned.id);
    }

    #[test]
    fn test_tile_info_debug() {
        let tile = TileInfo::new("+37-118", "/test", 37, -118);
        let debug = format!("{:?}", tile);
        assert!(debug.contains("+37-118"));
    }

    #[test]
    fn test_process_summary_new() {
        let summary = ProcessSummary::new();
        assert_eq!(summary.tile_count, 0);
        assert_eq!(summary.dsf_count, 0);
        assert!(!summary.has_warnings());
    }

    #[test]
    fn test_process_summary_default() {
        let summary = ProcessSummary::default();
        assert_eq!(summary.tile_count, 0);
    }

    #[test]
    fn test_process_summary_add_warning() {
        let mut summary = ProcessSummary::new();
        summary.add_warning("+37-118", "missing terrain files");

        assert!(summary.has_warnings());
        assert_eq!(summary.warnings.len(), 1);
        assert_eq!(summary.warnings[0].tile_id, "+37-118");
    }

    #[test]
    fn test_tile_warning_new() {
        let warning = TileWarning::new("+37-118", "test message");
        assert_eq!(warning.tile_id, "+37-118");
        assert_eq!(warning.message, "test message");
    }

    #[test]
    fn test_tile_warning_clone() {
        let warning = TileWarning::new("+37-118", "test");
        let cloned = warning.clone();
        assert_eq!(warning.tile_id, cloned.tile_id);
    }

    #[test]
    fn test_tile_warning_debug() {
        let warning = TileWarning::new("+37-118", "test");
        let debug = format!("{:?}", warning);
        assert!(debug.contains("+37-118"));
        assert!(debug.contains("test"));
    }
}
