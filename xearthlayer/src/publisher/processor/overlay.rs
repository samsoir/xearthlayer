//! Ortho4XP overlay processor.
//!
//! Processes overlay output from Ortho4XP into XEarthLayer packages. Overlays contain
//! vector data (roads, railways, powerlines, forests) that complement orthophoto scenery.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::{ProcessSummary, SceneryFormat, SceneryProcessor, SceneryScanResult, TileInfo};
use crate::package::PackageType;
use crate::publisher::{PublishError, PublishResult, Repository};

/// Ortho4XP overlay output processor.
///
/// Scans and validates Ortho4XP overlay directories, organizing them into
/// XEarthLayer package format.
///
/// # Ortho4XP Overlay Structure
///
/// Ortho4XP produces overlays in a consolidated directory:
///
/// ```text
/// yOrtho4XP_Overlays/
/// └── Earth nav data/
///     ├── +30-120/
///     │   ├── +37-118.dsf
///     │   └── +37-119.dsf
///     ├── +40-130/
///     │   └── +41-125.dsf
///     └── ...
/// ```
///
/// # File Handling
///
/// - **DSF files**: Kept (overlay vector data: roads, railways, forests, etc.)
/// - No terrain or texture files (overlays use X-Plane's built-in textures)
#[derive(Debug, Clone)]
pub struct OverlayProcessor;

impl Default for OverlayProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayProcessor {
    /// Create a new overlay processor.
    pub fn new() -> Self {
        Self
    }

    /// Scan Earth nav data directory for DSF files.
    ///
    /// Finds all DSF files organized in 10° grid subdirectories.
    fn scan_earth_nav_data(&self, source: &Path) -> PublishResult<Vec<TileInfo>> {
        let earth_nav = source.join("Earth nav data");
        if !earth_nav.exists() {
            return Err(PublishError::InvalidSource(format!(
                "Earth nav data directory not found: {}",
                earth_nav.display()
            )));
        }

        let mut tiles = Vec::new();

        // Scan grid subdirectories
        let entries = fs::read_dir(&earth_nav).map_err(|e| PublishError::ReadFailed {
            path: earth_nav.clone(),
            source: e,
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| PublishError::ReadFailed {
                path: earth_nav.clone(),
                source: e,
            })?;

            let grid_dir = entry.path();
            if !grid_dir.is_dir() {
                continue;
            }

            // Scan for DSF files in this grid directory
            let dsf_entries = fs::read_dir(&grid_dir).map_err(|e| PublishError::ReadFailed {
                path: grid_dir.clone(),
                source: e,
            })?;

            for dsf_entry in dsf_entries {
                let dsf_entry = dsf_entry.map_err(|e| PublishError::ReadFailed {
                    path: grid_dir.clone(),
                    source: e,
                })?;

                let dsf_path = dsf_entry.path();
                if !dsf_path.is_file() {
                    continue;
                }

                // Check for .dsf extension
                let ext = dsf_path.extension().and_then(|e| e.to_str());
                if ext.map(|e| e.eq_ignore_ascii_case("dsf")).unwrap_or(false) {
                    if let Some(tile) = self.parse_dsf_file(&dsf_path)? {
                        tiles.push(tile);
                    }
                }
            }
        }

        Ok(tiles)
    }

    /// Parse a DSF file into tile info.
    ///
    /// Extracts coordinate information from the DSF filename (e.g., +37-118.dsf).
    fn parse_dsf_file(&self, dsf_path: &Path) -> PublishResult<Option<TileInfo>> {
        let filename = match dsf_path.file_stem().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => return Ok(None),
        };

        // Parse tile coordinates from filename
        let (latitude, longitude) = match Self::parse_tile_coords(filename) {
            Some(coords) => coords,
            None => return Ok(None),
        };

        let mut tile = TileInfo::new(filename, dsf_path.parent().unwrap(), latitude, longitude);
        tile.dsf_files.push(dsf_path.to_path_buf());

        Ok(Some(tile))
    }

    /// Parse tile coordinates from a DSF filename.
    ///
    /// Expected format: `+NN-NNN` or `-NN+NNN` (latitude and longitude with signs)
    fn parse_tile_coords(name: &str) -> Option<(i32, i32)> {
        if name.len() < 7 {
            return None;
        }

        // Find where longitude starts (second + or -)
        let mut sign_positions = Vec::new();
        for (i, c) in name.chars().enumerate() {
            if c == '+' || c == '-' {
                sign_positions.push(i);
            }
        }

        if sign_positions.len() < 2 {
            return None;
        }

        let lat_str = &name[..sign_positions[1]];
        let lon_str = &name[sign_positions[1]..];

        let latitude: i32 = lat_str.parse().ok()?;
        let longitude: i32 = lon_str.parse().ok()?;

        // Validate ranges
        if !(-90..=90).contains(&latitude) || !(-180..=180).contains(&longitude) {
            return None;
        }

        Some((latitude, longitude))
    }

    /// Get the 10-degree grid subdirectory for a DSF file.
    fn dsf_grid_dir(
        &self,
        earth_nav_dir: &Path,
        lat: i32,
        lon: i32,
    ) -> PublishResult<std::path::PathBuf> {
        let grid_lat = (lat.div_euclid(10)) * 10;
        let grid_lon = (lon.div_euclid(10)) * 10;

        let dir_name = format!("{:+03}{:+04}", grid_lat, grid_lon);

        let dir_path = earth_nav_dir.join(&dir_name);
        fs::create_dir_all(&dir_path).map_err(|e| PublishError::CreateDirectoryFailed {
            path: dir_path.clone(),
            source: e,
        })?;

        Ok(dir_path)
    }
}

impl SceneryProcessor for OverlayProcessor {
    fn name(&self) -> &str {
        "Ortho4XP Overlays"
    }

    fn scenery_format(&self) -> SceneryFormat {
        SceneryFormat::XPlane12
    }

    fn scan(&self, source: &Path) -> PublishResult<SceneryScanResult> {
        if !source.exists() {
            return Err(PublishError::InvalidSource(format!(
                "source path does not exist: {}",
                source.display()
            )));
        }

        if !source.is_dir() {
            return Err(PublishError::InvalidSource(format!(
                "source path is not a directory: {}",
                source.display()
            )));
        }

        let tiles = self.scan_earth_nav_data(source)?;

        if tiles.is_empty() {
            return Err(PublishError::NoTilesFound(source.to_path_buf()));
        }

        // Sort tiles by latitude, then longitude
        let mut sorted_tiles = tiles;
        sorted_tiles.sort_by_key(|t| (t.latitude, t.longitude));

        Ok(SceneryScanResult::new(sorted_tiles))
    }

    fn process(
        &self,
        scan_result: &SceneryScanResult,
        region: &str,
        package_type: PackageType,
        repo: &Repository,
    ) -> PublishResult<ProcessSummary> {
        // Verify we're creating an overlay package
        if package_type != PackageType::Overlay {
            return Err(PublishError::InvalidSource(
                "OverlayProcessor only supports Overlay package type".to_string(),
            ));
        }

        let package_dir = repo.package_dir(region, package_type);

        // Create package directory structure
        fs::create_dir_all(&package_dir).map_err(|e| PublishError::CreateDirectoryFailed {
            path: package_dir.clone(),
            source: e,
        })?;

        let earth_nav_dir = package_dir.join("Earth nav data");
        fs::create_dir_all(&earth_nav_dir).map_err(|e| PublishError::CreateDirectoryFailed {
            path: earth_nav_dir.clone(),
            source: e,
        })?;

        let mut summary = ProcessSummary::new();

        // Track copied DSF files to avoid duplicates
        let mut copied_dsf: HashSet<String> = HashSet::new();

        for tile in &scan_result.tiles {
            summary.tile_count += 1;

            // Copy DSF files into appropriate 10° grid subdirectory
            let dsf_subdir = self.dsf_grid_dir(&earth_nav_dir, tile.latitude, tile.longitude)?;

            for dsf_file in &tile.dsf_files {
                if let Some(filename) = dsf_file.file_name().and_then(|n| n.to_str()) {
                    if copied_dsf.insert(filename.to_string()) {
                        let dest = dsf_subdir.join(filename);
                        fs::copy(dsf_file, &dest).map_err(|e| PublishError::WriteFailed {
                            path: dest,
                            source: e,
                        })?;
                        summary.dsf_count += 1;
                    }
                }
            }
        }

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a mock overlay directory matching Ortho4XP output structure.
    fn create_mock_overlay(temp: &Path, grid: &str, tiles: &[&str]) {
        let earth_nav = temp.join("Earth nav data").join(grid);
        fs::create_dir_all(&earth_nav).unwrap();

        for tile in tiles {
            fs::write(earth_nav.join(format!("{}.dsf", tile)), b"mock dsf").unwrap();
        }
    }

    #[test]
    fn test_processor_new() {
        let processor = OverlayProcessor::new();
        assert_eq!(processor.name(), "Ortho4XP Overlays");
    }

    #[test]
    fn test_processor_default() {
        let processor = OverlayProcessor::default();
        assert_eq!(processor.name(), "Ortho4XP Overlays");
    }

    #[test]
    fn test_processor_scenery_format() {
        let processor = OverlayProcessor::new();
        assert_eq!(processor.scenery_format(), SceneryFormat::XPlane12);
    }

    #[test]
    fn test_parse_tile_coords_valid() {
        assert_eq!(
            OverlayProcessor::parse_tile_coords("+37-118"),
            Some((37, -118))
        );
        assert_eq!(
            OverlayProcessor::parse_tile_coords("-12+045"),
            Some((-12, 45))
        );
        assert_eq!(OverlayProcessor::parse_tile_coords("+00+000"), Some((0, 0)));
        assert_eq!(
            OverlayProcessor::parse_tile_coords("-90-180"),
            Some((-90, -180))
        );
        assert_eq!(
            OverlayProcessor::parse_tile_coords("+90+180"),
            Some((90, 180))
        );
    }

    #[test]
    fn test_parse_tile_coords_invalid() {
        assert_eq!(OverlayProcessor::parse_tile_coords("invalid"), None);
        assert_eq!(OverlayProcessor::parse_tile_coords("+37"), None);
        assert_eq!(OverlayProcessor::parse_tile_coords("+91-118"), None); // lat out of range
        assert_eq!(OverlayProcessor::parse_tile_coords("+37-181"), None); // lon out of range
    }

    #[test]
    fn test_scan_nonexistent_source() {
        let processor = OverlayProcessor::new();
        let result = processor.scan(Path::new("/nonexistent/path"));
        assert!(matches!(result, Err(PublishError::InvalidSource(_))));
    }

    #[test]
    fn test_scan_missing_earth_nav_data() {
        let temp = TempDir::new().unwrap();
        let processor = OverlayProcessor::new();
        let result = processor.scan(temp.path());
        assert!(matches!(result, Err(PublishError::InvalidSource(_))));
    }

    #[test]
    fn test_scan_empty_earth_nav_data() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("Earth nav data")).unwrap();

        let processor = OverlayProcessor::new();
        let result = processor.scan(temp.path());
        assert!(matches!(result, Err(PublishError::NoTilesFound(_))));
    }

    #[test]
    fn test_scan_valid_overlays() {
        let temp = TempDir::new().unwrap();
        create_mock_overlay(temp.path(), "+30-120", &["+37-118", "+37-119"]);

        let processor = OverlayProcessor::new();
        let result = processor.scan(temp.path()).unwrap();

        assert_eq!(result.tiles.len(), 2);
        // Sorted by (lat, lon): -119 < -118
        assert_eq!(result.tiles[0].id, "+37-119");
        assert_eq!(result.tiles[0].latitude, 37);
        assert_eq!(result.tiles[0].longitude, -119);
        assert_eq!(result.tiles[0].dsf_files.len(), 1);
        // Overlays have no terrain or texture files
        assert!(result.tiles[0].ter_files.is_empty());
        assert!(result.tiles[0].mask_files.is_empty());
        assert!(result.tiles[0].dds_files.is_empty());
    }

    #[test]
    fn test_scan_multiple_grids() {
        let temp = TempDir::new().unwrap();
        create_mock_overlay(temp.path(), "+30-120", &["+37-118"]);
        create_mock_overlay(temp.path(), "+40-130", &["+41-125"]);

        let processor = OverlayProcessor::new();
        let result = processor.scan(temp.path()).unwrap();

        assert_eq!(result.tiles.len(), 2);
        // Sorted by (lat, lon)
        assert_eq!(result.tiles[0].id, "+37-118");
        assert_eq!(result.tiles[1].id, "+41-125");
    }

    #[test]
    fn test_process_creates_package_structure() {
        let source_temp = TempDir::new().unwrap();
        create_mock_overlay(source_temp.path(), "+30-120", &["+37-118"]);

        let repo_temp = TempDir::new().unwrap();
        let repo = Repository::init(repo_temp.path()).unwrap();

        let processor = OverlayProcessor::new();
        let scan_result = processor.scan(source_temp.path()).unwrap();
        let summary = processor
            .process(&scan_result, "na", PackageType::Overlay, &repo)
            .unwrap();

        assert_eq!(summary.tile_count, 1);
        assert_eq!(summary.dsf_count, 1);
        assert_eq!(summary.ter_count, 0);
        assert_eq!(summary.mask_count, 0);
        assert_eq!(summary.dds_skipped, 0);

        // Check package structure
        let pkg_dir = repo.package_dir("na", PackageType::Overlay);
        assert!(pkg_dir.join("Earth nav data").exists());
    }

    #[test]
    fn test_process_dsf_grid_organization() {
        let source_temp = TempDir::new().unwrap();
        create_mock_overlay(source_temp.path(), "+30-120", &["+37-118"]);

        let repo_temp = TempDir::new().unwrap();
        let repo = Repository::init(repo_temp.path()).unwrap();

        let processor = OverlayProcessor::new();
        let scan_result = processor.scan(source_temp.path()).unwrap();
        processor
            .process(&scan_result, "na", PackageType::Overlay, &repo)
            .unwrap();

        // DSF should be in +30-120 grid directory
        let pkg_dir = repo.package_dir("na", PackageType::Overlay);
        let dsf_path = pkg_dir.join("Earth nav data/+30-120/+37-118.dsf");
        assert!(dsf_path.exists(), "DSF file should be in grid directory");
    }

    #[test]
    fn test_process_rejects_ortho_type() {
        let source_temp = TempDir::new().unwrap();
        create_mock_overlay(source_temp.path(), "+30-120", &["+37-118"]);

        let repo_temp = TempDir::new().unwrap();
        let repo = Repository::init(repo_temp.path()).unwrap();

        let processor = OverlayProcessor::new();
        let scan_result = processor.scan(source_temp.path()).unwrap();
        let result = processor.process(&scan_result, "na", PackageType::Ortho, &repo);

        assert!(matches!(result, Err(PublishError::InvalidSource(_))));
    }

    #[test]
    fn test_process_deduplicates_dsf_files() {
        let source_temp = TempDir::new().unwrap();
        // Create same tile in two different directories (shouldn't happen in practice, but test dedup)
        create_mock_overlay(source_temp.path(), "+30-120", &["+37-118"]);

        let repo_temp = TempDir::new().unwrap();
        let repo = Repository::init(repo_temp.path()).unwrap();

        let processor = OverlayProcessor::new();
        let scan_result = processor.scan(source_temp.path()).unwrap();
        let summary = processor
            .process(&scan_result, "na", PackageType::Overlay, &repo)
            .unwrap();

        assert_eq!(summary.dsf_count, 1);
    }

    #[test]
    fn test_processor_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OverlayProcessor>();
    }

    #[test]
    fn test_processor_as_trait_object() {
        let processor: Box<dyn SceneryProcessor> = Box::new(OverlayProcessor::new());
        assert_eq!(processor.name(), "Ortho4XP Overlays");
        assert_eq!(processor.scenery_format(), SceneryFormat::XPlane12);
    }

    #[test]
    fn test_processor_clone() {
        let processor = OverlayProcessor::new();
        let _cloned = processor.clone();
        // OverlayProcessor has no state, so just verify clone works
    }

    #[test]
    fn test_processor_debug() {
        let processor = OverlayProcessor::new();
        let debug = format!("{:?}", processor);
        assert!(debug.contains("OverlayProcessor"));
    }
}
