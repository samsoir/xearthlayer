//! Library index management for the package publisher.
//!
//! Provides utilities for managing the package library index file
//! in a publisher repository.

use std::fs;
use std::path::Path;

use chrono::Utc;
use semver::Version;

use super::metadata::calculate_sha256;
use super::{PublishError, PublishResult};
use crate::package::{serialize_package_library, LibraryEntry, PackageLibrary, PackageType};

/// Library index filename.
pub const LIBRARY_FILENAME: &str = "xearthlayer_package_library.txt";

/// Default specification version for new libraries.
pub const SPEC_VERSION: &str = crate::package::CURRENT_SPEC_VERSION;

/// Default scope for libraries.
pub const DEFAULT_SCOPE: &str = "EARTH";

/// Manager for the package library index.
pub struct LibraryManager {
    /// Path to the library file.
    library_path: std::path::PathBuf,

    /// The library data.
    library: PackageLibrary,
}

impl LibraryManager {
    /// Open an existing library index or create a new one.
    pub fn open_or_create(repo_root: &Path) -> PublishResult<Self> {
        let library_path = repo_root.join(LIBRARY_FILENAME);

        let library = if library_path.exists() {
            Self::read_library(&library_path)?
        } else {
            Self::create_empty_library()
        };

        Ok(Self {
            library_path,
            library,
        })
    }

    /// Open an existing library index.
    pub fn open(repo_root: &Path) -> PublishResult<Self> {
        let library_path = repo_root.join(LIBRARY_FILENAME);

        if !library_path.exists() {
            return Err(PublishError::ReadFailed {
                path: library_path,
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "library file not found"),
            });
        }

        let library = Self::read_library(&library_path)?;

        Ok(Self {
            library_path,
            library,
        })
    }

    /// Read the library from a file.
    fn read_library(path: &Path) -> PublishResult<PackageLibrary> {
        let content = fs::read_to_string(path).map_err(|e| PublishError::ReadFailed {
            path: path.to_path_buf(),
            source: e,
        })?;

        crate::package::parse_package_library(&content)
            .map_err(|e| PublishError::InvalidRepository(format!("invalid library format: {}", e)))
    }

    /// Create an empty library.
    fn create_empty_library() -> PackageLibrary {
        PackageLibrary {
            spec_version: Version::parse(SPEC_VERSION).expect("valid version"),
            scope: DEFAULT_SCOPE.to_string(),
            sequence: 0,
            published_at: Utc::now(),
            entries: Vec::new(),
        }
    }

    /// Get a reference to the library.
    pub fn library(&self) -> &PackageLibrary {
        &self.library
    }

    /// Get the current sequence number.
    pub fn sequence(&self) -> u64 {
        self.library.sequence
    }

    /// Get all entries.
    pub fn entries(&self) -> &[LibraryEntry] {
        &self.library.entries
    }

    /// Find an entry by region and package type.
    pub fn find(&self, region: &str, package_type: PackageType) -> Option<&LibraryEntry> {
        self.library.find(region, package_type)
    }

    /// Check if a package exists in the library.
    pub fn contains(&self, region: &str, package_type: PackageType) -> bool {
        self.find(region, package_type).is_some()
    }

    /// Add or update a package in the library.
    ///
    /// If a package with the same region and type exists, it is replaced.
    /// The metadata file checksum is calculated automatically.
    ///
    /// # Arguments
    ///
    /// * `metadata_path` - Path to the package metadata file
    /// * `title` - Region title (e.g., "EUROPE", "NORTH AMERICA")
    /// * `package_type` - Package type (Ortho or Overlay)
    /// * `version` - Package version
    /// * `metadata_url` - URL to the metadata file
    pub fn add_or_update(
        &mut self,
        metadata_path: &Path,
        title: &str,
        package_type: PackageType,
        version: Version,
        metadata_url: &str,
    ) -> PublishResult<()> {
        // Calculate checksum of metadata file
        let checksum = calculate_sha256(metadata_path)?;

        let entry = LibraryEntry::new(checksum, package_type, title, version, metadata_url);

        // Find and replace existing entry, or add new one
        let position =
            self.library.entries.iter().position(|e| {
                e.title.eq_ignore_ascii_case(title) && e.package_type == package_type
            });

        match position {
            Some(idx) => {
                self.library.entries[idx] = entry;
            }
            None => {
                self.library.entries.push(entry);
            }
        }

        Ok(())
    }

    /// Remove a package from the library.
    ///
    /// Returns true if the package was found and removed.
    pub fn remove(&mut self, region: &str, package_type: PackageType) -> bool {
        let position =
            self.library.entries.iter().position(|e| {
                e.title.eq_ignore_ascii_case(region) && e.package_type == package_type
            });

        match position {
            Some(idx) => {
                self.library.entries.remove(idx);
                true
            }
            None => false,
        }
    }

    /// Save the library to disk.
    ///
    /// This increments the sequence number and updates the timestamp.
    pub fn save(&mut self) -> PublishResult<()> {
        // Increment sequence and update timestamp
        self.library.sequence += 1;
        self.library.published_at = Utc::now();

        // Sort entries by title, then by type
        self.library.entries.sort_by(|a, b| {
            a.title
                .cmp(&b.title)
                .then_with(|| a.package_type.code().cmp(&b.package_type.code()))
        });

        let content = serialize_package_library(&self.library);

        fs::write(&self.library_path, content).map_err(|e| PublishError::WriteFailed {
            path: self.library_path.clone(),
            source: e,
        })?;

        Ok(())
    }

    /// Get the path to the library file.
    pub fn library_path(&self) -> &Path {
        &self.library_path
    }

    /// Calculate the SHA-256 checksum of the library file.
    pub fn checksum(&self) -> PublishResult<String> {
        calculate_sha256(&self.library_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_metadata(dir: &Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("xearthlayer_scenery_package.txt");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_open_or_create_new() {
        let temp = TempDir::new().unwrap();
        let manager = LibraryManager::open_or_create(temp.path()).unwrap();

        assert_eq!(manager.sequence(), 0);
        assert!(manager.entries().is_empty());
    }

    #[test]
    fn test_open_or_create_existing() {
        let temp = TempDir::new().unwrap();

        // Create a library file
        let library = PackageLibrary {
            spec_version: Version::new(1, 0, 0),
            scope: "EARTH".to_string(),
            sequence: 5,
            published_at: Utc::now(),
            entries: vec![LibraryEntry::new(
                "abc123",
                PackageType::Ortho,
                "EUROPE",
                Version::new(1, 0, 0),
                "https://example.com/meta.txt",
            )],
        };
        let content = serialize_package_library(&library);
        fs::write(temp.path().join(LIBRARY_FILENAME), content).unwrap();

        let manager = LibraryManager::open_or_create(temp.path()).unwrap();
        assert_eq!(manager.sequence(), 5);
        assert_eq!(manager.entries().len(), 1);
    }

    #[test]
    fn test_open_missing_file() {
        let temp = TempDir::new().unwrap();
        let result = LibraryManager::open(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_add_or_update_new() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test metadata");

        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/eur/ortho/meta.txt",
            )
            .unwrap();

        assert_eq!(manager.entries().len(), 1);
        assert!(manager.contains("EUROPE", PackageType::Ortho));
        assert!(!manager.contains("EUROPE", PackageType::Overlay));
    }

    #[test]
    fn test_add_or_update_existing() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test metadata v1");

        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/v1.txt",
            )
            .unwrap();

        // Update with v2
        fs::write(&metadata_path, "test metadata v2").unwrap();
        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(2, 0, 0),
                "https://example.com/v2.txt",
            )
            .unwrap();

        assert_eq!(manager.entries().len(), 1);
        let entry = manager.find("EUROPE", PackageType::Ortho).unwrap();
        assert_eq!(entry.version, Version::new(2, 0, 0));
    }

    #[test]
    fn test_remove_existing() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test");

        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/meta.txt",
            )
            .unwrap();

        assert!(manager.remove("EUROPE", PackageType::Ortho));
        assert!(manager.entries().is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        assert!(!manager.remove("EUROPE", PackageType::Ortho));
    }

    #[test]
    fn test_remove_case_insensitive() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test");

        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/meta.txt",
            )
            .unwrap();

        assert!(manager.remove("europe", PackageType::Ortho));
        assert!(manager.entries().is_empty());
    }

    #[test]
    fn test_save_increments_sequence() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        assert_eq!(manager.sequence(), 0);
        manager.save().unwrap();
        assert_eq!(manager.sequence(), 1);
        manager.save().unwrap();
        assert_eq!(manager.sequence(), 2);
    }

    #[test]
    fn test_save_creates_file() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let library_path = temp.path().join(LIBRARY_FILENAME);
        assert!(!library_path.exists());

        manager.save().unwrap();
        assert!(library_path.exists());
    }

    #[test]
    fn test_save_sorts_entries() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test");

        // Add in non-sorted order
        manager
            .add_or_update(
                &metadata_path,
                "NORTH AMERICA",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/na.txt",
            )
            .unwrap();
        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Overlay,
                Version::new(1, 0, 0),
                "https://example.com/eur-overlay.txt",
            )
            .unwrap();
        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/eur-ortho.txt",
            )
            .unwrap();

        manager.save().unwrap();

        // Entries should be sorted: EUROPE Y, EUROPE Z, NORTH AMERICA Z
        let entries = manager.entries();
        assert_eq!(entries[0].title, "EUROPE");
        assert_eq!(entries[0].package_type, PackageType::Overlay); // Y < Z
        assert_eq!(entries[1].title, "EUROPE");
        assert_eq!(entries[1].package_type, PackageType::Ortho);
        assert_eq!(entries[2].title, "NORTH AMERICA");
    }

    #[test]
    fn test_roundtrip() {
        let temp = TempDir::new().unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test metadata");

        // Create and save
        {
            let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();
            manager
                .add_or_update(
                    &metadata_path,
                    "EUROPE",
                    PackageType::Ortho,
                    Version::new(1, 0, 0),
                    "https://example.com/meta.txt",
                )
                .unwrap();
            manager.save().unwrap();
        }

        // Reopen and verify
        {
            let manager = LibraryManager::open(temp.path()).unwrap();
            assert_eq!(manager.sequence(), 1);
            assert_eq!(manager.entries().len(), 1);

            let entry = manager.find("EUROPE", PackageType::Ortho).unwrap();
            assert_eq!(entry.version, Version::new(1, 0, 0));
        }
    }

    #[test]
    fn test_checksum() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();
        manager.save().unwrap();

        let checksum = manager.checksum().unwrap();
        assert_eq!(checksum.len(), 64); // SHA-256 hex
    }

    #[test]
    fn test_library_path() {
        let temp = TempDir::new().unwrap();
        let manager = LibraryManager::open_or_create(temp.path()).unwrap();

        assert_eq!(manager.library_path(), temp.path().join(LIBRARY_FILENAME));
    }

    #[test]
    fn test_find_case_insensitive() {
        let temp = TempDir::new().unwrap();
        let mut manager = LibraryManager::open_or_create(temp.path()).unwrap();

        let metadata_path = create_test_metadata(temp.path(), "test");

        manager
            .add_or_update(
                &metadata_path,
                "EUROPE",
                PackageType::Ortho,
                Version::new(1, 0, 0),
                "https://example.com/meta.txt",
            )
            .unwrap();

        assert!(manager.find("europe", PackageType::Ortho).is_some());
        assert!(manager.find("EuRoPe", PackageType::Ortho).is_some());
        assert!(manager.contains("EUROPE", PackageType::Ortho));
        assert!(manager.contains("europe", PackageType::Ortho));
    }
}
