//! Repository management for the package publisher.
//!
//! A repository is a local directory structure containing packages being prepared
//! for distribution. Since format 2.0.0 (#333) it holds one or more library
//! generations, each a complete scenery world in its own folder (`v1/`, `v2/`,
//! ...): its library index, region metadata, coverage map, package working
//! directories and built archives. The repository records which generation is
//! current, and every path below resolves inside it.
//!
//! A published library URL must never change meaning: clients that read the
//! old world must not be handed a new one. So the files a 1.x repository kept
//! at its root (the index, region metadata and coverage map) stay there,
//! frozen, for clients that predate generations, and no 2.0.0 operation writes
//! them. [`crate::generation::upgrade`] copies them into a generation.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use semver::Version;

use super::{PublishError, PublishResult};
use xearthlayer_package::{self as package, PackageType};

/// Repository marker filename.
const REPO_MARKER: &str = ".xearthlayer-repo";

/// Repository marker header.
const REPO_HEADER: &str = "XEARTHLAYER PACKAGE REPOSITORY";

/// Current repository format version.
pub(crate) const REPO_VERSION: &str = "2.0.0";

/// The marker line naming the current generation: `current v2`.
const CURRENT_PREFIX: &str = "current ";

/// The generation a new repository starts with.
pub const FIRST_GENERATION: &str = "v1";

/// Subdirectory for package working directories, inside a generation.
const PACKAGES_DIR: &str = "packages";

/// Subdirectory for built archives, inside a generation.
const DIST_DIR: &str = "dist";

/// Work area at the repository root, shared by every generation: where
/// Ortho4XP output is gathered before `publish add` (see the world rebuild
/// runbook). No command reads or writes inside it.
const STAGING_DIR: &str = "staging";

/// Library index filename.
pub(crate) const LIBRARY_FILE: &str = "xearthlayer_package_library.txt";

/// Region colour metadata consumed by `publish coverage` and the website legend.
pub const REGION_METADATA_FILE: &str = "region_metadata.json";

/// Stub written for a new generation. The empty `regions` map is intentional:
/// coverage map generation succeeds and renders everything grey, which is
/// visibly incomplete, rather than failing on a fresh generation.
const REGION_METADATA_STUB: &str = r#"{
  "_comment": "Region colours for the coverage map and website legend. 'color' accepts a CSS colour name (e.g. crimson) or hex (e.g. #ffaa00).",
  "regions": {}
}
"#;

/// Whether `name` is a library generation name: `v` and a number from 1,
/// without leading zeros (`v1`, `v2`, `v10`).
pub fn is_generation_name(name: &str) -> bool {
    let Some(number) = name.strip_prefix('v') else {
        return false;
    };
    !number.is_empty() && !number.starts_with('0') && number.bytes().all(|b| b.is_ascii_digit())
}

fn check_generation_name(name: &str) -> PublishResult<()> {
    if is_generation_name(name) {
        Ok(())
    } else {
        Err(PublishError::InvalidGeneration(name.to_string()))
    }
}

/// What a repository marker says.
#[derive(Debug, Clone)]
pub(crate) struct Marker {
    pub version: Version,
    pub created_at: DateTime<Utc>,
    /// The current generation; `None` before format 2.0.0.
    pub current: Option<String>,
}

impl Marker {
    /// Read the marker of the repository at `root`, whatever its format.
    pub(crate) fn read(root: &Path) -> PublishResult<Self> {
        let marker_path = root.join(REPO_MARKER);
        if !marker_path.exists() {
            return Err(PublishError::RepositoryNotFound(root.to_path_buf()));
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
        if lines[0].trim() != REPO_HEADER {
            return Err(PublishError::InvalidRepository(format!(
                "invalid header: expected '{}', got '{}'",
                REPO_HEADER,
                lines[0].trim()
            )));
        }
        let version = Version::parse(lines[1].trim())
            .map_err(|e| PublishError::InvalidRepository(format!("invalid version: {}", e)))?;
        let created_at = DateTime::parse_from_rfc3339(lines[2].trim())
            .map_err(|e| PublishError::InvalidRepository(format!("invalid timestamp: {}", e)))?
            .with_timezone(&Utc);
        let current = lines
            .get(3)
            .and_then(|line| line.trim().strip_prefix(CURRENT_PREFIX))
            .map(|g| g.trim().to_string());
        Ok(Self {
            version,
            created_at,
            current,
        })
    }

    /// Write a format 2.0.0 marker naming `current`.
    ///
    /// Only the header lines are rewritten. The marker also carries the
    /// repository's settings in a `[config]` section (see
    /// [`crate::write_config`]), and everything from the first section on
    /// is kept as it was.
    pub(crate) fn write(
        root: &Path,
        created_at: DateTime<Utc>,
        current: &str,
    ) -> PublishResult<()> {
        let marker_path = root.join(REPO_MARKER);
        let existing = fs::read_to_string(&marker_path).unwrap_or_default();
        let sections = existing
            .find("\n[")
            .map(|at| existing[at..].trim_start_matches('\n'))
            .unwrap_or("");
        let mut content = format!(
            "{}\n{}\n{}\n{}{}\n",
            REPO_HEADER,
            REPO_VERSION,
            created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            CURRENT_PREFIX,
            current
        );
        if !sections.is_empty() {
            content.push('\n');
            content.push_str(sections);
        }
        fs::write(&marker_path, content).map_err(|e| PublishError::WriteFailed {
            path: marker_path,
            source: e,
        })
    }
}

/// A package publisher repository, opened at one library generation.
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

    /// The library generation every path resolves in.
    generation: String,
}

impl Repository {
    /// Initialize a new repository at the given path.
    ///
    /// Creates the repository structure:
    /// - `.xearthlayer-repo` marker file, format 2.0.0, current generation `v1`
    /// - `v1/` with an empty library index, a `region_metadata.json` stub with
    ///   an empty region list, and `packages/` and `dist/` directories
    /// - `staging/`, a work area shared by every generation
    ///
    /// A new repository has no root index: the frozen root files exist only
    /// in repositories that predate generations.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A repository already exists at the path
    /// - Directory creation fails
    /// - File writing fails
    pub fn init(path: impl AsRef<Path>) -> PublishResult<Self> {
        let root = path.as_ref().to_path_buf();

        if Self::exists(&root) {
            return Err(PublishError::RepositoryExists(root));
        }
        if !root.exists() {
            fs::create_dir_all(&root).map_err(|e| PublishError::CreateDirectoryFailed {
                path: root.clone(),
                source: e,
            })?;
        }

        let staging = root.join(STAGING_DIR);
        fs::create_dir_all(&staging).map_err(|e| PublishError::CreateDirectoryFailed {
            path: staging,
            source: e,
        })?;
        let now = Utc::now();
        create_generation_files(&root.join(FIRST_GENERATION), &now)?;
        Marker::write(&root, now, FIRST_GENERATION)?;

        Ok(Self {
            root,
            version: Version::parse(REPO_VERSION).expect("valid version constant"),
            created_at: now,
            generation: FIRST_GENERATION.to_string(),
        })
    }

    /// Open an existing repository at its current generation.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository exists at the path
    /// - The repository marker is invalid
    /// - The repository predates generations ([`PublishError::UpgradeRequired`])
    /// - The repository is newer than this publisher
    /// - The current generation's folder is missing
    pub fn open(path: impl AsRef<Path>) -> PublishResult<Self> {
        let root = path.as_ref().to_path_buf();
        let marker = Marker::read(&root)?;
        match marker.version.major {
            0 | 1 => return Err(PublishError::UpgradeRequired(root)),
            2 => {}
            _ => return Err(PublishError::UnsupportedRepositoryVersion(marker.version)),
        }
        let generation = marker.current.ok_or_else(|| {
            PublishError::InvalidRepository("marker names no current generation".to_string())
        })?;
        let repo = Self {
            root,
            version: marker.version,
            created_at: marker.created_at,
            generation: String::new(),
        };
        repo.at_generation(&generation)
    }

    /// The same repository at another generation, for one command's
    /// `--library`. The recorded current generation is unchanged.
    pub fn at_generation(mut self, generation: &str) -> PublishResult<Self> {
        check_generation_name(generation)?;
        if !self.root.join(generation).join(LIBRARY_FILE).is_file() {
            return Err(PublishError::GenerationNotFound(generation.to_string()));
        }
        self.generation = generation.to_string();
        Ok(self)
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

    /// The library generation this repository was opened at.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// The folder of the generation in use: its library index, region
    /// metadata, coverage map, packages and archives.
    pub fn library_root(&self) -> PathBuf {
        self.root.join(&self.generation)
    }

    /// Every library generation in the repository, oldest first.
    pub fn generations(&self) -> PublishResult<Vec<String>> {
        let entries = fs::read_dir(&self.root).map_err(|e| PublishError::ReadFailed {
            path: self.root.clone(),
            source: e,
        })?;
        let mut found: Vec<(u64, String)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| is_generation_name(name))
            .filter(|name| self.root.join(name).join(LIBRARY_FILE).is_file())
            .filter_map(|name| name[1..].parse().ok().map(|n| (n, name)))
            .collect();
        found.sort();
        Ok(found.into_iter().map(|(_, name)| name).collect())
    }

    /// The generation the repository records as current.
    pub fn current_generation(&self) -> PublishResult<String> {
        Marker::read(&self.root)?.current.ok_or_else(|| {
            PublishError::InvalidRepository("marker names no current generation".to_string())
        })
    }

    /// Record `generation` as the current one.
    pub fn use_generation(&self, generation: &str) -> PublishResult<()> {
        check_generation_name(generation)?;
        if !self.root.join(generation).join(LIBRARY_FILE).is_file() {
            return Err(PublishError::GenerationNotFound(generation.to_string()));
        }
        Marker::write(&self.root, self.created_at, generation)
    }

    /// Create an empty generation: a library index with no packages, a
    /// region metadata stub, and `packages/` and `dist/`. Does not make it
    /// current.
    pub fn create_generation(&self, generation: &str) -> PublishResult<()> {
        check_generation_name(generation)?;
        let dir = self.root.join(generation);
        if dir.exists() {
            return Err(PublishError::GenerationExists(generation.to_string()));
        }
        create_generation_files(&dir, &Utc::now())
    }

    /// The shared work area at the repository root.
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    /// Get the path to the packages directory.
    pub fn packages_dir(&self) -> PathBuf {
        self.library_root().join(PACKAGES_DIR)
    }

    /// Get the path to the dist directory.
    pub fn dist_dir(&self) -> PathBuf {
        self.library_root().join(DIST_DIR)
    }

    /// Get the path to the library index file.
    pub fn library_path(&self) -> PathBuf {
        self.library_root().join(LIBRARY_FILE)
    }

    /// Get the path to the region metadata file.
    pub fn region_metadata_path(&self) -> PathBuf {
        self.library_root().join(REGION_METADATA_FILE)
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

    /// List all packages in the generation.
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
}

/// Generate empty library index content.
pub(crate) fn empty_library_content(timestamp: &DateTime<Utc>) -> String {
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

/// The working directories of a generation, created by every way a
/// generation comes to exist.
pub(crate) fn create_working_dirs(generation_dir: &Path) -> PublishResult<()> {
    for dir in [PACKAGES_DIR, DIST_DIR] {
        let dir_path = generation_dir.join(dir);
        fs::create_dir_all(&dir_path).map_err(|e| PublishError::CreateDirectoryFailed {
            path: dir_path,
            source: e,
        })?;
    }
    Ok(())
}

/// A generation folder with an empty index and a region metadata stub.
fn create_generation_files(dir: &Path, now: &DateTime<Utc>) -> PublishResult<()> {
    create_working_dirs(dir)?;
    for (name, content) in [
        (LIBRARY_FILE, empty_library_content(now)),
        (REGION_METADATA_FILE, REGION_METADATA_STUB.to_string()),
    ] {
        let path = dir.join(name);
        fs::write(&path, content).map_err(|e| PublishError::WriteFailed { path, source: e })?;
    }
    Ok(())
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

    /// A format 1.0.0 repository, as every repository was before #333.
    pub(crate) fn legacy_repo(root: &Path) {
        fs::create_dir_all(root.join(PACKAGES_DIR)).unwrap();
        fs::create_dir_all(root.join(DIST_DIR)).unwrap();
        fs::write(
            root.join(REPO_MARKER),
            format!("{REPO_HEADER}\n1.0.0\n2025-01-01T00:00:00Z\n"),
        )
        .unwrap();
        fs::write(root.join(LIBRARY_FILE), empty_library_content(&Utc::now())).unwrap();
        fs::write(root.join(REGION_METADATA_FILE), REGION_METADATA_STUB).unwrap();
    }

    #[test]
    fn generation_names_are_v_and_a_number() {
        for good in ["v1", "v2", "v10"] {
            assert!(is_generation_name(good), "{good}");
        }
        for bad in ["", "v", "v0", "v01", "1", "V1", "v1a", "v-1", "../v1"] {
            assert!(!is_generation_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn init_creates_the_first_generation_and_no_root_index() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        assert!(temp.path().join(REPO_MARKER).exists());
        assert_eq!(repo.generation(), FIRST_GENERATION);
        let v1 = temp.path().join(FIRST_GENERATION);
        for path in [
            v1.join(LIBRARY_FILE),
            v1.join(REGION_METADATA_FILE),
            v1.join(PACKAGES_DIR),
            v1.join(DIST_DIR),
        ] {
            assert!(path.exists(), "{}", path.display());
        }
        assert!(!temp.path().join(LIBRARY_FILE).exists());
        assert!(!temp.path().join(REGION_METADATA_FILE).exists());
        assert!(
            temp.path().join(STAGING_DIR).is_dir(),
            "the shared work area stays at the root"
        );
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
        assert_eq!(repo.generation(), FIRST_GENERATION);
    }

    #[test]
    fn test_open_not_found() {
        let temp = TempDir::new().unwrap();
        let result = Repository::open(temp.path());
        assert!(matches!(result, Err(PublishError::RepositoryNotFound(_))));
    }

    #[test]
    fn a_repository_from_before_generations_must_be_upgraded_first() {
        // #333: an old repository's root index is what pre-0.5 clients read.
        // Opening it as if nothing changed would let a release rewrite it.
        let temp = TempDir::new().unwrap();
        legacy_repo(temp.path());

        let err = Repository::open(temp.path()).unwrap_err();
        assert!(matches!(err, PublishError::UpgradeRequired(_)), "{err}");
        assert!(err.to_string().contains("upgrade --root-as v1"), "{err}");
    }

    #[test]
    fn a_repository_from_a_newer_publisher_is_refused() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(REPO_MARKER),
            format!("{REPO_HEADER}\n3.0.0\n2025-01-01T00:00:00Z\ncurrent v1\n"),
        )
        .unwrap();

        assert!(matches!(
            Repository::open(temp.path()),
            Err(PublishError::UnsupportedRepositoryVersion(_))
        ));
    }

    #[test]
    fn test_exists() {
        let temp = TempDir::new().unwrap();
        assert!(!Repository::exists(temp.path()));

        Repository::init(temp.path()).unwrap();
        assert!(Repository::exists(temp.path()));
    }

    #[test]
    fn every_path_resolves_inside_the_generation() {
        let (temp, repo) = temp_repo();
        let v1 = temp.path().join(FIRST_GENERATION);

        assert_eq!(repo.library_root(), v1);
        assert_eq!(repo.packages_dir(), v1.join(PACKAGES_DIR));
        assert_eq!(repo.dist_dir(), v1.join(DIST_DIR));
        assert_eq!(repo.library_path(), v1.join(LIBRARY_FILE));
        assert_eq!(repo.region_metadata_path(), v1.join(REGION_METADATA_FILE));
        assert!(repo
            .package_dir("na", PackageType::Ortho)
            .starts_with(v1.join(PACKAGES_DIR)));
    }

    #[test]
    fn generations_are_created_listed_switched_and_overridden() {
        let (temp, repo) = temp_repo();
        repo.create_generation("v2").unwrap();
        repo.create_generation("v10").unwrap();
        // A folder without an index is not a generation.
        fs::create_dir_all(temp.path().join("v3")).unwrap();

        assert_eq!(repo.generations().unwrap(), ["v1", "v2", "v10"]);
        assert_eq!(
            repo.current_generation().unwrap(),
            "v1",
            "creating does not switch"
        );

        repo.use_generation("v2").unwrap();
        let reopened = Repository::open(temp.path()).unwrap();
        assert_eq!(reopened.generation(), "v2");
        assert_eq!(reopened.library_root(), temp.path().join("v2"));

        // --library: one command at another generation, the record unchanged.
        let at_v1 = reopened.at_generation("v1").unwrap();
        assert_eq!(at_v1.library_root(), temp.path().join("v1"));
        assert_eq!(Repository::open(temp.path()).unwrap().generation(), "v2");
    }

    #[test]
    fn switching_generation_keeps_the_repository_settings() {
        // The marker also carries [config]; rewriting it must not drop it.
        let (temp, repo) = temp_repo();
        let config = crate::RepoConfig::new(64 * 1024 * 1024).unwrap();
        crate::write_config(temp.path(), &config).unwrap();
        repo.create_generation("v2").unwrap();

        repo.use_generation("v2").unwrap();

        assert_eq!(crate::read_config(temp.path()).unwrap(), config);
        assert_eq!(Repository::open(temp.path()).unwrap().generation(), "v2");
    }

    #[test]
    fn unknown_or_malformed_generations_are_refused() {
        let (_temp, repo) = temp_repo();

        assert!(matches!(
            repo.clone().at_generation("v7"),
            Err(PublishError::GenerationNotFound(_))
        ));
        assert!(matches!(
            repo.use_generation("v7"),
            Err(PublishError::GenerationNotFound(_))
        ));
        assert!(matches!(
            repo.create_generation("../v2"),
            Err(PublishError::InvalidGeneration(_))
        ));
        assert!(matches!(
            repo.create_generation("v1"),
            Err(PublishError::GenerationExists(_))
        ));
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
        assert_eq!(lines[3], "current v1");
    }

    #[test]
    fn test_library_content_format() {
        let (_temp, repo) = temp_repo();

        let content = fs::read_to_string(repo.library_path()).unwrap();
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

    // The stub must be loadable by the coverage map, not merely present.
    // An empty regions map is intentional: `publish coverage` then succeeds
    // and renders every region grey, which is obviously incomplete.
    #[test]
    fn init_stub_parses_as_region_metadata_with_no_regions() {
        let (_temp, repo) = temp_repo();

        let metadata = crate::RegionMetadata::load(&repo.region_metadata_path())
            .expect("stub must parse as RegionMetadata");
        assert!(metadata.regions.is_empty());
    }
}
