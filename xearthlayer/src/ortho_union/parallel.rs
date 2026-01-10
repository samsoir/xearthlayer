//! Parallel index building with optimizations.
//!
//! This module provides parallel scanning of ortho sources using rayon,
//! with optimizations to skip scanning terrain/textures directories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use super::index::{DirEntry, FileSource, OrthoUnionIndex};
use super::progress::{IndexBuildProgress, IndexBuildProgressCallback};
use super::source::OrthoSource;

/// Result of scanning a single source.
///
/// Each source is scanned independently and produces a `PartialIndex`.
/// These are then merged respecting priority order.
#[derive(Debug)]
pub struct PartialIndex {
    /// Source index (for priority ordering during merge).
    pub source_idx: usize,

    /// Source name for display/logging.
    pub source_name: String,

    /// Files found in this source.
    /// Key: virtual path (relative to source root)
    /// Value: real filesystem path
    pub files: HashMap<PathBuf, PathBuf>,

    /// Directories found in this source.
    /// Key: virtual path
    /// Value: entries in that directory
    pub directories: HashMap<PathBuf, Vec<DirEntry>>,

    /// Directories that should be handled lazily (terrain, textures).
    /// These exist but their contents are not pre-scanned.
    pub lazy_directories: Vec<PathBuf>,

    /// Total files scanned.
    pub file_count: usize,

    /// Total directories scanned.
    pub directory_count: usize,
}

impl PartialIndex {
    /// Create a new partial index for a source.
    pub fn new(source_idx: usize, source_name: impl Into<String>) -> Self {
        Self {
            source_idx,
            source_name: source_name.into(),
            files: HashMap::new(),
            directories: HashMap::new(),
            lazy_directories: Vec::new(),
            file_count: 0,
            directory_count: 0,
        }
    }

    /// Add a file to the index.
    pub fn add_file(&mut self, virtual_path: PathBuf, real_path: PathBuf) {
        self.files.insert(virtual_path, real_path);
        self.file_count += 1;
    }

    /// Add a directory entry.
    pub fn add_directory_entry(&mut self, parent: &Path, entry: DirEntry) {
        self.directories
            .entry(parent.to_path_buf())
            .or_default()
            .push(entry);
    }

    /// Ensure a directory exists in the index.
    pub fn ensure_directory(&mut self, path: &Path) {
        if !self.directories.contains_key(path) {
            self.directories.insert(path.to_path_buf(), Vec::new());
            self.directory_count += 1;
        }
    }

    /// Mark a directory as lazy (don't scan contents).
    pub fn add_lazy_directory(&mut self, path: PathBuf) {
        self.lazy_directories.push(path);
    }
}

/// Directories to skip full scanning (handled lazily).
const LAZY_DIRECTORIES: &[&str] = &["terrain", "textures"];

/// Scan a source directory with optimizations.
///
/// - Fully scans `Earth nav data/` (DSF files need priority resolution)
/// - Marks `terrain/` and `textures/` as lazy (don't scan contents)
/// - Other directories are scanned normally
pub fn scan_source_optimized(
    source_idx: usize,
    source: &OrthoSource,
    files_counter: Option<&AtomicUsize>,
) -> std::io::Result<PartialIndex> {
    let mut partial = PartialIndex::new(source_idx, &source.display_name);

    // Ensure root directory exists in index
    partial.ensure_directory(Path::new(""));

    // Read source root directory
    let entries = match std::fs::read_dir(&source.source_path) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                source = %source.display_name,
                path = %source.source_path.display(),
                error = %e,
                "Failed to read source directory"
            );
            return Ok(partial);
        }
    };

    for entry in entries.flatten() {
        let real_path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let virtual_path = PathBuf::from(&*name_str);

        if real_path.is_dir() {
            let is_lazy = LAZY_DIRECTORIES.contains(&name_str.as_ref());

            if is_lazy {
                // Mark as lazy - don't scan contents
                partial.add_lazy_directory(virtual_path.clone());

                // Add directory entry to root
                if let Ok(dir_entry) = DirEntry::from_path(&real_path) {
                    partial.add_directory_entry(Path::new(""), dir_entry);
                }

                // Add to files index so it can be resolved
                partial.add_file(virtual_path, real_path);
            } else {
                // Full scan for non-lazy directories (including "Earth nav data")
                scan_directory_recursive(&mut partial, &real_path, &virtual_path, files_counter)?;

                // Add directory entry to root
                if let Ok(dir_entry) = DirEntry::from_path(&real_path) {
                    partial.add_directory_entry(Path::new(""), dir_entry);
                }
            }
        } else {
            // Files at root level
            if let Ok(dir_entry) = DirEntry::from_path(&real_path) {
                partial.add_directory_entry(Path::new(""), dir_entry);
            }
            partial.add_file(virtual_path, real_path);

            if let Some(counter) = files_counter {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    Ok(partial)
}

/// Recursively scan a directory.
fn scan_directory_recursive(
    partial: &mut PartialIndex,
    real_dir: &Path,
    virtual_dir: &Path,
    files_counter: Option<&AtomicUsize>,
) -> std::io::Result<()> {
    // Ensure this directory exists in the index
    partial.ensure_directory(virtual_dir);

    // Add this directory as a file entry (for resolution)
    partial.add_file(virtual_dir.to_path_buf(), real_dir.to_path_buf());

    let entries = match std::fs::read_dir(real_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(
                path = %real_dir.display(),
                error = %e,
                "Failed to read directory"
            );
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let real_path = entry.path();
        let name = entry.file_name();
        let virtual_path = virtual_dir.join(&name);

        if real_path.is_dir() {
            // Recursively scan subdirectories
            scan_directory_recursive(partial, &real_path, &virtual_path, files_counter)?;

            // Add directory entry
            if let Ok(dir_entry) = DirEntry::from_path(&real_path) {
                partial.add_directory_entry(virtual_dir, dir_entry);
            }
        } else {
            // Add file entry
            if let Ok(dir_entry) = DirEntry::from_path(&real_path) {
                partial.add_directory_entry(virtual_dir, dir_entry);
            }
            partial.add_file(virtual_path, real_path);

            if let Some(counter) = files_counter {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    Ok(())
}

/// Scan all sources in parallel and report progress.
pub fn scan_sources_parallel(
    sources: &[OrthoSource],
    progress: Option<&IndexBuildProgressCallback>,
) -> Vec<PartialIndex> {
    let total_sources = sources.len();
    let completed = AtomicUsize::new(0);
    let total_files = AtomicUsize::new(0);

    // Report starting
    if let Some(cb) = progress {
        cb(IndexBuildProgress::at_scanning_start(total_sources));
    }

    // Parallel scan using rayon
    let partial_indexes: Vec<PartialIndex> = sources
        .par_iter()
        .enumerate()
        .filter_map(|(idx, source)| {
            // Report starting this source
            if let Some(cb) = progress {
                cb(IndexBuildProgress::at_scanning_source(
                    &source.display_name,
                    completed.load(Ordering::Relaxed),
                    total_sources,
                    total_files.load(Ordering::Relaxed),
                ));
            }

            // Scan the source
            let result = scan_source_optimized(idx, source, Some(&total_files));

            // Report completion
            let sources_done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(cb) = progress {
                cb(IndexBuildProgress::at_scanning_source(
                    &source.display_name,
                    sources_done,
                    total_sources,
                    total_files.load(Ordering::Relaxed),
                ));
            }

            match result {
                Ok(partial) => Some(partial),
                Err(e) => {
                    tracing::error!(
                        source = %source.display_name,
                        error = %e,
                        "Failed to scan source"
                    );
                    None
                }
            }
        })
        .collect();

    partial_indexes
}

/// Merge partial indexes into a final OrthoUnionIndex.
///
/// Sources with lower indices (higher priority) win on collision.
pub fn merge_partial_indexes(
    mut partial_indexes: Vec<PartialIndex>,
    sources: Vec<OrthoSource>,
    progress: Option<&IndexBuildProgressCallback>,
) -> OrthoUnionIndex {
    // Report merging phase
    if let Some(cb) = progress {
        let files_scanned: usize = partial_indexes.iter().map(|p| p.file_count).sum();
        cb(IndexBuildProgress::at_merging(sources.len(), files_scanned));
    }

    // Sort by source_idx to ensure priority order
    partial_indexes.sort_by_key(|p| p.source_idx);

    // Create the final index
    let mut index = OrthoUnionIndex::with_sources(sources);

    // Merge files (first source wins)
    let mut merged_files: HashMap<PathBuf, FileSource> = HashMap::new();
    let mut merged_directories: HashMap<PathBuf, Vec<DirEntry>> = HashMap::new();
    let mut total_files = 0;

    for partial in partial_indexes {
        // Merge files (first source wins on collision)
        for (virtual_path, real_path) in partial.files {
            use std::collections::hash_map::Entry;
            if let Entry::Vacant(e) = merged_files.entry(virtual_path) {
                e.insert(FileSource::new(partial.source_idx, real_path));
                total_files += 1;
            }
        }

        // Merge directories
        for (dir_path, entries) in partial.directories {
            let dir_entries = merged_directories.entry(dir_path).or_default();
            for entry in entries {
                // Only add if entry with same name doesn't exist
                if !dir_entries.iter().any(|e| e.name == entry.name) {
                    dir_entries.push(entry);
                }
            }
        }
    }

    // Set the merged data on the index
    index.set_files(merged_files);
    index.set_directories(merged_directories);
    index.set_total_files(total_files);

    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ortho_union::SourceType;
    use tempfile::TempDir;

    fn create_test_source(temp: &TempDir, name: &str) -> OrthoSource {
        let path = temp.path().join(name);
        std::fs::create_dir_all(&path).unwrap();

        // Create Earth nav data structure
        std::fs::create_dir_all(path.join("Earth nav data/+30-120")).unwrap();
        std::fs::write(
            path.join("Earth nav data/+30-120/+33-119.dsf"),
            b"dsf content",
        )
        .unwrap();

        // Create terrain directory with some files
        std::fs::create_dir_all(path.join("terrain")).unwrap();
        std::fs::write(path.join("terrain/12345_67890_ZL16.ter"), b"terrain").unwrap();
        std::fs::write(path.join("terrain/12346_67890_ZL16.ter"), b"terrain").unwrap();

        OrthoSource {
            sort_key: name.to_string(),
            display_name: name.to_string(),
            source_path: path,
            source_type: SourceType::RegionalPackage,
            enabled: true,
        }
    }

    #[test]
    fn test_scan_source_skips_terrain() {
        let temp = TempDir::new().unwrap();
        let source = create_test_source(&temp, "test");

        let partial = scan_source_optimized(0, &source, None).unwrap();

        // terrain should be in lazy_directories
        assert!(partial
            .lazy_directories
            .iter()
            .any(|p| p == Path::new("terrain")));

        // terrain directory itself should be in files (for resolution)
        assert!(partial.files.contains_key(Path::new("terrain")));

        // But terrain FILES should NOT be individually indexed
        assert!(!partial
            .files
            .contains_key(Path::new("terrain/12345_67890_ZL16.ter")));
    }

    #[test]
    fn test_scan_source_indexes_earth_nav_data() {
        let temp = TempDir::new().unwrap();
        let source = create_test_source(&temp, "test");

        let partial = scan_source_optimized(0, &source, None).unwrap();

        // Earth nav data should be fully scanned
        assert!(partial
            .files
            .contains_key(Path::new("Earth nav data/+30-120/+33-119.dsf")));
    }

    #[test]
    fn test_partial_index_add_file() {
        let mut partial = PartialIndex::new(0, "test");

        partial.add_file(
            PathBuf::from("terrain/file.ter"),
            PathBuf::from("/real/path/terrain/file.ter"),
        );

        assert_eq!(partial.file_count, 1);
        assert!(partial.files.contains_key(Path::new("terrain/file.ter")));
    }

    #[test]
    fn test_merge_partial_indexes_priority() {
        let temp = TempDir::new().unwrap();

        // Create two sources with overlapping files
        let source1 = create_test_source(&temp, "source1");
        let source2_path = temp.path().join("source2");
        std::fs::create_dir_all(source2_path.join("Earth nav data/+30-120")).unwrap();
        std::fs::write(
            source2_path.join("Earth nav data/+30-120/+33-119.dsf"),
            b"different content",
        )
        .unwrap();

        let source2 = OrthoSource {
            sort_key: "source2".to_string(),
            display_name: "source2".to_string(),
            source_path: source2_path,
            source_type: SourceType::RegionalPackage,
            enabled: true,
        };

        let partial1 = scan_source_optimized(0, &source1, None).unwrap();
        let partial2 = scan_source_optimized(1, &source2, None).unwrap();

        let sources = vec![source1, source2];
        let index = merge_partial_indexes(vec![partial1, partial2], sources, None);

        // First source should win
        let resolved = index.resolve(Path::new("Earth nav data/+30-120/+33-119.dsf"));
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().source_idx, 0);
    }
}
