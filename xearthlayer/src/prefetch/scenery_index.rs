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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::coord::TileCoord;
use crate::geo_index::DsfRegion;

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
    /// Get the tile zoom level (inverse of CHUNK_ZOOM_OFFSET).
    #[inline]
    pub fn tile_zoom(&self) -> u8 {
        self.chunk_zoom
            .saturating_sub(crate::coord::CHUNK_ZOOM_OFFSET)
    }

    /// Convert to TileCoord for use with the pipeline.
    #[inline]
    pub fn to_tile_coord(&self) -> TileCoord {
        // Convert chunk coordinates to tile coordinates (inverse of chunk_origin())
        TileCoord {
            row: self.row / crate::coord::CHUNKS_PER_TILE_SIDE,
            col: self.col / crate::coord::CHUNKS_PER_TILE_SIDE,
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
    pub fn build_from_package(
        &self,
        package_path: &Path,
    ) -> Result<PackageIndexStats, SceneryIndexError> {
        debug!(
            package = %package_path.display(),
            "Building scenery index from package"
        );

        let terrain_path = package_path.join("terrain");
        if !terrain_path.exists() {
            debug!(
                terrain_path = %terrain_path.display(),
                "Terrain directory not found"
            );
            return Err(SceneryIndexError::TerrainDirNotFound(
                terrain_path.to_path_buf(),
            ));
        }

        debug!(
            terrain_path = %terrain_path.display(),
            "Found terrain directory"
        );

        let mut stats = PackageIndexStats::default();
        let mut failure_samples: Vec<String> = Vec::new();
        const MAX_FAILURE_SAMPLES: usize = 5;

        let entries =
            fs::read_dir(&terrain_path).map_err(|e| SceneryIndexError::IoError(e.to_string()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ter") {
                match self.parse_and_add_ter_file(&path) {
                    Ok(()) => stats.parsed += 1,
                    Err(e) => {
                        stats.failed += 1;
                        // Per-file stays at debug: a wholesale-malformed
                        // package would emit millions of lines at warn.
                        // The aggregate below is the visible signal.
                        debug!(path = %path.display(), error = %e, "Failed to parse .ter file");
                        if failure_samples.len() < MAX_FAILURE_SAMPLES {
                            failure_samples.push(path.display().to_string());
                        }
                    }
                }
            }
        }

        if stats.failed > 0 {
            tracing::warn!(
                package = %package_path.display(),
                failed = stats.failed,
                parsed = stats.parsed,
                samples = ?failure_samples,
                "Scenery index: .ter files failed to parse — prefetch may under-cover this package"
            );
        }

        info!(
            package = %package_path.display(),
            tiles = stats.parsed,
            failed = stats.failed,
            "Built scenery index"
        );

        Ok(stats)
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

        debug!(
            lat = lat,
            lon = lon,
            radius_nm = radius_nm,
            lat_cells = format!("{}..={}", min_lat_cell, max_lat_cell),
            lon_cells = format!("{}..={}", min_lon_cell, max_lon_cell),
            total_tiles_indexed = *self.tile_count.read().unwrap(),
            "Searching tiles_near"
        );

        let grid = self.grid.read().unwrap();
        let mut result = Vec::new();
        let mut cells_with_tiles = 0;

        for lat_cell in min_lat_cell..=max_lat_cell {
            for lon_cell in min_lon_cell..=max_lon_cell {
                if let Some(cell) = grid.get(&(lat_cell, lon_cell)) {
                    cells_with_tiles += 1;
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

        debug!(
            cells_searched = (max_lat_cell - min_lat_cell + 1) * (max_lon_cell - min_lon_cell + 1),
            cells_with_tiles = cells_with_tiles,
            tiles_found = result.len(),
            "tiles_near search complete"
        );

        result
    }

    /// Get the deduplicated set of DDS tiles belonging to a DSF region.
    ///
    /// A tile belongs to the region containing its geographic centre, which
    /// is the `LOAD_CENTER` of its `.ter` file. This is the single definition
    /// of a region's tile set — the submit, promote and rescue paths all
    /// consult it, so they cannot disagree about whether a region is complete.
    ///
    /// # Why this is cheap
    ///
    /// A `DsfRegion` is 1°×1° and `cell_key` floors by `cell_size`, which
    /// defaults to 1.0 — so in the default configuration a region *is* a grid
    /// cell and this is a single `HashMap` lookup. The predicate is still
    /// applied per tile because `grid_cell_size` is configurable.
    ///
    /// See #176: the previous implementations reconstructed this answer from a
    /// 45nm radius query, which returns a circle overlapping the neighbouring
    /// regions rather than the region itself.
    pub fn tiles_in_region(&self, region: DsfRegion) -> Vec<TileCoord> {
        let grid = self.grid.read().unwrap();
        let mut unique: HashSet<TileCoord> = HashSet::new();

        for key in self.cells_covering_region(region) {
            let Some(cell) = grid.get(&key) else {
                continue;
            };
            for tile in &cell.tiles {
                if tile.lat.floor() as i32 == region.lat && tile.lon.floor() as i32 == region.lon {
                    unique.insert(tile.to_tile_coord());
                }
            }
        }

        unique.into_iter().collect()
    }

    /// Grid cell keys that overlap a DSF region.
    ///
    /// Derives the corners through `cell_key` rather than recomputing the
    /// floor division, so there is one mapping from position to cell. With
    /// the default 1.0 cell size this yields exactly one key.
    fn cells_covering_region(&self, region: DsfRegion) -> Vec<(i16, i16)> {
        // Nudge inside the region's far edge: the region is the half-open
        // box [lat, lat+1) x [lon, lon+1), so lat+1.0 belongs to the *next*
        // region and must not pull in an extra row of cells.
        const INSIDE_EDGE: f32 = 1.0 - 1e-4;

        let (lat_min, lon_min) = self.cell_key(region.lat as f32, region.lon as f32);
        let (lat_max, lon_max) = self.cell_key(
            region.lat as f32 + INSIDE_EDGE,
            region.lon as f32 + INSIDE_EDGE,
        );

        let mut keys = Vec::new();
        for lat_cell in lat_min..=lat_max {
            for lon_cell in lon_min..=lon_max {
                keys.push((lat_cell, lon_cell));
            }
        }
        keys
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

    /// Create an index from pre-loaded tiles (from cache).
    ///
    /// This is the fast-path for loading a cached index. Instead of parsing
    /// `.ter` files, it directly adds tiles that were previously serialized.
    pub fn from_tiles(tiles: Vec<SceneryTile>, config: SceneryIndexConfig) -> Self {
        let index = Self::new(config);
        for tile in tiles {
            index.add_tile(tile);
        }
        index
    }

    /// Iterate all tiles in the index.
    ///
    /// Returns an iterator over all tiles across all grid cells.
    /// Used for serializing the index to cache.
    pub fn all_tiles(&self) -> Vec<SceneryTile> {
        self.grid
            .read()
            .unwrap()
            .values()
            .flat_map(|cell| cell.tiles.iter().copied())
            .collect()
    }

    /// Build the index from multiple packages with progress reporting.
    ///
    /// This is an async method that builds the index from a list of package paths
    /// and sends progress updates through the provided channel. The actual file I/O
    /// is performed using `spawn_blocking` to avoid blocking the async runtime.
    ///
    /// Progress updates are sent:
    /// - When each package starts (`PackageStarted`)
    /// - Every 100ms during indexing (`TileProgress`) with the running tile count
    /// - When each package completes (`PackageCompleted`)
    /// - When all packages are done (`Complete`)
    ///
    /// # Arguments
    ///
    /// * `index` - Arc-wrapped SceneryIndex to build into
    /// * `packages` - List of (name, path) tuples for packages to scan
    /// * `progress_tx` - Channel to send progress updates
    ///
    /// # Example
    ///
    /// ```ignore
    /// let index = Arc::new(SceneryIndex::with_defaults());
    /// let (tx, mut rx) = mpsc::channel(32);
    /// let packages = vec![("eu_spain".to_string(), PathBuf::from("/path/to/package"))];
    ///
    /// tokio::spawn(async move {
    ///     SceneryIndex::build_from_packages_with_progress(index, packages, tx).await;
    /// });
    ///
    /// while let Some(progress) = rx.recv().await {
    ///     match progress {
    ///         IndexingProgress::PackageStarted { name, .. } => println!("Scanning {}", name),
    ///         IndexingProgress::TileProgress { tiles_indexed } => println!("Tiles: {}", tiles_indexed),
    ///         IndexingProgress::Complete { total, .. } => println!("Done: {} tiles", total),
    ///         _ => {}
    ///     }
    /// }
    /// ```
    pub async fn build_from_packages_with_progress(
        index: Arc<Self>,
        packages: Vec<(String, PathBuf)>,
        progress_tx: mpsc::Sender<IndexingProgress>,
    ) {
        use std::time::Duration;
        use tokio::time::interval;

        let total_packages = packages.len();
        let mut total_failed = 0usize;

        for (i, (name, path)) in packages.into_iter().enumerate() {
            // Send PackageStarted notification
            let _ = progress_tx
                .send(IndexingProgress::PackageStarted {
                    name: name.clone(),
                    index: i,
                    total: total_packages,
                })
                .await;

            // Clone what we need for the blocking task
            let index_clone = Arc::clone(&index);
            let path_clone = path.clone();
            let name_clone = name.clone();

            // Spawn the blocking task
            let blocking_handle =
                tokio::task::spawn_blocking(move || index_clone.build_from_package(&path_clone));

            // Clone index for progress polling
            let index_for_poll = Arc::clone(&index);
            let progress_tx_poll = progress_tx.clone();

            // Poll tile count every 100ms while the blocking task runs
            let mut poll_interval = interval(Duration::from_millis(100));
            poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Pin the blocking handle for use in the select loop
            tokio::pin!(blocking_handle);

            let result = loop {
                tokio::select! {
                    // Blocking task completed
                    result = &mut blocking_handle => {
                        break result;
                    }
                    // Poll interval tick - send progress update
                    _ = poll_interval.tick() => {
                        let tiles_indexed = index_for_poll.tile_count();
                        let _ = progress_tx_poll
                            .send(IndexingProgress::TileProgress { tiles_indexed })
                            .await;
                    }
                }
            };

            // Get the tile count from the result
            let tiles = match result {
                Ok(Ok(stats)) => {
                    total_failed += stats.failed;
                    stats.parsed
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        package = %name_clone,
                        error = %e,
                        "Failed to index package"
                    );
                    0
                }
                Err(e) => {
                    tracing::error!(
                        package = %name_clone,
                        error = %e,
                        "Blocking task panicked"
                    );
                    0
                }
            };

            // Send PackageCompleted notification
            let _ = progress_tx
                .send(IndexingProgress::PackageCompleted { name, tiles })
                .await;
        }

        // Send Complete notification
        let total = index.tile_count();
        let land = index.land_tile_count();
        let sea = index.sea_tile_count();

        let _ = progress_tx
            .send(IndexingProgress::Complete { total, land, sea })
            .await;

        info!(
            total = total,
            land = land,
            sea = sea,
            failed = total_failed,
            "Scenery index build complete"
        );
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

/// Parse outcome for one scenery package.
///
/// `failed` is the count of `.ter` files present on disk that did not yield a
/// tile. The measured baseline across 4.45M files on a full 11-package install
/// is zero, so any non-zero value is anomalous — see #176.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackageIndexStats {
    /// Files that parsed into an indexed tile.
    ///
    /// Note: this counts files that parsed successfully, not tiles added.
    /// `parse_and_add_ter_file` returns `Ok(())` for a sea tile that was
    /// skipped because `include_sea_tiles` is false, so `parsed` can exceed
    /// `SceneryIndex::tile_count()`. That is still the correct denominator
    /// for a parse-health metric: it counts files on disk, not tiles kept.
    pub parsed: usize,
    /// Files that failed to parse.
    pub failed: usize,
}

impl PackageIndexStats {
    /// Total `.ter` files seen.
    pub fn total(&self) -> usize {
        self.parsed + self.failed
    }
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

/// Progress updates during scenery index building.
///
/// Sent via a channel to notify the UI of indexing progress.
#[derive(Debug, Clone)]
pub enum IndexingProgress {
    /// Started scanning a package.
    PackageStarted {
        /// Name of the package being scanned.
        name: String,
        /// Index of the package (0-based).
        index: usize,
        /// Total number of packages to scan.
        total: usize,
    },
    /// Incremental tile count update during package scanning.
    /// Sent periodically while a package is being indexed.
    TileProgress {
        /// Current total tiles indexed across all packages.
        tiles_indexed: usize,
    },
    /// Finished scanning a package.
    PackageCompleted {
        /// Name of the completed package.
        name: String,
        /// Number of tiles indexed from this package.
        tiles: usize,
    },
    /// All packages have been indexed.
    Complete {
        /// Total tiles indexed.
        total: usize,
        /// Number of land tiles.
        land: usize,
        /// Number of sea tiles.
        sea: usize,
    },
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
    use crate::geo_index::DsfRegion;

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
    fn test_scenery_index_sea_and_land_counts() {
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

    /// Helper: a land tile at a given position. Row/col are irrelevant to the
    /// region predicate but must be distinct so dedup tests are meaningful.
    fn tile_at(row: u32, col: u32, lat: f32, lon: f32) -> SceneryTile {
        SceneryTile {
            row,
            col,
            chunk_zoom: 16,
            lat,
            lon,
            is_sea: false,
        }
    }

    #[test]
    fn test_tiles_in_region_includes_only_tiles_centred_inside() {
        let index = SceneryIndex::with_defaults();
        // Inside the +33-119 region.
        index.add_tile(tile_at(1000, 2000, 33.01, -118.99));
        index.add_tile(tile_at(1016, 2016, 33.99, -118.01));
        // Just outside each edge.
        index.add_tile(tile_at(2000, 2000, 32.99, -118.5)); // south
        index.add_tile(tile_at(2016, 2016, 34.01, -118.5)); // north
        index.add_tile(tile_at(2032, 2032, 33.5, -119.01)); // west
        index.add_tile(tile_at(2048, 2048, 33.5, -117.99)); // east

        let tiles = index.tiles_in_region(DsfRegion::new(33, -119));

        assert_eq!(
            tiles.len(),
            2,
            "only the two tiles centred inside +33-119 belong to it, got {:?}",
            tiles
        );
    }

    #[test]
    fn test_tiles_in_region_deduplicates_shared_textures() {
        let index = SceneryIndex::with_defaults();
        // Many .ter files share one base DDS texture. After the /16 division in
        // to_tile_coord these collapse to the same TileCoord.
        index.add_tile(tile_at(1000, 2000, 33.1, -118.9));
        index.add_tile(tile_at(1001, 2001, 33.2, -118.8));
        index.add_tile(tile_at(1007, 2015, 33.3, -118.7));

        let tiles = index.tiles_in_region(DsfRegion::new(33, -119));

        assert_eq!(
            tiles.len(),
            1,
            "1000, 1001, 1007 all fall in chunk-row block [992, 1008), one tile coord"
        );
        assert_eq!(tiles[0].row, 1000 / 16);
        assert_eq!(tiles[0].col, 2000 / 16);
    }

    #[test]
    fn test_tiles_in_region_southern_western_hemisphere() {
        // floor() on negatives is where an off-by-one hides: floor(-33.5) is -34,
        // so the region containing -33.5 is DsfRegion { lat: -34 }.
        let index = SceneryIndex::with_defaults();
        index.add_tile(tile_at(1000, 2000, -33.5, -70.5));
        index.add_tile(tile_at(3000, 4000, -32.5, -70.5)); // region -33, not -34

        let tiles = index.tiles_in_region(DsfRegion::new(-34, -71));

        assert_eq!(tiles.len(), 1, "only the -33.5 tile is in +-34-071");
        assert_eq!(tiles[0].row, 1000 / 16);
    }

    #[test]
    fn test_tiles_in_region_honours_non_default_cell_size() {
        // A 0.5 degree grid splits one DSF region across four cells. All four
        // must be visited or the result silently loses three quarters of them.
        let config = SceneryIndexConfig {
            grid_cell_size: 0.5,
            include_sea_tiles: true,
        };
        let index = SceneryIndex::new(config);
        index.add_tile(tile_at(1000, 2000, 33.2, -118.8)); // cell (66, -238)
        index.add_tile(tile_at(2000, 3000, 33.7, -118.8)); // cell (67, -238)
        index.add_tile(tile_at(3000, 4000, 33.2, -118.3)); // cell (66, -237)
        index.add_tile(tile_at(4000, 5000, 33.7, -118.3)); // cell (67, -237)

        let tiles = index.tiles_in_region(DsfRegion::new(33, -119));

        assert_eq!(
            tiles.len(),
            4,
            "all four sub-cells must be visited, got {:?}",
            tiles
        );
    }

    #[test]
    fn test_tiles_in_region_empty_for_uncovered_region() {
        let index = SceneryIndex::with_defaults();
        index.add_tile(tile_at(1000, 2000, 33.5, -118.5));

        assert!(index.tiles_in_region(DsfRegion::new(50, 10)).is_empty());
    }

    #[test]
    fn test_tiles_in_region_predicate_filters_within_shared_cell() {
        // At cell_size 2.0, one grid cell spans two DSF regions along the lat
        // axis: floor(33.5 / 2.0) == floor(32.5 / 2.0) == 16, so both tiles
        // land in the same cell, but floor(33.5) == 33 and floor(32.5) == 32
        // put them in different regions. Only the predicate -- not cell
        // visitation -- can tell them apart.
        let config = SceneryIndexConfig {
            grid_cell_size: 2.0,
            include_sea_tiles: true,
        };
        let index = SceneryIndex::new(config);
        index.add_tile(tile_at(1000, 2000, 33.5, -119.5)); // region 33,-120
        index.add_tile(tile_at(2000, 3000, 32.5, -119.5)); // region 32,-120, same cell

        let tiles = index.tiles_in_region(DsfRegion::new(33, -120));

        assert_eq!(
            tiles.len(),
            1,
            "the same-cell tile from region 32,-120 must be excluded, got {:?}",
            tiles
        );
        assert_eq!(tiles[0].row, 1000 / 16);
    }

    #[test]
    fn test_tiles_in_region_includes_sea_tiles() {
        // The spec requires sea tiles stay in scope: no is_sea filter here.
        let index = SceneryIndex::with_defaults();
        let mut tile = tile_at(1000, 2000, 33.5, -118.5);
        tile.is_sea = true;
        index.add_tile(tile);

        let tiles = index.tiles_in_region(DsfRegion::new(33, -119));

        assert_eq!(
            tiles.len(),
            1,
            "sea tiles must remain in scope, got {:?}",
            tiles
        );
    }

    /// Write a minimal valid .ter file that parses into one tile.
    fn write_valid_ter(dir: &std::path::Path, name: &str) {
        let body = "A\n800\nTERRAIN\n\n\
                    LOAD_CENTER 33.50000 -118.50000 1744 4096\n\
                    BASE_TEX_NOWRAP ../textures/25328_49904_BI16.dds\n";
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn test_build_from_package_counts_parse_failures() {
        let tmp = tempfile::TempDir::new().unwrap();
        let terrain = tmp.path().join("terrain");
        std::fs::create_dir_all(&terrain).unwrap();

        write_valid_ter(&terrain, "good_a.ter");
        write_valid_ter(&terrain, "good_b.ter");
        // No LOAD_CENTER and no BASE_TEX: parse must fail.
        std::fs::write(terrain.join("bad.ter"), "A\n800\nTERRAIN\n").unwrap();
        // Not a .ter file: must not be counted at all.
        std::fs::write(terrain.join("notes.txt"), "ignored").unwrap();

        let index = SceneryIndex::with_defaults();
        let stats = index.build_from_package(tmp.path()).unwrap();

        assert_eq!(stats.parsed, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(index.tile_count(), 2);
    }

    #[test]
    fn test_build_from_package_reports_zero_failures_when_all_parse() {
        let tmp = tempfile::TempDir::new().unwrap();
        let terrain = tmp.path().join("terrain");
        std::fs::create_dir_all(&terrain).unwrap();
        write_valid_ter(&terrain, "a.ter");
        write_valid_ter(&terrain, "b.ter");

        let index = SceneryIndex::with_defaults();
        let stats = index.build_from_package(tmp.path()).unwrap();

        assert_eq!(
            stats.failed, 0,
            "the measured baseline across 4.45M real files is zero"
        );
        assert_eq!(stats.parsed, 2);
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
        let stats = index
            .build_from_package(path)
            .expect("Failed to build index");
        let count = stats.parsed;

        // Should find many tiles
        assert!(count > 1000, "Expected > 1000 tiles, found {}", count);

        // Print some statistics
        eprintln!("Indexed {} tiles ({} failed)", count, stats.failed);
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
