//! Removal of the chunk tier's region directories left in the cache root.
//!
//! Before #398 the chunk disk tier was `cache.directory` itself, shared by
//! every provider, so its 1°×1° region directories (`+37-123/`, ...) sit
//! directly in the cache root. The tier now lives at
//! [`layout::chunk_dir`](super::layout::chunk_dir), under its provider, and
//! nothing indexes the old directories: they would never be counted against
//! the budget nor evicted. Their chunk keys name no provider, so they cannot
//! be adopted by one; they are deleted instead, once per start.

use std::io;
use std::path::Path;

use super::layout::{holds_only_cache_entries, is_region_dir_name};

/// What one cleanup pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LegacyChunkCleanup {
    /// Region directories deleted.
    pub removed: usize,
    /// Bytes of the files in the deleted directories.
    pub bytes_freed: u64,
    /// Region directories that could not be deleted (each logged as a warning).
    pub failed: usize,
    /// Region-named directories left alone because they hold something other
    /// than cache files: not the old chunk tier, whatever their name.
    pub kept: usize,
}

/// Delete every region directory directly in `cache_dir`.
///
/// Only directories whose name is a region name
/// ([`is_region_dir_name`]) and that hold nothing but cache files
/// ([`holds_only_cache_entries`]) are touched: `cache.directory` may also
/// hold the user's own folders, scenery among them, named like regions.
/// Provider directories, `night/`, `tmp/`, index cache files and anything
/// else are left alone, as are symbolic links. A missing or unreadable `cache_dir` is a no-op. A
/// directory that cannot be deleted is logged and counted, never fatal.
///
/// Blocking: call it from a blocking thread, never an async worker.
pub fn remove_legacy_chunk_regions(cache_dir: &Path) -> LegacyChunkCleanup {
    let mut outcome = LegacyChunkCleanup::default();
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return outcome;
    };

    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_region = entry.file_name().to_str().is_some_and(is_region_dir_name);
        if !is_dir || !is_region {
            continue;
        }

        let path = entry.path();
        if !holds_only_cache_entries(&path) {
            outcome.kept += 1;
            tracing::debug!(
                directory = %path.display(),
                "Kept a region-named directory that holds more than cache files"
            );
            continue;
        }
        let bytes = tree_size(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                outcome.removed += 1;
                outcome.bytes_freed += bytes;
            }
            Err(e) => {
                outcome.failed += 1;
                tracing::warn!(
                    directory = %path.display(),
                    error = %e,
                    "Could not delete a legacy chunk cache region"
                );
            }
        }
    }

    outcome
}

/// Total size of the files under `dir`, not following symbolic links.
/// Unreadable parts count as zero.
fn tree_size(dir: &Path) -> u64 {
    fn walk(dir: &Path) -> io::Result<u64> {
        let mut total = 0;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                total += walk(&entry.path()).unwrap_or(0);
            } else if file_type.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        Ok(total)
    }
    walk(dir).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; len]).unwrap();
    }

    #[test]
    fn removes_root_regions_and_nothing_else() {
        let root = tempdir().unwrap();
        let root = root.path();

        // The pre-#398 chunk tier: regions directly in the root.
        write(&root.join("+37-123").join("chunk_16_1_2_3_4.cache"), 100);
        write(&root.join("-05+010").join("chunk_16_5_6_7_8.cache"), 50);
        fs::create_dir_all(root.join("+00+000")).unwrap();

        // Everything else in the root.
        let keep = [
            root.join("Bing Maps")
                .join("dds")
                .join("+37-123")
                .join("tile_15_1_2.cache"),
            root.join("Bing Maps")
                .join("chunks")
                .join("+37-123")
                .join("chunk_16_1_2_3_4.cache"),
            root.join("night").join("+37-123").join("lit_16_1_2.cache"),
            root.join("tmp").join("install_x").join("part.000"),
            root.join("scenery_index.cache"),
            root.join("ortho_union_index.cache"),
            root.join("+37-123.cache"),
            root.join("unrelated").join("+37-123").join("file"),
        ];
        for path in &keep {
            write(path, 10);
        }

        let outcome = remove_legacy_chunk_regions(root);

        assert_eq!(
            outcome,
            LegacyChunkCleanup {
                removed: 3,
                bytes_freed: 150,
                failed: 0,
                kept: 0,
            }
        );
        assert!(!root.join("+37-123").exists());
        assert!(!root.join("-05+010").exists());
        assert!(!root.join("+00+000").exists());
        for path in &keep {
            assert!(path.exists(), "{} must be kept", path.display());
        }
    }

    /// A region name is not proof: `cache.directory` may hold the user's own
    /// folders, scenery among them, named like regions.
    #[test]
    fn a_region_holding_anything_but_cache_files_is_kept() {
        let root = tempdir().unwrap();
        let root = root.path();
        write(&root.join("+30-120").join("+30-120.dsf"), 10);
        write(&root.join("+31-120").join("chunk_16_1_2_3_4.cache"), 10);
        write(&root.join("+31-120").join("notes.txt"), 10);
        write(&root.join("+32-120").join("chunk_16_1_2_3_4.cache"), 10);

        let outcome = remove_legacy_chunk_regions(root);

        assert_eq!(outcome.removed, 1, "only the region of cache files");
        assert_eq!(outcome.kept, 2);
        assert!(root.join("+30-120/+30-120.dsf").exists());
        assert!(root.join("+31-120/notes.txt").exists());
        assert!(!root.join("+32-120").exists());
    }

    #[test]
    fn a_file_with_a_region_name_is_not_a_region() {
        let root = tempdir().unwrap();
        write(&root.path().join("+37-123"), 10);

        let outcome = remove_legacy_chunk_regions(root.path());

        assert_eq!(outcome, LegacyChunkCleanup::default());
        assert!(root.path().join("+37-123").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_with_a_region_name_is_left_alone() {
        let root = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        write(&elsewhere.path().join("keep.cache"), 10);
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("+37-123")).unwrap();

        let outcome = remove_legacy_chunk_regions(root.path());

        assert_eq!(outcome, LegacyChunkCleanup::default());
        assert!(elsewhere.path().join("keep.cache").exists());
    }

    #[test]
    fn an_empty_directory_is_a_no_op() {
        let root = tempdir().unwrap();
        assert_eq!(
            remove_legacy_chunk_regions(root.path()),
            LegacyChunkCleanup::default()
        );
        assert!(root.path().exists());
    }

    #[test]
    fn a_missing_directory_is_a_no_op() {
        let root = tempdir().unwrap();
        let missing = root.path().join("not-there");
        assert_eq!(
            remove_legacy_chunk_regions(&missing),
            LegacyChunkCleanup::default()
        );
        assert!(!missing.exists());
    }
}
