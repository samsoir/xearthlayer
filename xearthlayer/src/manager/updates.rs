//! Update detection for comparing local and remote package versions.

use semver::Version;

use crate::package::{LibraryEntry, PackageLibrary, PackageType};

use super::traits::LibraryClient;
use super::{LocalPackageStore, ManagerResult};

/// Status of a package comparing local vs remote versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageStatus {
    /// Package is installed and up-to-date.
    UpToDate,
    /// Package has an update available.
    UpdateAvailable {
        /// Currently installed version.
        installed: Version,
        /// Latest available version.
        available: Version,
    },
    /// Package is available but not installed.
    NotInstalled {
        /// Latest available version.
        available: Version,
    },
    /// Package is installed but not in any library (orphaned).
    Orphaned {
        /// Installed version.
        installed: Version,
    },
}

impl PackageStatus {
    /// Returns true if an update is available.
    pub fn has_update(&self) -> bool {
        matches!(self, Self::UpdateAvailable { .. })
    }

    /// Returns true if the package is installed.
    pub fn is_installed(&self) -> bool {
        matches!(
            self,
            Self::UpToDate | Self::UpdateAvailable { .. } | Self::Orphaned { .. }
        )
    }
}

/// Information about a package's update status.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// Region code (e.g., "na", "eu").
    pub region: String,
    /// Package type.
    pub package_type: PackageType,
    /// Current status.
    pub status: PackageStatus,
    /// Remote library entry (if available in a library).
    pub library_entry: Option<LibraryEntry>,
}

impl PackageInfo {
    /// Get the installed version if any.
    pub fn installed_version(&self) -> Option<&Version> {
        match &self.status {
            PackageStatus::UpToDate => self.library_entry.as_ref().map(|e| &e.version),
            PackageStatus::UpdateAvailable { installed, .. } => Some(installed),
            PackageStatus::Orphaned { installed } => Some(installed),
            PackageStatus::NotInstalled { .. } => None,
        }
    }

    /// Get the available version if any.
    pub fn available_version(&self) -> Option<&Version> {
        match &self.status {
            PackageStatus::UpToDate => self.library_entry.as_ref().map(|e| &e.version),
            PackageStatus::UpdateAvailable { available, .. } => Some(available),
            PackageStatus::NotInstalled { available } => Some(available),
            PackageStatus::Orphaned { .. } => None,
        }
    }
}

/// Update checker that compares local packages against remote libraries.
pub struct UpdateChecker<'a, C: LibraryClient> {
    local_store: &'a LocalPackageStore,
    client: &'a C,
}

impl<'a, C: LibraryClient> UpdateChecker<'a, C> {
    /// Create a new update checker.
    pub fn new(local_store: &'a LocalPackageStore, client: &'a C) -> Self {
        Self {
            local_store,
            client,
        }
    }

    /// Fetch libraries from all URLs and merge into a single library.
    ///
    /// Later URLs take precedence for duplicate packages.
    pub fn fetch_libraries(&self, urls: &[String]) -> ManagerResult<PackageLibrary> {
        let mut merged = PackageLibrary::new();

        for url in urls {
            match self.client.fetch_library(url) {
                Ok(library) => {
                    // Merge entries, later libraries override earlier ones
                    for entry in library.entries {
                        // Remove existing entry for same region/type if present
                        merged.entries.retain(|e| {
                            !(e.title.eq_ignore_ascii_case(&entry.title)
                                && e.package_type == entry.package_type)
                        });
                        merged.entries.push(entry);
                    }
                }
                Err(e) => {
                    // Log warning but continue with other libraries
                    tracing::warn!("Failed to fetch library from {}: {}", url, e);
                }
            }
        }

        Ok(merged)
    }

    /// Check status of a specific package.
    pub fn check_package(
        &self,
        region: &str,
        package_type: PackageType,
        library: &PackageLibrary,
    ) -> PackageInfo {
        let installed = self.local_store.get(region, package_type).ok();
        let remote = library.find(region, package_type);

        let status = match (installed.as_ref(), remote) {
            // Both installed and in library - compare versions
            (Some(local), Some(entry)) => {
                if local.version() >= &entry.version {
                    PackageStatus::UpToDate
                } else {
                    PackageStatus::UpdateAvailable {
                        installed: local.version().clone(),
                        available: entry.version.clone(),
                    }
                }
            }
            // In library but not installed
            (None, Some(entry)) => PackageStatus::NotInstalled {
                available: entry.version.clone(),
            },
            // Installed but not in library (orphaned)
            (Some(local), None) => PackageStatus::Orphaned {
                installed: local.version().clone(),
            },
            // Neither installed nor in library - shouldn't normally happen
            // but handle gracefully
            (None, None) => PackageStatus::NotInstalled {
                available: Version::new(0, 0, 0),
            },
        };

        PackageInfo {
            region: region.to_string(),
            package_type,
            status,
            library_entry: remote.cloned(),
        }
    }

    /// Check all installed packages for updates.
    pub fn check_installed(&self, library: &PackageLibrary) -> ManagerResult<Vec<PackageInfo>> {
        let installed = self.local_store.list()?;
        let mut results = Vec::new();

        for pkg in installed {
            let info = self.check_package(pkg.region(), pkg.package_type(), library);
            results.push(info);
        }

        Ok(results)
    }

    /// Get all available packages (installed and not installed).
    pub fn list_all(&self, library: &PackageLibrary) -> ManagerResult<Vec<PackageInfo>> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // First, add all installed packages
        let installed = self.local_store.list()?;
        for pkg in &installed {
            let info = self.check_package(pkg.region(), pkg.package_type(), library);
            seen.insert((pkg.region().to_lowercase(), pkg.package_type()));
            results.push(info);
        }

        // Then add packages from library that aren't installed
        for entry in &library.entries {
            let key = (entry.title.to_lowercase(), entry.package_type);
            if !seen.contains(&key) {
                results.push(PackageInfo {
                    region: entry.title.clone(),
                    package_type: entry.package_type,
                    status: PackageStatus::NotInstalled {
                        available: entry.version.clone(),
                    },
                    library_entry: Some(entry.clone()),
                });
                seen.insert(key);
            }
        }

        // Sort by region, then type
        results.sort_by(|a, b| {
            a.region
                .to_lowercase()
                .cmp(&b.region.to_lowercase())
                .then_with(|| a.package_type.cmp(&b.package_type))
        });

        Ok(results)
    }

    /// Get only packages with available updates.
    pub fn list_updates(&self, library: &PackageLibrary) -> ManagerResult<Vec<PackageInfo>> {
        let all = self.check_installed(library)?;
        Ok(all.into_iter().filter(|p| p.status.has_update()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ManagerError;
    use crate::package::PackageMetadata;
    use std::fs;
    use tempfile::TempDir;

    /// Mock library client for testing.
    struct MockLibraryClient {
        library: Option<PackageLibrary>,
    }

    impl MockLibraryClient {
        fn with_library(library: PackageLibrary) -> Self {
            Self {
                library: Some(library),
            }
        }

        fn empty() -> Self {
            Self { library: None }
        }
    }

    impl LibraryClient for MockLibraryClient {
        fn fetch_library(&self, _url: &str) -> ManagerResult<PackageLibrary> {
            self.library
                .clone()
                .ok_or_else(|| ManagerError::LibraryFetchFailed {
                    url: "mock".to_string(),
                    reason: "no library configured".to_string(),
                })
        }

        fn fetch_metadata(&self, _url: &str) -> ManagerResult<PackageMetadata> {
            Err(ManagerError::MetadataFetchFailed {
                url: "mock".to_string(),
                reason: "not implemented".to_string(),
            })
        }
    }

    fn create_test_library() -> PackageLibrary {
        let mut library = PackageLibrary::new();
        library.entries.push(LibraryEntry {
            checksum: "abc123".to_string(),
            package_type: PackageType::Ortho,
            title: "NA".to_string(),
            version: Version::new(2, 0, 0),
            metadata_url: "http://example.com/na.txt".to_string(),
        });
        library.entries.push(LibraryEntry {
            checksum: "def456".to_string(),
            package_type: PackageType::Ortho,
            title: "EU".to_string(),
            version: Version::new(1, 0, 0),
            metadata_url: "http://example.com/eu.txt".to_string(),
        });
        library
    }

    fn create_mock_package(dir: &std::path::Path, region: &str, version: &str) {
        let mountpoint = crate::package::package_mountpoint(region, PackageType::Ortho);
        let package_dir = dir.join(&mountpoint);
        fs::create_dir_all(&package_dir).unwrap();

        let metadata = format!(
            "REGIONAL SCENERY PACKAGE\n\
             1.0.0\n\
             {}  {}\n\
             2024-01-01T00:00:00Z\n\
             Z\n\
             {}\n\
             test.tar.gz\n\
             1\n\n\
             abc123  test.tar.gz  http://example.com/test.tar.gz\n",
            region.to_uppercase(),
            version,
            mountpoint
        );

        fs::write(
            package_dir.join("xearthlayer_scenery_package.txt"),
            metadata,
        )
        .unwrap();
    }

    #[test]
    fn test_package_status_has_update() {
        assert!(!PackageStatus::UpToDate.has_update());
        assert!(PackageStatus::UpdateAvailable {
            installed: Version::new(1, 0, 0),
            available: Version::new(2, 0, 0),
        }
        .has_update());
        assert!(!PackageStatus::NotInstalled {
            available: Version::new(1, 0, 0),
        }
        .has_update());
        assert!(!PackageStatus::Orphaned {
            installed: Version::new(1, 0, 0),
        }
        .has_update());
    }

    #[test]
    fn test_package_status_is_installed() {
        assert!(PackageStatus::UpToDate.is_installed());
        assert!(PackageStatus::UpdateAvailable {
            installed: Version::new(1, 0, 0),
            available: Version::new(2, 0, 0),
        }
        .is_installed());
        assert!(!PackageStatus::NotInstalled {
            available: Version::new(1, 0, 0),
        }
        .is_installed());
        assert!(PackageStatus::Orphaned {
            installed: Version::new(1, 0, 0),
        }
        .is_installed());
    }

    #[test]
    fn test_check_package_up_to_date() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        // Install NA v2.0.0
        create_mock_package(temp.path(), "na", "2.0.0");

        // Library has NA v2.0.0
        let library = create_test_library();

        let checker = UpdateChecker::new(&store, &client);
        let info = checker.check_package("na", PackageType::Ortho, &library);

        assert_eq!(info.status, PackageStatus::UpToDate);
        assert!(info.library_entry.is_some());
    }

    #[test]
    fn test_check_package_update_available() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        // Install NA v1.0.0
        create_mock_package(temp.path(), "na", "1.0.0");

        // Library has NA v2.0.0
        let library = create_test_library();

        let checker = UpdateChecker::new(&store, &client);
        let info = checker.check_package("na", PackageType::Ortho, &library);

        assert!(matches!(info.status, PackageStatus::UpdateAvailable { .. }));
        assert_eq!(info.installed_version(), Some(&Version::new(1, 0, 0)));
        assert_eq!(info.available_version(), Some(&Version::new(2, 0, 0)));
    }

    #[test]
    fn test_check_package_not_installed() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        let library = create_test_library();

        let checker = UpdateChecker::new(&store, &client);
        let info = checker.check_package("na", PackageType::Ortho, &library);

        assert!(matches!(info.status, PackageStatus::NotInstalled { .. }));
        assert_eq!(info.installed_version(), None);
        assert_eq!(info.available_version(), Some(&Version::new(2, 0, 0)));
    }

    #[test]
    fn test_check_package_orphaned() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        // Install package not in library
        create_mock_package(temp.path(), "sa", "1.0.0");

        let library = create_test_library(); // Doesn't have SA

        let checker = UpdateChecker::new(&store, &client);
        let info = checker.check_package("sa", PackageType::Ortho, &library);

        assert!(matches!(info.status, PackageStatus::Orphaned { .. }));
        assert_eq!(info.installed_version(), Some(&Version::new(1, 0, 0)));
        assert_eq!(info.available_version(), None);
    }

    #[test]
    fn test_list_updates() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        // Install NA v1.0.0 (update available) and EU v1.0.0 (up to date)
        create_mock_package(temp.path(), "na", "1.0.0");
        create_mock_package(temp.path(), "eu", "1.0.0");

        let library = create_test_library(); // NA v2.0.0, EU v1.0.0

        let checker = UpdateChecker::new(&store, &client);
        let updates = checker.list_updates(&library).unwrap();

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].region, "NA");
    }

    #[test]
    fn test_list_all() {
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());
        let client = MockLibraryClient::empty();

        // Install NA only
        create_mock_package(temp.path(), "na", "1.0.0");

        let library = create_test_library(); // Has NA and EU

        let checker = UpdateChecker::new(&store, &client);
        let all = checker.list_all(&library).unwrap();

        // Should have both NA (installed) and EU (not installed)
        assert_eq!(all.len(), 2);

        let na = all.iter().find(|p| p.region == "NA").unwrap();
        assert!(na.status.is_installed());

        let eu = all.iter().find(|p| p.region == "EU").unwrap();
        assert!(!eu.status.is_installed());
    }

    #[test]
    fn test_fetch_libraries_merges() {
        let library = create_test_library();
        let client = MockLibraryClient::with_library(library);
        let temp = TempDir::new().unwrap();
        let store = LocalPackageStore::new(temp.path());

        let checker = UpdateChecker::new(&store, &client);
        let merged = checker
            .fetch_libraries(&["http://example.com/lib1.txt".to_string()])
            .unwrap();

        assert_eq!(merged.entries.len(), 2);
    }
}
