//! Core traits for the command handler pattern.
//!
//! This module defines the interfaces that handlers depend on, enabling
//! dependency injection and testability. Handlers never depend on concrete
//! implementations directly.

use std::collections::HashMap;
use std::path::Path;

use semver::Version;

use crate::error::CliError;
use xearthlayer::package::{PackageMetadata, PackageType};
use xearthlayer::publisher::{
    BuildResult, ProcessSummary, RegionSuggestion, ReleaseResult, ReleaseStatus, RepoConfig,
    SceneryScanResult, UrlConfigResult, VersionBump,
};

/// Result of coverage map generation.
#[derive(Debug)]
pub struct CoverageResult {
    /// Total number of tiles included in the map.
    pub total_tiles: usize,
    /// Count of tiles by region.
    pub tiles_by_region: HashMap<String, usize>,
}

// ============================================================================
// Output Trait - Abstracts console/UI output
// ============================================================================

/// Trait for outputting messages to the user.
///
/// This abstraction allows handlers to produce output without depending on
/// `println!` directly, making them testable.
pub trait Output: Send + Sync {
    /// Print a line of text.
    fn println(&self, message: &str);

    /// Print text without a newline.
    fn print(&self, message: &str);

    /// Print an empty line.
    fn newline(&self) {
        self.println("");
    }

    /// Print a section header.
    fn header(&self, title: &str) {
        self.println(title);
        self.println(&"=".repeat(title.len()));
    }

    /// Print a sub-section header.
    fn subheader(&self, title: &str) {
        self.println(title);
        self.println(&"-".repeat(title.len()));
    }

    /// Print an indented line.
    fn indented(&self, message: &str) {
        self.println(&format!("  {}", message));
    }
}

// ============================================================================
// Repository Operations Trait - Abstracts repository access
// ============================================================================

/// Trait for repository operations.
///
/// Abstracts the `Repository` struct to allow mocking in tests.
pub trait RepositoryOperations: Send + Sync {
    /// Get the root path of the repository.
    fn root(&self) -> &Path;

    /// Get the package directory for a region and type.
    fn package_dir(&self, region: &str, package_type: PackageType) -> std::path::PathBuf;

    /// List all packages in the repository.
    fn list_packages(&self) -> Result<Vec<(String, PackageType)>, CliError>;
}

// ============================================================================
// Publisher Service Trait - Abstracts publisher operations
// ============================================================================

/// Trait for publisher service operations.
///
/// Abstracts the various publisher functions to allow mocking in tests.
/// Each method corresponds to a publisher module function.
pub trait PublisherService: Send + Sync {
    /// Initialize a new repository at the given path.
    fn init_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError>;

    /// Open an existing repository.
    fn open_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError>;

    /// Read repository configuration.
    fn read_config(&self, repo_root: &Path) -> Result<RepoConfig, CliError>;

    /// Write repository configuration.
    fn write_config(&self, repo_root: &Path, config: &RepoConfig) -> Result<(), CliError>;

    /// Scan scenery source for tiles (ortho tiles).
    fn scan_scenery(&self, source: &Path) -> Result<SceneryScanResult, CliError>;

    /// Scan scenery source for overlays.
    fn scan_overlay(&self, source: &Path) -> Result<SceneryScanResult, CliError>;

    /// Analyze tiles and suggest a region.
    fn analyze_tiles(&self, coords: &[(i32, i32)]) -> RegionSuggestion;

    /// Process scanned tiles into a package.
    fn process_tiles(
        &self,
        scan_result: &SceneryScanResult,
        region: &str,
        package_type: PackageType,
        repo: &dyn RepositoryOperations,
    ) -> Result<ProcessSummary, CliError>;

    /// Generate initial metadata for a new package.
    fn generate_initial_metadata(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        version: Version,
    ) -> Result<(), CliError>;

    /// Read package metadata.
    fn read_metadata(&self, package_dir: &Path) -> Result<PackageMetadata, CliError>;

    /// Build package archives.
    fn build_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        config: &RepoConfig,
    ) -> Result<BuildResult, CliError>;

    /// Generate part URLs from a base URL.
    fn generate_part_urls(
        &self,
        base_url: &str,
        archive_name: &str,
        suffixes: &[&str],
    ) -> Vec<String>;

    /// Configure download URLs for a package.
    fn configure_urls(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        urls: &[String],
        verify: bool,
    ) -> Result<UrlConfigResult, CliError>;

    /// Bump package version.
    fn bump_package_version(
        &self,
        package_dir: &Path,
        bump: VersionBump,
    ) -> Result<PackageMetadata, CliError>;

    /// Set package version.
    fn update_version(
        &self,
        package_dir: &Path,
        version: Version,
    ) -> Result<PackageMetadata, CliError>;

    /// Release package to the library index.
    fn release_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        metadata_url: &str,
    ) -> Result<ReleaseResult, CliError>;

    /// Get the release status of a package.
    fn get_release_status(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
    ) -> ReleaseStatus;

    /// Validate repository integrity.
    fn validate_repository(&self, repo: &dyn RepositoryOperations) -> Result<(), CliError>;

    /// Generate a coverage map image (PNG).
    fn generate_coverage_map(
        &self,
        packages_dir: &Path,
        output_path: &Path,
        width: u32,
        height: u32,
        dark: bool,
    ) -> Result<CoverageResult, CliError>;

    /// Generate a coverage map in GeoJSON format.
    fn generate_coverage_geojson(
        &self,
        packages_dir: &Path,
        output_path: &Path,
    ) -> Result<CoverageResult, CliError>;
}

// ============================================================================
// Command Context - Bundles dependencies for handlers
// ============================================================================

/// Context providing dependencies to command handlers.
///
/// This struct bundles all the interfaces that handlers need, allowing
/// dependency injection. In production, this uses real implementations;
/// in tests, it can use mocks.
pub struct CommandContext<'a> {
    /// Output interface for user messages.
    pub output: &'a dyn Output,

    /// Publisher service for repository operations.
    pub publisher: &'a dyn PublisherService,
}

impl<'a> CommandContext<'a> {
    /// Create a new command context.
    pub fn new(output: &'a dyn Output, publisher: &'a dyn PublisherService) -> Self {
        Self { output, publisher }
    }
}

// ============================================================================
// Command Handler Trait
// ============================================================================

/// Trait for command handlers.
///
/// Each publish subcommand has a handler that implements this trait.
/// Handlers receive their arguments and a context providing dependencies.
pub trait CommandHandler {
    /// The arguments type for this handler.
    type Args;

    /// Execute the command with the given arguments and context.
    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError>;
}
