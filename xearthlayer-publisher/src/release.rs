//! Release workflow orchestration for package publishing.
//!
//! Coordinates the multi-phase release workflow:
//! 1. Build - Create archives and generate metadata
//! 2. URLs - Verify archive URLs after user uploads
//! 3. Release - Update library index and finalize
//!
//! The workflow handles the "genesis paradox" where URLs can't be known
//! until files are uploaded, but metadata needs URLs to be complete.

use semver::Version;

use super::archive::{build_archive, ArchiveBuildResult};
use super::library::LibraryManager;
use super::metadata::{add_archive_parts, read_metadata, write_metadata, METADATA_FILENAME};
use super::urls::{validate_url, UrlVerifier};
use super::{PublishError, PublishResult, RepoConfig, Repository};
use xearthlayer_package::PackageType;

/// Status of a package in the release workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseStatus {
    /// Package exists but no archive built yet.
    NotBuilt,

    /// Archive built, awaiting URL configuration.
    AwaitingUrls {
        /// Archive build result.
        archive_name: String,
        /// Number of parts.
        part_count: usize,
    },

    /// URLs configured and verified, ready for release.
    Ready,

    /// Package released to library index.
    Released,
}

/// Result of the build phase.
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// Region code.
    pub region: String,

    /// Package type.
    pub package_type: PackageType,

    /// Package version.
    pub version: Version,

    /// Archive build information.
    pub archive: ArchiveBuildResult,

    /// Path to the package metadata file.
    pub metadata_path: std::path::PathBuf,
}

/// Result of URL configuration.
#[derive(Debug, Clone)]
pub struct UrlConfigResult {
    /// Region code.
    pub region: String,

    /// Package type.
    pub package_type: PackageType,

    /// Number of URLs configured.
    pub urls_configured: usize,

    /// Number of URLs verified successfully.
    pub urls_verified: usize,

    /// Any URLs that failed verification.
    pub failed_urls: Vec<(String, String)>, // (url, error)
}

/// Result of the release phase.
#[derive(Debug, Clone)]
pub struct ReleaseResult {
    /// Region code.
    pub region: String,

    /// Package type.
    pub package_type: PackageType,

    /// Package version.
    pub version: Version,

    /// New library sequence number.
    pub sequence: u64,
}

/// Build archives for a package.
///
/// This is the first phase of the release workflow. It:
/// 1. Creates tar.gz archives from the package directory
/// 2. Splits large archives into parts
/// 3. Generates checksums for each part
/// 4. Updates the package metadata (without URLs)
///
/// After this phase, the user should upload the archives and then
/// run the URL configuration phase.
pub fn build_package(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
    config: &RepoConfig,
) -> PublishResult<BuildResult> {
    let package_dir = repo.package_dir(region, package_type);

    if !package_dir.exists() {
        return Err(PublishError::PackageNotFound {
            region: region.to_string(),
            package_type: package_type.code().to_string(),
        });
    }

    // Read existing metadata to get version
    let metadata = read_metadata(&package_dir)?;
    let version = metadata.package_version.clone();

    // Build archive
    let archive = build_archive(
        &package_dir,
        &repo.dist_dir(),
        region,
        package_type,
        &version,
        config,
    )?;

    // Update metadata with part information (no URLs yet)
    let parts: Vec<_> = archive
        .parts
        .iter()
        .map(|p| xearthlayer_package::ArchivePart::new(p.checksum.clone(), p.filename.clone(), ""))
        .collect();

    add_archive_parts(&package_dir, parts)?;
    let metadata_path = package_dir.join(METADATA_FILENAME);

    Ok(BuildResult {
        region: region.to_string(),
        package_type,
        version,
        archive,
        metadata_path,
    })
}

/// Configure URLs for archive parts.
///
/// This is the second phase of the release workflow. It:
/// 1. Validates URL format
/// 2. Optionally verifies each URL is accessible
/// 3. Optionally verifies checksums match
/// 4. Updates the package metadata with URLs
///
/// # Arguments
///
/// * `repo` - The repository
/// * `region` - Region code
/// * `package_type` - Package type
/// * `urls` - URLs for each archive part (in order)
/// * `verify` - Whether to verify URLs are accessible and checksums match
pub fn configure_urls(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
    urls: &[String],
    verify: bool,
) -> PublishResult<UrlConfigResult> {
    let package_dir = repo.package_dir(region, package_type);
    let mut metadata = read_metadata(&package_dir)?;

    // Validate we have the right number of URLs
    if urls.len() != metadata.parts.len() {
        return Err(PublishError::InvalidUrl(format!(
            "expected {} URLs, got {}",
            metadata.parts.len(),
            urls.len()
        )));
    }

    // Validate URL format
    for url in urls {
        validate_url(url)?;
    }

    let mut failed_urls = Vec::new();
    let mut urls_verified = 0;

    // Optionally verify each URL
    if verify {
        let verifier = UrlVerifier::new();

        for (i, url) in urls.iter().enumerate() {
            let expected_checksum = &metadata.parts[i].checksum;
            let verification = verifier.verify_checksum(url, expected_checksum);

            if verification.is_valid() {
                urls_verified += 1;
            } else {
                let error = verification
                    .error
                    .unwrap_or_else(|| "unknown error".to_string());
                failed_urls.push((url.clone(), error));
            }
        }
    } else {
        urls_verified = urls.len();
    }

    // Update metadata with URLs
    for (i, url) in urls.iter().enumerate() {
        metadata.parts[i].url = url.clone();
    }

    write_metadata(&metadata, &package_dir)?;

    Ok(UrlConfigResult {
        region: region.to_string(),
        package_type,
        urls_configured: urls.len(),
        urls_verified,
        failed_urls,
    })
}

/// Release a package to the library index.
///
/// This is the final phase of the release workflow. It:
/// 1. Validates the package is ready (has URLs configured)
/// 2. Adds or updates the package in the library index
/// 3. Saves the library with incremented sequence number
///
/// # Arguments
///
/// * `repo` - The repository
/// * `region` - Region code
/// * `package_type` - Package type
/// * `metadata_url` - URL where the package metadata file will be hosted
pub fn release_package(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
    metadata_url: &str,
) -> PublishResult<ReleaseResult> {
    use xearthlayer_package::ValidationContext;

    let package_dir = repo.package_dir(region, package_type);
    let metadata = read_metadata(&package_dir)?;
    let metadata_path = package_dir.join(METADATA_FILENAME);

    // Validate metadata is ready for release
    let errors = metadata.validate(ValidationContext::Release);
    if !errors.is_empty() {
        return Err(PublishError::ReleaseValidation(
            errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    // Validate metadata URL
    validate_url(metadata_url)?;

    // Update library index
    let mut library = LibraryManager::open_or_create(repo.root())?;

    library.add_or_update(
        &metadata_path,
        &metadata.title,
        package_type,
        metadata.package_version.clone(),
        metadata_url,
    )?;

    library.save()?;

    Ok(ReleaseResult {
        region: region.to_string(),
        package_type,
        version: metadata.package_version,
        sequence: library.sequence(),
    })
}

/// Get the release status of a package.
pub fn get_release_status(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
) -> ReleaseStatus {
    let package_dir = repo.package_dir(region, package_type);

    // Check if package exists
    if !package_dir.exists() {
        return ReleaseStatus::NotBuilt;
    }

    // Read metadata
    let metadata = match read_metadata(&package_dir) {
        Ok(m) => m,
        Err(_) => return ReleaseStatus::NotBuilt,
    };

    // Check if any parts exist (package has been built)
    if !metadata.has_parts() {
        return ReleaseStatus::NotBuilt;
    }

    // Check if all parts have URLs configured
    if !metadata.has_all_urls() {
        return ReleaseStatus::AwaitingUrls {
            archive_name: metadata.filename.clone(),
            part_count: metadata.parts.len(),
        };
    }

    // Check if in library
    let library = match LibraryManager::open_or_create(repo.root()) {
        Ok(l) => l,
        Err(_) => return ReleaseStatus::Ready,
    };

    if library.contains(region, package_type) {
        ReleaseStatus::Released
    } else {
        ReleaseStatus::Ready
    }
}

/// Validate a repository is ready for releases.
///
/// Checks that all required files and directories exist.
pub fn validate_repository(repo: &Repository) -> PublishResult<()> {
    // Check packages directory
    let packages_dir = repo.packages_dir();
    if !packages_dir.exists() {
        return Err(PublishError::InvalidRepository(
            "packages directory missing".to_string(),
        ));
    }

    // Check dist directory
    let dist_dir = repo.dist_dir();
    if !dist_dir.exists() {
        return Err(PublishError::InvalidRepository(
            "dist directory missing".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        (temp, repo)
    }

    fn setup_test_package(repo: &Repository, region: &str, package_type: PackageType) {
        use crate::archive::archive_filename;
        use crate::metadata::create_metadata;

        let package_dir = repo.package_dir(region, package_type);
        fs::create_dir_all(&package_dir).unwrap();

        // Create some content
        let terrain_dir = package_dir.join("terrain");
        fs::create_dir_all(&terrain_dir).unwrap();
        fs::write(terrain_dir.join("test.ter"), "terrain content").unwrap();

        // Create initial metadata with no parts (will be added by build_package)
        let mountpoint = package_dir.file_name().unwrap().to_str().unwrap();
        let version = Version::new(1, 0, 0);
        let filename = archive_filename(region, package_type, &version);
        let metadata = create_metadata(
            &region.to_uppercase(),
            version,
            package_type,
            mountpoint,
            &filename,
        );
        write_metadata(&metadata, &package_dir).unwrap();
    }

    #[test]
    fn test_release_status_not_built() {
        let (_temp, repo) = setup_test_repo();
        let status = get_release_status(&repo, "na", PackageType::Ortho);
        assert_eq!(status, ReleaseStatus::NotBuilt);
    }

    #[test]
    fn test_release_status_awaiting_urls() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        let status = get_release_status(&repo, "na", PackageType::Ortho);
        assert!(matches!(status, ReleaseStatus::AwaitingUrls { .. }));
    }

    #[test]
    fn test_validate_repository() {
        let (_temp, repo) = setup_test_repo();
        assert!(validate_repository(&repo).is_ok());
    }

    #[test]
    fn test_build_package() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        let result = build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        assert_eq!(result.region, "na");
        assert_eq!(result.package_type, PackageType::Ortho);
        assert!(!result.archive.parts.is_empty());
    }

    #[test]
    fn test_build_package_not_found() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        let result = build_package(&repo, "na", PackageType::Ortho, &config);
        assert!(matches!(result, Err(PublishError::PackageNotFound { .. })));
    }

    #[test]
    fn test_configure_urls_wrong_count() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        // Try with wrong number of URLs
        let result = configure_urls(
            &repo,
            "na",
            PackageType::Ortho,
            &[
                "https://example.com/a".to_string(),
                "https://example.com/b".to_string(),
            ],
            false,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_configure_urls_invalid_format() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        // Try with invalid URL
        let result = configure_urls(
            &repo,
            "na",
            PackageType::Ortho,
            &["not-a-url".to_string()],
            false,
        );

        assert!(matches!(result, Err(PublishError::InvalidUrl(_))));
    }

    #[test]
    fn test_configure_urls_valid() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        let build = build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        // Configure valid URLs (without verification)
        let urls: Vec<String> = build
            .archive
            .parts
            .iter()
            .enumerate()
            .map(|(i, _)| format!("https://example.com/part{}.tar.gz.aa", i))
            .collect();

        let result = configure_urls(&repo, "na", PackageType::Ortho, &urls, false).unwrap();

        assert_eq!(result.urls_configured, build.archive.parts.len());
        assert_eq!(result.urls_verified, build.archive.parts.len());
        assert!(result.failed_urls.is_empty());
    }

    #[test]
    fn test_release_package_no_urls() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        // Try to release without URLs configured
        let result = release_package(
            &repo,
            "na",
            PackageType::Ortho,
            "https://example.com/meta.txt",
        );

        assert!(matches!(result, Err(PublishError::ReleaseValidation(_))));
    }

    #[test]
    fn test_full_workflow() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        // Setup package
        setup_test_package(&repo, "na", PackageType::Ortho);

        // Phase 1: Build
        let build = build_package(&repo, "na", PackageType::Ortho, &config).unwrap();
        assert_eq!(
            get_release_status(&repo, "na", PackageType::Ortho),
            ReleaseStatus::AwaitingUrls {
                archive_name: build.archive.archive_name.clone(),
                part_count: build.archive.parts.len(),
            }
        );

        // Phase 2: Configure URLs
        let urls: Vec<String> = build
            .archive
            .parts
            .iter()
            .enumerate()
            .map(|(i, _)| format!("https://example.com/part{}.tar.gz", i))
            .collect();
        configure_urls(&repo, "na", PackageType::Ortho, &urls, false).unwrap();
        assert_eq!(
            get_release_status(&repo, "na", PackageType::Ortho),
            ReleaseStatus::Ready
        );

        // Phase 3: Release
        let release = release_package(
            &repo,
            "na",
            PackageType::Ortho,
            "https://example.com/na/ortho/meta.txt",
        )
        .unwrap();
        assert_eq!(release.sequence, 1);
        assert_eq!(
            get_release_status(&repo, "na", PackageType::Ortho),
            ReleaseStatus::Released
        );
    }

    #[test]
    fn test_build_result_debug() {
        let (_temp, repo) = setup_test_repo();
        let config = RepoConfig::default();

        setup_test_package(&repo, "na", PackageType::Ortho);
        let result = build_package(&repo, "na", PackageType::Ortho, &config).unwrap();

        let debug = format!("{:?}", result);
        assert!(debug.contains("BuildResult"));
    }

    #[test]
    fn test_url_config_result_debug() {
        let result = UrlConfigResult {
            region: "na".to_string(),
            package_type: PackageType::Ortho,
            urls_configured: 3,
            urls_verified: 3,
            failed_urls: vec![],
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("UrlConfigResult"));
    }

    #[test]
    fn test_release_result_debug() {
        let result = ReleaseResult {
            region: "na".to_string(),
            package_type: PackageType::Ortho,
            version: Version::new(1, 0, 0),
            sequence: 1,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("ReleaseResult"));
    }

    #[test]
    fn test_release_status_eq() {
        assert_eq!(ReleaseStatus::NotBuilt, ReleaseStatus::NotBuilt);
        assert_eq!(ReleaseStatus::Ready, ReleaseStatus::Ready);
        assert_eq!(ReleaseStatus::Released, ReleaseStatus::Released);
        assert_ne!(ReleaseStatus::NotBuilt, ReleaseStatus::Ready);
    }
}
