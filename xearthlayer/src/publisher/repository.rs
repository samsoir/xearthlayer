//! Repository management for the package publisher.
//!
//! A repository is a local directory structure containing packages being prepared
//! for distribution. It includes the library index, package working directories,
//! and built archives.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use semver::Version;

use super::{PublishError, PublishResult};
use crate::package::{self, PackageType};

/// Repository marker filename.
const REPO_MARKER: &str = ".xearthlayer-repo";

/// Repository marker header.
const REPO_HEADER: &str = "XEARTHLAYER PACKAGE REPOSITORY";

/// Current repository format version.
const REPO_VERSION: &str = "1.0.0";

/// Subdirectory for package working directories.
const PACKAGES_DIR: &str = "packages";

/// Subdirectory for built archives.
const DIST_DIR: &str = "dist";

/// Subdirectory for work in progress.
const STAGING_DIR: &str = "staging";

/// Library index filename.
const LIBRARY_FILE: &str = "xearthlayer_package_library.txt";

/// Region colour metadata consumed by `publish coverage` and the website legend.
const REGION_METADATA_FILE: &str = "region_metadata.json";

/// Stub written by `init`. The empty `regions` map is intentional — coverage
/// map generation succeeds and renders everything grey, which is visibly
/// incomplete, rather than failing on a fresh repository.
const REGION_METADATA_STUB: &str = r#"{
  "_comment": "Region colours for the coverage map and website legend. 'color' accepts a CSS colour name (e.g. crimson) or hex (e.g. #ffaa00).",
  "regions": {}
}
"#;

/// A package publisher repository.
///
/// Manages the structure and state of a local package repository used to
/// create and publish XEarthLayer scenery packages.
#[derive(Debug, Clone)]
pub struct Repository {
    /// Root path of the repository.
    root: PathBuf,

    /// Repository format version.
    version: Version,

    /// When the repository was created.
    created_at: DateTime<Utc>,
}

impl Repository {
    /// Initialize a new repository at the given path.
    ///
    /// Creates the repository structure:
    /// - `.xearthlayer-repo` marker file
    /// - `packages/` directory for package working directories
    /// - `dist/` directory for built archives
    /// - `staging/` directory for work in progress
    /// - Empty `xearthlayer_package_library.txt`
    /// - `region_metadata.json` stub with an empty region list
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A repository already exists at the path
    /// - Directory creation fails
    /// - File writing fails
    pub fn init(path: impl AsRef<Path>) -> PublishResult<Self> {
        let root = path.as_ref().to_path_buf();

        // Check if repository already exists
        let marker_path = root.join(REPO_MARKER);
        if marker_path.exists() {
            return Err(PublishError::RepositoryExists(root));
        }

        // Create root directory if it doesn't exist
        if !root.exists() {
            fs::create_dir_all(&root).map_err(|e| PublishError::CreateDirectoryFailed {
                path: root.clone(),
                source: e,
            })?;
        }

        // Create subdirectories
        for dir in [PACKAGES_DIR, DIST_DIR, STAGING_DIR] {
            let dir_path = root.join(dir);
            fs::create_dir_all(&dir_path).map_err(|e| PublishError::CreateDirectoryFailed {
                path: dir_path,
                source: e,
            })?;
        }

        let now = Utc::now();
        let version = Version::parse(REPO_VERSION).expect("valid version constant");

        // Write repository marker
        let marker_content = format!(
            "{}\n{}\n{}\n",
            REPO_HEADER,
            REPO_VERSION,
            now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        fs::write(&marker_path, marker_content).map_err(|e| PublishError::WriteFailed {
            path: marker_path,
            source: e,
        })?;

        // Write empty library index
        let library_path = root.join(LIBRARY_FILE);
        let library_content = Self::empty_library_content(&now);
        fs::write(&library_path, library_content).map_err(|e| PublishError::WriteFailed {
            path: library_path,
            source: e,
        })?;

        // Write region metadata stub
        let region_metadata_path = root.join(REGION_METADATA_FILE);
        fs::write(&region_metadata_path, REGION_METADATA_STUB).map_err(|e| {
            PublishError::WriteFailed {
                path: region_metadata_path,
                source: e,
            }
        })?;

        Ok(Self {
            root,
            version,
            created_at: now,
        })
    }

    /// Open an existing repository at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository exists at the path
    /// - The repository marker is invalid
    pub fn open(path: impl AsRef<Path>) -> PublishResult<Self> {
        let root = path.as_ref().to_path_buf();
        let marker_path = root.join(REPO_MARKER);

        if !marker_path.exists() {
            return Err(PublishError::RepositoryNotFound(root));
        }

        let content = fs::read_to_string(&marker_path).map_err(|e| PublishError::ReadFailed {
            path: marker_path.clone(),
            source: e,
        })?;

        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < 3 {
            return Err(PublishError::InvalidRepository(
                "marker file has insufficient lines".to_string(),
            ));
        }

        // Validate header
        if lines[0].trim() != REPO_HEADER {
            return Err(PublishError::InvalidRepository(format!(
                "invalid header: expected '{}', got '{}'",
                REPO_HEADER,
                lines[0].trim()
            )));
        }

        // Parse version
        let version = Version::parse(lines[1].trim())
            .map_err(|e| PublishError::InvalidRepository(format!("invalid version: {}", e)))?;

        // Parse creation timestamp
        let created_at = DateTime::parse_from_rfc3339(lines[2].trim())
            .map_err(|e| PublishError::InvalidRepository(format!("invalid timestamp: {}", e)))?
            .with_timezone(&Utc);

        Ok(Self {
            root,
            version,
            created_at,
        })
    }

    /// Check if a repository exists at the given path.
    pub fn exists(path: impl AsRef<Path>) -> bool {
        path.as_ref().join(REPO_MARKER).exists()
    }

    /// Get the root path of the repository.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the repository format version.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Get when the repository was created.
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    /// Get the path to the packages directory.
    pub fn packages_dir(&self) -> PathBuf {
        self.root.join(PACKAGES_DIR)
    }

    /// Get the path to the dist directory.
    pub fn dist_dir(&self) -> PathBuf {
        self.root.join(DIST_DIR)
    }

    /// Get the path to the staging directory.
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    /// Get the path to the library index file.
    pub fn library_path(&self) -> PathBuf {
        self.root.join(LIBRARY_FILE)
    }

    /// Get the path for a package directory.
    ///
    /// The package directory name follows the pattern:
    /// `{sort_prefix}XEL_{region}_{type_suffix}`
    ///
    /// For example: `zzXEL_na_ortho` or `yzXEL_eur_overlay`
    pub fn package_dir(&self, region: &str, package_type: PackageType) -> PathBuf {
        self.packages_dir()
            .join(package::package_mountpoint(region, package_type))
    }

    /// Check if a package exists in the repository.
    pub fn package_exists(&self, region: &str, package_type: PackageType) -> bool {
        self.package_dir(region, package_type).exists()
    }

    /// List all packages in the repository.
    ///
    /// Returns a list of (region, package_type) tuples.
    pub fn list_packages(&self) -> PublishResult<Vec<(String, PackageType)>> {
        let packages_dir = self.packages_dir();
        if !packages_dir.exists() {
            return Ok(Vec::new());
        }

        let mut packages = Vec::new();

        let entries = fs::read_dir(&packages_dir).map_err(|e| PublishError::ReadFailed {
            path: packages_dir.clone(),
            source: e,
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| PublishError::ReadFailed {
                path: packages_dir.clone(),
                source: e,
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some((region, package_type)) = Self::parse_package_dir_name(name) {
                    packages.push((region, package_type));
                }
            }
        }

        // Sort by region, then by type
        packages.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.code().cmp(&b.1.code())));

        Ok(packages)
    }

    /// Parse a package directory name into region and type.
    ///
    /// Expected format: `{prefix}XEL_{region}_{suffix}`
    /// Examples: `zzXEL_na_ortho`, `yzXEL_eur_overlay`
    fn parse_package_dir_name(name: &str) -> Option<(String, PackageType)> {
        // Must contain "XEL_"
        let xel_pos = name.find("XEL_")?;

        // Get the part after "XEL_"
        let after_xel = &name[xel_pos + 4..];

        // Find the last underscore to separate region from suffix
        let last_underscore = after_xel.rfind('_')?;

        let region = &after_xel[..last_underscore];
        let suffix = &after_xel[last_underscore + 1..];

        // Determine package type from suffix
        let package_type = match suffix {
            "ortho" => PackageType::Ortho,
            "overlay" => PackageType::Overlay,
            _ => return None,
        };

        Some((region.to_string(), package_type))
    }

    /// Generate empty library index content.
    fn empty_library_content(timestamp: &DateTime<Utc>) -> String {
        format!(
            "XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY\n\
             1.0.0\n\
             EARTH\n\
             0\n\
             {}\n\
             0\n",
            timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn test_init_creates_marker() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();
        assert!(temp.path().join(REPO_MARKER).exists());
    }

    #[test]
    fn test_init_creates_directories() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        assert!(temp.path().join(PACKAGES_DIR).exists());
        assert!(temp.path().join(DIST_DIR).exists());
        assert!(temp.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn test_init_creates_library() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();
        assert!(temp.path().join(LIBRARY_FILE).exists());
    }

    #[test]
    fn test_init_fails_if_exists() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        let result = Repository::init(temp.path());
        assert!(matches!(result, Err(PublishError::RepositoryExists(_))));
    }

    #[test]
    fn test_init_creates_parent_dirs() {
        let temp = TempDir::new().unwrap();
        let nested = temp.path().join("deeply/nested/repo");

        Repository::init(&nested).unwrap();
        assert!(nested.join(REPO_MARKER).exists());
    }

    #[test]
    fn test_open_existing() {
        let (temp, _) = temp_repo();
        let repo = Repository::open(temp.path()).unwrap();

        assert_eq!(repo.root(), temp.path());
        assert_eq!(repo.version().to_string(), REPO_VERSION);
    }

    #[test]
    fn test_open_not_found() {
        let temp = TempDir::new().unwrap();
        let result = Repository::open(temp.path());
        assert!(matches!(result, Err(PublishError::RepositoryNotFound(_))));
    }

    #[test]
    fn test_exists() {
        let temp = TempDir::new().unwrap();
        assert!(!Repository::exists(temp.path()));

        Repository::init(temp.path()).unwrap();
        assert!(Repository::exists(temp.path()));
    }

    #[test]
    fn test_packages_dir() {
        let (temp, repo) = temp_repo();
        assert_eq!(repo.packages_dir(), temp.path().join(PACKAGES_DIR));
    }

    #[test]
    fn test_dist_dir() {
        let (temp, repo) = temp_repo();
        assert_eq!(repo.dist_dir(), temp.path().join(DIST_DIR));
    }

    #[test]
    fn test_staging_dir() {
        let (temp, repo) = temp_repo();
        assert_eq!(repo.staging_dir(), temp.path().join(STAGING_DIR));
    }

    #[test]
    fn test_library_path() {
        let (temp, repo) = temp_repo();
        assert_eq!(repo.library_path(), temp.path().join(LIBRARY_FILE));
    }

    #[test]
    fn test_package_dir_ortho() {
        let (_temp, repo) = temp_repo();
        let dir = repo.package_dir("na", PackageType::Ortho);
        assert!(dir.ends_with("zzXEL_na_ortho"));
    }

    #[test]
    fn test_package_dir_overlay() {
        let (_temp, repo) = temp_repo();
        let dir = repo.package_dir("eur", PackageType::Overlay);
        assert!(dir.ends_with("yzXEL_eur_overlay"));
    }

    #[test]
    fn test_package_dir_lowercase() {
        let (_temp, repo) = temp_repo();
        let dir = repo.package_dir("NA", PackageType::Ortho);
        assert!(dir.ends_with("zzXEL_na_ortho"));
    }

    #[test]
    fn test_package_exists_false() {
        let (_temp, repo) = temp_repo();
        assert!(!repo.package_exists("na", PackageType::Ortho));
    }

    #[test]
    fn test_package_exists_true() {
        let (_temp, repo) = temp_repo();
        let pkg_dir = repo.package_dir("na", PackageType::Ortho);
        fs::create_dir_all(&pkg_dir).unwrap();

        assert!(repo.package_exists("na", PackageType::Ortho));
    }

    #[test]
    fn test_list_packages_empty() {
        let (_temp, repo) = temp_repo();
        let packages = repo.list_packages().unwrap();
        assert!(packages.is_empty());
    }

    #[test]
    fn test_list_packages() {
        let (_temp, repo) = temp_repo();

        // Create some package directories
        fs::create_dir_all(repo.package_dir("na", PackageType::Ortho)).unwrap();
        fs::create_dir_all(repo.package_dir("eur", PackageType::Ortho)).unwrap();
        fs::create_dir_all(repo.package_dir("na", PackageType::Overlay)).unwrap();

        let packages = repo.list_packages().unwrap();

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&("eur".to_string(), PackageType::Ortho)));
        assert!(packages.contains(&("na".to_string(), PackageType::Ortho)));
        assert!(packages.contains(&("na".to_string(), PackageType::Overlay)));
    }

    #[test]
    fn test_list_packages_sorted() {
        let (_temp, repo) = temp_repo();

        fs::create_dir_all(repo.package_dir("na", PackageType::Overlay)).unwrap();
        fs::create_dir_all(repo.package_dir("eur", PackageType::Ortho)).unwrap();
        fs::create_dir_all(repo.package_dir("na", PackageType::Ortho)).unwrap();

        let packages = repo.list_packages().unwrap();

        // Should be sorted by region, then by type (Y before Z)
        assert_eq!(packages[0], ("eur".to_string(), PackageType::Ortho));
        assert_eq!(packages[1], ("na".to_string(), PackageType::Overlay));
        assert_eq!(packages[2], ("na".to_string(), PackageType::Ortho));
    }

    #[test]
    fn test_parse_package_dir_name_ortho() {
        let result = Repository::parse_package_dir_name("zzXEL_na_ortho");
        assert_eq!(result, Some(("na".to_string(), PackageType::Ortho)));
    }

    #[test]
    fn test_parse_package_dir_name_overlay() {
        let result = Repository::parse_package_dir_name("yzXEL_eur_overlay");
        assert_eq!(result, Some(("eur".to_string(), PackageType::Overlay)));
    }

    #[test]
    fn test_parse_package_dir_name_multi_word_region() {
        let result = Repository::parse_package_dir_name("zzXEL_north_america_ortho");
        assert_eq!(
            result,
            Some(("north_america".to_string(), PackageType::Ortho))
        );
    }

    #[test]
    fn test_parse_package_dir_name_invalid() {
        assert_eq!(Repository::parse_package_dir_name("invalid"), None);
        assert_eq!(Repository::parse_package_dir_name("zzXEL_na"), None);
        assert_eq!(Repository::parse_package_dir_name("zzXEL_na_unknown"), None);
    }

    #[test]
    fn test_marker_content_format() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        let content = fs::read_to_string(temp.path().join(REPO_MARKER)).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines[0], REPO_HEADER);
        assert_eq!(lines[1], REPO_VERSION);
        // Line 3 is the timestamp, just verify it parses
        assert!(DateTime::parse_from_rfc3339(lines[2]).is_ok());
    }

    #[test]
    fn test_library_content_format() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        let content = fs::read_to_string(temp.path().join(LIBRARY_FILE)).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines[0], "XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY");
        assert_eq!(lines[1], "1.0.0");
        assert_eq!(lines[2], "EARTH");
        assert_eq!(lines[3], "0"); // sequence
                                   // lines[4] is timestamp
        assert_eq!(lines[5], "0"); // package count
    }

    #[test]
    fn test_repo_clone() {
        let (_temp, repo) = temp_repo();
        let cloned = repo.clone();
        assert_eq!(cloned.root(), repo.root());
    }

    #[test]
    fn test_repo_debug() {
        let (_temp, repo) = temp_repo();
        let debug = format!("{:?}", repo);
        assert!(debug.contains("Repository"));
    }

    #[test]
    fn init_creates_region_metadata_stub() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let path = repo.root().join("region_metadata.json");
        assert!(path.exists(), "init should create region_metadata.json");
    }

    // The stub must be loadable by the coverage map, not merely present.
    // An empty regions map is intentional: `publish coverage` then succeeds
    // and renders every region grey, which is obviously incomplete.
    #[test]
    fn init_stub_parses_as_region_metadata_with_no_regions() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let metadata =
            crate::publisher::RegionMetadata::load(&repo.root().join("region_metadata.json"))
                .expect("stub must parse as RegionMetadata");
        assert!(metadata.regions.is_empty());
    }
}
