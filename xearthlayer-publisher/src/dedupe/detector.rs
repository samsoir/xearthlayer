//! Overlap detection for zoom level tiles.
//!
//! Scans package terrain directories and detects overlapping tiles
//! across different zoom levels.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use tracing::{debug, trace};

use super::types::{
    CoverageGap, DedupeError, DedupeFilter, GapAnalysisResult, MissingTile, OverlapCoverage,
    TileReference, ZoomOverlap,
};

/// Detects overlapping zoom level tiles in scenery packages.
#[derive(Debug, Default)]
pub struct OverlapDetector {
    /// Optional filter for targeted operations.
    filter: DedupeFilter,
}

impl OverlapDetector {
    /// Create a new overlap detector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a detector with a filter for targeted operations.
    pub fn with_filter(filter: DedupeFilter) -> Self {
        Self { filter }
    }

    /// Scan a package directory for tile references.
    ///
    /// Parses all `.ter` files in the package's `terrain/` subdirectory.
    pub fn scan_package(&self, package_path: &Path) -> Result<Vec<TileReference>, DedupeError> {
        let terrain_path = package_path.join("terrain");
        if !terrain_path.exists() {
            return Err(DedupeError::TerrainDirNotFound(terrain_path));
        }

        debug!(
            terrain_path = %terrain_path.display(),
            "Scanning terrain directory for tiles"
        );

        let mut tiles = Vec::new();
        let entries =
            fs::read_dir(&terrain_path).map_err(|e| DedupeError::IoError(e.to_string()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ter") {
                match self.parse_ter_file(&path) {
                    Ok(tile) => {
                        if self.filter.matches(&tile) {
                            tiles.push(tile);
                        }
                    }
                    Err(e) => {
                        trace!(path = %path.display(), error = %e, "Failed to parse .ter file");
                    }
                }
            }
        }

        debug!(tiles = tiles.len(), "Scanned tiles from package");
        Ok(tiles)
    }

    /// Detect all overlaps in a collection of tiles.
    ///
    /// Returns a list of `ZoomOverlap` structs describing each overlap.
    pub fn detect_overlaps(&self, tiles: &[TileReference]) -> Vec<ZoomOverlap> {
        let mut overlaps = Vec::new();

        // Group tiles by zoom level
        let by_zoom: HashMap<u8, Vec<&TileReference>> =
            tiles.iter().fold(HashMap::new(), |mut acc, tile| {
                acc.entry(tile.zoom).or_default().push(tile);
                acc
            });

        // Get sorted zoom levels (highest first)
        let mut zoom_levels: Vec<u8> = by_zoom.keys().copied().collect();
        zoom_levels.sort_by(|a, b| b.cmp(a));

        debug!(
            zoom_levels = ?zoom_levels,
            "Detecting overlaps across zoom levels"
        );

        // For each higher ZL, check against all lower ZLs
        for (i, &high_zl) in zoom_levels.iter().enumerate() {
            for &low_zl in &zoom_levels[i + 1..] {
                // Skip if zoom difference is odd (tiles don't align)
                if (high_zl - low_zl) % 2 != 0 {
                    continue;
                }

                let detected = self.detect_between_levels(&by_zoom, high_zl, low_zl);
                debug!(
                    high_zl = high_zl,
                    low_zl = low_zl,
                    overlaps = detected.len(),
                    "Detected overlaps between zoom levels"
                );
                overlaps.extend(detected);
            }
        }

        overlaps
    }

    /// Analyze gaps in coverage where higher ZL tiles partially overlap lower ZL tiles.
    ///
    /// Returns a `GapAnalysisResult` containing all gaps and missing tiles needed
    /// to complete coverage.
    pub fn analyze_gaps(&self, tiles: &[TileReference]) -> GapAnalysisResult {
        // Get sorted zoom levels
        let zoom_levels_present = {
            let mut levels: Vec<u8> = tiles
                .iter()
                .map(|t| t.zoom)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            levels.sort();
            levels
        };

        let mut result = GapAnalysisResult {
            tiles_analyzed: tiles.len(),
            zoom_levels_present,
            tile_filter: self.filter.tile.map(|c| (c.lat, c.lon)),
            ..Default::default()
        };

        // Group tiles by zoom level
        let by_zoom: HashMap<u8, Vec<&TileReference>> =
            tiles.iter().fold(HashMap::new(), |mut acc, tile| {
                acc.entry(tile.zoom).or_default().push(tile);
                acc
            });

        // Get sorted zoom levels (highest first)
        let mut zoom_levels: Vec<u8> = by_zoom.keys().copied().collect();
        zoom_levels.sort_by(|a, b| b.cmp(a));

        // For each pair of adjacent zoom levels, find gaps
        for (i, &high_zl) in zoom_levels.iter().enumerate() {
            for &low_zl in &zoom_levels[i + 1..] {
                // Only process even differences (tiles align)
                if (high_zl - low_zl) % 2 != 0 {
                    continue;
                }

                let gaps = self.find_gaps_between_levels(&by_zoom, high_zl, low_zl);
                result.gaps.extend(gaps);
            }
        }

        // Calculate total missing tiles
        result.total_missing_tiles = result.gaps.iter().map(|g| g.missing_count()).sum();

        debug!(
            gaps = result.gaps.len(),
            missing_tiles = result.total_missing_tiles,
            "Gap analysis complete"
        );

        result
    }

    /// Find gaps between two specific zoom levels.
    fn find_gaps_between_levels(
        &self,
        by_zoom: &HashMap<u8, Vec<&TileReference>>,
        high_zl: u8,
        low_zl: u8,
    ) -> Vec<CoverageGap> {
        let Some(high_tiles) = by_zoom.get(&high_zl) else {
            return Vec::new();
        };
        let Some(low_tiles) = by_zoom.get(&low_zl) else {
            return Vec::new();
        };

        // Build lookup for high ZL tiles: (row, col) → &tile
        let high_lookup: HashMap<(u32, u32), &TileReference> =
            high_tiles.iter().map(|t| ((t.row, t.col), *t)).collect();

        // Calculate children per parent
        let zl_diff = high_zl - low_zl;
        let scale = 4u32.pow((zl_diff / 2) as u32);
        let expected_children = scale * scale;

        let mut gaps = Vec::new();

        for low in low_tiles {
            let child_row_start = low.row * scale;
            let child_col_start = low.col * scale;

            let mut existing_children: Vec<TileReference> = Vec::new();
            let mut missing_tiles: Vec<MissingTile> = Vec::new();

            for row_offset in 0..scale {
                for col_offset in 0..scale {
                    let child_row = child_row_start + row_offset;
                    let child_col = child_col_start + col_offset;

                    if let Some(child) = high_lookup.get(&(child_row, child_col)) {
                        existing_children.push((*child).clone());
                    } else {
                        // Calculate approximate lat/lon for the missing tile
                        let (lat, lon) = self.estimate_tile_center(
                            child_row, child_col, high_zl, low.lat, low.lon, scale,
                        );
                        missing_tiles.push(MissingTile {
                            row: child_row,
                            col: child_col,
                            zoom: high_zl,
                            lat,
                            lon,
                        });
                    }
                }
            }

            // Only report gaps where there's partial coverage (some but not all children)
            if !existing_children.is_empty() && !missing_tiles.is_empty() {
                gaps.push(CoverageGap {
                    parent: (*low).clone(),
                    existing_children,
                    missing_tiles,
                    expected_count: expected_children,
                });
            }
        }

        gaps
    }

    /// Estimate the center coordinates for a tile based on parent and offset.
    fn estimate_tile_center(
        &self,
        child_row: u32,
        child_col: u32,
        _high_zl: u8,
        parent_lat: f32,
        parent_lon: f32,
        scale: u32,
    ) -> (f32, f32) {
        // The parent's lat/lon is approximately the center of its coverage area.
        // Children are distributed in a grid within that area.
        // This is an approximation - actual coordinates depend on the projection.

        // Calculate which child position this is relative to parent's first child
        let parent_first_row = (child_row / scale) * scale;
        let parent_first_col = (child_col / scale) * scale;
        let row_offset = child_row - parent_first_row;
        let col_offset = child_col - parent_first_col;

        // Estimate offset from parent center
        // Tile size varies by latitude, but this gives a reasonable approximation
        let tile_fraction = 1.0 / scale as f32;
        let lat_offset = (row_offset as f32 - scale as f32 / 2.0 + 0.5) * tile_fraction * 0.5;
        let lon_offset = (col_offset as f32 - scale as f32 / 2.0 + 0.5) * tile_fraction * 0.5;

        // Note: latitude decreases as row increases in Web Mercator
        (parent_lat - lat_offset, parent_lon + lon_offset)
    }

    /// Detect overlaps between two specific zoom levels.
    ///
    /// This uses a "parent-centric" approach: for each low ZL tile, we check
    /// if ALL of its children at the high ZL exist. This ensures we only
    /// mark overlaps as Complete when removing the low ZL tile won't create gaps.
    fn detect_between_levels(
        &self,
        by_zoom: &HashMap<u8, Vec<&TileReference>>,
        high_zl: u8,
        low_zl: u8,
    ) -> Vec<ZoomOverlap> {
        let Some(high_tiles) = by_zoom.get(&high_zl) else {
            return Vec::new();
        };
        let Some(low_tiles) = by_zoom.get(&low_zl) else {
            return Vec::new();
        };

        // Build lookup for high ZL tiles: (row, col) → &tile
        let high_lookup: HashMap<(u32, u32), &TileReference> =
            high_tiles.iter().map(|t| ((t.row, t.col), *t)).collect();

        // Calculate how many children each low ZL tile should have
        let zl_diff = high_zl - low_zl;
        let scale = 4u32.pow((zl_diff / 2) as u32); // 4 for 2 levels, 16 for 4 levels
        let expected_children = scale * scale; // 16 for 2 levels, 256 for 4 levels

        let mut overlaps = Vec::new();

        // For each low ZL tile, check how many high ZL children exist
        for low in low_tiles {
            // Calculate the range of child coordinates
            let child_row_start = low.row * scale;
            let child_col_start = low.col * scale;

            // Collect all existing children
            let mut existing_children: Vec<TileReference> = Vec::new();

            for row_offset in 0..scale {
                for col_offset in 0..scale {
                    let child_row = child_row_start + row_offset;
                    let child_col = child_col_start + col_offset;
                    if let Some(child) = high_lookup.get(&(child_row, child_col)) {
                        existing_children.push((*child).clone());
                    }
                }
            }

            // Only create an overlap if at least one child exists
            if !existing_children.is_empty() {
                let coverage = if existing_children.len() as u32 == expected_children {
                    OverlapCoverage::Complete
                } else {
                    OverlapCoverage::Partial
                };

                trace!(
                    low_row = low.row,
                    low_col = low.col,
                    low_zl = low_zl,
                    high_zl = high_zl,
                    existing = existing_children.len(),
                    expected = expected_children,
                    coverage = ?coverage,
                    "Detected overlap"
                );

                overlaps.push(ZoomOverlap {
                    higher_zl: existing_children[0].clone(), // First child as representative
                    lower_zl: (*low).clone(),
                    all_higher_zl: existing_children,
                    zl_diff,
                    coverage,
                });
            }
        }

        overlaps
    }

    /// Parse a .ter file to extract tile information.
    ///
    /// Expected format:
    /// ```text
    /// A
    /// 800
    /// TERRAIN
    ///
    /// LOAD_CENTER <lat> <lon> <elevation> <size>
    /// BASE_TEX_NOWRAP ../textures/<row>_<col>_<provider><zoom>.dds
    /// NO_ALPHA
    /// ```
    fn parse_ter_file(&self, path: &Path) -> Result<TileReference, DedupeError> {
        let file = File::open(path).map_err(|e| DedupeError::IoError(e.to_string()))?;
        let reader = BufReader::new(file);

        let mut lat: Option<f32> = None;
        let mut lon: Option<f32> = None;
        let mut row: Option<u32> = None;
        let mut col: Option<u32> = None;
        let mut zoom: Option<u8> = None;
        let mut provider: Option<String> = None;

        // Check filename for sea indicator
        let is_sea = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.contains("_sea"));

        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim();

            if line.starts_with("LOAD_CENTER") {
                // Parse: LOAD_CENTER <lat> <lon> <elevation> <size>
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    lat = parts[1].parse().ok();
                    lon = parts[2].parse().ok();
                }
            } else if line.starts_with("BASE_TEX_NOWRAP") || line.starts_with("BASE_TEX") {
                // Parse: BASE_TEX_NOWRAP ../textures/<row>_<col>_<provider><zoom>.dds
                if let Some(dds_part) = line.split('/').next_back() {
                    if let Some((r, c, z, p)) = parse_dds_filename(dds_part) {
                        row = Some(r);
                        col = Some(c);
                        zoom = Some(z);
                        provider = Some(p);
                    }
                }
            }
        }

        // Validate we got all required fields
        let lat = lat.ok_or_else(|| DedupeError::ParseError("Missing LOAD_CENTER".to_string()))?;
        let lon = lon.ok_or_else(|| DedupeError::ParseError("Missing LOAD_CENTER".to_string()))?;
        let row = row.ok_or_else(|| DedupeError::ParseError("Missing BASE_TEX".to_string()))?;
        let col = col.ok_or_else(|| DedupeError::ParseError("Missing BASE_TEX".to_string()))?;
        let zoom = zoom.ok_or_else(|| DedupeError::ParseError("Missing zoom level".to_string()))?;
        let provider =
            provider.ok_or_else(|| DedupeError::ParseError("Missing provider".to_string()))?;

        Ok(TileReference {
            row,
            col,
            zoom,
            provider,
            lat,
            lon,
            ter_path: path.to_path_buf(),
            is_sea,
        })
    }
}

/// Parse a DDS filename to extract row, col, zoom, and provider.
///
/// Format: `<row>_<col>_<provider><zoom>.dds`
/// Examples:
/// - `94800_47888_BI18.dds` → (94800, 47888, 18, "BI")
/// - `25264_10368_GO216.dds` → (25264, 10368, 16, "GO2")
fn parse_dds_filename(filename: &str) -> Option<(u32, u32, u8, String)> {
    // Remove .dds extension
    let name = filename.strip_suffix(".dds")?;

    // Split by underscore
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() < 3 {
        return None;
    }

    // Parse row and col
    let row: u32 = parts[0].parse().ok()?;
    let col: u32 = parts[1].parse().ok()?;

    // Parse provider+zoom (e.g., "BI18", "GO216")
    let provider_zoom = parts[2];

    // Extract zoom from the last 2 characters (zoom levels are 12-20, always 2 digits)
    // Provider is everything before the last 2 characters
    if provider_zoom.len() < 3 {
        return None;
    }
    let zoom_str = &provider_zoom[provider_zoom.len() - 2..];
    let zoom: u8 = zoom_str.parse().ok()?;
    let provider = provider_zoom[..provider_zoom.len() - 2].to_uppercase();

    Some((row, col, zoom, provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_ter_file(
        dir: &Path,
        name: &str,
        lat: f32,
        lon: f32,
        row: u32,
        col: u32,
        zoom: u8,
    ) {
        let content = format!(
            "A\n800\nTERRAIN\n\nLOAD_CENTER {} {} 1000 4096\nBASE_TEX_NOWRAP ../textures/{}_{}_{}{}.dds\nNO_ALPHA\n",
            lat, lon, row, col, "BI", zoom
        );
        let path = dir.join(name);
        let mut file = File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_parse_dds_filename() {
        assert_eq!(
            parse_dds_filename("94800_47888_BI18.dds"),
            Some((94800, 47888, 18, "BI".to_string()))
        );
        assert_eq!(
            parse_dds_filename("25264_10368_GO216.dds"),
            Some((25264, 10368, 16, "GO2".to_string()))
        );
        assert_eq!(
            parse_dds_filename("100000_125184_GO18.dds"),
            Some((100000, 125184, 18, "GO".to_string()))
        );
        assert_eq!(parse_dds_filename("invalid.dds"), None);
        assert_eq!(parse_dds_filename("no_extension"), None);
    }

    #[test]
    fn test_scan_package() {
        let temp = TempDir::new().unwrap();
        let terrain_dir = temp.path().join("terrain");
        fs::create_dir_all(&terrain_dir).unwrap();

        // Create test .ter files
        create_test_ter_file(
            &terrain_dir,
            "100032_42688_BI18.ter",
            39.15,
            -121.36,
            100032,
            42688,
            18,
        );
        create_test_ter_file(
            &terrain_dir,
            "25008_10672_BI16.ter",
            39.13,
            -121.33,
            25008,
            10672,
            16,
        );

        let detector = OverlapDetector::new();
        let tiles = detector.scan_package(temp.path()).unwrap();

        assert_eq!(tiles.len(), 2);
    }

    #[test]
    fn test_detect_overlaps() {
        // Create tiles that overlap
        let tiles = vec![
            TileReference {
                row: 100032,
                col: 42688,
                zoom: 18,
                provider: "BI".to_string(),
                lat: 39.15,
                lon: -121.36,
                ter_path: PathBuf::from("100032_42688_BI18.ter"),
                is_sea: false,
            },
            TileReference {
                row: 25008, // 100032 / 4
                col: 10672, // 42688 / 4
                zoom: 16,
                provider: "BI".to_string(),
                lat: 39.13,
                lon: -121.33,
                ter_path: PathBuf::from("25008_10672_BI16.ter"),
                is_sea: false,
            },
        ];

        let detector = OverlapDetector::new();
        let overlaps = detector.detect_overlaps(&tiles);

        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].higher_zl.zoom, 18);
        assert_eq!(overlaps[0].lower_zl.zoom, 16);
        assert_eq!(overlaps[0].zl_diff, 2);
    }

    #[test]
    fn test_detect_no_overlaps() {
        // Create tiles that don't overlap
        let tiles = vec![
            TileReference {
                row: 100032,
                col: 42688,
                zoom: 18,
                provider: "BI".to_string(),
                lat: 39.15,
                lon: -121.36,
                ter_path: PathBuf::from("100032_42688_BI18.ter"),
                is_sea: false,
            },
            TileReference {
                row: 25009, // Not a parent of 100032 (100032 / 4 = 25008)
                col: 10672,
                zoom: 16,
                provider: "BI".to_string(),
                lat: 39.2,
                lon: -121.33,
                ter_path: PathBuf::from("25009_10672_BI16.ter"),
                is_sea: false,
            },
        ];

        let detector = OverlapDetector::new();
        let overlaps = detector.detect_overlaps(&tiles);

        assert!(overlaps.is_empty());
    }

    #[test]
    fn test_detect_multi_level_overlaps() {
        // Create tiles at ZL18, ZL16, and ZL14 that all overlap
        let tiles = vec![
            TileReference {
                row: 100032,
                col: 42688,
                zoom: 18,
                provider: "BI".to_string(),
                lat: 39.15,
                lon: -121.36,
                ter_path: PathBuf::from("100032_42688_BI18.ter"),
                is_sea: false,
            },
            TileReference {
                row: 25008, // 100032 / 4
                col: 10672, // 42688 / 4
                zoom: 16,
                provider: "BI".to_string(),
                lat: 39.13,
                lon: -121.33,
                ter_path: PathBuf::from("25008_10672_BI16.ter"),
                is_sea: false,
            },
            TileReference {
                row: 6252, // 25008 / 4 = 6252
                col: 2668, // 10672 / 4 = 2668
                zoom: 14,
                provider: "BI".to_string(),
                lat: 39.1,
                lon: -121.3,
                ter_path: PathBuf::from("6252_2668_BI14.ter"),
                is_sea: false,
            },
        ];

        let detector = OverlapDetector::new();
        let overlaps = detector.detect_overlaps(&tiles);

        // Should detect: ZL18→ZL16, ZL18→ZL14, ZL16→ZL14
        assert_eq!(overlaps.len(), 3);
    }
}
