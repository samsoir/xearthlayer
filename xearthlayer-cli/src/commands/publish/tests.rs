//! Tests for publish command handlers.
//!
//! This module provides mock implementations of the service traits and
//! comprehensive tests for each command handler.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use semver::Version;

use super::args::*;
use super::handlers::*;
use super::traits::*;
use crate::error::CliError;
use xearthlayer::package::{ArchivePart, PackageMetadata, PackageType};
use xearthlayer::publisher::{
    ArchiveBuildResult, BuildResult, ProcessSummary, RegionSuggestion, ReleaseResult,
    ReleaseStatus, RepoConfig, SceneryScanResult, SuggestedRegion, TileInfo, UrlConfigResult,
    VersionBump,
};

use chrono::Utc;

// ============================================================================
// Mock Output Implementation
// ============================================================================

/// Mock output that captures all messages for verification.
pub struct MockOutput {
    messages: RwLock<Vec<String>>,
}

impl Default for MockOutput {
    fn default() -> Self {
        Self {
            messages: RwLock::new(Vec::new()),
        }
    }
}

impl MockOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all captured messages.
    #[allow(dead_code)]
    pub fn messages(&self) -> Vec<String> {
        self.messages.read().unwrap().clone()
    }

    /// Check if any message contains the given substring.
    pub fn contains(&self, substring: &str) -> bool {
        self.messages
            .read()
            .unwrap()
            .iter()
            .any(|m| m.contains(substring))
    }

    /// Get the full output as a single string.
    #[allow(dead_code)]
    pub fn full_output(&self) -> String {
        self.messages.read().unwrap().join("\n")
    }
}

impl Output for MockOutput {
    fn println(&self, message: &str) {
        self.messages.write().unwrap().push(message.to_string());
    }

    fn print(&self, message: &str) {
        self.messages.write().unwrap().push(message.to_string());
    }
}

// ============================================================================
// Mock Repository Implementation
// ============================================================================

/// Mock repository for testing.
pub struct MockRepository {
    root: PathBuf,
    packages: Vec<(String, PackageType)>,
}

impl MockRepository {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            packages: Vec::new(),
        }
    }

    pub fn with_packages(root: PathBuf, packages: Vec<(String, PackageType)>) -> Self {
        Self { root, packages }
    }
}

impl RepositoryOperations for MockRepository {
    fn root(&self) -> &Path {
        &self.root
    }

    fn package_dir(&self, region: &str, package_type: PackageType) -> PathBuf {
        self.root
            .join("packages")
            .join(format!("{}_{}", region, package_type.folder_suffix()))
    }

    fn list_packages(&self) -> Result<Vec<(String, PackageType)>, CliError> {
        Ok(self.packages.clone())
    }
}

// ============================================================================
// Mock Publisher Service Implementation
// ============================================================================

/// Builder for configuring mock publisher behavior.
#[derive(Default)]
pub struct MockPublisherServiceBuilder {
    init_result: Option<Result<PathBuf, String>>,
    open_result: Option<Result<PathBuf, String>>,
    packages: Vec<(String, PackageType)>,
    scan_result: Option<Result<SceneryScanResult, String>>,
    process_result: Option<Result<ProcessSummary, String>>,
    build_result: Option<Result<BuildResult, String>>,
    read_metadata_result: Option<Result<PackageMetadata, String>>,
    config: Option<RepoConfig>,
    release_status: Option<ReleaseStatus>,
    url_config_result: Option<Result<UrlConfigResult, String>>,
    release_result: Option<Result<ReleaseResult, String>>,
}

impl MockPublisherServiceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_init_success(mut self, path: PathBuf) -> Self {
        self.init_result = Some(Ok(path));
        self
    }

    pub fn with_init_error(mut self, error: &str) -> Self {
        self.init_result = Some(Err(error.to_string()));
        self
    }

    pub fn with_open_success(mut self, path: PathBuf) -> Self {
        self.open_result = Some(Ok(path));
        self
    }

    #[allow(dead_code)]
    pub fn with_open_error(mut self, error: &str) -> Self {
        self.open_result = Some(Err(error.to_string()));
        self
    }

    pub fn with_packages(mut self, packages: Vec<(String, PackageType)>) -> Self {
        self.packages = packages;
        self
    }

    pub fn with_scan_success(mut self, result: SceneryScanResult) -> Self {
        self.scan_result = Some(Ok(result));
        self
    }

    pub fn with_scan_error(mut self, error: &str) -> Self {
        self.scan_result = Some(Err(error.to_string()));
        self
    }

    pub fn with_process_success(mut self, summary: ProcessSummary) -> Self {
        self.process_result = Some(Ok(summary));
        self
    }

    pub fn with_build_success(mut self, result: BuildResult) -> Self {
        self.build_result = Some(Ok(result));
        self
    }

    pub fn with_metadata(mut self, metadata: PackageMetadata) -> Self {
        self.read_metadata_result = Some(Ok(metadata));
        self
    }

    #[allow(dead_code)]
    pub fn with_metadata_error(mut self, error: &str) -> Self {
        self.read_metadata_result = Some(Err(error.to_string()));
        self
    }

    pub fn with_config(mut self, config: RepoConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_release_status(mut self, status: ReleaseStatus) -> Self {
        self.release_status = Some(status);
        self
    }

    pub fn with_url_config_success(mut self, result: UrlConfigResult) -> Self {
        self.url_config_result = Some(Ok(result));
        self
    }

    pub fn with_release_success(mut self, result: ReleaseResult) -> Self {
        self.release_result = Some(Ok(result));
        self
    }

    pub fn build(self) -> MockPublisherService {
        MockPublisherService {
            init_result: self.init_result.unwrap_or(Ok(PathBuf::from("/tmp/repo"))),
            open_result: self.open_result.unwrap_or(Ok(PathBuf::from("/tmp/repo"))),
            packages: self.packages,
            scan_result: self
                .scan_result
                .unwrap_or_else(|| Ok(SceneryScanResult::new(Vec::new()))),
            process_result: self.process_result.unwrap_or(Ok(ProcessSummary::default())),
            build_result: self.build_result,
            read_metadata_result: self.read_metadata_result,
            config: self.config.unwrap_or_default(),
            release_status: self.release_status.unwrap_or(ReleaseStatus::NotBuilt),
            url_config_result: self.url_config_result,
            release_result: self.release_result,
        }
    }
}

/// Mock publisher service for testing handlers.
pub struct MockPublisherService {
    init_result: Result<PathBuf, String>,
    open_result: Result<PathBuf, String>,
    packages: Vec<(String, PackageType)>,
    scan_result: Result<SceneryScanResult, String>,
    process_result: Result<ProcessSummary, String>,
    build_result: Option<Result<BuildResult, String>>,
    read_metadata_result: Option<Result<PackageMetadata, String>>,
    config: RepoConfig,
    release_status: ReleaseStatus,
    url_config_result: Option<Result<UrlConfigResult, String>>,
    release_result: Option<Result<ReleaseResult, String>>,
}

impl PublisherService for MockPublisherService {
    fn init_repository(&self, _path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError> {
        match &self.init_result {
            Ok(path) => Ok(Box::new(MockRepository::new(path.clone()))),
            Err(e) => Err(CliError::Publish(e.clone())),
        }
    }

    fn open_repository(&self, _path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError> {
        match &self.open_result {
            Ok(path) => Ok(Box::new(MockRepository::with_packages(
                path.clone(),
                self.packages.clone(),
            ))),
            Err(e) => Err(CliError::Publish(e.clone())),
        }
    }

    fn read_config(&self, _repo_root: &Path) -> Result<RepoConfig, CliError> {
        Ok(self.config.clone())
    }

    fn write_config(&self, _repo_root: &Path, _config: &RepoConfig) -> Result<(), CliError> {
        Ok(())
    }

    fn scan_scenery(&self, _source: &Path) -> Result<SceneryScanResult, CliError> {
        match &self.scan_result {
            Ok(result) => Ok(result.clone()),
            Err(e) => Err(CliError::Publish(e.clone())),
        }
    }

    fn scan_overlay(&self, _source: &Path) -> Result<SceneryScanResult, CliError> {
        // Use the same scan_result for overlays in tests
        match &self.scan_result {
            Ok(result) => Ok(result.clone()),
            Err(e) => Err(CliError::Publish(e.clone())),
        }
    }

    fn analyze_tiles(&self, coords: &[(i32, i32)]) -> RegionSuggestion {
        if coords.is_empty() {
            RegionSuggestion {
                region: None,
                regions_found: Vec::new(),
                ambiguous_tiles: Vec::new(),
                overlapping_tiles: Vec::new(),
            }
        } else {
            RegionSuggestion {
                region: Some(SuggestedRegion::NorthAmerica),
                regions_found: vec![SuggestedRegion::NorthAmerica],
                ambiguous_tiles: Vec::new(),
                overlapping_tiles: Vec::new(),
            }
        }
    }

    fn process_tiles(
        &self,
        _scan_result: &SceneryScanResult,
        _region: &str,
        _package_type: PackageType,
        _repo: &dyn RepositoryOperations,
    ) -> Result<ProcessSummary, CliError> {
        match &self.process_result {
            Ok(result) => Ok(result.clone()),
            Err(e) => Err(CliError::Publish(e.clone())),
        }
    }

    fn generate_initial_metadata(
        &self,
        _repo: &dyn RepositoryOperations,
        _region: &str,
        _package_type: PackageType,
        _version: Version,
    ) -> Result<(), CliError> {
        Ok(())
    }

    fn read_metadata(&self, _package_dir: &Path) -> Result<PackageMetadata, CliError> {
        match &self.read_metadata_result {
            Some(Ok(metadata)) => Ok(metadata.clone()),
            Some(Err(e)) => Err(CliError::Publish(e.clone())),
            None => Ok(create_test_metadata()),
        }
    }

    fn build_package(
        &self,
        _repo: &dyn RepositoryOperations,
        _region: &str,
        _package_type: PackageType,
        _config: &RepoConfig,
    ) -> Result<BuildResult, CliError> {
        match &self.build_result {
            Some(Ok(result)) => Ok(result.clone()),
            Some(Err(e)) => Err(CliError::Publish(e.clone())),
            None => Ok(create_test_build_result()),
        }
    }

    fn generate_part_urls(
        &self,
        base_url: &str,
        archive_name: &str,
        suffixes: &[&str],
    ) -> Vec<String> {
        suffixes
            .iter()
            .map(|s| format!("{}{}.{}", base_url, archive_name, s))
            .collect()
    }

    fn configure_urls(
        &self,
        _repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        _urls: &[String],
        _verify: bool,
    ) -> Result<UrlConfigResult, CliError> {
        match &self.url_config_result {
            Some(Ok(result)) => Ok(result.clone()),
            Some(Err(e)) => Err(CliError::Publish(e.clone())),
            None => Ok(UrlConfigResult {
                region: region.to_string(),
                package_type,
                urls_configured: 2,
                urls_verified: 0,
                failed_urls: Vec::new(),
            }),
        }
    }

    fn bump_package_version(
        &self,
        _package_dir: &Path,
        _bump: VersionBump,
    ) -> Result<PackageMetadata, CliError> {
        let mut metadata = create_test_metadata();
        metadata.package_version = Version::new(1, 1, 0);
        Ok(metadata)
    }

    fn update_version(
        &self,
        _package_dir: &Path,
        version: Version,
    ) -> Result<PackageMetadata, CliError> {
        let mut metadata = create_test_metadata();
        metadata.package_version = version;
        Ok(metadata)
    }

    fn release_package(
        &self,
        _repo: &dyn RepositoryOperations,
        _region: &str,
        _package_type: PackageType,
        _metadata_url: &str,
    ) -> Result<ReleaseResult, CliError> {
        match &self.release_result {
            Some(Ok(result)) => Ok(result.clone()),
            Some(Err(e)) => Err(CliError::Publish(e.clone())),
            None => Ok(ReleaseResult {
                region: "na".to_string(),
                package_type: PackageType::Ortho,
                version: Version::new(1, 0, 0),
                sequence: 1,
            }),
        }
    }

    fn get_release_status(
        &self,
        _repo: &dyn RepositoryOperations,
        _region: &str,
        _package_type: PackageType,
    ) -> ReleaseStatus {
        self.release_status.clone()
    }

    fn validate_repository(&self, _repo: &dyn RepositoryOperations) -> Result<(), CliError> {
        Ok(())
    }

    fn generate_coverage_map(
        &self,
        _packages_dir: &Path,
        _output_path: &Path,
        _width: u32,
        _height: u32,
        _dark: bool,
    ) -> Result<CoverageResult, CliError> {
        Ok(CoverageResult {
            total_tiles: 100,
            tiles_by_region: [("na".to_string(), 100)].into_iter().collect(),
        })
    }

    fn generate_coverage_geojson(
        &self,
        _packages_dir: &Path,
        _output_path: &Path,
    ) -> Result<CoverageResult, CliError> {
        Ok(CoverageResult {
            total_tiles: 100,
            tiles_by_region: [("na".to_string(), 100)].into_iter().collect(),
        })
    }
}

// ============================================================================
// Test Helpers
// ============================================================================

fn create_test_metadata() -> PackageMetadata {
    PackageMetadata {
        spec_version: Version::new(1, 0, 0),
        title: "NORTH AMERICA".to_string(),
        package_version: Version::new(1, 0, 0),
        published_at: Utc::now(),
        package_type: PackageType::Ortho,
        mountpoint: "zzXEL_na".to_string(),
        filename: "zzXEL_na_ortho-1.0.0.tar.gz".to_string(),
        parts: vec![
            ArchivePart::new(
                "abc123",
                "zzXEL_na_ortho-1.0.0.tar.gz.aa",
                "https://example.com/aa",
            ),
            ArchivePart::new(
                "def456",
                "zzXEL_na_ortho-1.0.0.tar.gz.ab",
                "https://example.com/ab",
            ),
        ],
    }
}

fn create_test_build_result() -> BuildResult {
    BuildResult {
        region: "na".to_string(),
        package_type: PackageType::Ortho,
        version: Version::new(1, 0, 0),
        archive: ArchiveBuildResult {
            archive_name: "zzXEL_na_ortho-1.0.0.tar.gz".to_string(),
            total_size: 1024 * 1024 * 800,
            parts: vec![
                xearthlayer::publisher::ArchivePart {
                    filename: "zzXEL_na_ortho-1.0.0.tar.gz.aa".to_string(),
                    path: PathBuf::from("/tmp/dist/na/ortho/zzXEL_na_ortho-1.0.0.tar.gz.aa"),
                    size: 1024 * 1024 * 500,
                    checksum: "abc123".to_string(),
                },
                xearthlayer::publisher::ArchivePart {
                    filename: "zzXEL_na_ortho-1.0.0.tar.gz.ab".to_string(),
                    path: PathBuf::from("/tmp/dist/na/ortho/zzXEL_na_ortho-1.0.0.tar.gz.ab"),
                    size: 1024 * 1024 * 300,
                    checksum: "def456".to_string(),
                },
            ],
        },
        metadata_path: PathBuf::from("/tmp/packages/na/ortho/xearthlayer_scenery_package.txt"),
    }
}

fn create_test_scan_result() -> SceneryScanResult {
    SceneryScanResult::new(vec![
        TileInfo {
            id: "+37-122".to_string(),
            path: PathBuf::from("/tiles/+37-122"),
            latitude: 37,
            longitude: -122,
            dsf_files: vec![PathBuf::from("test.dsf")],
            ter_files: vec![PathBuf::from("test.ter")],
            mask_files: vec![PathBuf::from("test_mask.png")],
            dds_files: Vec::new(),
        },
        TileInfo {
            id: "+38-122".to_string(),
            path: PathBuf::from("/tiles/+38-122"),
            latitude: 38,
            longitude: -122,
            dsf_files: vec![PathBuf::from("test.dsf")],
            ter_files: vec![PathBuf::from("test.ter")],
            mask_files: Vec::new(),
            dds_files: Vec::new(),
        },
    ])
}

// ============================================================================
// Init Handler Tests
// ============================================================================

#[cfg(test)]
mod init_tests {
    use super::*;

    #[test]
    fn test_init_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_init_success(PathBuf::from("/test/repo"))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = InitArgs {
            path: PathBuf::from("/test/repo"),
            part_size: "500M".to_string(),
        };

        let result = InitHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Initialized XEarthLayer package repository"));
        assert!(output.contains("/test/repo"));
        assert!(output.contains("packages/"));
        assert!(output.contains("dist/"));
    }

    #[test]
    fn test_init_with_custom_part_size() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_init_success(PathBuf::from("/test/repo"))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = InitArgs {
            path: PathBuf::from("/test/repo"),
            part_size: "1G".to_string(),
        };

        let result = InitHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("1 GB"));
    }

    #[test]
    fn test_init_invalid_part_size() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_init_success(PathBuf::from("/test/repo"))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = InitArgs {
            path: PathBuf::from("/test/repo"),
            part_size: "invalid".to_string(),
        };

        let result = InitHandler::execute(args, &ctx);

        assert!(result.is_err());
        if let Err(CliError::Publish(msg)) = result {
            assert!(msg.contains("Invalid part size"));
        }
    }

    #[test]
    fn test_init_repository_error() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_init_error("Permission denied")
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = InitArgs {
            path: PathBuf::from("/test/repo"),
            part_size: "500M".to_string(),
        };

        let result = InitHandler::execute(args, &ctx);

        assert!(result.is_err());
        if let Err(CliError::Publish(msg)) = result {
            assert!(msg.contains("Permission denied"));
        }
    }
}

// ============================================================================
// Scan Handler Tests
// ============================================================================

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn test_scan_success_with_tiles() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_scan_success(create_test_scan_result())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ScanArgs {
            source: PathBuf::from("/ortho4xp/tiles"),
            package_type: PackageTypeArg::Ortho,
        };

        let result = ScanHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Scanning Ortho4XP tiles"));
        assert!(output.contains("Tiles:  2"));
        assert!(output.contains("+37-122"));
        assert!(output.contains("+38-122"));
        assert!(output.contains("Region Suggestion"));
    }

    #[test]
    fn test_scan_empty_result() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_scan_success(SceneryScanResult::new(Vec::new()))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ScanArgs {
            source: PathBuf::from("/ortho4xp/tiles"),
            package_type: PackageTypeArg::Ortho,
        };

        let result = ScanHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Tiles:  0"));
    }

    #[test]
    fn test_scan_error() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_scan_error("Directory not found")
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ScanArgs {
            source: PathBuf::from("/nonexistent"),
            package_type: PackageTypeArg::Ortho,
        };

        let result = ScanHandler::execute(args, &ctx);

        assert!(result.is_err());
    }
}

// ============================================================================
// Add Handler Tests
// ============================================================================

#[cfg(test)]
mod add_tests {
    use super::*;

    #[test]
    fn test_add_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_scan_success(create_test_scan_result())
            .with_process_success(ProcessSummary {
                tile_count: 2,
                dsf_count: 2,
                ter_count: 2,
                mask_count: 1,
                dds_skipped: 100,
                warnings: Vec::new(),
            })
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = AddArgs {
            source: PathBuf::from("/ortho4xp/tiles"),
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            version: "1.0.0".to_string(),
            repo: PathBuf::from("/test/repo"),
        };

        let result = AddHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Processing Ortho4XP tiles output"));
        assert!(output.contains("Region: NA"));
        assert!(output.contains("Found 2 tiles"));
        assert!(output.contains("Package created successfully"));
    }

    #[test]
    fn test_add_no_tiles_found() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_scan_success(SceneryScanResult::new(Vec::new()))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = AddArgs {
            source: PathBuf::from("/ortho4xp/tiles"),
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            version: "1.0.0".to_string(),
            repo: PathBuf::from("/test/repo"),
        };

        let result = AddHandler::execute(args, &ctx);

        assert!(result.is_err());
        if let Err(CliError::Publish(msg)) = result {
            assert!(msg.contains("No valid tiles found"));
        }
    }

    #[test]
    fn test_add_invalid_version() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = AddArgs {
            source: PathBuf::from("/ortho4xp/tiles"),
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            version: "not-a-version".to_string(),
            repo: PathBuf::from("/test/repo"),
        };

        let result = AddHandler::execute(args, &ctx);

        assert!(result.is_err());
        if let Err(CliError::Publish(msg)) = result {
            assert!(msg.contains("Invalid version"));
        }
    }

    #[test]
    fn test_add_overlay_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_scan_success(create_test_scan_result())
            .with_process_success(ProcessSummary {
                tile_count: 2,
                dsf_count: 2,
                ter_count: 0,
                mask_count: 0,
                dds_skipped: 0,
                warnings: Vec::new(),
            })
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = AddArgs {
            source: PathBuf::from("/ortho4xp/overlays"),
            region: "na".to_string(),
            package_type: PackageTypeArg::Overlay,
            version: "1.0.0".to_string(),
            repo: PathBuf::from("/test/repo"),
        };

        let result = AddHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Processing Ortho4XP overlays output"));
        assert!(output.contains("Region: NA"));
        assert!(output.contains("Found 2 tiles"));
        assert!(output.contains("Package created successfully"));
    }
}

// ============================================================================
// List Handler Tests
// ============================================================================

#[cfg(test)]
mod list_tests {
    use super::*;

    #[test]
    fn test_list_with_packages() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(vec![
                ("na".to_string(), PackageType::Ortho),
                ("eur".to_string(), PackageType::Ortho),
            ])
            .with_release_status(ReleaseStatus::Ready)
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ListArgs {
            repo: PathBuf::from("/test/repo"),
            verbose: false,
        };

        let result = ListHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Packages in repository"));
        assert!(output.contains("NA"));
        assert!(output.contains("EUR"));
    }

    #[test]
    fn test_list_empty_repository() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(Vec::new())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ListArgs {
            repo: PathBuf::from("/test/repo"),
            verbose: false,
        };

        let result = ListHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("No packages in repository"));
    }

    #[test]
    fn test_list_verbose() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(vec![("na".to_string(), PackageType::Ortho)])
            .with_metadata(create_test_metadata())
            .with_release_status(ReleaseStatus::Ready)
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ListArgs {
            repo: PathBuf::from("/test/repo"),
            verbose: true,
        };

        let result = ListHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Version: 1.0.0"));
        assert!(output.contains("Parts:   2"));
    }
}

// ============================================================================
// Build Handler Tests
// ============================================================================

#[cfg(test)]
mod build_tests {
    use super::*;

    #[test]
    fn test_build_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_config(RepoConfig::default())
            .with_build_success(create_test_build_result())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = BuildArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            repo: PathBuf::from("/test/repo"),
        };

        let result = BuildHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Building archive for NA"));
        assert!(output.contains("Archive created successfully"));
        assert!(output.contains("Parts:   2"));
    }
}

// ============================================================================
// Urls Handler Tests
// ============================================================================

#[cfg(test)]
mod urls_tests {
    use super::*;

    #[test]
    fn test_urls_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_metadata(create_test_metadata())
            .with_url_config_success(UrlConfigResult {
                region: "na".to_string(),
                package_type: PackageType::Ortho,
                urls_configured: 2,
                urls_verified: 0,
                failed_urls: Vec::new(),
            })
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = UrlsArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            base_url: "https://cdn.example.com/".to_string(),
            verify: false,
            repo: PathBuf::from("/test/repo"),
        };

        let result = UrlsHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Configuring URLs for NA"));
        assert!(output.contains("URLs configured successfully"));
    }

    #[test]
    fn test_urls_no_parts() {
        let output = MockOutput::new();
        let mut metadata = create_test_metadata();
        metadata.parts = Vec::new();

        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_metadata(metadata)
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = UrlsArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            base_url: "https://cdn.example.com/".to_string(),
            verify: false,
            repo: PathBuf::from("/test/repo"),
        };

        let result = UrlsHandler::execute(args, &ctx);

        assert!(result.is_err());
        if let Err(CliError::Publish(msg)) = result {
            assert!(msg.contains("no archive parts"));
        }
    }
}

// ============================================================================
// Version Handler Tests
// ============================================================================

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn test_version_show_current() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_metadata(create_test_metadata())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = VersionArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            bump: None,
            set: None,
            repo: PathBuf::from("/test/repo"),
        };

        let result = VersionHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("NA ortho version: 1.0.0"));
    }

    #[test]
    fn test_version_bump_minor() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_metadata(create_test_metadata())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = VersionArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            bump: Some(BumpType::Minor),
            set: None,
            repo: PathBuf::from("/test/repo"),
        };

        let result = VersionHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Updated NA ortho version"));
        assert!(output.contains("1.0.0 → 1.1.0"));
    }

    #[test]
    fn test_version_set_explicit() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_metadata(create_test_metadata())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = VersionArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            bump: None,
            set: Some("2.0.0".to_string()),
            repo: PathBuf::from("/test/repo"),
        };

        let result = VersionHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("1.0.0 → 2.0.0"));
    }
}

// ============================================================================
// Release Handler Tests
// ============================================================================

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn test_release_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_release_success(ReleaseResult {
                region: "na".to_string(),
                package_type: PackageType::Ortho,
                version: Version::new(1, 0, 0),
                sequence: 1,
            })
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ReleaseArgs {
            region: "na".to_string(),
            package_type: PackageTypeArg::Ortho,
            metadata_url: "https://cdn.example.com/na/metadata.txt".to_string(),
            repo: PathBuf::from("/test/repo"),
        };

        let result = ReleaseHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Releasing NA ortho to library index"));
        assert!(output.contains("Package released successfully"));
        assert!(output.contains("Sequence: 1"));
    }
}

// ============================================================================
// Status Handler Tests
// ============================================================================

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn test_status_with_packages() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(vec![("na".to_string(), PackageType::Ortho)])
            .with_metadata(create_test_metadata())
            .with_release_status(ReleaseStatus::Ready)
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = StatusArgs {
            region: None,
            package_type: None,
            repo: PathBuf::from("/test/repo"),
        };

        let result = StatusHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Package Status"));
        assert!(output.contains("NA ortho"));
        assert!(output.contains("Ready"));
    }

    #[test]
    fn test_status_empty_repository() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(Vec::new())
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = StatusArgs {
            region: None,
            package_type: None,
            repo: PathBuf::from("/test/repo"),
        };

        let result = StatusHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("No packages in repository"));
    }
}

// ============================================================================
// Validate Handler Tests
// ============================================================================

#[cfg(test)]
mod validate_tests {
    use super::*;

    #[test]
    fn test_validate_success() {
        let output = MockOutput::new();
        let publisher = MockPublisherServiceBuilder::new()
            .with_open_success(PathBuf::from("/test/repo"))
            .with_packages(vec![("na".to_string(), PackageType::Ortho)])
            .build();
        let ctx = CommandContext::new(&output, &publisher);

        let args = ValidateArgs {
            repo: PathBuf::from("/test/repo"),
        };

        let result = ValidateHandler::execute(args, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Validating repository"));
        assert!(output.contains("Repository is valid"));
    }
}
