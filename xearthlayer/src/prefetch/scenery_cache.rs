//! Persistent cache for SceneryIndex.
//!
//! Saves and loads the scenery tile index to avoid rebuilding on every launch.
//! The cache stores all tiles from `.ter` files along with package metadata
//! for invalidation checks.
//!
//! # Cache Format
//!
//! The cache uses a line-based text format following the project's established
//! patterns (see `package/metadata.rs`):
//!
//! ```text
//! SCENERY INDEX CACHE
//! 1                                          # format version
//! 2                                          # number of packages
//! 445053                                     # total tile count
//! 12847                                      # sea tile count
//! eu_ortho  /path/to/package  1703980800  125000   # package metadata
//! na_ortho  /path/to/package  1703980900  320053
//!                                            # blank line separator
//! 94800  47888  18  44.50434  -114.22485  0  # tile data (row col zoom lat lon is_sea)
//! 94801  47889  18  44.51234  -114.21485  1
//! ...
//! ```
//!
//! # Cache Invalidation
//!
//! The cache is invalidated when:
//! - Package list changes (different packages)
//! - Any terrain directory's mtime changes
//! - Any terrain directory's file count changes

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use tracing::{debug, info, warn};

use super::scenery_index::{SceneryIndex, SceneryTile};
use crate::config::config_directory;

/// Cache format version. Increment when format changes.
const CACHE_VERSION: u32 = 1;

/// Magic header for cache file validation.
const CACHE_HEADER: &str = "SCENERY INDEX CACHE";

/// Field separator used in the file format (two spaces).
const FIELD_SEPARATOR: &str = "  ";

/// Cache filename.
const CACHE_FILENAME: &str = "scenery_index.cache";

/// Metadata about a cached package (for invalidation).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedPackageInfo {
    /// Package name (e.g., "eu_ortho").
    pub name: String,
    /// Path to the package directory.
    pub path: PathBuf,
    /// Terrain directory modification time (seconds since UNIX epoch).
    pub terrain_mtime_secs: u64,
    /// Number of .ter files in the terrain directory.
    pub terrain_file_count: usize,
}

/// Result of attempting to load the scenery index cache.
#[derive(Debug)]
pub enum CacheLoadResult {
    /// Cache loaded successfully with all tiles.
    Loaded {
        /// The loaded tiles.
        tiles: Vec<SceneryTile>,
        /// Total tile count from cache header.
        total_tiles: usize,
        /// Sea tile count from cache header.
        sea_tiles: usize,
    },
    /// Cache exists but is stale (packages changed).
    Stale {
        /// Reason for staleness.
        reason: String,
    },
    /// Cache file doesn't exist.
    NotFound,
    /// Cache is corrupted or unreadable.
    Invalid {
        /// Error description.
        error: String,
    },
}

/// Get the path to the scenery index cache file.
pub fn cache_path() -> PathBuf {
    config_directory().join(CACHE_FILENAME)
}

/// Gather package metadata for cache validation.
///
/// Collects mtime and file count for each package's terrain directory.
/// This is used to detect when packages have been modified.
pub fn gather_package_info(packages: &[(String, PathBuf)]) -> io::Result<Vec<CachedPackageInfo>> {
    let mut infos = Vec::with_capacity(packages.len());

    for (name, path) in packages {
        let terrain_path = path.join("terrain");

        // Get terrain directory metadata
        let metadata = fs::metadata(&terrain_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to stat terrain directory {}: {}",
                    terrain_path.display(),
                    e
                ),
            )
        })?;

        let mtime = metadata
            .modified()
            .map_err(|e| {
                io::Error::other(format!(
                    "Failed to get mtime for {}: {}",
                    terrain_path.display(),
                    e
                ))
            })?
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Count .ter files
        let count = fs::read_dir(&terrain_path)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to read terrain directory {}: {}",
                        terrain_path.display(),
                        e
                    ),
                )
            })?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ter"))
            .count();

        infos.push(CachedPackageInfo {
            name: name.clone(),
            path: path.clone(),
            terrain_mtime_secs: mtime,
            terrain_file_count: count,
        });
    }

    Ok(infos)
}

/// Save the scenery index to the cache file.
///
/// Writes the index in text format with package metadata for validation.
pub fn save_cache(index: &SceneryIndex, packages: &[(String, PathBuf)]) -> io::Result<()> {
    let path = cache_path();
    let package_infos = gather_package_info(packages)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);

    // Header section
    writeln!(writer, "{}", CACHE_HEADER)?;
    writeln!(writer, "{}", CACHE_VERSION)?;
    writeln!(writer, "{}", packages.len())?;
    writeln!(writer, "{}", index.tile_count())?;
    writeln!(writer, "{}", index.sea_tile_count())?;

    // Package metadata
    for info in &package_infos {
        writeln!(
            writer,
            "{}{}{}{}{}{}{}",
            info.name,
            FIELD_SEPARATOR,
            info.path.display(),
            FIELD_SEPARATOR,
            info.terrain_mtime_secs,
            FIELD_SEPARATOR,
            info.terrain_file_count
        )?;
    }

    // Blank line separator
    writeln!(writer)?;

    // Tile data
    for tile in index.all_tiles() {
        writeln!(
            writer,
            "{}{}{}{}{}{}{}{}{}{}{}",
            tile.row,
            FIELD_SEPARATOR,
            tile.col,
            FIELD_SEPARATOR,
            tile.chunk_zoom,
            FIELD_SEPARATOR,
            tile.lat,
            FIELD_SEPARATOR,
            tile.lon,
            FIELD_SEPARATOR,
            if tile.is_sea { 1 } else { 0 }
        )?;
    }

    writer.flush()?;

    info!(
        path = %path.display(),
        tiles = index.tile_count(),
        packages = packages.len(),
        "Saved scenery index cache"
    );

    Ok(())
}

/// Load the scenery index from the cache file.
///
/// Validates the cache against current package metadata before loading.
/// Returns `Stale` if packages have changed, `NotFound` if cache doesn't exist,
/// or `Invalid` if the cache is corrupted.
pub fn load_cache(packages: &[(String, PathBuf)]) -> CacheLoadResult {
    let path = cache_path();

    // Check cache exists
    if !path.exists() {
        debug!(path = %path.display(), "Cache file not found");
        return CacheLoadResult::NotFound;
    }

    // Gather current package info for validation
    let current_infos = match gather_package_info(packages) {
        Ok(infos) => infos,
        Err(e) => {
            return CacheLoadResult::Invalid {
                error: format!("Failed to gather package info: {}", e),
            }
        }
    };

    // Open and parse cache file
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            return CacheLoadResult::Invalid {
                error: format!("Failed to open cache file: {}", e),
            }
        }
    };

    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Helper to get next line
    let mut next_line = || -> Option<String> { lines.next().and_then(|r| r.ok()) };

    // Validate header
    let header = match next_line() {
        Some(h) => h,
        None => {
            return CacheLoadResult::Invalid {
                error: "Empty cache file".to_string(),
            }
        }
    };

    if header != CACHE_HEADER {
        return CacheLoadResult::Invalid {
            error: format!(
                "Invalid header: expected '{}', got '{}'",
                CACHE_HEADER, header
            ),
        };
    }

    // Parse version
    let version: u32 = match next_line().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => {
            return CacheLoadResult::Invalid {
                error: "Failed to parse cache version".to_string(),
            }
        }
    };

    if version != CACHE_VERSION {
        return CacheLoadResult::Stale {
            reason: format!(
                "Cache version mismatch: expected {}, got {}",
                CACHE_VERSION, version
            ),
        };
    }

    // Parse package count
    let package_count: usize = match next_line().and_then(|s| s.parse().ok()) {
        Some(c) => c,
        None => {
            return CacheLoadResult::Invalid {
                error: "Failed to parse package count".to_string(),
            }
        }
    };

    // Check package count matches
    if package_count != packages.len() {
        return CacheLoadResult::Stale {
            reason: format!(
                "Package count changed: cache has {}, current has {}",
                package_count,
                packages.len()
            ),
        };
    }

    // Parse tile counts
    let total_tiles: usize = match next_line().and_then(|s| s.parse().ok()) {
        Some(c) => c,
        None => {
            return CacheLoadResult::Invalid {
                error: "Failed to parse total tile count".to_string(),
            }
        }
    };

    let sea_tiles: usize = match next_line().and_then(|s| s.parse().ok()) {
        Some(c) => c,
        None => {
            return CacheLoadResult::Invalid {
                error: "Failed to parse sea tile count".to_string(),
            }
        }
    };

    // Parse and validate package metadata
    let mut cached_infos = Vec::with_capacity(package_count);
    for i in 0..package_count {
        let line = match next_line() {
            Some(l) => l,
            None => {
                return CacheLoadResult::Invalid {
                    error: format!("Missing package metadata line {}", i),
                }
            }
        };

        match parse_package_line(&line) {
            Some(info) => cached_infos.push(info),
            None => {
                return CacheLoadResult::Invalid {
                    error: format!("Failed to parse package metadata line {}: {}", i, line),
                }
            }
        }
    }

    // Validate package metadata against current state
    if let Some(reason) = validate_packages(&cached_infos, &current_infos) {
        return CacheLoadResult::Stale { reason };
    }

    // Skip blank line separator
    if let Some(line) = next_line() {
        if !line.is_empty() {
            // This line is actually tile data, we need to parse it
            // Put it back by parsing it first
            let mut tiles = Vec::with_capacity(total_tiles);
            if let Some(tile) = parse_tile_line(&line) {
                tiles.push(tile);
            }

            // Parse remaining tiles
            for line_result in lines {
                let line = match line_result {
                    Ok(l) => l,
                    Err(e) => {
                        return CacheLoadResult::Invalid {
                            error: format!("Failed to read tile line: {}", e),
                        }
                    }
                };

                if line.is_empty() {
                    continue;
                }

                match parse_tile_line(&line) {
                    Some(tile) => tiles.push(tile),
                    None => {
                        warn!(line = %line, "Failed to parse tile line, skipping");
                    }
                }
            }

            info!(
                path = %path.display(),
                tiles = tiles.len(),
                expected = total_tiles,
                "Loaded scenery index from cache"
            );

            return CacheLoadResult::Loaded {
                tiles,
                total_tiles,
                sea_tiles,
            };
        }
    }

    // Parse tiles
    let mut tiles = Vec::with_capacity(total_tiles);
    for line_result in lines {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                return CacheLoadResult::Invalid {
                    error: format!("Failed to read tile line: {}", e),
                }
            }
        };

        if line.is_empty() {
            continue;
        }

        match parse_tile_line(&line) {
            Some(tile) => tiles.push(tile),
            None => {
                warn!(line = %line, "Failed to parse tile line, skipping");
            }
        }
    }

    info!(
        path = %path.display(),
        tiles = tiles.len(),
        expected = total_tiles,
        "Loaded scenery index from cache"
    );

    CacheLoadResult::Loaded {
        tiles,
        total_tiles,
        sea_tiles,
    }
}

/// Parse a package metadata line.
///
/// Format: `name  path  mtime_secs  file_count`
fn parse_package_line(line: &str) -> Option<CachedPackageInfo> {
    let parts: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
    if parts.len() < 4 {
        return None;
    }

    Some(CachedPackageInfo {
        name: parts[0].to_string(),
        path: PathBuf::from(parts[1]),
        terrain_mtime_secs: parts[2].parse().ok()?,
        terrain_file_count: parts[3].parse().ok()?,
    })
}

/// Parse a tile data line.
///
/// Format: `row  col  chunk_zoom  lat  lon  is_sea`
fn parse_tile_line(line: &str) -> Option<SceneryTile> {
    let parts: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
    if parts.len() < 6 {
        return None;
    }

    Some(SceneryTile {
        row: parts[0].parse().ok()?,
        col: parts[1].parse().ok()?,
        chunk_zoom: parts[2].parse().ok()?,
        lat: parts[3].parse().ok()?,
        lon: parts[4].parse().ok()?,
        is_sea: parts[5] == "1",
    })
}

/// Validate cached package info against current package info.
///
/// Returns `Some(reason)` if packages have changed, `None` if valid.
fn validate_packages(
    cached: &[CachedPackageInfo],
    current: &[CachedPackageInfo],
) -> Option<String> {
    if cached.len() != current.len() {
        return Some(format!(
            "Package count changed: {} → {}",
            cached.len(),
            current.len()
        ));
    }

    for (i, (cached_pkg, current_pkg)) in cached.iter().zip(current.iter()).enumerate() {
        if cached_pkg.name != current_pkg.name {
            return Some(format!(
                "Package {} name changed: {} → {}",
                i, cached_pkg.name, current_pkg.name
            ));
        }

        if cached_pkg.path != current_pkg.path {
            return Some(format!(
                "Package {} path changed: {} → {}",
                cached_pkg.name,
                cached_pkg.path.display(),
                current_pkg.path.display()
            ));
        }

        if cached_pkg.terrain_mtime_secs != current_pkg.terrain_mtime_secs {
            return Some(format!(
                "Package {} terrain directory modified",
                cached_pkg.name
            ));
        }

        if cached_pkg.terrain_file_count != current_pkg.terrain_file_count {
            return Some(format!(
                "Package {} file count changed: {} → {}",
                cached_pkg.name, cached_pkg.terrain_file_count, current_pkg.terrain_file_count
            ));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::super::scenery_index::SceneryIndexConfig;
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_tile(
        row: u32,
        col: u32,
        zoom: u8,
        lat: f32,
        lon: f32,
        is_sea: bool,
    ) -> SceneryTile {
        SceneryTile {
            row,
            col,
            chunk_zoom: zoom,
            lat,
            lon,
            is_sea,
        }
    }

    #[test]
    fn test_parse_tile_line() {
        let line = "94800  47888  18  44.50434  -114.22485  0";
        let tile = parse_tile_line(line).expect("Should parse");

        assert_eq!(tile.row, 94800);
        assert_eq!(tile.col, 47888);
        assert_eq!(tile.chunk_zoom, 18);
        assert!((tile.lat - 44.50434).abs() < 0.0001);
        assert!((tile.lon - (-114.22485)).abs() < 0.0001);
        assert!(!tile.is_sea);
    }

    #[test]
    fn test_parse_tile_line_sea() {
        let line = "94801  47889  16  45.0  -120.0  1";
        let tile = parse_tile_line(line).expect("Should parse");

        assert!(tile.is_sea);
    }

    #[test]
    fn test_parse_package_line() {
        let line = "eu_ortho  /path/to/package  1703980800  125000";
        let info = parse_package_line(line).expect("Should parse");

        assert_eq!(info.name, "eu_ortho");
        assert_eq!(info.path, PathBuf::from("/path/to/package"));
        assert_eq!(info.terrain_mtime_secs, 1703980800);
        assert_eq!(info.terrain_file_count, 125000);
    }

    #[test]
    fn test_validate_packages_match() {
        let pkg1 = CachedPackageInfo {
            name: "test".to_string(),
            path: PathBuf::from("/test"),
            terrain_mtime_secs: 12345,
            terrain_file_count: 100,
        };

        let result = validate_packages(&[pkg1.clone()], &[pkg1]);
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_packages_mtime_changed() {
        let cached = CachedPackageInfo {
            name: "test".to_string(),
            path: PathBuf::from("/test"),
            terrain_mtime_secs: 12345,
            terrain_file_count: 100,
        };

        let current = CachedPackageInfo {
            name: "test".to_string(),
            path: PathBuf::from("/test"),
            terrain_mtime_secs: 12346, // Different mtime
            terrain_file_count: 100,
        };

        let result = validate_packages(&[cached], &[current]);
        assert!(result.is_some());
        assert!(result.unwrap().contains("modified"));
    }

    #[test]
    fn test_validate_packages_count_changed() {
        let cached = CachedPackageInfo {
            name: "test".to_string(),
            path: PathBuf::from("/test"),
            terrain_mtime_secs: 12345,
            terrain_file_count: 100,
        };

        let current = CachedPackageInfo {
            name: "test".to_string(),
            path: PathBuf::from("/test"),
            terrain_mtime_secs: 12345,
            terrain_file_count: 101, // Different count
        };

        let result = validate_packages(&[cached], &[current]);
        assert!(result.is_some());
        assert!(result.unwrap().contains("file count"));
    }

    #[test]
    fn test_cache_round_trip() {
        let temp_dir = TempDir::new().unwrap();

        // Create a mock terrain directory with .ter files
        let package_path = temp_dir.path().join("test_package");
        let terrain_path = package_path.join("terrain");
        fs::create_dir_all(&terrain_path).unwrap();

        // Create some .ter files
        fs::write(terrain_path.join("tile1.ter"), "dummy").unwrap();
        fs::write(terrain_path.join("tile2.ter"), "dummy").unwrap();

        // Create index with test tiles
        let index = SceneryIndex::new(SceneryIndexConfig::default());
        index.add_tile(create_test_tile(94800, 47888, 18, 44.5, -114.2, false));
        index.add_tile(create_test_tile(94801, 47889, 16, 45.0, -120.0, true));

        let packages = vec![("test_package".to_string(), package_path.clone())];

        // Override cache path for test
        let cache_file = temp_dir.path().join("test_cache.cache");

        // Save cache manually to test location
        let package_infos = gather_package_info(&packages).unwrap();
        let mut file = File::create(&cache_file).unwrap();

        writeln!(file, "{}", CACHE_HEADER).unwrap();
        writeln!(file, "{}", CACHE_VERSION).unwrap();
        writeln!(file, "{}", packages.len()).unwrap();
        writeln!(file, "{}", index.tile_count()).unwrap();
        writeln!(file, "{}", index.sea_tile_count()).unwrap();

        for info in &package_infos {
            writeln!(
                file,
                "{}{}{}{}{}{}{}",
                info.name,
                FIELD_SEPARATOR,
                info.path.display(),
                FIELD_SEPARATOR,
                info.terrain_mtime_secs,
                FIELD_SEPARATOR,
                info.terrain_file_count
            )
            .unwrap();
        }

        writeln!(file).unwrap();

        for tile in index.all_tiles() {
            writeln!(
                file,
                "{}{}{}{}{}{}{}{}{}{}{}",
                tile.row,
                FIELD_SEPARATOR,
                tile.col,
                FIELD_SEPARATOR,
                tile.chunk_zoom,
                FIELD_SEPARATOR,
                tile.lat,
                FIELD_SEPARATOR,
                tile.lon,
                FIELD_SEPARATOR,
                if tile.is_sea { 1 } else { 0 }
            )
            .unwrap();
        }

        drop(file);

        // Read back and verify
        let content = fs::read_to_string(&cache_file).unwrap();
        let mut lines = content.lines();

        assert_eq!(lines.next(), Some(CACHE_HEADER));
        assert_eq!(lines.next(), Some("1")); // version
        assert_eq!(lines.next(), Some("1")); // package count
        assert_eq!(lines.next(), Some("2")); // total tiles
        assert_eq!(lines.next(), Some("1")); // sea tiles

        // Skip package line
        lines.next();

        // Skip blank line
        lines.next();

        // Collect and verify tiles (order is not guaranteed due to HashMap)
        let mut tile_rows: Vec<u32> = Vec::new();
        while let Some(line) = lines.next() {
            if !line.is_empty() {
                if let Some(tile) = parse_tile_line(line) {
                    tile_rows.push(tile.row);
                }
            }
        }

        tile_rows.sort();
        assert_eq!(tile_rows, vec![94800, 94801], "Should contain both tiles");
    }
}
