//! Package metadata generation for the publisher.
//!
//! This module handles generating and updating `xearthlayer_scenery_package.txt`
//! files, including SHA-256 checksum calculation and version management.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;

use chrono::Utc;
use semver::Version;

use super::{archive_filename, PublishError, PublishResult, Repository};
use crate::package::{
    parse_package_metadata, serialize_package_metadata, ArchivePart, PackageMetadata, PackageType,
};

/// Metadata file name within package directories.
pub const METADATA_FILENAME: &str = "xearthlayer_scenery_package.txt";

/// Default specification version for new packages.
pub const DEFAULT_SPEC_VERSION: &str = "1.0.0";

/// Generate initial package metadata.
///
/// Creates a new `PackageMetadata` with the given parameters. The metadata
/// will have no archive parts yet - those are added when archives are built.
///
/// # Arguments
///
/// * `title` - The package title/region (e.g., "NORTH_AMERICA", "EUROPE")
/// * `version` - The package version
/// * `package_type` - Ortho or Overlay
/// * `mountpoint` - The folder name for mounting (e.g., "zzXEL_na_ortho")
/// * `filename` - The base archive filename (e.g., "zzXEL_na-1.0.0.tar.gz")
pub fn create_metadata(
    title: &str,
    version: Version,
    package_type: PackageType,
    mountpoint: &str,
    filename: &str,
) -> PackageMetadata {
    PackageMetadata {
        spec_version: Version::parse(DEFAULT_SPEC_VERSION).expect("valid version"),
        title: title.to_uppercase(),
        package_version: version,
        published_at: Utc::now(),
        package_type,
        mountpoint: mountpoint.to_string(),
        filename: filename.to_string(),
        parts: Vec::new(),
    }
}

/// Write package metadata to a file.
///
/// Writes the metadata to `xearthlayer_scenery_package.txt` in the package directory.
pub fn write_metadata(metadata: &PackageMetadata, package_dir: &Path) -> PublishResult<()> {
    let path = package_dir.join(METADATA_FILENAME);
    let content = serialize_package_metadata(metadata);

    fs::write(&path, content).map_err(|e| PublishError::WriteFailed { path, source: e })
}

/// Read package metadata from a package directory.
///
/// Reads and parses `xearthlayer_scenery_package.txt` from the package directory.
pub fn read_metadata(package_dir: &Path) -> PublishResult<PackageMetadata> {
    let path = package_dir.join(METADATA_FILENAME);

    let content = fs::read_to_string(&path).map_err(|e| PublishError::ReadFailed {
        path: path.clone(),
        source: e,
    })?;

    parse_package_metadata(&content)
        .map_err(|e| PublishError::InvalidRepository(format!("failed to parse metadata: {}", e)))
}

/// Check if a package directory has metadata.
pub fn has_metadata(package_dir: &Path) -> bool {
    package_dir.join(METADATA_FILENAME).exists()
}

/// Calculate SHA-256 checksum of a file.
///
/// Returns the checksum as a lowercase hex string.
pub fn calculate_sha256(path: &Path) -> PublishResult<String> {
    use sha2::{Digest, Sha256};

    let file = File::open(path).map_err(|e| PublishError::ReadFailed {
        path: path.to_path_buf(),
        source: e,
    })?;

    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|e| PublishError::ReadFailed {
                path: path.to_path_buf(),
                source: e,
            })?;

        if bytes_read == 0 {
            break;
        }

        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

/// Version bump type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionBump {
    /// Increment major version (x.0.0)
    Major,
    /// Increment minor version (_.x.0)
    Minor,
    /// Increment patch version (_._.x)
    Patch,
}

/// Bump a version according to the bump type.
pub fn bump_version(version: &Version, bump: VersionBump) -> Version {
    match bump {
        VersionBump::Major => Version::new(version.major + 1, 0, 0),
        VersionBump::Minor => Version::new(version.major, version.minor + 1, 0),
        VersionBump::Patch => Version::new(version.major, version.minor, version.patch + 1),
    }
}

/// Update the version in package metadata.
///
/// Reads the metadata, updates the version, and writes it back.
pub fn update_version(package_dir: &Path, new_version: Version) -> PublishResult<PackageMetadata> {
    let mut metadata = read_metadata(package_dir)?;
    metadata.package_version = new_version;
    metadata.published_at = Utc::now();

    // Update the filename to reflect new version
    metadata.filename = update_filename_version(&metadata.filename, &metadata.package_version);

    write_metadata(&metadata, package_dir)?;
    Ok(metadata)
}

/// Bump the version in package metadata.
///
/// Reads the metadata, bumps the version, and writes it back.
pub fn bump_package_version(
    package_dir: &Path,
    bump: VersionBump,
) -> PublishResult<PackageMetadata> {
    let metadata = read_metadata(package_dir)?;
    let new_version = bump_version(&metadata.package_version, bump);
    update_version(package_dir, new_version)
}

/// Update the version in a filename.
///
/// Assumes filename format: `prefix-X.Y.Z.extension` where extension starts with `.tar`
fn update_filename_version(filename: &str, version: &Version) -> String {
    // Find the version pattern in the filename
    if let Some(dash_pos) = filename.rfind('-') {
        // Find the start of the extension (.tar.gz, .tar.gz.aa, etc.)
        if let Some(ext_pos) = filename[dash_pos..].find(".tar") {
            let prefix = &filename[..dash_pos];
            let extension = &filename[dash_pos + ext_pos..];
            return format!("{}-{}{}", prefix, version, extension);
        }
    }
    // Fallback: return original if pattern not found
    filename.to_string()
}

/// Add archive parts to package metadata.
///
/// Updates the metadata with the archive parts (checksums, filenames, URLs).
pub fn add_archive_parts(
    package_dir: &Path,
    parts: Vec<ArchivePart>,
) -> PublishResult<PackageMetadata> {
    let mut metadata = read_metadata(package_dir)?;
    metadata.parts = parts;
    metadata.published_at = Utc::now();
    write_metadata(&metadata, package_dir)?;
    Ok(metadata)
}

/// Generate package metadata for a newly processed package.
///
/// This is called after tiles have been processed into a package directory.
/// It creates the initial metadata file with no archive parts.
pub fn generate_initial_metadata(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
    version: Version,
) -> PublishResult<PackageMetadata> {
    let package_dir = repo.package_dir(region, package_type);

    if !package_dir.exists() {
        return Err(PublishError::PackageNotFound {
            region: region.to_string(),
            package_type: package_type.to_string(),
        });
    }

    let mountpoint = package_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Use the archive_filename function for consistency with the archive builder
    let filename = archive_filename(region, package_type, &version);

    let metadata = create_metadata(region, version, package_type, &mountpoint, &filename);

    write_metadata(&metadata, &package_dir)?;

    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create metadata with a placeholder part for tests.
    /// The parser requires at least 1 part, so tests that write and read
    /// metadata must include at least one part.
    fn create_test_metadata(
        title: &str,
        version: Version,
        package_type: PackageType,
        mountpoint: &str,
        filename: &str,
    ) -> PackageMetadata {
        let mut metadata = create_metadata(title, version, package_type, mountpoint, filename);
        metadata.parts = vec![ArchivePart::new(
            "0000000000000000000000000000000000000000000000000000000000000000",
            filename,
            "https://example.com/placeholder",
        )];
        metadata
    }

    #[test]
    fn test_create_metadata() {
        let metadata = create_metadata(
            "north_america",
            Version::new(1, 0, 0),
            PackageType::Ortho,
            "zzXEL_na_ortho",
            "zzXEL_na-1.0.0.tar.gz",
        );

        assert_eq!(metadata.title, "NORTH_AMERICA");
        assert_eq!(metadata.package_version, Version::new(1, 0, 0));
        assert_eq!(metadata.package_type, PackageType::Ortho);
        assert_eq!(metadata.mountpoint, "zzXEL_na_ortho");
        assert!(metadata.parts.is_empty());
    }

    #[test]
    fn test_write_and_read_metadata() {
        let temp = TempDir::new().unwrap();
        let metadata = create_test_metadata(
            "europe",
            Version::new(2, 1, 0),
            PackageType::Overlay,
            "yzXEL_eur_overlay",
            "yzXEL_eur-2.1.0.tar.gz",
        );

        write_metadata(&metadata, temp.path()).unwrap();
        assert!(has_metadata(temp.path()));

        let read_back = read_metadata(temp.path()).unwrap();
        assert_eq!(read_back.title, "EUROPE");
        assert_eq!(read_back.package_version, Version::new(2, 1, 0));
    }

    #[test]
    fn test_has_metadata() {
        let temp = TempDir::new().unwrap();
        assert!(!has_metadata(temp.path()));

        let metadata = create_metadata(
            "na",
            Version::new(1, 0, 0),
            PackageType::Ortho,
            "zzXEL_na_ortho",
            "zzXEL_na-1.0.0.tar.gz",
        );
        write_metadata(&metadata, temp.path()).unwrap();

        assert!(has_metadata(temp.path()));
    }

    #[test]
    fn test_calculate_sha256() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let checksum = calculate_sha256(&file_path).unwrap();

        // Known SHA-256 of "hello world"
        assert_eq!(
            checksum,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_calculate_sha256_empty_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("empty.txt");
        fs::write(&file_path, b"").unwrap();

        let checksum = calculate_sha256(&file_path).unwrap();

        // Known SHA-256 of empty string
        assert_eq!(
            checksum,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_bump_version_major() {
        let version = Version::new(1, 2, 3);
        let bumped = bump_version(&version, VersionBump::Major);
        assert_eq!(bumped, Version::new(2, 0, 0));
    }

    #[test]
    fn test_bump_version_minor() {
        let version = Version::new(1, 2, 3);
        let bumped = bump_version(&version, VersionBump::Minor);
        assert_eq!(bumped, Version::new(1, 3, 0));
    }

    #[test]
    fn test_bump_version_patch() {
        let version = Version::new(1, 2, 3);
        let bumped = bump_version(&version, VersionBump::Patch);
        assert_eq!(bumped, Version::new(1, 2, 4));
    }

    #[test]
    fn test_update_filename_version() {
        assert_eq!(
            update_filename_version("zzXEL_na-1.0.0.tar.gz", &Version::new(2, 0, 0)),
            "zzXEL_na-2.0.0.tar.gz"
        );

        assert_eq!(
            update_filename_version("yzXEL_eur-1.2.3.tar.gz.aa", &Version::new(1, 3, 0)),
            "yzXEL_eur-1.3.0.tar.gz.aa"
        );
    }

    #[test]
    fn test_update_version() {
        let temp = TempDir::new().unwrap();
        let metadata = create_test_metadata(
            "na",
            Version::new(1, 0, 0),
            PackageType::Ortho,
            "zzXEL_na_ortho",
            "zzXEL_na-1.0.0.tar.gz",
        );
        write_metadata(&metadata, temp.path()).unwrap();

        let updated = update_version(temp.path(), Version::new(2, 0, 0)).unwrap();

        assert_eq!(updated.package_version, Version::new(2, 0, 0));
        assert_eq!(updated.filename, "zzXEL_na-2.0.0.tar.gz");
    }

    #[test]
    fn test_bump_package_version() {
        let temp = TempDir::new().unwrap();
        let metadata = create_test_metadata(
            "na",
            Version::new(1, 0, 0),
            PackageType::Ortho,
            "zzXEL_na_ortho",
            "zzXEL_na-1.0.0.tar.gz",
        );
        write_metadata(&metadata, temp.path()).unwrap();

        let bumped = bump_package_version(temp.path(), VersionBump::Minor).unwrap();

        assert_eq!(bumped.package_version, Version::new(1, 1, 0));
    }

    #[test]
    fn test_add_archive_parts() {
        let temp = TempDir::new().unwrap();
        let metadata = create_test_metadata(
            "na",
            Version::new(1, 0, 0),
            PackageType::Ortho,
            "zzXEL_na_ortho",
            "zzXEL_na-1.0.0.tar.gz",
        );
        write_metadata(&metadata, temp.path()).unwrap();

        let parts = vec![
            ArchivePart::new("abc123", "file.tar.gz.aa", "https://example.com/aa"),
            ArchivePart::new("def456", "file.tar.gz.ab", "https://example.com/ab"),
        ];

        let updated = add_archive_parts(temp.path(), parts).unwrap();

        assert_eq!(updated.parts.len(), 2);
        assert_eq!(updated.parts[0].checksum, "abc123");
    }

    #[test]
    fn test_generate_initial_metadata() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        // Create the package directory
        let pkg_dir = repo.package_dir("na", PackageType::Ortho);
        fs::create_dir_all(&pkg_dir).unwrap();

        let metadata =
            generate_initial_metadata(&repo, "na", PackageType::Ortho, Version::new(1, 0, 0))
                .unwrap();

        assert_eq!(metadata.title, "NA");
        assert_eq!(metadata.package_version, Version::new(1, 0, 0));
        assert!(metadata.mountpoint.contains("zzXEL_na_ortho"));

        // Verify file was written
        assert!(has_metadata(&pkg_dir));
    }

    #[test]
    fn test_generate_initial_metadata_package_not_found() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        // Don't create the package directory
        let result =
            generate_initial_metadata(&repo, "na", PackageType::Ortho, Version::new(1, 0, 0));

        assert!(matches!(result, Err(PublishError::PackageNotFound { .. })));
    }
}
