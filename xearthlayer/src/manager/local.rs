//! Local package store for discovering and managing installed packages.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::sizes::SizeRecord;

use semver::Version;

use xearthlayer_package::{self as package, parse_package_metadata, PackageMetadata, PackageType};

use super::{aside, ManagerError, ManagerResult};

/// Information about an installed package.
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    /// The package metadata.
    pub metadata: PackageMetadata,
    /// Path to the package directory.
    pub path: PathBuf,
    /// Size on disk, measured the first time [`size_bytes`](Self::size_bytes)
    /// is called. Listing never measures it: `run` lists every package and
    /// shows no size, and measuring walks every file in the package (#325).
    size: OnceLock<u64>,
    /// Where a size measured here is recorded, so the next listing reads it
    /// instead of walking again (#324). `None` records nothing.
    record: Option<Arc<Mutex<SizeRecord>>>,
}

impl InstalledPackage {
    /// A package found at `path`, its size not yet measured.
    pub fn new(metadata: PackageMetadata, path: PathBuf) -> Self {
        Self {
            metadata,
            path,
            size: OnceLock::new(),
            record: None,
        }
    }

    /// A package whose size, once measured, is recorded in `record`.
    fn recording(metadata: PackageMetadata, path: PathBuf, record: Arc<Mutex<SizeRecord>>) -> Self {
        Self {
            record: Some(record),
            ..Self::new(metadata, path)
        }
    }

    /// A package whose size is already known, so it is never measured.
    pub fn with_size(metadata: PackageMetadata, path: PathBuf, size_bytes: u64) -> Self {
        Self {
            metadata,
            path,
            size: OnceLock::from(size_bytes),
            record: None,
        }
    }

    /// Size of the package on disk in bytes, 0 if it cannot be read.
    ///
    /// The first call walks every file in the package, which is slow on a
    /// large one; later calls return the same answer.
    pub fn size_bytes(&self) -> u64 {
        *self.size.get_or_init(|| {
            let size = calculate_dir_size(&self.path).unwrap_or(0);
            if let Some(record) = &self.record {
                record
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .record(&self.path, &self.metadata.package_version.to_string(), size);
            }
            size
        })
    }

    /// Get the region code from the package title.
    pub fn region(&self) -> &str {
        &self.metadata.title
    }

    /// Get the package type.
    pub fn package_type(&self) -> PackageType {
        self.metadata.package_type
    }

    /// Get the package version.
    pub fn version(&self) -> &Version {
        &self.metadata.package_version
    }
}

/// Store for managing locally installed packages.
///
/// This handles discovering, listing, and removing installed packages.
pub struct LocalPackageStore {
    /// Root directory where packages are installed.
    root: PathBuf,
}

impl LocalPackageStore {
    /// Create a new local package store.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory where packages are installed
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Get the root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Check if a package is installed.
    ///
    /// # Arguments
    ///
    /// * `region` - The region code (e.g., "na", "eu")
    /// * `package_type` - The package type (ortho or overlay)
    pub fn is_installed(&self, region: &str, package_type: PackageType) -> bool {
        let mountpoint = package_mountpoint(region, package_type);
        let package_dir = self.root.join(&mountpoint);
        let metadata_file = package_dir.join("xearthlayer_scenery_package.txt");
        metadata_file.exists()
    }

    /// Get an installed package.
    ///
    /// # Arguments
    ///
    /// * `region` - The region code
    /// * `package_type` - The package type
    pub fn get(&self, region: &str, package_type: PackageType) -> ManagerResult<InstalledPackage> {
        let mountpoint = package_mountpoint(region, package_type);
        let package_dir = self.root.join(&mountpoint);

        if !package_dir.exists() {
            return Err(ManagerError::PackageNotFound {
                region: region.to_string(),
                package_type: package_type.to_string(),
            });
        }

        let metadata_path = package_dir.join("xearthlayer_scenery_package.txt");
        let metadata_content =
            fs::read_to_string(&metadata_path).map_err(|e| ManagerError::ReadFailed {
                path: metadata_path.clone(),
                source: e,
            })?;

        let metadata = parse_package_metadata(&metadata_content).map_err(|e| {
            ManagerError::MetadataParseFailed {
                url: metadata_path.display().to_string(),
                reason: e.to_string(),
            }
        })?;

        Ok(self.sized(
            metadata,
            package_dir,
            &Arc::new(Mutex::new(self.size_record())),
        ))
    }

    /// List all installed packages.
    ///
    /// Sizes come from the record where it is current and are otherwise
    /// measured on first use, never here (#324, #325).
    pub fn list(&self) -> ManagerResult<Vec<InstalledPackage>> {
        let record = Arc::new(Mutex::new(self.size_record()));
        Ok(self
            .scan()?
            .into_iter()
            .map(|(metadata, path)| self.sized(metadata, path, &record))
            .collect())
    }

    fn size_record(&self) -> SizeRecord {
        SizeRecord::load(&self.root)
    }

    /// The package with its recorded size if that is current, else with its
    /// size measured on first use and recorded then.
    fn sized(
        &self,
        metadata: PackageMetadata,
        path: PathBuf,
        record: &Arc<Mutex<SizeRecord>>,
    ) -> InstalledPackage {
        let version = metadata.package_version.to_string();
        let recorded = record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .size_of(&path, &version);
        match recorded {
            Some(size) => InstalledPackage::with_size(metadata, path, size),
            None => InstalledPackage::recording(metadata, path, Arc::clone(record)),
        }
    }

    /// Record the size of the package installed in `package_dir`, as the
    /// installer does with the bytes it extracted (#324).
    pub fn record_size(&self, package_dir: &Path, version: &Version, size: u64) {
        self.size_record()
            .record(package_dir, &version.to_string(), size);
    }

    /// The region and type of every installed package, in the order `list`
    /// gives them. It reads each package's metadata but never walks its
    /// tree for a size, so it stays quick on a large install.
    pub fn list_keys(&self) -> ManagerResult<Vec<(String, PackageType)>> {
        Ok(self
            .scan()?
            .into_iter()
            .map(|(metadata, _)| (metadata.title, metadata.package_type))
            .collect())
    }

    /// The metadata and folder of every installed package, sorted by region,
    /// then by type.
    fn scan(&self) -> ManagerResult<Vec<(PackageMetadata, PathBuf)>> {
        let mut packages = Vec::new();

        if !self.root.exists() {
            return Ok(packages);
        }

        let entries = fs::read_dir(&self.root).map_err(|e| ManagerError::ReadFailed {
            path: self.root.clone(),
            source: e,
        })?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if aside::is_transient(&entry.file_name()) {
                continue;
            }

            // Check for XEarthLayer package marker
            let metadata_path = path.join("xearthlayer_scenery_package.txt");
            if !metadata_path.exists() {
                continue;
            }

            // Try to parse the metadata
            if let Ok(content) = fs::read_to_string(&metadata_path) {
                if let Ok(metadata) = parse_package_metadata(&content) {
                    packages.push((metadata, path));
                }
            }
        }

        // Sort by region, then by type
        packages.sort_by(|(a, _), (b, _)| {
            a.title
                .cmp(&b.title)
                .then_with(|| a.package_type.cmp(&b.package_type))
        });

        Ok(packages)
    }

    /// Remove an installed package.
    ///
    /// # Arguments
    ///
    /// * `region` - The region code
    /// * `package_type` - The package type
    ///
    /// # Safety
    ///
    /// This permanently deletes the package directory and all its contents.
    /// The folder is first renamed to its removed name, which the startup
    /// sweep only ever deletes, so a remove that is killed part way through
    /// its delete never leaves a half-deleted package behind.
    pub fn remove(&self, region: &str, package_type: PackageType) -> ManagerResult<()> {
        let mountpoint = package_mountpoint(region, package_type);
        let package_dir = self.root.join(&mountpoint);
        let not_found = || ManagerError::PackageNotFound {
            region: region.to_string(),
            package_type: package_type.to_string(),
        };

        if !package_dir.exists() {
            return Err(not_found());
        }
        // An aside left by an earlier swap is stale, because the queue runs
        // one operation at a time. Delete it while the package is still in
        // place, or once the package is gone the sweep would rename it back.
        // If it cannot be deleted, stop: the package stays installed rather
        // than leaving the aside behind as its only copy.
        let stale_aside = aside::aside_path(&package_dir);
        if stale_aside.exists() {
            aside::discard(&stale_aside);
            if stale_aside.exists() {
                return Err(ManagerError::WriteFailed {
                    path: stale_aside,
                    source: std::io::Error::other(
                        "a folder left by an earlier update could not be deleted",
                    ),
                });
            }
        }

        let removed = match aside::set_removed(&package_dir) {
            Ok(removed) => removed,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found()),
            Err(e) => {
                return Err(ManagerError::WriteFailed {
                    path: package_dir,
                    source: e,
                })
            }
        };
        aside::discard(&removed);
        Ok(())
    }

    /// Get the expected install path for a package.
    ///
    /// This is useful for determining where a package will be installed
    /// before actually installing it.
    pub fn install_path(&self, region: &str, package_type: PackageType) -> PathBuf {
        let mountpoint = package_mountpoint(region, package_type);
        self.root.join(mountpoint)
    }
}

/// Generate the mountpoint (folder) name for a package.
///
/// This is a local alias for [`xearthlayer_package::package_mountpoint`].
fn package_mountpoint(region: &str, package_type: PackageType) -> String {
    package::package_mountpoint(region, package_type)
}

/// Calculate the total size of a directory recursively.
pub(super) fn calculate_dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;

    if path.is_file() {
        return Ok(path.metadata()?.len());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            total += path.metadata()?.len();
        } else if path.is_dir() {
            total += calculate_dir_size(&path)?;
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_mock_package(dir: &Path, region: &str, package_type: PackageType, version: &str) {
        let mountpoint = package_mountpoint(region, package_type);
        let package_dir = dir.join(&mountpoint);
        fs::create_dir_all(&package_dir).unwrap();

        let type_char = match package_type {
            PackageType::Ortho => "Z",
            PackageType::Overlay => "Y",
        };

        let metadata = format!(
            "REGIONAL SCENERY PACKAGE\n\
             1.0.0\n\
             {}  {}\n\
             2024-01-01T00:00:00Z\n\
             {}\n\
             {}\n\
             test.tar.gz\n\
             1\n\n\
             abc123  test.tar.gz  http://example.com/test.tar.gz\n",
            region.to_uppercase(),
            version,
            type_char,
            mountpoint
        );

        fs::write(
            package_dir.join("xearthlayer_scenery_package.txt"),
            metadata,
        )
        .unwrap();

        // Create some dummy files
        fs::write(package_dir.join("dummy.dsf"), vec![0u8; 1000]).unwrap();
    }

    #[test]
    fn test_package_mountpoint() {
        assert_eq!(
            package_mountpoint("na", PackageType::Ortho),
            "zzXEL_na_ortho"
        );
        assert_eq!(
            package_mountpoint("EU", PackageType::Ortho),
            "zzXEL_eu_ortho"
        );
        assert_eq!(
            package_mountpoint("na", PackageType::Overlay),
            "yzXEL_na_overlay"
        );
    }

    #[test]
    fn test_is_installed() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        // Not installed yet
        assert!(!store.is_installed("na", PackageType::Ortho));

        // Create mock package
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");

        // Now installed
        assert!(store.is_installed("na", PackageType::Ortho));
        assert!(!store.is_installed("eu", PackageType::Ortho));
    }

    #[test]
    fn test_get_package() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");

        let package = store.get("na", PackageType::Ortho).unwrap();
        assert_eq!(package.region(), "NA");
        assert_eq!(package.package_type(), PackageType::Ortho);
        assert_eq!(package.version(), &Version::new(1, 0, 0));
        assert!(package.size_bytes() > 0);
    }

    #[test]
    fn a_recorded_size_is_shown_without_walking_the_package() {
        // #324: the record answers, so a whole world lists without 5.3
        // million stat calls. A deliberately wrong record proves no walk.
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        let dir = temp.path().join("zzXEL_na_ortho");
        store.record_size(&dir, &Version::new(1, 0, 0), 123_456_789);

        assert_eq!(store.list().unwrap()[0].size_bytes(), 123_456_789);
        assert_eq!(
            store.get("na", PackageType::Ortho).unwrap().size_bytes(),
            123_456_789
        );
    }

    #[test]
    fn a_size_measured_once_is_recorded_for_the_next_listing() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        let dir = temp.path().join("zzXEL_na_ortho");
        let measured = store.list().unwrap()[0].size_bytes();
        assert_eq!(measured, calculate_dir_size(&dir).unwrap());

        // Grow the package without changing its metadata: a fresh listing
        // shows the recorded size, so it did not walk again.
        fs::write(dir.join("extra.bin"), vec![0u8; 4096]).unwrap();
        assert_eq!(store.list().unwrap()[0].size_bytes(), measured);
    }

    #[test]
    fn listing_does_not_measure_sizes_until_one_is_asked_for() {
        // #325: `run` lists every package before its dashboard opens and
        // never shows a size. Measuring at list time walked every file of
        // every package (5.3 million on a whole world) with nothing drawn.
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");

        let listed = store.list().unwrap();
        let got = store.get("na", PackageType::Ortho).unwrap();

        // Grow the package after listing: a size measured at list time
        // would miss this file.
        let extra = temp.path().join("zzXEL_na_ortho").join("extra.bin");
        fs::write(&extra, vec![0u8; 4096]).unwrap();
        let expected = calculate_dir_size(&temp.path().join("zzXEL_na_ortho")).unwrap();

        assert_eq!(listed[0].size_bytes(), expected);
        assert_eq!(got.size_bytes(), expected);
        // Measured once, then remembered.
        fs::remove_file(&extra).unwrap();
        assert_eq!(listed[0].size_bytes(), expected);
    }

    #[test]
    fn test_get_package_not_found() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        let result = store.get("na", PackageType::Ortho);
        assert!(matches!(result, Err(ManagerError::PackageNotFound { .. })));
    }

    #[test]
    fn test_list_packages() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        // Create multiple packages
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "2.0.0");
        create_mock_package(temp.path(), "na", PackageType::Overlay, "1.0.0");

        let packages = store.list().unwrap();
        assert_eq!(packages.len(), 3);

        // Should be sorted by region, then type
        assert_eq!(packages[0].region(), "EU");
        assert_eq!(packages[1].region(), "NA");
        assert_eq!(packages[1].package_type(), PackageType::Ortho);
        assert_eq!(packages[2].region(), "NA");
        assert_eq!(packages[2].package_type(), PackageType::Overlay);
    }

    #[test]
    fn the_key_listing_names_what_list_does_in_the_same_order() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "na", PackageType::Overlay, "1.0.0");
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "2.0.0");
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        // A folder with no package marker is no package.
        fs::create_dir_all(temp.path().join("not_a_package")).unwrap();

        let keys = store.list_keys().unwrap();

        let listed: Vec<(String, PackageType)> = store
            .list()
            .unwrap()
            .iter()
            .map(|p| (p.region().to_string(), p.package_type()))
            .collect();
        assert_eq!(keys, listed);
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn the_key_listing_of_a_missing_root_is_empty() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path().join("absent"));

        assert!(store.list_keys().unwrap().is_empty());
    }

    #[test]
    fn test_list_empty() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        let packages = store.list().unwrap();
        assert!(packages.is_empty());
    }

    #[test]
    fn test_remove_package() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        assert!(store.is_installed("na", PackageType::Ortho));

        store.remove("na", PackageType::Ortho).unwrap();
        assert!(!store.is_installed("na", PackageType::Ortho));
    }

    #[test]
    fn a_folder_set_aside_is_not_listed_as_a_package() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);
        let aside = crate::manager::aside::aside_path(&install_path);
        fs::create_dir_all(&aside).unwrap();
        fs::copy(
            install_path.join("xearthlayer_scenery_package.txt"),
            aside.join("xearthlayer_scenery_package.txt"),
        )
        .unwrap();

        let packages = store.list().unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].region(), "EU");
        assert_eq!(packages[0].path, install_path);
    }

    #[test]
    fn a_staging_folder_is_not_listed_as_a_package() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);
        let staging = crate::manager::aside::staging_path(&install_path);
        fs::create_dir_all(&staging).unwrap();
        fs::copy(
            install_path.join("xearthlayer_scenery_package.txt"),
            staging.join("xearthlayer_scenery_package.txt"),
        )
        .unwrap();

        let packages = store.list().unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].path, install_path);
    }

    #[test]
    fn remove_leaves_neither_the_package_nor_an_aside_folder() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);

        store.remove("eu", PackageType::Ortho).unwrap();

        assert!(!install_path.exists());
        assert!(!crate::manager::aside::aside_path(&install_path).exists());
    }

    #[test]
    fn a_removed_folder_is_not_listed_as_a_package() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);
        let removed = temp.path().join(".xel-removed-zzXEL_eu_ortho");
        fs::create_dir_all(&removed).unwrap();
        fs::copy(
            install_path.join("xearthlayer_scenery_package.txt"),
            removed.join("xearthlayer_scenery_package.txt"),
        )
        .unwrap();

        let packages = store.list().unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].path, install_path);
    }

    /// A remove renames the package straight to its removed name, which the
    /// sweep only ever deletes. A stale aside beside it (an earlier swap's
    /// old version that could not be deleted) goes too, or the next sweep
    /// would rename it back as the package.
    #[test]
    fn remove_leaves_no_package_and_no_transient_folder() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        create_mock_package(temp.path(), "na", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);
        let stale_aside = temp.path().join(".xel-aside-zzXEL_eu_ortho");
        let stale_removed = temp.path().join(".xel-removed-zzXEL_eu_ortho");
        fs::create_dir_all(stale_aside.join("terrain")).unwrap();
        fs::create_dir_all(stale_removed.join("terrain")).unwrap();

        store.remove("eu", PackageType::Ortho).unwrap();

        assert!(!install_path.exists());
        let left: Vec<String> = fs::read_dir(temp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["zzXEL_na_ortho"]);
    }

    /// A stale aside that cannot be deleted must stop the remove. Removing
    /// the package anyway would leave the aside as the only folder of that
    /// name, and the startup sweep restores such an aside as the package.
    #[test]
    fn a_stale_aside_that_cannot_be_deleted_stops_the_remove() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        create_mock_package(temp.path(), "eu", PackageType::Ortho, "1.0.0");
        let install_path = store.install_path("eu", PackageType::Ortho);
        // A read-only folder inside the aside keeps its files undeletable.
        let locked = temp.path().join(".xel-aside-zzXEL_eu_ortho/terrain");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("tile.ter"), "old").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let deletable = fs::remove_file(locked.join("tile.ter")).is_ok();

        let result = if deletable {
            None // running as root: permissions cannot induce the failure
        } else {
            Some(store.remove("eu", PackageType::Ortho))
        };

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let Some(result) = result else { return };
        assert!(
            matches!(result, Err(ManagerError::WriteFailed { .. })),
            "{result:?}"
        );
        assert!(store.is_installed("eu", PackageType::Ortho));
        assert!(install_path
            .join("xearthlayer_scenery_package.txt")
            .exists());
    }

    #[test]
    fn test_remove_not_found() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        let result = store.remove("na", PackageType::Ortho);
        assert!(matches!(result, Err(ManagerError::PackageNotFound { .. })));
    }

    #[test]
    fn test_install_path() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        let path = store.install_path("na", PackageType::Ortho);
        assert!(path.ends_with("zzXEL_na_ortho"));
    }
}
