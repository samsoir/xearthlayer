//! Local package store for discovering and managing installed packages.

use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::package::{self, parse_package_metadata, PackageMetadata, PackageType};

use super::{ManagerError, ManagerResult};

/// Mount status for ortho packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountStatus {
    /// Package is mounted (FUSE filesystem active).
    Mounted,
    /// Package is not mounted.
    NotMounted,
    /// Mount status unknown (not applicable or couldn't determine).
    Unknown,
}

impl MountStatus {
    /// Returns true if the package is mounted.
    pub fn is_mounted(&self) -> bool {
        matches!(self, Self::Mounted)
    }
}

/// Information about an installed package.
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    /// The package metadata.
    pub metadata: PackageMetadata,
    /// Path to the package directory.
    pub path: PathBuf,
    /// Size of the package on disk (bytes).
    pub size_bytes: u64,
    /// Mount status (for ortho packages).
    pub mount_status: MountStatus,
}

impl InstalledPackage {
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

        let size_bytes = calculate_dir_size(&package_dir).unwrap_or(0);
        let mount_status = check_mount_status(&package_dir, package_type);

        Ok(InstalledPackage {
            metadata,
            path: package_dir,
            size_bytes,
            mount_status,
        })
    }

    /// List all installed packages.
    pub fn list(&self) -> ManagerResult<Vec<InstalledPackage>> {
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

            // Check for XEarthLayer package marker
            let metadata_path = path.join("xearthlayer_scenery_package.txt");
            if !metadata_path.exists() {
                continue;
            }

            // Try to parse the metadata
            if let Ok(content) = fs::read_to_string(&metadata_path) {
                if let Ok(metadata) = parse_package_metadata(&content) {
                    let size_bytes = calculate_dir_size(&path).unwrap_or(0);
                    let mount_status = check_mount_status(&path, metadata.package_type);
                    packages.push(InstalledPackage {
                        metadata,
                        path,
                        size_bytes,
                        mount_status,
                    });
                }
            }
        }

        // Sort by region, then by type
        packages.sort_by(|a, b| {
            a.metadata
                .title
                .cmp(&b.metadata.title)
                .then_with(|| a.metadata.package_type.cmp(&b.metadata.package_type))
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
    pub fn remove(&self, region: &str, package_type: PackageType) -> ManagerResult<()> {
        let mountpoint = package_mountpoint(region, package_type);
        let package_dir = self.root.join(&mountpoint);

        if !package_dir.exists() {
            return Err(ManagerError::PackageNotFound {
                region: region.to_string(),
                package_type: package_type.to_string(),
            });
        }

        fs::remove_dir_all(&package_dir).map_err(|e| ManagerError::WriteFailed {
            path: package_dir,
            source: e,
        })
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
/// This is a local alias for [`crate::package::package_mountpoint`].
fn package_mountpoint(region: &str, package_type: PackageType) -> String {
    package::package_mountpoint(region, package_type)
}

/// Calculate the total size of a directory recursively.
fn calculate_dir_size(path: &Path) -> std::io::Result<u64> {
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

/// Check if a package directory is mounted.
///
/// Only ortho packages can be mounted (via FUSE). Overlay packages
/// return `Unknown` since they don't use FUSE mounts.
fn check_mount_status(path: &Path, package_type: PackageType) -> MountStatus {
    // Only ortho packages can be mounted
    if package_type != PackageType::Ortho {
        return MountStatus::Unknown;
    }

    // On Linux, check /proc/mounts for the path
    #[cfg(target_os = "linux")]
    {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            let path_str = path.to_string_lossy();
            for line in mounts.lines() {
                // /proc/mounts format: device mountpoint type options dump pass
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == path_str {
                    return MountStatus::Mounted;
                }
            }
            return MountStatus::NotMounted;
        }
    }

    // On other platforms, return Unknown
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path; // Suppress unused warning
    }

    MountStatus::Unknown
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
        assert!(package.size_bytes > 0);
        // Not actually mounted, so should be NotMounted on Linux
        #[cfg(target_os = "linux")]
        assert_eq!(package.mount_status, MountStatus::NotMounted);
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

    #[test]
    fn test_mount_status_overlay_unknown() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        create_mock_package(temp.path(), "na", PackageType::Overlay, "1.0.0");

        let package = store.get("na", PackageType::Overlay).unwrap();
        // Overlay packages don't use FUSE mounts
        assert_eq!(package.mount_status, MountStatus::Unknown);
    }

    #[test]
    fn test_mount_status_is_mounted() {
        assert!(MountStatus::Mounted.is_mounted());
        assert!(!MountStatus::NotMounted.is_mounted());
        assert!(!MountStatus::Unknown.is_mounted());
    }
}
