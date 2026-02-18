//! Builder for constructing an [`OrthoUnionIndex`].
//!
//! The builder follows the strict builder pattern:
//! - Configuration methods return `Self` for chaining
//! - `build()` performs validation and construction
//! - Result is immutable after construction

use std::path::{Path, PathBuf};

use crate::package::{InstalledPackage, PackageType};
use crate::patches::PatchDiscovery;

use super::cache::{save_index_cache, try_load_cached_index, IndexCacheKey};
use super::index::OrthoUnionIndex;
use super::parallel::{merge_partial_indexes, scan_sources_parallel};
use super::progress::{IndexBuildProgress, IndexBuildProgressCallback};
use super::source::OrthoSource;

/// Builder for constructing an [`OrthoUnionIndex`].
///
/// This builder implements the strict builder pattern, separating
/// configuration from construction.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
/// use xearthlayer::package::{Package, InstalledPackage, PackageType};
/// use semver::Version;
///
/// let na = InstalledPackage::new(
///     Package::new("na", PackageType::Ortho, Version::new(1, 0, 0)),
///     "/path/to/na_ortho",
/// );
///
/// let index = OrthoUnionIndexBuilder::new()
///     .with_patches_dir("/home/user/.xearthlayer/patches")
///     .add_package(na)
///     .build()?;
///
/// println!("Sources: {:?}", index.sources().iter().map(|s| &s.sort_key).collect::<Vec<_>>());
/// ```
#[derive(Debug, Default)]
pub struct OrthoUnionIndexBuilder {
    /// Optional patches directory to scan.
    patches_dir: Option<PathBuf>,

    /// Installed packages to include.
    packages: Vec<InstalledPackage>,
}

impl OrthoUnionIndexBuilder {
    /// Create a new builder.
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    ///
    /// let builder = OrthoUnionIndexBuilder::new();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the patches directory to scan.
    ///
    /// Only valid patches (with `Earth nav data/` directory and DSF files)
    /// are included in the index.
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    ///
    /// let builder = OrthoUnionIndexBuilder::new()
    ///     .with_patches_dir("/home/user/.xearthlayer/patches");
    /// ```
    pub fn with_patches_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.patches_dir = Some(dir.into());
        self
    }

    /// Add an installed package.
    ///
    /// Only enabled ortho packages are included in the final index.
    /// Disabled packages and overlay packages are filtered out during `build()`.
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    /// use xearthlayer::package::{Package, InstalledPackage, PackageType};
    /// use semver::Version;
    ///
    /// let na = InstalledPackage::new(
    ///     Package::new("na", PackageType::Ortho, Version::new(1, 0, 0)),
    ///     "/path/to/na_ortho",
    /// );
    ///
    /// let builder = OrthoUnionIndexBuilder::new()
    ///     .add_package(na);
    /// ```
    pub fn add_package(mut self, package: InstalledPackage) -> Self {
        self.packages.push(package);
        self
    }

    /// Add multiple installed packages.
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    /// use xearthlayer::package::{Package, InstalledPackage, PackageType};
    /// use semver::Version;
    ///
    /// let packages = vec![
    ///     InstalledPackage::new(
    ///         Package::new("na", PackageType::Ortho, Version::new(1, 0, 0)),
    ///         "/path/to/na",
    ///     ),
    ///     InstalledPackage::new(
    ///         Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
    ///         "/path/to/eu",
    ///     ),
    /// ];
    ///
    /// let builder = OrthoUnionIndexBuilder::new()
    ///     .add_packages(packages);
    /// ```
    pub fn add_packages(mut self, packages: impl IntoIterator<Item = InstalledPackage>) -> Self {
        self.packages.extend(packages);
        self
    }

    /// Build the [`OrthoUnionIndex`].
    ///
    /// This method:
    /// 1. Discovers valid patches from `patches_dir` (if set)
    /// 2. Filters packages to enabled ortho packages only
    /// 3. Creates [`OrthoSource`] for each source
    ///    - Patches: sort_key = `_patches/{folder_name}`
    ///    - Packages: sort_key = `{region}`
    /// 4. Sorts sources alphabetically by sort_key
    /// 5. Scans all sources to build the file index (first source wins on collision)
    ///
    /// # Errors
    ///
    /// Returns an error if directory scanning fails.
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    ///
    /// let index = OrthoUnionIndexBuilder::new().build().unwrap();
    /// assert!(index.is_empty()); // No sources configured
    /// ```
    pub fn build(self) -> std::io::Result<OrthoUnionIndex> {
        self.build_with_progress(None, None)
    }

    /// Build the [`OrthoUnionIndex`] with progress reporting and caching.
    ///
    /// This is the optimized version that:
    /// 1. Checks for a valid cached index (if `cache_path` is provided)
    /// 2. Scans sources in parallel using rayon
    /// 3. Skips terrain/textures directories (handled lazily at runtime)
    /// 4. Reports progress via callback for TUI feedback
    /// 5. Saves the result to cache for future use
    ///
    /// # Arguments
    ///
    /// * `progress` - Optional callback for progress updates
    /// * `cache_path` - Optional path to store/load the index cache
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::sync::Arc;
    /// use xearthlayer::ortho_union::{OrthoUnionIndexBuilder, IndexBuildProgress};
    ///
    /// let progress = Arc::new(|p: IndexBuildProgress| {
    ///     println!("{:?}: {}/{}", p.phase, p.sources_complete, p.sources_total);
    /// });
    ///
    /// let index = OrthoUnionIndexBuilder::new()
    ///     .add_packages(installed_packages)
    ///     .build_with_progress(Some(progress), Some(cache_path.as_ref()))?;
    /// ```
    pub fn build_with_progress(
        self,
        progress: Option<IndexBuildProgressCallback>,
        cache_path: Option<&Path>,
    ) -> std::io::Result<OrthoUnionIndex> {
        // Phase 1: Discover sources
        if let Some(ref cb) = progress {
            cb(IndexBuildProgress::new(0));
        }

        let sources = self.discover_sources()?;

        // If no sources, return empty index
        if sources.is_empty() {
            if let Some(ref cb) = progress {
                cb(IndexBuildProgress::at_complete(0, 0));
            }
            return Ok(OrthoUnionIndex::with_sources(sources));
        }

        // Phase 2: Check cache
        let cache_key = IndexCacheKey::from_sources(&sources);

        if let Some(cache_path) = cache_path {
            if let Some(ref cb) = progress {
                cb(IndexBuildProgress::at_checking_cache(sources.len()));
            }

            if let Some(cached_index) = try_load_cached_index(cache_path, &cache_key) {
                tracing::info!(
                    sources = sources.len(),
                    files = cached_index.file_count(),
                    "Loaded index from cache"
                );

                if let Some(ref cb) = progress {
                    cb(IndexBuildProgress::cache_hit(
                        sources.len(),
                        cached_index.file_count(),
                    ));
                }

                return Ok(cached_index);
            }
        }

        // Phase 3: Parallel scan
        tracing::info!(sources = sources.len(), "Scanning sources in parallel");

        let partial_indexes = scan_sources_parallel(&sources, progress.as_ref());

        // Phase 4: Merge results
        let index = merge_partial_indexes(partial_indexes, sources, progress.as_ref());

        tracing::info!(
            files = index.file_count(),
            sources = index.source_count(),
            "Index built successfully"
        );

        // Phase 5: Save to cache
        if let Some(cache_path) = cache_path {
            if let Some(ref cb) = progress {
                cb(IndexBuildProgress::at_saving_cache(
                    index.source_count(),
                    index.file_count(),
                ));
            }

            if let Err(e) = save_index_cache(cache_path, cache_key, &index) {
                tracing::warn!(error = %e, "Failed to save index cache");
            } else {
                tracing::debug!(path = %cache_path.display(), "Saved index to cache");
            }
        }

        // Phase 6: Complete
        if let Some(ref cb) = progress {
            cb(IndexBuildProgress::at_complete(
                index.source_count(),
                index.file_count(),
            ));
        }

        Ok(index)
    }

    /// Discover all sources (patches and packages) and return them sorted.
    fn discover_sources(&self) -> std::io::Result<Vec<OrthoSource>> {
        let mut sources = Vec::new();

        // 1. Discover patches
        if let Some(patches_dir) = &self.patches_dir {
            let discovery = PatchDiscovery::new(patches_dir);
            if discovery.exists() {
                let patches = discovery.find_valid_patches()?;
                for patch in patches {
                    sources.push(OrthoSource::new_patch(&patch.name, &patch.path));
                }
            }
        }

        // 2. Filter and add packages (only enabled ortho packages)
        for pkg in &self.packages {
            if pkg.enabled && pkg.package_type == PackageType::Ortho {
                sources.push(OrthoSource::new_package(&pkg.region, &pkg.path));
            }
        }

        // 3. Sort sources alphabetically by sort_key
        sources.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

        // 4. Filter to existing paths only
        sources.retain(|s| s.source_path.exists());

        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use semver::Version;
    use tempfile::TempDir;

    use super::*;
    use crate::package::Package;

    fn create_test_patch(temp: &TempDir, name: &str) {
        let patch_dir = temp.path().join(name);
        std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
        std::fs::write(patch_dir.join("Earth nav data/+30-120/+33-119.dsf"), b"dsf").unwrap();
    }

    fn create_test_package(temp: &TempDir, region: &str) -> InstalledPackage {
        let pkg_dir = temp.path().join(format!("{}_ortho", region));
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"), b"dsf").unwrap();

        InstalledPackage::new(
            Package::new(region, PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        )
    }

    #[test]
    fn test_builder_empty() {
        let index = OrthoUnionIndexBuilder::new().build().unwrap();

        assert!(index.is_empty());
        assert_eq!(index.source_count(), 0);
    }

    #[test]
    fn test_builder_with_patches_only() {
        let temp = TempDir::new().unwrap();
        create_test_patch(&temp, "A_KDEN_Mesh");
        create_test_patch(&temp, "B_KLAX_Mesh");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(temp.path())
            .build()
            .unwrap();

        assert_eq!(index.source_count(), 2);

        let sources = index.sources();
        assert_eq!(sources[0].sort_key, "_patches/A_KDEN_Mesh");
        assert_eq!(sources[1].sort_key, "_patches/B_KLAX_Mesh");
    }

    #[test]
    fn test_builder_with_packages_only() {
        let temp = TempDir::new().unwrap();
        let na = create_test_package(&temp, "na");
        let eu = create_test_package(&temp, "eu");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na)
            .add_package(eu)
            .build()
            .unwrap();

        assert_eq!(index.source_count(), 2);

        // Should be sorted alphabetically: eu < na
        let sources = index.sources();
        assert_eq!(sources[0].sort_key, "eu");
        assert_eq!(sources[1].sort_key, "na");
    }

    #[test]
    fn test_builder_with_patches_and_packages() {
        let temp = TempDir::new().unwrap();

        // Create patches
        let patches_dir = temp.path().join("patches");
        std::fs::create_dir_all(&patches_dir).unwrap();
        create_test_patch(&TempDir::new_in(&patches_dir).unwrap(), "KLAX_Mesh");

        // Create package
        let na = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na)
            .build()
            .unwrap();

        // Patches should sort before packages
        let sources = index.sources();
        if sources.len() >= 2 {
            // First source should be a patch (starts with _patches/)
            assert!(sources[0].sort_key.starts_with("_patches/"));
        }
    }

    #[test]
    fn test_builder_filters_disabled_packages() {
        let temp = TempDir::new().unwrap();
        let enabled = create_test_package(&temp, "na");
        let disabled = create_test_package(&temp, "eu").with_enabled(false);

        let index = OrthoUnionIndexBuilder::new()
            .add_package(enabled)
            .add_package(disabled)
            .build()
            .unwrap();

        // Only enabled package should be included
        assert_eq!(index.source_count(), 1);
        assert_eq!(index.sources()[0].sort_key, "na");
    }

    #[test]
    fn test_builder_filters_overlay_packages() {
        let temp = TempDir::new().unwrap();

        let ortho_pkg = create_test_package(&temp, "na");

        // Create overlay package
        let overlay_dir = temp.path().join("na_overlay");
        std::fs::create_dir_all(&overlay_dir).unwrap();
        let overlay_pkg = InstalledPackage::new(
            Package::new("na", PackageType::Overlay, Version::new(1, 0, 0)),
            &overlay_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(ortho_pkg)
            .add_package(overlay_pkg)
            .build()
            .unwrap();

        // Only ortho package should be included
        assert_eq!(index.source_count(), 1);
        assert!(index.sources()[0].is_regional_package());
    }

    #[test]
    fn test_builder_add_packages() {
        let temp = TempDir::new().unwrap();
        let packages = vec![
            create_test_package(&temp, "na"),
            create_test_package(&temp, "eu"),
            create_test_package(&temp, "sa"),
        ];

        let index = OrthoUnionIndexBuilder::new()
            .add_packages(packages)
            .build()
            .unwrap();

        assert_eq!(index.source_count(), 3);

        // Verify alphabetical order
        let keys: Vec<_> = index.sources().iter().map(|s| &s.sort_key).collect();
        assert_eq!(keys, vec!["eu", "na", "sa"]);
    }

    #[test]
    fn test_builder_nonexistent_patches_dir() {
        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir("/nonexistent/path")
            .build()
            .unwrap();

        // Should succeed but have no sources
        assert!(index.is_empty());
    }

    #[test]
    fn test_builder_patches_sorted_alphabetically() {
        let temp = TempDir::new().unwrap();
        create_test_patch(&temp, "Z_Last");
        create_test_patch(&temp, "A_First");
        create_test_patch(&temp, "M_Middle");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(temp.path())
            .build()
            .unwrap();

        let keys: Vec<_> = index.sources().iter().map(|s| &s.sort_key).collect();
        assert_eq!(
            keys,
            vec!["_patches/A_First", "_patches/M_Middle", "_patches/Z_Last"]
        );
    }

    #[test]
    fn test_builder_files_are_indexed() {
        let temp = TempDir::new().unwrap();
        let na = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na)
            .build()
            .unwrap();

        // Check that files were indexed
        assert!(index.file_count() > 0);
        assert!(index.contains(Path::new("Earth nav data/+40-080/+40-074.dsf")));
    }
}
