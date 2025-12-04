//! Concrete implementations of the service traits.
//!
//! These implementations wrap the actual xearthlayer publisher functions,
//! adapting them to the trait interfaces used by handlers.

use std::path::{Path, PathBuf};

use semver::Version;

use super::traits::{Output, PublisherService, RepositoryOperations};
use crate::error::CliError;
use xearthlayer::package::{PackageMetadata, PackageType};
use xearthlayer::publisher::{
    BuildResult, ProcessSummary, RegionSuggestion, ReleaseResult, ReleaseStatus, RepoConfig,
    SceneryScanResult, UrlConfigResult, VersionBump,
};

// ============================================================================
// Console Output Implementation
// ============================================================================

/// Standard console output implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConsoleOutput;

impl ConsoleOutput {
    /// Create a new console output.
    pub fn new() -> Self {
        Self
    }
}

impl Output for ConsoleOutput {
    fn println(&self, message: &str) {
        println!("{}", message);
    }

    fn print(&self, message: &str) {
        print!("{}", message);
    }
}

// ============================================================================
// Repository Wrapper
// ============================================================================

/// Wrapper around `xearthlayer::publisher::Repository` implementing the trait.
pub struct RepositoryWrapper {
    inner: xearthlayer::publisher::Repository,
}

impl RepositoryWrapper {
    /// Create a new wrapper from a repository.
    pub fn new(repo: xearthlayer::publisher::Repository) -> Self {
        Self { inner: repo }
    }
}

impl RepositoryOperations for RepositoryWrapper {
    fn root(&self) -> &Path {
        self.inner.root()
    }

    fn package_dir(&self, region: &str, package_type: PackageType) -> PathBuf {
        self.inner.package_dir(region, package_type)
    }

    fn list_packages(&self) -> Result<Vec<(String, PackageType)>, CliError> {
        self.inner
            .list_packages()
            .map_err(|e| CliError::Publish(format!("Failed to list packages: {}", e)))
    }
}

// ============================================================================
// Default Publisher Service Implementation
// ============================================================================

/// Default implementation of the publisher service.
///
/// This wraps the actual xearthlayer publisher functions.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPublisherService;

impl DefaultPublisherService {
    /// Create a new default publisher service.
    pub fn new() -> Self {
        Self
    }
}

impl PublisherService for DefaultPublisherService {
    fn init_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError> {
        let repo = xearthlayer::publisher::Repository::init(path)
            .map_err(|e| CliError::Publish(format!("Failed to initialize repository: {}", e)))?;
        Ok(Box::new(RepositoryWrapper::new(repo)))
    }

    fn open_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError> {
        let repo = xearthlayer::publisher::Repository::open(path)
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;
        Ok(Box::new(RepositoryWrapper::new(repo)))
    }

    fn read_config(&self, repo_root: &Path) -> Result<RepoConfig, CliError> {
        xearthlayer::publisher::read_config(repo_root)
            .map_err(|e| CliError::Publish(format!("Failed to read config: {}", e)))
    }

    fn write_config(&self, repo_root: &Path, config: &RepoConfig) -> Result<(), CliError> {
        xearthlayer::publisher::write_config(repo_root, config)
            .map_err(|e| CliError::Publish(format!("Failed to write config: {}", e)))
    }

    fn scan_scenery(&self, source: &Path) -> Result<SceneryScanResult, CliError> {
        use xearthlayer::publisher::{Ortho4XPProcessor, SceneryProcessor};
        let processor = Ortho4XPProcessor::new();
        processor
            .scan(source)
            .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))
    }

    fn scan_overlay(&self, source: &Path) -> Result<SceneryScanResult, CliError> {
        use xearthlayer::publisher::{OverlayProcessor, SceneryProcessor};
        let processor = OverlayProcessor::new();
        processor
            .scan(source)
            .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))
    }

    fn analyze_tiles(&self, coords: &[(i32, i32)]) -> RegionSuggestion {
        xearthlayer::publisher::analyze_tiles(coords)
    }

    fn process_tiles(
        &self,
        scan_result: &SceneryScanResult,
        region: &str,
        package_type: PackageType,
        repo: &dyn RepositoryOperations,
    ) -> Result<ProcessSummary, CliError> {
        use xearthlayer::publisher::{Ortho4XPProcessor, OverlayProcessor, SceneryProcessor};

        // We need to get the actual Repository from the wrapper
        // This is a limitation - we need to downcast or use a different approach
        // For now, we'll re-open the repository
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        // Use appropriate processor based on package type
        match package_type {
            PackageType::Ortho => {
                let processor = Ortho4XPProcessor::new();
                processor
                    .process(scan_result, region, package_type, &actual_repo)
                    .map_err(|e| CliError::Publish(format!("Processing failed: {}", e)))
            }
            PackageType::Overlay => {
                let processor = OverlayProcessor::new();
                processor
                    .process(scan_result, region, package_type, &actual_repo)
                    .map_err(|e| CliError::Publish(format!("Processing failed: {}", e)))
            }
        }
    }

    fn generate_initial_metadata(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        version: Version,
    ) -> Result<(), CliError> {
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer::publisher::generate_initial_metadata(
            &actual_repo,
            region,
            package_type,
            version,
        )
        .map_err(|e| CliError::Publish(format!("Failed to generate metadata: {}", e)))?;
        Ok(())
    }

    fn read_metadata(&self, package_dir: &Path) -> Result<PackageMetadata, CliError> {
        xearthlayer::publisher::read_metadata(package_dir)
            .map_err(|e| CliError::Publish(format!("Failed to read metadata: {}", e)))
    }

    fn build_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        config: &RepoConfig,
    ) -> Result<BuildResult, CliError> {
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer::publisher::build_package(&actual_repo, region, package_type, config)
            .map_err(|e| CliError::Publish(format!("Build failed: {}", e)))
    }

    fn generate_part_urls(
        &self,
        base_url: &str,
        archive_name: &str,
        suffixes: &[&str],
    ) -> Vec<String> {
        xearthlayer::publisher::generate_part_urls(base_url, archive_name, suffixes)
    }

    fn configure_urls(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        urls: &[String],
        verify: bool,
    ) -> Result<UrlConfigResult, CliError> {
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer::publisher::configure_urls(&actual_repo, region, package_type, urls, verify)
            .map_err(|e| CliError::Publish(format!("Failed to configure URLs: {}", e)))
    }

    fn bump_package_version(
        &self,
        package_dir: &Path,
        bump: VersionBump,
    ) -> Result<PackageMetadata, CliError> {
        xearthlayer::publisher::bump_package_version(package_dir, bump)
            .map_err(|e| CliError::Publish(format!("Failed to bump version: {}", e)))
    }

    fn update_version(
        &self,
        package_dir: &Path,
        version: Version,
    ) -> Result<PackageMetadata, CliError> {
        xearthlayer::publisher::update_version(package_dir, version)
            .map_err(|e| CliError::Publish(format!("Failed to set version: {}", e)))
    }

    fn release_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        metadata_url: &str,
    ) -> Result<ReleaseResult, CliError> {
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer::publisher::release_package(&actual_repo, region, package_type, metadata_url)
            .map_err(|e| CliError::Publish(format!("Release failed: {}", e)))
    }

    fn get_release_status(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
    ) -> ReleaseStatus {
        // We need an actual Repository here - try to open it
        match xearthlayer::publisher::Repository::open(repo.root()) {
            Ok(actual_repo) => {
                xearthlayer::publisher::get_release_status(&actual_repo, region, package_type)
            }
            Err(_) => ReleaseStatus::NotBuilt,
        }
    }

    fn validate_repository(&self, repo: &dyn RepositoryOperations) -> Result<(), CliError> {
        let actual_repo = xearthlayer::publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer::publisher::validate_repository(&actual_repo)
            .map_err(|e| CliError::Publish(format!("Validation failed: {}", e)))
    }
}
