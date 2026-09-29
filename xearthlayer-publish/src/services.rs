//! Concrete implementations of the service traits.
//!
//! These implementations wrap the actual xearthlayer publisher functions,
//! adapting them to the trait interfaces used by handlers.

use std::path::{Path, PathBuf};

use semver::Version;

use super::traits::{
    CoverageResult, DedupeReport, Output, OverlapSummary, Prompt, PublisherService,
    RepositoryOperations,
};
use crate::error::CliError;
use xearthlayer_package::{PackageMetadata, PackageType};
use xearthlayer_publisher::dedupe::{
    resolve_overlaps, DedupeFilter, GapAnalysisResult, OverlapDetector, ZoomPriority,
};
use xearthlayer_publisher::{
    coverage::{CoverageConfig, CoverageMapGenerator},
    BuildResult, DeletionPlan, ProcessSummary, PublishError, RegionMetadata, RegionSuggestion,
    ReleaseResult, ReleaseStatus, RepoConfig, SceneryScanResult, UrlConfigResult, VersionBump,
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
// Console Prompt Implementation
// ============================================================================

/// Reads a yes or no answer from stdin.
pub struct ConsolePrompt;

impl ConsolePrompt {
    /// Create a console prompt.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConsolePrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl Prompt for ConsolePrompt {
    fn confirm(&self, question: &str) -> Result<bool, CliError> {
        use std::io::{self, Write};

        print!("{} [y/N] ", question);
        io::stdout()
            .flush()
            .map_err(|e| CliError::Publish(format!("could not write the prompt: {e}")))?;

        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|e| CliError::Publish(format!("could not read the answer: {e}")))?;

        Ok(is_affirmative(&answer))
    }
}

/// True only for an explicit yes. Anything else, including an empty answer
/// from a closed stdin, is a no.
fn is_affirmative(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Turn a [`RegionMetadata::load`] failure into a `CliError`, adding the
/// `--metadata` hint only for `coverage`, which has that flag. `release` uses
/// the same loader through [`write_region_size`](xearthlayer_publisher::write_region_size)
/// but has no such flag, so its own call site must not gain this hint.
fn coverage_metadata_load_error(error: PublishError) -> CliError {
    let hint = if matches!(error, PublishError::RegionMetadataNotFound(_)) {
        " Pass --metadata to override."
    } else {
        ""
    };
    CliError::Publish(format!("{error}{hint}"))
}

#[cfg(test)]
mod coverage_metadata_load_error_tests {
    use super::coverage_metadata_load_error;
    use std::path::PathBuf;
    use xearthlayer_publisher::PublishError;

    #[test]
    fn a_missing_file_gets_the_metadata_flag_hint() {
        let err = coverage_metadata_load_error(PublishError::RegionMetadataNotFound(
            PathBuf::from("/repo/region_metadata.json"),
        ));
        assert!(err.to_string().contains("--metadata"));
    }

    #[test]
    fn a_parse_failure_gets_no_hint() {
        let err = coverage_metadata_load_error(PublishError::InvalidRegionMetadata {
            path: PathBuf::from("/repo/region_metadata.json"),
            message: "unexpected token".to_string(),
        });
        assert!(!err.to_string().contains("--metadata"));
    }
}

#[cfg(test)]
mod is_affirmative_tests {
    use super::is_affirmative;

    #[test]
    fn lowercase_y_is_affirmative() {
        assert!(is_affirmative("y"));
    }

    #[test]
    fn lowercase_yes_is_affirmative() {
        assert!(is_affirmative("yes"));
    }

    #[test]
    fn uppercase_y_is_affirmative() {
        assert!(is_affirmative("Y"));
    }

    #[test]
    fn uppercase_yes_is_affirmative() {
        assert!(is_affirmative("YES"));
    }

    #[test]
    fn a_trailing_newline_is_still_affirmative() {
        assert!(is_affirmative("y\n"));
        assert!(is_affirmative("yes\n"));
    }

    #[test]
    fn an_empty_answer_is_not_affirmative() {
        // The closed-stdin case: read_line returns Ok(0) with an empty string.
        assert!(!is_affirmative(""));
    }

    #[test]
    fn a_bare_newline_is_not_affirmative() {
        assert!(!is_affirmative("\n"));
    }

    #[test]
    fn no_is_not_affirmative() {
        assert!(!is_affirmative("n"));
        assert!(!is_affirmative("no"));
    }

    #[test]
    fn an_unrecognised_answer_is_not_affirmative() {
        assert!(!is_affirmative("maybe"));
    }
}

// ============================================================================
// Repository Wrapper
// ============================================================================

/// Wrapper around `xearthlayer_publisher::Repository` implementing the trait.
pub struct RepositoryWrapper {
    inner: xearthlayer_publisher::Repository,
}

impl RepositoryWrapper {
    /// Create a new wrapper from a repository.
    pub fn new(repo: xearthlayer_publisher::Repository) -> Self {
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
        let repo = xearthlayer_publisher::Repository::init(path)
            .map_err(|e| CliError::Publish(format!("Failed to initialize repository: {}", e)))?;
        Ok(Box::new(RepositoryWrapper::new(repo)))
    }

    fn open_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError> {
        let repo = xearthlayer_publisher::Repository::open(path)
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;
        Ok(Box::new(RepositoryWrapper::new(repo)))
    }

    fn read_config(&self, repo_root: &Path) -> Result<RepoConfig, CliError> {
        xearthlayer_publisher::read_config(repo_root)
            .map_err(|e| CliError::Publish(format!("Failed to read config: {}", e)))
    }

    fn write_config(&self, repo_root: &Path, config: &RepoConfig) -> Result<(), CliError> {
        xearthlayer_publisher::write_config(repo_root, config)
            .map_err(|e| CliError::Publish(format!("Failed to write config: {}", e)))
    }

    fn scan_scenery(&self, source: &Path) -> Result<SceneryScanResult, CliError> {
        use xearthlayer_publisher::{Ortho4XPProcessor, SceneryProcessor};
        let processor = Ortho4XPProcessor::new();
        processor
            .scan(source)
            .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))
    }

    fn scan_overlay(&self, source: &Path) -> Result<SceneryScanResult, CliError> {
        use xearthlayer_publisher::{OverlayProcessor, SceneryProcessor};
        let processor = OverlayProcessor::new();
        processor
            .scan(source)
            .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))
    }

    fn analyze_tiles(&self, coords: &[(i32, i32)]) -> RegionSuggestion {
        xearthlayer_publisher::analyze_tiles(coords)
    }

    fn process_tiles(
        &self,
        scan_result: &SceneryScanResult,
        region: &str,
        package_type: PackageType,
        repo: &dyn RepositoryOperations,
    ) -> Result<ProcessSummary, CliError> {
        use xearthlayer_publisher::{Ortho4XPProcessor, OverlayProcessor, SceneryProcessor};

        // We need to get the actual Repository from the wrapper
        // This is a limitation - we need to downcast or use a different approach
        // For now, we'll re-open the repository
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
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
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::generate_initial_metadata(
            &actual_repo,
            region,
            package_type,
            version,
        )
        .map_err(|e| CliError::Publish(format!("Failed to generate metadata: {}", e)))?;
        Ok(())
    }

    fn read_metadata(&self, package_dir: &Path) -> Result<PackageMetadata, CliError> {
        xearthlayer_publisher::read_metadata(package_dir)
            .map_err(|e| CliError::Publish(format!("Failed to read metadata: {}", e)))
    }

    fn build_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        config: &RepoConfig,
    ) -> Result<BuildResult, CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::build_package(&actual_repo, region, package_type, config)
            .map_err(|e| CliError::Publish(format!("Build failed: {}", e)))
    }

    fn generate_part_urls(
        &self,
        base_url: &str,
        archive_name: &str,
        suffixes: &[&str],
    ) -> Vec<String> {
        xearthlayer_publisher::generate_part_urls(base_url, archive_name, suffixes)
    }

    fn configure_urls(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        urls: &[String],
        verify: bool,
    ) -> Result<UrlConfigResult, CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::configure_urls(&actual_repo, region, package_type, urls, verify)
            .map_err(|e| CliError::Publish(format!("Failed to configure URLs: {}", e)))
    }

    fn bump_package_version(
        &self,
        package_dir: &Path,
        bump: VersionBump,
    ) -> Result<PackageMetadata, CliError> {
        xearthlayer_publisher::bump_package_version(package_dir, bump)
            .map_err(|e| CliError::Publish(format!("Failed to bump version: {}", e)))
    }

    fn update_version(
        &self,
        package_dir: &Path,
        version: Version,
    ) -> Result<PackageMetadata, CliError> {
        xearthlayer_publisher::update_version(package_dir, version)
            .map_err(|e| CliError::Publish(format!("Failed to set version: {}", e)))
    }

    fn release_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        metadata_url: &str,
    ) -> Result<ReleaseResult, CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::release_package(&actual_repo, region, package_type, metadata_url)
            .map_err(|e| CliError::Publish(format!("Release failed: {}", e)))
    }

    fn get_release_status(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
    ) -> ReleaseStatus {
        // We need an actual Repository here - try to open it
        match xearthlayer_publisher::Repository::open(repo.root()) {
            Ok(actual_repo) => {
                xearthlayer_publisher::get_release_status(&actual_repo, region, package_type)
            }
            Err(_) => ReleaseStatus::NotBuilt,
        }
    }

    fn validate_repository(&self, repo: &dyn RepositoryOperations) -> Result<(), CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::validate_repository(&actual_repo)
            .map_err(|e| CliError::Publish(format!("Validation failed: {}", e)))
    }

    fn generate_coverage_map(
        &self,
        packages_dir: &Path,
        output_path: &Path,
        metadata_path: &Path,
        width: u32,
        height: u32,
        dark: bool,
    ) -> Result<CoverageResult, CliError> {
        let metadata = RegionMetadata::load(metadata_path).map_err(coverage_metadata_load_error)?;

        let base = if dark {
            CoverageConfig::dark()
        } else {
            CoverageConfig::default()
        };
        let mut config = base
            .with_regions(&metadata)
            .map_err(|e| CliError::Publish(format!("{}", e)))?;
        config.width = width;
        config.height = height;

        let generator = CoverageMapGenerator::new(config);

        // Scan packages
        let tiles = generator
            .scan_packages(packages_dir)
            .map_err(|e| CliError::Publish(format!("Failed to scan packages: {}", e)))?;

        // Get counts before generating map
        let tiles_by_region = CoverageMapGenerator::count_by_region(&tiles);
        let total_tiles = tiles.len();

        // Generate the map
        generator
            .generate_map(&tiles, output_path)
            .map_err(|e| CliError::Publish(format!("Failed to generate coverage map: {}", e)))?;

        Ok(CoverageResult {
            total_tiles,
            tiles_by_region,
        })
    }

    fn generate_coverage_geojson(
        &self,
        packages_dir: &Path,
        output_path: &Path,
        metadata_path: &Path,
    ) -> Result<CoverageResult, CliError> {
        let metadata = RegionMetadata::load(metadata_path).map_err(coverage_metadata_load_error)?;
        let config = CoverageConfig::default()
            .with_regions(&metadata)
            .map_err(|e| CliError::Publish(format!("{}", e)))?;
        let generator = CoverageMapGenerator::new(config);

        // Scan packages
        let tiles = generator
            .scan_packages(packages_dir)
            .map_err(|e| CliError::Publish(format!("Failed to scan packages: {}", e)))?;

        // Get counts before generating
        let tiles_by_region = CoverageMapGenerator::count_by_region(&tiles);
        let total_tiles = tiles.len();

        // Generate the GeoJSON
        generator
            .generate_geojson(&tiles, output_path)
            .map_err(|e| CliError::Publish(format!("Failed to generate GeoJSON: {}", e)))?;

        Ok(CoverageResult {
            total_tiles,
            tiles_by_region,
        })
    }

    fn dedupe_package(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        priority: ZoomPriority,
        filter: Option<DedupeFilter>,
        dry_run: bool,
    ) -> Result<DedupeReport, CliError> {
        // Get the package directory
        let package_dir = repo.package_dir(region, package_type);
        if !package_dir.exists() {
            return Err(CliError::Publish(format!(
                "Package not found: {} {}",
                region.to_uppercase(),
                package_type
            )));
        }

        // Scan the package for tiles
        let detector = OverlapDetector::new();
        let all_tiles = detector
            .scan_package(&package_dir)
            .map_err(|e| CliError::Publish(format!("Failed to scan package: {}", e)))?;

        // Apply filter if specified
        let tiles: Vec<_> = match &filter {
            Some(f) => all_tiles.iter().filter(|t| f.matches(t)).cloned().collect(),
            None => all_tiles,
        };

        // Detect overlaps
        let overlaps = detector.detect_overlaps(&tiles);

        // Resolve overlaps based on priority
        let result = resolve_overlaps(&tiles, &overlaps, priority);

        // If not a dry run, actually remove the files
        if !dry_run {
            for tile in &result.tiles_removed {
                // Remove the .ter file
                if tile.ter_path.exists() {
                    std::fs::remove_file(&tile.ter_path).map_err(|e| {
                        CliError::Publish(format!(
                            "Failed to remove {}: {}",
                            tile.ter_path.display(),
                            e
                        ))
                    })?;
                }
            }
        }

        Ok(DedupeReport {
            tiles_analyzed: result.tiles_analyzed,
            zoom_levels_present: result.zoom_levels_present,
            overlaps_by_pair: result.overlaps_by_pair,
            tiles_removed: result.tiles_removed,
            tiles_preserved: result.tiles_preserved,
            dry_run,
        })
    }

    fn scan_overlaps(&self, source: &Path) -> Result<OverlapSummary, CliError> {
        let detector = OverlapDetector::new();

        // `source` is an Ortho4XP tiles root: one directory per tile, each
        // with its own terrain/. Aggregate across all of them rather than
        // treating the root itself as a single package (#286).
        let tiles = detector
            .scan_tiles_root(source)
            .map_err(|e| CliError::Publish(format!("Failed to scan for overlaps: {}", e)))?;

        if tiles.is_empty() {
            return Ok(OverlapSummary::default());
        }

        // Get zoom levels and counts
        let mut tiles_by_zoom = std::collections::HashMap::new();
        for tile in &tiles {
            *tiles_by_zoom.entry(tile.zoom).or_insert(0usize) += 1;
        }

        // Detect overlaps
        let overlaps = detector.detect_overlaps(&tiles);

        // Count overlaps by pair
        let mut overlaps_by_pair = std::collections::HashMap::new();
        for overlap in &overlaps {
            let key = (overlap.higher_zl.zoom, overlap.lower_zl.zoom);
            *overlaps_by_pair.entry(key).or_insert(0usize) += 1;
        }

        Ok(OverlapSummary {
            tiles_scanned: tiles.len(),
            tiles_by_zoom,
            overlaps_by_pair,
            total_overlaps: overlaps.len(),
        })
    }

    fn analyze_gaps(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
        filter: Option<DedupeFilter>,
    ) -> Result<GapAnalysisResult, CliError> {
        let package_dir = repo.package_dir(region, package_type);

        // Create detector with optional filter
        let detector = match filter {
            Some(f) => OverlapDetector::with_filter(f),
            None => OverlapDetector::new(),
        };

        // Scan package for tiles
        let tiles = detector
            .scan_package(&package_dir)
            .map_err(|e| CliError::Publish(format!("Failed to scan package: {}", e)))?;

        // Analyze gaps
        let result = detector.analyze_gaps(&tiles);

        Ok(result)
    }

    fn plan_deletion(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        package_type: PackageType,
    ) -> Result<Option<DeletionPlan>, CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        match xearthlayer_publisher::plan_deletion(&actual_repo, region, package_type) {
            Ok(plan) => Ok(Some(plan)),
            // Nothing of this package type to delete is an answer, not a
            // failure: a region legitimately has only one of ortho and
            // overlay. Every other error is real and must reach the caller.
            Err(PublishError::PackageNotFound { .. }) => Ok(None),
            Err(e) => Err(CliError::Publish(format!("Cannot delete: {}", e))),
        }
    }

    fn execute_deletion(
        &self,
        repo: &dyn RepositoryOperations,
        plan: &DeletionPlan,
    ) -> Result<(), CliError> {
        let actual_repo = xearthlayer_publisher::Repository::open(repo.root())
            .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

        xearthlayer_publisher::execute_deletion(&actual_repo, plan)
            .map_err(|e| CliError::Publish(format!("Deletion failed: {}", e)))
    }
}
