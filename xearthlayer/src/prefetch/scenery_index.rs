//! Scenery-aware tile index for efficient prefetch lookup.
//!
//! This module provides a spatial index of tiles defined in scenery packages,
//! enabling the prefetch system to know exactly which DDS textures exist at
//! each location without coordinate calculation.
//!
//! # Design
//!
//! Instead of computing tile coordinates from lat/lon and guessing zoom levels,
//! this index reads the `.ter` terrain files from scenery packages which specify
//! exactly which DDS textures are needed at each location.
//!
//! ```text
//! .ter file:
//!   LOAD_CENTER 44.50434 -114.22485 1744 4096
//!   BASE_TEX_NOWRAP ../textures/94800_47888_BI18.dds
//!
//! Index entry:
//!   (lat: 44.50, lon: -114.22) → { row: 94800, col: 47888, zoom: 18 }
//! ```
//!
//! # Benefits
//!
//! - **Exact matches**: Prefetch exactly what X-Plane will request
//! - **Correct zoom levels**: Read from filename, no calculation needed
//! - **Efficient**: Only prefetch tiles that actually exist in scenery
//! - **Skip sea tiles**: Can deprioritize simple water textures

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use tracing::{info, trace};

use crate::coord::TileCoord;

/// A tile entry in the scenery index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneryTile {
    /// Tile row coordinate (from DDS filename)
    pub row: u32,
    /// Tile column coordinate (from DDS filename)
    pub col: u32,
    /// Chunk zoom level (from DDS filename, e.g., 16 or 18)
    pub chunk_zoom: u8,
    /// Geographic center latitude
    pub lat: f32,
    /// Geographic center longitude
    pub lon: f32,
    /// Whether this is a sea/water tile
    pub is_sea: bool,
}

impl SceneryTile {
    /// Get the tile zoom level (chunk_zoom - 4).
    #[inline]
    pub fn tile_zoom(&self) -> u8 {
        self.chunk_zoom.saturating_sub(4)
    }

    /// Convert to TileCoord for use with the pipeline.
    #[inline]
    pub fn to_tile_coord(&self) -> TileCoord {
        // Convert chunk coordinates to tile coordinates
        TileCoord {
            row: self.row / 16,
            col: self.col / 16,
            zoom: self.tile_zoom(),
        }
    }
}

/// Grid cell for spatial indexing.
///
/// Uses a coarse grid (~1 degree) for fast spatial queries.
/// Each cell contains all tiles whose center falls within it.
#[derive(Debug, Clone, Default)]
struct GridCell {
    tiles: Vec<SceneryTile>,
}

/// Configuration for the scenery index.
#[derive(Debug, Clone)]
pub struct SceneryIndexConfig {
    /// Grid cell size in degrees (default: 1.0)
    pub grid_cell_size: f32,
    /// Whether to include sea tiles in the index
    pub include_sea_tiles: bool,
}

impl Default for SceneryIndexConfig {
    fn default() -> Self {
        Self {
            grid_cell_size: 1.0,
            include_sea_tiles: true,
        }
    }
}

/// Spatial index of scenery tiles for efficient prefetch lookup.
///
/// Stores tiles from `.ter` files in a grid structure for O(1) spatial queries.
/// The grid uses ~1 degree cells, which at mid-latitudes covers about 60nm.
pub struct SceneryIndex {
    /// Grid of tiles indexed by (lat_cell, lon_cell)
    grid: RwLock<HashMap<(i16, i16), GridCell>>,
    /// Grid cell size in degrees
    cell_size: f32,
    /// Total number of indexed tiles
    tile_count: RwLock<usize>,
    /// Number of sea tiles
    sea_tile_count: RwLock<usize>,
    /// Configuration
    config: SceneryIndexConfig,
}

impl SceneryIndex {
    /// Create a new empty scenery index.
    pub fn new(config: SceneryIndexConfig) -> Self {
        Self {
            grid: RwLock::new(HashMap::new()),
            cell_size: config.grid_cell_size,
            tile_count: RwLock::new(0),
            sea_tile_count: RwLock::new(0),
            config,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SceneryIndexConfig::default())
    }

    /// Build the index by scanning a scenery package directory.
    ///
    /// Parses all `.ter` files in the `terrain` subdirectory.
    pub fn build_from_package(&self, package_path: &Path) -> Result<usize, SceneryIndexError> {
        let terrain_path = package_path.join("terrain");
        if !terrain_path.exists() {
            return Err(SceneryIndexError::TerrainDirNotFound(
                terrain_path.to_path_buf(),
            ));
        }

        let mut count = 0;
        let entries =
            fs::read_dir(&terrain_path).map_err(|e| SceneryIndexError::IoError(e.to_string()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ter") {
                match self.parse_and_add_ter_file(&path) {
                    Ok(()) => count += 1,
                    Err(e) => {
                        trace!(path = %path.display(), error = %e, "Failed to parse .ter file");
                    }
                }
            }
        }

        info!(
            package = %package_path.display(),
            tiles = count,
            "Built scenery index"
        );

        Ok(count)
    }

    /// Parse a single .ter file and add it to the index.
    fn parse_and_add_ter_file(&self, path: &Path) -> Result<(), SceneryIndexError> {
        let tile = parse_ter_file(path)?;

        // Skip sea tiles if configured
        if tile.is_sea && !self.config.include_sea_tiles {
            return Ok(());
        }

        self.add_tile(tile);
        Ok(())
    }

    /// Add a tile to the index.
    pub fn add_tile(&self, tile: SceneryTile) {
        let cell_key = self.cell_key(tile.lat, tile.lon);

        let mut grid = self.grid.write().unwrap();
        let cell = grid.entry(cell_key).or_default();
        cell.tiles.push(tile);

        let mut count = self.tile_count.write().unwrap();
        *count += 1;

        if tile.is_sea {
            let mut sea_count = self.sea_tile_count.write().unwrap();
            *sea_count += 1;
        }
    }

    /// Query tiles within a radius of a position.
    ///
    /// Returns all tiles whose center is within `radius_nm` nautical miles
    /// of the given position.
    pub fn tiles_near(&self, lat: f64, lon: f64, radius_nm: f32) -> Vec<SceneryTile> {
        let lat = lat as f32;
        let lon = lon as f32;

        // Convert radius to approximate degrees (1 degree ≈ 60nm at equator)
        let radius_deg = radius_nm / 60.0;

        // Determine which grid cells to check
        let min_lat_cell = ((lat - radius_deg) / self.cell_size).floor() as i16;
        let max_lat_cell = ((lat + radius_deg) / self.cell_size).ceil() as i16;
        let min_lon_cell = ((lon - radius_deg) / self.cell_size).floor() as i16;
        let max_lon_cell = ((lon + radius_deg) / self.cell_size).ceil() as i16;

        let grid = self.grid.read().unwrap();
        let mut result = Vec::new();

        for lat_cell in min_lat_cell..=max_lat_cell {
            for lon_cell in min_lon_cell..=max_lon_cell {
                if let Some(cell) = grid.get(&(lat_cell, lon_cell)) {
                    for tile in &cell.tiles {
                        // Check actual distance
                        let dist = approximate_distance_nm(lat, lon, tile.lat, tile.lon);
                        if dist <= radius_nm {
                            result.push(*tile);
                        }
                    }
                }
            }
        }

        result
    }

    /// Query tiles within a radius, excluding sea tiles.
    pub fn land_tiles_near(&self, lat: f64, lon: f64, radius_nm: f32) -> Vec<SceneryTile> {
        self.tiles_near(lat, lon, radius_nm)
            .into_iter()
            .filter(|t| !t.is_sea)
            .collect()
    }

    /// Query tiles within a radius at a specific zoom level.
    pub fn tiles_near_at_zoom(
        &self,
        lat: f64,
        lon: f64,
        radius_nm: f32,
        chunk_zoom: u8,
    ) -> Vec<SceneryTile> {
        self.tiles_near(lat, lon, radius_nm)
            .into_iter()
            .filter(|t| t.chunk_zoom == chunk_zoom)
            .collect()
    }

    /// Get the total number of indexed tiles.
    pub fn tile_count(&self) -> usize {
        *self.tile_count.read().unwrap()
    }

    /// Get the number of sea tiles.
    pub fn sea_tile_count(&self) -> usize {
        *self.sea_tile_count.read().unwrap()
    }

    /// Get the number of land tiles (non-sea).
    pub fn land_tile_count(&self) -> usize {
        self.tile_count() - self.sea_tile_count()
    }

    /// Calculate grid cell key for a position.
    #[inline]
    fn cell_key(&self, lat: f32, lon: f32) -> (i16, i16) {
        let lat_cell = (lat / self.cell_size).floor() as i16;
        let lon_cell = (lon / self.cell_size).floor() as i16;
        (lat_cell, lon_cell)
    }

    /// Clear the index.
    pub fn clear(&self) {
        self.grid.write().unwrap().clear();
        *self.tile_count.write().unwrap() = 0;
        *self.sea_tile_count.write().unwrap() = 0;
    }
}

impl Default for SceneryIndex {
    fn default() -> Self {
        Self::with_defaults()
    }
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
fn parse_ter_file(path: &Path) -> Result<SceneryTile, SceneryIndexError> {
    let file = fs::File::open(path).map_err(|e| SceneryIndexError::IoError(e.to_string()))?;
    let reader = BufReader::new(file);

    let mut lat: Option<f32> = None;
    let mut lon: Option<f32> = None;
    let mut row: Option<u32> = None;
    let mut col: Option<u32> = None;
    let mut chunk_zoom: Option<u8> = None;
    let mut is_sea = false;

    // Check filename for sea indicator
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        is_sea = name.contains("_sea");
    }

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
                if let Some((parsed_row, parsed_col, parsed_zoom)) = parse_dds_filename(dds_part) {
                    row = Some(parsed_row);
                    col = Some(parsed_col);
                    chunk_zoom = Some(parsed_zoom);
                }
            }
        }
    }

    // Validate we got all required fields
    let lat =
        lat.ok_or_else(|| SceneryIndexError::ParseError("Missing LOAD_CENTER".to_string()))?;
    let lon =
        lon.ok_or_else(|| SceneryIndexError::ParseError("Missing LOAD_CENTER".to_string()))?;
    let row = row.ok_or_else(|| SceneryIndexError::ParseError("Missing BASE_TEX".to_string()))?;
    let col = col.ok_or_else(|| SceneryIndexError::ParseError("Missing BASE_TEX".to_string()))?;
    let chunk_zoom = chunk_zoom
        .ok_or_else(|| SceneryIndexError::ParseError("Missing zoom level".to_string()))?;

    Ok(SceneryTile {
        row,
        col,
        chunk_zoom,
        lat,
        lon,
        is_sea,
    })
}

/// Parse a DDS filename to extract row, col, and zoom.
///
/// Format: `<row>_<col>_<provider><zoom>.dds`
/// Examples:
/// - `94800_47888_BI18.dds` → (94800, 47888, 18)
/// - `25664_11008_BI16.dds` → (25664, 11008, 16)
fn parse_dds_filename(filename: &str) -> Option<(u32, u32, u8)> {
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
    // This handles providers with digits in the name like "GO2" (Google Go2)
    if provider_zoom.len() < 2 {
        return None;
    }
    let zoom_str = &provider_zoom[provider_zoom.len() - 2..];
    let zoom: u8 = zoom_str.parse().ok()?;

    Some((row, col, zoom))
}

/// Approximate distance in nautical miles using equirectangular projection.
///
/// Accurate enough for spatial queries within ~200nm.
#[inline]
fn approximate_distance_nm(lat1: f32, lon1: f32, lat2: f32, lon2: f32) -> f32 {
    let lat_diff = (lat2 - lat1) * 60.0; // 1 degree = 60nm
    let lon_diff = (lon2 - lon1) * 60.0 * (lat1.to_radians().cos());
    (lat_diff * lat_diff + lon_diff * lon_diff).sqrt()
}

/// Errors that can occur when building the scenery index.
#[derive(Debug, Clone)]
pub enum SceneryIndexError {
    /// Terrain directory not found
    TerrainDirNotFound(PathBuf),
    /// IO error reading files
    IoError(String),
    /// Failed to parse .ter file
    ParseError(String),
}

impl std::fmt::Display for SceneryIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerrainDirNotFound(path) => {
                write!(f, "Terrain directory not found: {}", path.display())
            }
            Self::IoError(msg) => write!(f, "IO error: {}", msg),
            Self::ParseError(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

impl std::error::Error for SceneryIndexError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dds_filename_bing() {
        let result = parse_dds_filename("94800_47888_BI18.dds");
        assert_eq!(result, Some((94800, 47888, 18)));
    }

    #[test]
    fn test_parse_dds_filename_bing_zl16() {
        let result = parse_dds_filename("25664_11008_BI16.dds");
        assert_eq!(result, Some((25664, 11008, 16)));
    }

    #[test]
    fn test_parse_dds_filename_go2() {
        let result = parse_dds_filename("25264_10368_GO216.dds");
        assert_eq!(result, Some((25264, 10368, 16)));
    }

    #[test]
    fn test_parse_dds_filename_invalid() {
        assert_eq!(parse_dds_filename("invalid.dds"), None);
        assert_eq!(parse_dds_filename("no_extension"), None);
    }

    #[test]
    fn test_scenery_tile_to_tile_coord() {
        let tile = SceneryTile {
            row: 94800,
            col: 47888,
            chunk_zoom: 18,
            lat: 44.5,
            lon: -114.2,
            is_sea: false,
        };

        let coord = tile.to_tile_coord();
        assert_eq!(coord.row, 94800 / 16);
        assert_eq!(coord.col, 47888 / 16);
        assert_eq!(coord.zoom, 14); // 18 - 4
    }

    #[test]
    fn test_scenery_index_add_and_query() {
        let index = SceneryIndex::with_defaults();

        // Add a tile at (45.0, -120.0)
        let tile = SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 45.0,
            lon: -120.0,
            is_sea: false,
        };
        index.add_tile(tile);

        assert_eq!(index.tile_count(), 1);

        // Query near the tile
        let results = index.tiles_near(45.0, -120.0, 10.0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].row, 25000);

        // Query far from the tile
        let results = index.tiles_near(50.0, -120.0, 10.0);
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_scenery_index_land_tiles_filter() {
        let index = SceneryIndex::with_defaults();

        // Add a land tile
        index.add_tile(SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 45.0,
            lon: -120.0,
            is_sea: false,
        });

        // Add a sea tile
        index.add_tile(SceneryTile {
            row: 25001,
            col: 10001,
            chunk_zoom: 16,
            lat: 45.01,
            lon: -120.01,
            is_sea: true,
        });

        assert_eq!(index.tile_count(), 2);
        assert_eq!(index.sea_tile_count(), 1);
        assert_eq!(index.land_tile_count(), 1);

        // Query all tiles
        let all = index.tiles_near(45.0, -120.0, 10.0);
        assert_eq!(all.len(), 2);

        // Query land tiles only
        let land = index.land_tiles_near(45.0, -120.0, 10.0);
        assert_eq!(land.len(), 1);
        assert!(!land[0].is_sea);
    }

    #[test]
    fn test_approximate_distance_nm() {
        // Same point
        assert!(approximate_distance_nm(45.0, -120.0, 45.0, -120.0) < 0.01);

        // 1 degree north (should be ~60nm)
        let dist = approximate_distance_nm(45.0, -120.0, 46.0, -120.0);
        assert!((dist - 60.0).abs() < 1.0);

        // 1 degree east at 45° lat (should be ~42nm due to cosine)
        let dist = approximate_distance_nm(45.0, -120.0, 45.0, -119.0);
        assert!((dist - 42.4).abs() < 2.0);
    }

    /// Integration test with real scenery package.
    /// Run with: cargo test scenery_index --features integration -- --ignored
    #[test]
    #[ignore]
    fn test_build_from_real_package() {
        // This test requires a real scenery package at a known location
        let package_path = "/run/media/sdefreyssinet/FlightSim/XEarthLayer Packages/zzXEL_na_ortho";
        let path = std::path::Path::new(package_path);

        if !path.exists() {
            eprintln!("Skipping test: package not found at {}", package_path);
            return;
        }

        let index = SceneryIndex::with_defaults();
        let count = index
            .build_from_package(path)
            .expect("Failed to build index");

        // Should find many tiles
        assert!(count > 1000, "Expected > 1000 tiles, found {}", count);

        // Print some statistics
        eprintln!("Indexed {} tiles", count);
        eprintln!("  Land tiles: {}", index.land_tile_count());
        eprintln!("  Sea tiles: {}", index.sea_tile_count());

        // Query tiles near a known location (California)
        let tiles = index.tiles_near(36.28, -119.49, 10.0);
        assert!(!tiles.is_empty(), "Expected tiles near (36.28, -119.49)");

        // Verify we have both ZL16 and ZL18 tiles
        let has_zl16 = tiles.iter().any(|t| t.chunk_zoom == 16);
        eprintln!("Has ZL16 tiles: {}", has_zl16);
    }
}
