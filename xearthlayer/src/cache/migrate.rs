//! Cache migration from flat layout to region-based subdirectories.
//!
//! Moves cache files from a flat directory layout into 1°×1° DSF region
//! subdirectories for faster parallel scanning on startup.
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::cache::migrate::migrate_cache;
//! use std::path::Path;
//!
//! let result = migrate_cache(Path::new("/home/user/.cache/xearthlayer"))?;
//! println!("Moved {} files into {} regions", result.files_moved, result.regions_created);
//! ```

use std::collections::HashSet;
use std::path::Path;

use crate::cache::lru_index::{key_to_region, LruIndex};

/// Result of migrating cache files to region subdirectories.
#[derive(Debug, Default)]
pub struct MigrateResult {
    /// Number of files moved into region directories.
    pub files_moved: u64,
    /// Number of files skipped (not parseable as cache keys).
    pub files_skipped: u64,
    /// Total bytes migrated.
    pub bytes_migrated: u64,
    /// Number of new region directories created.
    pub regions_created: u64,
}

/// Migrate flat cache files into region subdirectories.
///
/// Scans the cache directory for `.cache` files at the root level (not
/// already in subdirectories), parses each filename back to a cache key,
/// derives the DSF region, and moves the file into the appropriate region
/// subdirectory via `rename()`.
///
/// This is an offline operation — the cache should not be in active use.
/// Running it twice is safe (idempotent): files already in region dirs
/// are not touched.
pub fn migrate_cache(cache_dir: &Path) -> std::io::Result<MigrateResult> {
    let mut result = MigrateResult::default();

    if !cache_dir.exists() {
        return Ok(result);
    }

    let mut created_dirs = HashSet::new();

    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only process files at the root level (skip directories)
        if !path.is_file() {
            continue;
        }

        // Only process .cache files
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(f) if f.ends_with(".cache") => f.to_string(),
            _ => {
                result.files_skipped += 1;
                continue;
            }
        };

        // Parse filename back to cache key
        let key = match LruIndex::filename_to_key(&filename) {
            Some(k) => k,
            None => {
                result.files_skipped += 1;
                continue;
            }
        };

        // Derive region directory
        let region = match key_to_region(&key) {
            Some(r) => r,
            None => {
                result.files_skipped += 1;
                continue;
            }
        };

        // Create region directory if needed
        let region_dir = cache_dir.join(&region);
        if created_dirs.insert(region.clone()) && !region_dir.exists() {
            std::fs::create_dir_all(&region_dir)?;
            result.regions_created += 1;
        }

        // Get file size for reporting
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

        // Move file into region directory (atomic on same filesystem)
        let dest = region_dir.join(&filename);
        std::fs::rename(&path, &dest)?;

        result.files_moved += 1;
        result.bytes_migrated += size;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_migrate_moves_files_to_region_dirs() {
        let temp_dir = TempDir::new().unwrap();

        // Create flat cache files with known tile coordinates
        // tile:15:12754:5279 → a location in southern California
        std::fs::write(
            temp_dir.path().join("tile_15_12754_5279.cache"),
            vec![0u8; 1000],
        )
        .unwrap();

        let result = migrate_cache(temp_dir.path()).unwrap();

        assert_eq!(result.files_moved, 1);
        assert_eq!(result.bytes_migrated, 1000);
        assert!(result.regions_created >= 1);

        // Original file should be gone
        assert!(!temp_dir.path().join("tile_15_12754_5279.cache").exists());

        // File should be in a region subdirectory
        let region = key_to_region("tile:15:12754:5279").unwrap();
        assert!(temp_dir
            .path()
            .join(&region)
            .join("tile_15_12754_5279.cache")
            .exists());
    }

    #[test]
    fn test_migrate_creates_region_directories() {
        let temp_dir = TempDir::new().unwrap();

        // Create files that map to different regions
        std::fs::write(
            temp_dir.path().join("tile_15_12754_5279.cache"),
            vec![0u8; 100],
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("tile_16_24640_19295.cache"),
            vec![0u8; 200],
        )
        .unwrap();

        let result = migrate_cache(temp_dir.path()).unwrap();

        assert_eq!(result.files_moved, 2);
        // Should have created at least 1 region dir (may be 1 or 2 depending on coords)
        assert!(result.regions_created >= 1);
    }

    #[test]
    fn test_migrate_skips_unparseable_files() {
        let temp_dir = TempDir::new().unwrap();

        // Non-cache files
        std::fs::write(temp_dir.path().join("readme.txt"), "hello").unwrap();
        std::fs::write(temp_dir.path().join("data.json"), "{}").unwrap();

        // Valid cache file
        std::fs::write(
            temp_dir.path().join("tile_15_12754_5279.cache"),
            vec![0u8; 100],
        )
        .unwrap();

        let result = migrate_cache(temp_dir.path()).unwrap();

        assert_eq!(result.files_moved, 1);
        assert_eq!(result.files_skipped, 2);

        // Non-cache files should remain untouched
        assert!(temp_dir.path().join("readme.txt").exists());
        assert!(temp_dir.path().join("data.json").exists());
    }

    #[test]
    fn test_migrate_idempotent() {
        let temp_dir = TempDir::new().unwrap();

        std::fs::write(
            temp_dir.path().join("tile_15_12754_5279.cache"),
            vec![0u8; 100],
        )
        .unwrap();

        // First migration
        let result1 = migrate_cache(temp_dir.path()).unwrap();
        assert_eq!(result1.files_moved, 1);

        // Second migration — should be a no-op
        let result2 = migrate_cache(temp_dir.path()).unwrap();
        assert_eq!(result2.files_moved, 0);
        assert_eq!(result2.files_skipped, 0);
    }

    #[test]
    fn test_migrate_handles_empty_cache() {
        let temp_dir = TempDir::new().unwrap();

        let result = migrate_cache(temp_dir.path()).unwrap();

        assert_eq!(result.files_moved, 0);
        assert_eq!(result.files_skipped, 0);
        assert_eq!(result.regions_created, 0);
    }

    #[test]
    fn test_migrate_handles_nonexistent_directory() {
        let result = migrate_cache(Path::new("/nonexistent/path/that/doesnt/exist")).unwrap();

        assert_eq!(result.files_moved, 0);
        assert_eq!(result.files_skipped, 0);
    }

    #[test]
    fn test_migrate_chunk_files() {
        let temp_dir = TempDir::new().unwrap();

        // Chunk files should also be migrated to the same region as their parent tile
        std::fs::write(
            temp_dir.path().join("chunk_15_12754_5279_8_12.cache"),
            vec![0u8; 500],
        )
        .unwrap();

        let result = migrate_cache(temp_dir.path()).unwrap();

        assert_eq!(result.files_moved, 1);
        assert_eq!(result.bytes_migrated, 500);

        // Should be in same region as the parent tile
        let region = key_to_region("chunk:15:12754:5279:8:12").unwrap();
        assert!(temp_dir
            .path()
            .join(&region)
            .join("chunk_15_12754_5279_8_12.cache")
            .exists());
    }
}
