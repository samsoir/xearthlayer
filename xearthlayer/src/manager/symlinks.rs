//! Symlink management for overlay packages.
//!
//! This module handles creating and removing symlinks for overlay packages
//! in the X-Plane Custom Scenery directory.
//!
//! # Consolidated Overlays
//!
//! The preferred approach is to use [`create_consolidated_overlay`] which merges
//! all overlay packages into a single `yzXEL_overlay` folder. This provides:
//!
//! - Single scenery entry in X-Plane
//! - Clear precedence (alphabetical by region)
//! - Easier scenery management

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::package::{self, PackageType};

use super::local::LocalPackageStore;
use super::{ManagerError, ManagerResult};

/// Marker file name used to identify XEarthLayer symlinks.
const SYMLINK_MARKER: &str = ".xearthlayer_symlink";

/// Create a per-region symlink for an overlay package in the Custom Scenery directory.
///
/// This approach is superseded by the consolidated overlay (`yzXEL_overlay/`).
/// Kept for test setup only.
#[cfg(test)]
fn create_overlay_symlink(
    package_path: &Path,
    custom_scenery_path: &Path,
) -> ManagerResult<PathBuf> {
    // Verify package exists
    if !package_path.exists() {
        return Err(ManagerError::InvalidPath(format!(
            "package does not exist: {}",
            package_path.display()
        )));
    }

    // Verify Custom Scenery directory exists
    if !custom_scenery_path.exists() {
        return Err(ManagerError::InvalidPath(format!(
            "Custom Scenery directory does not exist: {}",
            custom_scenery_path.display()
        )));
    }

    // Get the package folder name
    let folder_name = package_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            ManagerError::InvalidPath(format!("invalid package path: {}", package_path.display()))
        })?;

    let symlink_path = custom_scenery_path.join(folder_name);

    // Check if something already exists at the target
    if symlink_path.exists() || symlink_path.symlink_metadata().is_ok() {
        // Check if it's already our symlink
        if is_xearthlayer_symlink(&symlink_path)? {
            // Remove old symlink and recreate
            fs::remove_file(&symlink_path).map_err(|e| ManagerError::WriteFailed {
                path: symlink_path.clone(),
                source: e,
            })?;
        } else if symlink_path.is_symlink() {
            // It's a symlink but not ours - remove it anyway (might be stale)
            fs::remove_file(&symlink_path).map_err(|e| ManagerError::WriteFailed {
                path: symlink_path.clone(),
                source: e,
            })?;
        } else {
            // It's a real file/directory - don't touch it
            return Err(ManagerError::SymlinkFailed {
                source: symlink_path.clone(),
                target: package_path.to_path_buf(),
                reason: "a file or directory already exists at the symlink location".to_string(),
            });
        }
    }

    // Create the symlink
    symlink(package_path, &symlink_path).map_err(|e| ManagerError::SymlinkFailed {
        source: package_path.to_path_buf(),
        target: symlink_path.clone(),
        reason: e.to_string(),
    })?;

    // Create marker file in the package to identify this as an XEarthLayer-managed symlink
    let marker_path = package_path.join(SYMLINK_MARKER);
    fs::write(&marker_path, symlink_path.display().to_string())
        .map_err(|e| {
            // Best effort - if marker fails, still return success
            tracing::warn!("Failed to create symlink marker: {}", e);
            ManagerError::WriteFailed {
                path: marker_path,
                source: e,
            }
        })
        .ok();

    Ok(symlink_path)
}

/// Remove a per-region overlay symlink from the Custom Scenery directory.
///
/// # Deprecation
///
/// Per-region overlay symlinks are superseded by the consolidated overlay
/// (`yzXEL_overlay/`). This function is retained only for cleaning up stale
/// per-region symlinks from prior versions. Remove in v0.4.0 once all users
/// have migrated.
///
/// # Returns
///
/// `Ok(true)` if symlink was removed, `Ok(false)` if no symlink existed.
///
/// # Safety
///
/// This only removes symlinks, never real directories. If a real directory
/// exists at the expected symlink path, it will not be removed.
pub fn remove_overlay_symlink(region: &str, custom_scenery_path: &Path) -> ManagerResult<bool> {
    let folder_name = package::package_mountpoint(region, PackageType::Overlay);
    let symlink_path = custom_scenery_path.join(&folder_name);

    // Check if symlink exists
    if symlink_path.symlink_metadata().is_err() {
        return Ok(false);
    }

    // Safety check: only remove if it's a symlink
    if !symlink_path.is_symlink() {
        return Err(ManagerError::SymlinkFailed {
            source: symlink_path.clone(),
            target: PathBuf::new(),
            reason: "target is not a symlink, refusing to remove".to_string(),
        });
    }

    // Remove the symlink
    fs::remove_file(&symlink_path).map_err(|e| ManagerError::WriteFailed {
        path: symlink_path,
        source: e,
    })?;

    Ok(true)
}

/// Check if a path is an XEarthLayer-managed symlink.
///
/// This checks if the symlink points to a directory containing our marker file.
/// Only used by `create_overlay_symlink` which is test-only.
#[cfg(test)]
fn is_xearthlayer_symlink(path: &Path) -> ManagerResult<bool> {
    if !path.is_symlink() {
        return Ok(false);
    }

    // Read symlink target
    let target = fs::read_link(path).map_err(|e| ManagerError::ReadFailed {
        path: path.to_path_buf(),
        source: e,
    })?;

    // Check for marker file
    let marker_path = target.join(SYMLINK_MARKER);
    Ok(marker_path.exists())
}

/// Get the symlink path for a per-region overlay package.
#[cfg(test)]
fn overlay_symlink_path(region: &str, custom_scenery_path: &Path) -> PathBuf {
    let folder_name = package::package_mountpoint(region, PackageType::Overlay);
    custom_scenery_path.join(folder_name)
}

/// Check if a per-region overlay symlink exists.
#[cfg(test)]
fn overlay_symlink_exists(region: &str, custom_scenery_path: &Path) -> bool {
    let symlink_path = overlay_symlink_path(region, custom_scenery_path);
    symlink_path.is_symlink()
}

/// Consolidated overlay folder name.
pub const CONSOLIDATED_OVERLAY_NAME: &str = "yzXEL_overlay";

/// Result of creating the consolidated overlay folder.
#[derive(Debug)]
pub struct ConsolidatedOverlayResult {
    /// Path to the consolidated overlay folder.
    pub path: PathBuf,
    /// Number of overlay packages included.
    pub package_count: usize,
    /// Total number of DSF files symlinked.
    pub file_count: usize,
    /// Regions included (sorted alphabetically).
    pub regions: Vec<String>,
    /// Whether the operation succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
}

impl ConsolidatedOverlayResult {
    /// Create a successful result.
    pub fn success(
        path: PathBuf,
        package_count: usize,
        file_count: usize,
        regions: Vec<String>,
    ) -> Self {
        Self {
            path,
            package_count,
            file_count,
            regions,
            success: true,
            error: None,
        }
    }

    /// Create a failure result.
    pub fn failure(path: PathBuf, error: String) -> Self {
        Self {
            path,
            package_count: 0,
            file_count: 0,
            regions: Vec::new(),
            success: false,
            error: Some(error),
        }
    }

    /// Create a result indicating no overlay packages found.
    pub fn no_packages() -> Self {
        Self {
            path: PathBuf::new(),
            package_count: 0,
            file_count: 0,
            regions: Vec::new(),
            success: true,
            error: None,
        }
    }
}

/// Scan Custom Scenery for any per-region overlay symlinks and remove them.
///
/// Scans the directory for symlinks matching the `yzXEL_<region>_overlay` pattern
/// (but not `yzXEL_overlay` which is the consolidated folder). This catches symlinks
/// for regions that are no longer installed — not just currently known regions.
fn cleanup_all_per_region_overlay_symlinks(custom_scenery_path: &Path) {
    let prefix = "yzXEL_";
    let suffix = "_overlay";

    let entries = match fs::read_dir(custom_scenery_path) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Match yzXEL_<region>_overlay pattern (but not yzXEL_overlay which is the consolidated folder)
        if name_str.starts_with(prefix)
            && name_str.ends_with(suffix)
            && name_str != CONSOLIDATED_OVERLAY_NAME
        {
            let path = entry.path();
            if path.is_symlink() {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        tracing::info!(name = %name_str, "Removed stale per-region overlay symlink")
                    }
                    Err(e) => {
                        tracing::warn!(name = %name_str, error = %e, "Failed to remove stale per-region overlay symlink")
                    }
                }
            }
        }
    }
}

/// Create a consolidated overlay folder merging all installed overlay packages.
///
/// This creates a single `yzXEL_overlay` folder in Custom Scenery containing
/// symlinks to all DSF files from installed overlay packages. When multiple
/// packages have overlapping files, the first package (alphabetically by region)
/// wins.
///
/// # Directory Structure
///
/// ```text
/// Custom Scenery/yzXEL_overlay/
/// └── Earth nav data/
///     ├── +30-120/
///     │   ├── +33-119.dsf → ~/.xearthlayer/packages/yzXEL_eu_overlay/Earth nav data/+30-120/+33-119.dsf
///     │   └── ...
///     └── +40-080/
///         └── ...
/// ```
///
/// # Arguments
///
/// * `store` - Local package store for discovering installed overlay packages
/// * `custom_scenery_path` - Path to X-Plane's Custom Scenery directory
///
/// # Returns
///
/// `ConsolidatedOverlayResult` with details about the created folder.
pub fn create_consolidated_overlay(
    store: &LocalPackageStore,
    custom_scenery_path: &Path,
) -> ConsolidatedOverlayResult {
    // Verify Custom Scenery directory exists
    if !custom_scenery_path.exists() {
        return ConsolidatedOverlayResult::failure(
            PathBuf::new(),
            format!(
                "Custom Scenery directory does not exist: {}",
                custom_scenery_path.display()
            ),
        );
    }

    // Discover overlay packages
    let packages = match store.list() {
        Ok(p) => p,
        Err(e) => {
            return ConsolidatedOverlayResult::failure(
                PathBuf::new(),
                format!("Failed to list packages: {}", e),
            );
        }
    };

    // Filter to overlay packages and sort alphabetically by region
    let mut overlay_packages: Vec<_> = packages
        .into_iter()
        .filter(|p| p.package_type() == PackageType::Overlay)
        .collect();
    overlay_packages.sort_by_key(|a| a.region().to_lowercase());

    if overlay_packages.is_empty() {
        tracing::debug!("No overlay packages found, cleaning up overlay artifacts");
        // Remove consolidated overlay folder if it exists
        if let Err(e) = remove_consolidated_overlay(custom_scenery_path) {
            tracing::warn!(error = %e, "Failed to remove consolidated overlay during cleanup");
        }
        // Clean up any stale per-region overlay symlinks
        cleanup_all_per_region_overlay_symlinks(custom_scenery_path);
        return ConsolidatedOverlayResult::no_packages();
    }

    let regions: Vec<String> = overlay_packages
        .iter()
        .map(|p| p.region().to_string())
        .collect();

    // Create consolidated overlay folder
    let consolidated_path = custom_scenery_path.join(CONSOLIDATED_OVERLAY_NAME);
    let earth_nav_data_path = consolidated_path.join("Earth nav data");

    // Clean up existing consolidated folder if it exists
    if consolidated_path.exists() {
        if let Err(e) = fs::remove_dir_all(&consolidated_path) {
            return ConsolidatedOverlayResult::failure(
                consolidated_path,
                format!("Failed to remove existing consolidated folder: {}", e),
            );
        }
    }

    // Create directory structure
    if let Err(e) = fs::create_dir_all(&earth_nav_data_path) {
        return ConsolidatedOverlayResult::failure(
            consolidated_path,
            format!("Failed to create consolidated folder: {}", e),
        );
    }

    // Track which files we've already symlinked (for collision detection)
    let mut symlinked_files: HashSet<PathBuf> = HashSet::new();
    let mut file_count = 0;

    // Process each overlay package in priority order
    for package in &overlay_packages {
        let package_earth_nav = package.path.join("Earth nav data");
        if !package_earth_nav.exists() {
            tracing::warn!(
                region = package.region(),
                path = %package.path.display(),
                "Overlay package missing Earth nav data directory"
            );
            continue;
        }

        // Scan 10° grid folders
        let grid_folders = match fs::read_dir(&package_earth_nav) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    region = package.region(),
                    error = %e,
                    "Failed to read Earth nav data directory"
                );
                continue;
            }
        };

        for grid_entry in grid_folders.flatten() {
            let grid_path = grid_entry.path();
            if !grid_path.is_dir() {
                continue;
            }

            let grid_name = match grid_path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };

            // Create 10° grid folder in consolidated overlay
            let consolidated_grid_path = earth_nav_data_path.join(&grid_name);
            if !consolidated_grid_path.exists() {
                if let Err(e) = fs::create_dir(&consolidated_grid_path) {
                    tracing::warn!(
                        grid = grid_name,
                        error = %e,
                        "Failed to create grid folder"
                    );
                    continue;
                }
            }

            // Symlink DSF files in this grid folder
            let dsf_files = match fs::read_dir(&grid_path) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for dsf_entry in dsf_files.flatten() {
                let dsf_path = dsf_entry.path();
                if !dsf_path.is_file() {
                    continue;
                }

                // Only process .dsf files
                if dsf_path.extension().is_none_or(|e| e != "dsf") {
                    continue;
                }

                let dsf_name = match dsf_path.file_name() {
                    Some(n) => n.to_string_lossy().to_string(),
                    None => continue,
                };

                // Virtual path for collision detection
                let virtual_path = PathBuf::from(&grid_name).join(&dsf_name);

                // Skip if already symlinked (first package wins)
                if symlinked_files.contains(&virtual_path) {
                    tracing::trace!(
                        file = %virtual_path.display(),
                        region = package.region(),
                        "Skipping overlapping DSF (already symlinked from higher priority package)"
                    );
                    continue;
                }

                // Create symlink
                let symlink_path = consolidated_grid_path.join(&dsf_name);
                if let Err(e) = symlink(&dsf_path, &symlink_path) {
                    tracing::warn!(
                        source = %dsf_path.display(),
                        target = %symlink_path.display(),
                        error = %e,
                        "Failed to create DSF symlink"
                    );
                    continue;
                }

                symlinked_files.insert(virtual_path);
                file_count += 1;
            }
        }
    }

    // Create marker file
    let marker_path = consolidated_path.join(SYMLINK_MARKER);
    if let Err(e) = fs::write(&marker_path, "consolidated overlay") {
        tracing::warn!(error = %e, "Failed to create symlink marker");
    }

    tracing::info!(
        path = %consolidated_path.display(),
        packages = overlay_packages.len(),
        files = file_count,
        "Created consolidated overlay folder"
    );

    // Clean up all stale per-region overlay symlinks (including regions no longer installed).
    cleanup_all_per_region_overlay_symlinks(custom_scenery_path);

    ConsolidatedOverlayResult::success(
        consolidated_path,
        overlay_packages.len(),
        file_count,
        regions,
    )
}

/// Remove the consolidated overlay folder.
///
/// # Safety
///
/// This removes the entire `yzXEL_overlay` folder, but only if it contains
/// our marker file indicating it was created by XEarthLayer.
pub fn remove_consolidated_overlay(custom_scenery_path: &Path) -> ManagerResult<bool> {
    let consolidated_path = custom_scenery_path.join(CONSOLIDATED_OVERLAY_NAME);

    if !consolidated_path.exists() {
        return Ok(false);
    }

    // Safety check: only remove if it has our marker
    let marker_path = consolidated_path.join(SYMLINK_MARKER);
    if !marker_path.exists() {
        return Err(ManagerError::SymlinkFailed {
            source: consolidated_path,
            target: PathBuf::new(),
            reason: "consolidated overlay folder is missing marker file, refusing to remove"
                .to_string(),
        });
    }

    fs::remove_dir_all(&consolidated_path).map_err(|e| ManagerError::WriteFailed {
        path: consolidated_path,
        source: e,
    })?;

    Ok(true)
}

/// Check if consolidated overlay exists.
pub fn consolidated_overlay_exists(custom_scenery_path: &Path) -> bool {
    let consolidated_path = custom_scenery_path.join(CONSOLIDATED_OVERLAY_NAME);
    consolidated_path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_mock_overlay_package(dir: &Path, region: &str) -> PathBuf {
        let folder_name = package::package_mountpoint(region, PackageType::Overlay);
        let package_dir = dir.join(&folder_name);
        fs::create_dir_all(&package_dir).unwrap();

        // Create a dummy file
        fs::write(package_dir.join("test.dsf"), "test").unwrap();

        package_dir
    }

    #[test]
    fn test_create_overlay_symlink() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        let package_path = create_mock_overlay_package(packages_dir.path(), "na");

        let result = create_overlay_symlink(&package_path, scenery_dir.path());
        assert!(result.is_ok());

        let symlink_path = result.unwrap();
        assert!(symlink_path.is_symlink());
        assert!(symlink_path.join("test.dsf").exists());
    }

    #[test]
    fn test_create_overlay_symlink_replaces_existing() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        let package_path = create_mock_overlay_package(packages_dir.path(), "na");

        // Create first symlink
        let symlink1 = create_overlay_symlink(&package_path, scenery_dir.path()).unwrap();
        assert!(symlink1.is_symlink());

        // Create again - should replace
        let symlink2 = create_overlay_symlink(&package_path, scenery_dir.path()).unwrap();
        assert!(symlink2.is_symlink());
        assert_eq!(symlink1, symlink2);
    }

    #[test]
    fn test_create_overlay_symlink_fails_if_dir_exists() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        let package_path = create_mock_overlay_package(packages_dir.path(), "na");

        // Create a real directory at the symlink location
        let blocking_dir = scenery_dir.path().join("yzXEL_na_overlay");
        fs::create_dir(&blocking_dir).unwrap();

        let result = create_overlay_symlink(&package_path, scenery_dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_overlay_symlink() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        let package_path = create_mock_overlay_package(packages_dir.path(), "na");

        // Create symlink
        create_overlay_symlink(&package_path, scenery_dir.path()).unwrap();
        assert!(overlay_symlink_exists("na", scenery_dir.path()));

        // Remove symlink
        let result = remove_overlay_symlink("na", scenery_dir.path());
        assert!(result.is_ok());
        assert!(result.unwrap());
        assert!(!overlay_symlink_exists("na", scenery_dir.path()));
    }

    #[test]
    fn test_remove_overlay_symlink_nonexistent() {
        let scenery_dir = TempDir::new().unwrap();

        let result = remove_overlay_symlink("na", scenery_dir.path());
        assert!(result.is_ok());
        assert!(!result.unwrap()); // Returns false when nothing to remove
    }

    #[test]
    fn test_remove_overlay_symlink_refuses_real_dir() {
        let scenery_dir = TempDir::new().unwrap();

        // Create a real directory
        let dir_path = scenery_dir.path().join("yzXEL_na_overlay");
        fs::create_dir(&dir_path).unwrap();

        let result = remove_overlay_symlink("na", scenery_dir.path());
        assert!(result.is_err());

        // Directory should still exist
        assert!(dir_path.exists());
    }

    #[test]
    fn test_overlay_symlink_path() {
        let scenery_dir = PathBuf::from("/path/to/Custom Scenery");
        let path = overlay_symlink_path("na", &scenery_dir);
        assert_eq!(
            path,
            PathBuf::from("/path/to/Custom Scenery/yzXEL_na_overlay")
        );
    }

    #[test]
    fn test_overlay_symlink_exists() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        assert!(!overlay_symlink_exists("na", scenery_dir.path()));

        let package_path = create_mock_overlay_package(packages_dir.path(), "na");
        create_overlay_symlink(&package_path, scenery_dir.path()).unwrap();

        assert!(overlay_symlink_exists("na", scenery_dir.path()));
    }

    // =========================================================================
    // Consolidated overlay tests
    // =========================================================================

    fn create_mock_overlay_with_dsf(packages_dir: &Path, region: &str) -> PathBuf {
        let folder_name = package::package_mountpoint(region, PackageType::Overlay);
        let package_dir = packages_dir.join(&folder_name);

        // Create Earth nav data structure with DSF files
        let grid_dir = package_dir.join("Earth nav data/+30-120");
        fs::create_dir_all(&grid_dir).unwrap();
        fs::write(grid_dir.join("+33-119.dsf"), "fake dsf").unwrap();
        fs::write(grid_dir.join("+34-118.dsf"), "fake dsf").unwrap();

        // Create package marker (note: two spaces between title and version)
        let type_char = "Y";
        let metadata = format!(
            "REGIONAL SCENERY PACKAGE\n1.0.0\n{}  1.0.0\n2024-01-01T00:00:00Z\n{}\n{}\ntest.tar.gz\n1\n\nabc123  test.tar.gz  http://example.com/test.tar.gz\n",
            region.to_uppercase(),
            type_char,
            folder_name
        );
        fs::write(
            package_dir.join("xearthlayer_scenery_package.txt"),
            metadata,
        )
        .unwrap();

        package_dir
    }

    #[test]
    fn test_consolidated_overlay_no_packages() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        let store = LocalPackageStore::new(packages_dir.path());
        let result = create_consolidated_overlay(&store, scenery_dir.path());

        assert!(result.success);
        assert_eq!(result.package_count, 0);
        assert_eq!(result.file_count, 0);
        assert!(result.path.as_os_str().is_empty());
    }

    #[test]
    fn test_consolidated_overlay_no_packages_cleans_up_artifacts() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        // Create a stale consolidated overlay folder with marker
        let consolidated = scenery_dir.path().join(CONSOLIDATED_OVERLAY_NAME);
        fs::create_dir_all(&consolidated).unwrap();
        fs::write(consolidated.join(SYMLINK_MARKER), "consolidated overlay").unwrap();

        // Create stale per-region symlinks
        let fake_target = packages_dir.path().join("fake_na");
        fs::create_dir(&fake_target).unwrap();
        symlink(&fake_target, scenery_dir.path().join("yzXEL_na_overlay")).unwrap();

        let fake_target_eu = packages_dir.path().join("fake_eu");
        fs::create_dir(&fake_target_eu).unwrap();
        symlink(&fake_target_eu, scenery_dir.path().join("yzXEL_eu_overlay")).unwrap();

        assert!(consolidated.exists());
        assert!(scenery_dir.path().join("yzXEL_na_overlay").is_symlink());
        assert!(scenery_dir.path().join("yzXEL_eu_overlay").is_symlink());

        // Rebuild with empty store — should clean up everything
        let store = LocalPackageStore::new(packages_dir.path());
        let result = create_consolidated_overlay(&store, scenery_dir.path());

        assert!(result.success);
        assert_eq!(result.package_count, 0);

        // All artifacts should be removed
        assert!(
            !consolidated.exists(),
            "Consolidated overlay folder should have been removed"
        );
        assert!(
            !scenery_dir.path().join("yzXEL_na_overlay").exists(),
            "Stale na per-region symlink should have been removed"
        );
        assert!(
            !scenery_dir.path().join("yzXEL_eu_overlay").exists(),
            "Stale eu per-region symlink should have been removed"
        );
    }

    #[test]
    fn test_consolidated_overlay_single_package() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        create_mock_overlay_with_dsf(packages_dir.path(), "na");

        let store = LocalPackageStore::new(packages_dir.path());
        let result = create_consolidated_overlay(&store, scenery_dir.path());

        assert!(result.success, "Error: {:?}", result.error);
        assert_eq!(result.package_count, 1);
        assert_eq!(result.file_count, 2); // Two DSF files
        assert_eq!(result.regions, vec!["NA".to_string()]);

        // Verify folder structure
        let consolidated_path = scenery_dir.path().join(CONSOLIDATED_OVERLAY_NAME);
        assert!(consolidated_path.exists());
        assert!(consolidated_path
            .join("Earth nav data/+30-120/+33-119.dsf")
            .is_symlink());
        assert!(consolidated_path
            .join("Earth nav data/+30-120/+34-118.dsf")
            .is_symlink());
    }

    #[test]
    fn test_consolidated_overlay_multiple_packages() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        create_mock_overlay_with_dsf(packages_dir.path(), "eu");
        create_mock_overlay_with_dsf(packages_dir.path(), "na");

        let store = LocalPackageStore::new(packages_dir.path());
        let result = create_consolidated_overlay(&store, scenery_dir.path());

        assert!(result.success, "Error: {:?}", result.error);
        assert_eq!(result.package_count, 2);
        // Both packages have same grid, but first (eu) wins, so only 2 files
        assert_eq!(result.file_count, 2);
        // Regions sorted alphabetically
        assert_eq!(result.regions, vec!["EU".to_string(), "NA".to_string()]);
    }

    #[test]
    fn test_consolidated_overlay_exists() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        assert!(!consolidated_overlay_exists(scenery_dir.path()));

        create_mock_overlay_with_dsf(packages_dir.path(), "na");
        let store = LocalPackageStore::new(packages_dir.path());
        create_consolidated_overlay(&store, scenery_dir.path());

        assert!(consolidated_overlay_exists(scenery_dir.path()));
    }

    #[test]
    fn test_remove_consolidated_overlay() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        create_mock_overlay_with_dsf(packages_dir.path(), "na");
        let store = LocalPackageStore::new(packages_dir.path());
        create_consolidated_overlay(&store, scenery_dir.path());

        assert!(consolidated_overlay_exists(scenery_dir.path()));

        let result = remove_consolidated_overlay(scenery_dir.path());
        assert!(result.is_ok());
        assert!(result.unwrap());
        assert!(!consolidated_overlay_exists(scenery_dir.path()));
    }

    #[test]
    fn test_remove_consolidated_overlay_nonexistent() {
        let scenery_dir = TempDir::new().unwrap();

        let result = remove_consolidated_overlay(scenery_dir.path());
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_remove_consolidated_overlay_refuses_without_marker() {
        let scenery_dir = TempDir::new().unwrap();

        // Create folder without marker
        let fake_overlay = scenery_dir.path().join(CONSOLIDATED_OVERLAY_NAME);
        fs::create_dir(&fake_overlay).unwrap();

        let result = remove_consolidated_overlay(scenery_dir.path());
        assert!(result.is_err());

        // Should still exist
        assert!(fake_overlay.exists());
    }

    #[test]
    fn test_consolidated_overlay_cleans_up_per_region_symlinks() {
        let packages_dir = TempDir::new().unwrap();
        let scenery_dir = TempDir::new().unwrap();

        // Create overlay packages
        create_mock_overlay_with_dsf(packages_dir.path(), "na");
        create_mock_overlay_with_dsf(packages_dir.path(), "eu");

        // Create stale per-region symlinks (simulating old behavior)
        let na_package = create_mock_overlay_package(packages_dir.path(), "na");
        create_overlay_symlink(&na_package, scenery_dir.path()).unwrap();
        assert!(overlay_symlink_exists("na", scenery_dir.path()));

        let eu_package = create_mock_overlay_package(packages_dir.path(), "eu");
        create_overlay_symlink(&eu_package, scenery_dir.path()).unwrap();
        assert!(overlay_symlink_exists("eu", scenery_dir.path()));

        // Build consolidated overlay — should clean up per-region symlinks
        let store = LocalPackageStore::new(packages_dir.path());
        let result = create_consolidated_overlay(&store, scenery_dir.path());

        assert!(result.success, "Error: {:?}", result.error);
        assert!(consolidated_overlay_exists(scenery_dir.path()));

        // Per-region symlinks should be removed
        assert!(
            !overlay_symlink_exists("na", scenery_dir.path()),
            "Stale na per-region symlink should have been removed"
        );
        assert!(
            !overlay_symlink_exists("eu", scenery_dir.path()),
            "Stale eu per-region symlink should have been removed"
        );
    }

    #[test]
    fn test_consolidated_overlay_result_types() {
        let result = ConsolidatedOverlayResult::success(
            PathBuf::from("/test"),
            2,
            10,
            vec!["eu".to_string(), "na".to_string()],
        );
        assert!(result.success);
        assert!(result.error.is_none());

        let result =
            ConsolidatedOverlayResult::failure(PathBuf::from("/test"), "test error".to_string());
        assert!(!result.success);
        assert_eq!(result.error, Some("test error".to_string()));

        let result = ConsolidatedOverlayResult::no_packages();
        assert!(result.success);
        assert_eq!(result.package_count, 0);
    }
}
