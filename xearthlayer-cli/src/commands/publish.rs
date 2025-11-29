//! Publisher CLI commands for creating and managing scenery packages.
//!
//! Provides commands for the package publishing workflow:
//! - `init`: Initialize a new repository
//! - `scan`: Scan Ortho4XP output and report tile info
//! - `add`: Process tiles into a package
//! - `list`: List packages in repository
//! - `build`: Create distributable archives
//! - `urls`: Configure download URLs
//! - `version`: Manage package versions
//! - `release`: Publish to library index
//! - `status`: Show package release status

use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};
use semver::Version;

use crate::error::CliError;
use xearthlayer::config::format_size;
use xearthlayer::package::{PackageType, ValidationContext};
use xearthlayer::publisher::{
    build_package, configure_urls, get_release_status, parse_size, release_package,
    validate_repository, Ortho4XPProcessor, ProcessSummary, ReleaseStatus, RepoConfig, Repository,
    SceneryProcessor, SceneryScanResult, VersionBump, LIBRARY_FILENAME,
};

/// Package type argument for CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PackageTypeArg {
    /// Orthophoto package (satellite imagery)
    Ortho,
    /// Overlay package (roads, buildings, etc.)
    Overlay,
}

impl From<PackageTypeArg> for PackageType {
    fn from(arg: PackageTypeArg) -> Self {
        match arg {
            PackageTypeArg::Ortho => PackageType::Ortho,
            PackageTypeArg::Overlay => PackageType::Overlay,
        }
    }
}

/// Version bump type argument for CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BumpType {
    /// Increment major version (x.0.0)
    Major,
    /// Increment minor version (0.x.0)
    Minor,
    /// Increment patch version (0.0.x)
    Patch,
}

impl From<BumpType> for VersionBump {
    fn from(bump: BumpType) -> Self {
        match bump {
            BumpType::Major => VersionBump::Major,
            BumpType::Minor => VersionBump::Minor,
            BumpType::Patch => VersionBump::Patch,
        }
    }
}

/// Publisher subcommands.
#[derive(Subcommand)]
pub enum PublishCommands {
    /// Initialize a new package repository
    Init {
        /// Path to create repository (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Archive part size (e.g., "500MB", "1GB")
        #[arg(long, default_value = "500M")]
        part_size: String,
    },

    /// Scan Ortho4XP output and report tile information
    Scan {
        /// Path to Ortho4XP Tiles directory
        #[arg(long)]
        source: PathBuf,
    },

    /// Process Ortho4XP tiles into a package
    Add {
        /// Path to Ortho4XP Tiles directory
        #[arg(long)]
        source: PathBuf,

        /// Region code (e.g., "na", "eur", "asia")
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Initial version (default: 1.0.0)
        #[arg(long, default_value = "1.0.0")]
        version: String,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// List packages in the repository
    List {
        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,

        /// Show detailed information
        #[arg(long, short)]
        verbose: bool,
    },

    /// Build distributable archives for a package
    Build {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Configure download URLs for a package
    Urls {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Base URL (part filenames will be appended)
        #[arg(long)]
        base_url: String,

        /// Verify URLs are accessible (HEAD request)
        #[arg(long)]
        verify: bool,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Manage package version
    Version {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Bump version (major, minor, or patch)
        #[arg(long, value_enum, conflicts_with = "set")]
        bump: Option<BumpType>,

        /// Set specific version (e.g., "2.0.0")
        #[arg(long, conflicts_with = "bump")]
        set: Option<String>,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Release package to the library index
    Release {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// URL where the metadata file will be hosted
        #[arg(long)]
        metadata_url: String,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Show package release status
    Status {
        /// Region code (optional, shows all if not specified)
        #[arg(long)]
        region: Option<String>,

        /// Package type (optional)
        #[arg(long, value_enum)]
        r#type: Option<PackageTypeArg>,

        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },

    /// Validate repository integrity
    Validate {
        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },
}

/// Run a publish subcommand.
pub fn run(command: PublishCommands) -> Result<(), CliError> {
    match command {
        PublishCommands::Init { path, part_size } => run_init(path, part_size),
        PublishCommands::Scan { source } => run_scan(source),
        PublishCommands::Add {
            source,
            region,
            r#type,
            version,
            repo,
        } => run_add(source, region, r#type, version, repo),
        PublishCommands::List { repo, verbose } => run_list(repo, verbose),
        PublishCommands::Build {
            region,
            r#type,
            repo,
        } => run_build(region, r#type, repo),
        PublishCommands::Urls {
            region,
            r#type,
            base_url,
            verify,
            repo,
        } => run_urls(region, r#type, base_url, verify, repo),
        PublishCommands::Version {
            region,
            r#type,
            bump,
            set,
            repo,
        } => run_version(region, r#type, bump, set, repo),
        PublishCommands::Release {
            region,
            r#type,
            metadata_url,
            repo,
        } => run_release(region, r#type, metadata_url, repo),
        PublishCommands::Status {
            region,
            r#type,
            repo,
        } => run_status(region, r#type, repo),
        PublishCommands::Validate { repo } => run_validate(repo),
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

fn run_init(path: PathBuf, part_size: String) -> Result<(), CliError> {
    // Parse part size
    let part_size_bytes = parse_size(&part_size)
        .map_err(|e| CliError::Publish(format!("Invalid part size '{}': {}", part_size, e)))?;

    // Validate part size range
    let config = RepoConfig::new(part_size_bytes)
        .map_err(|e| CliError::Publish(format!("Invalid part size: {}", e)))?;

    // Initialize repository
    let repo = Repository::init(&path)
        .map_err(|e| CliError::Publish(format!("Failed to initialize repository: {}", e)))?;

    // Write config if non-default
    if config.part_size != xearthlayer::publisher::DEFAULT_PART_SIZE {
        xearthlayer::publisher::write_config(repo.root(), &config)
            .map_err(|e| CliError::Publish(format!("Failed to write config: {}", e)))?;
    }

    println!("Initialized XEarthLayer package repository at:");
    println!("  {}", repo.root().display());
    println!();
    println!("Repository structure:");
    println!("  packages/  - Package working directories");
    println!("  dist/      - Built archives for distribution");
    println!("  staging/   - Temporary processing area");
    println!();
    println!(
        "Archive part size: {}",
        format_size(config.part_size as usize)
    );
    println!();
    println!("Next steps:");
    println!("  1. Run 'xearthlayer publish scan --source <ortho4xp_tiles>' to preview tiles");
    println!(
        "  2. Run 'xearthlayer publish add --source <ortho4xp_tiles> --region <code>' to add a package"
    );

    Ok(())
}

fn run_scan(source: PathBuf) -> Result<(), CliError> {
    println!("Scanning Ortho4XP output at: {}", source.display());
    println!();

    let processor = Ortho4XPProcessor::new();
    let scan_result = processor
        .scan(&source)
        .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))?;

    print_scan_result(&scan_result);

    // Suggest region if tiles found
    if !scan_result.tiles.is_empty() {
        println!();
        let coords: Vec<(i32, i32)> = scan_result
            .tiles
            .iter()
            .map(|t| (t.latitude, t.longitude))
            .collect();
        let suggestion = xearthlayer::publisher::analyze_tiles(&coords);
        print_region_suggestion(&suggestion);
    }

    Ok(())
}

fn run_add(
    source: PathBuf,
    region: String,
    package_type: PackageTypeArg,
    version: String,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let package_type = PackageType::from(package_type);

    // Parse version
    let version = Version::parse(&version)
        .map_err(|e| CliError::Publish(format!("Invalid version '{}': {}", version, e)))?;

    // Open repository
    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    println!("Processing Ortho4XP output...");
    println!("  Source: {}", source.display());
    println!("  Region: {}", region.to_uppercase());
    println!("  Type:   {}", package_type);
    println!("  Version: {}", version);
    println!();

    // Scan and process
    let processor = Ortho4XPProcessor::new();

    println!("Scanning tiles...");
    let scan_result = processor
        .scan(&source)
        .map_err(|e| CliError::Publish(format!("Scan failed: {}", e)))?;

    if scan_result.tiles.is_empty() {
        return Err(CliError::Publish("No valid tiles found".to_string()));
    }

    println!("Found {} tiles", scan_result.tiles.len());
    println!();

    println!("Processing into package...");
    let summary = processor
        .process(&scan_result, &region, package_type, &repo)
        .map_err(|e| CliError::Publish(format!("Processing failed: {}", e)))?;

    print_process_summary(&summary);

    // Generate initial metadata
    println!();
    println!("Generating metadata...");
    xearthlayer::publisher::generate_initial_metadata(
        &repo,
        &region,
        package_type,
        version.clone(),
    )
    .map_err(|e| CliError::Publish(format!("Failed to generate metadata: {}", e)))?;

    println!();
    println!("Package created successfully!");
    println!(
        "  Location: {}",
        repo.package_dir(&region, package_type).display()
    );
    println!();
    println!("Next steps:");
    println!(
        "  1. Run 'xearthlayer publish build --region {} --type {}' to create archives",
        region,
        package_type.folder_suffix()
    );

    Ok(())
}

fn run_list(repo_path: PathBuf, verbose: bool) -> Result<(), CliError> {
    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    let packages = repo
        .list_packages()
        .map_err(|e| CliError::Publish(format!("Failed to list packages: {}", e)))?;

    if packages.is_empty() {
        println!("No packages in repository.");
        println!();
        println!("Run 'xearthlayer publish add' to create a package.");
        return Ok(());
    }

    println!("Packages in repository:");
    println!();

    for (region, package_type) in &packages {
        let status = get_release_status(&repo, region, *package_type);

        print!("  {} {} - ", region.to_uppercase(), package_type);
        print_status_short(&status);
        println!();

        if verbose {
            // Show more details
            let package_dir = repo.package_dir(region, *package_type);
            if let Ok(metadata) = xearthlayer::publisher::read_metadata(&package_dir) {
                println!("    Version: {}", metadata.package_version);
                println!("    Parts:   {}", metadata.parts.len());
                if metadata.has_all_urls() {
                    println!("    URLs:    Configured");
                } else if metadata.has_parts() {
                    println!("    URLs:    Pending");
                }
            }
            println!();
        }
    }

    if !verbose {
        println!();
        println!("Use --verbose for more details.");
    }

    Ok(())
}

fn run_build(
    region: String,
    package_type: PackageTypeArg,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let package_type = PackageType::from(package_type);

    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    let config = xearthlayer::publisher::read_config(repo.root())
        .map_err(|e| CliError::Publish(format!("Failed to read config: {}", e)))?;

    println!(
        "Building archive for {} {}...",
        region.to_uppercase(),
        package_type
    );
    println!("  Part size: {}", format_size(config.part_size as usize));
    println!();

    let result = build_package(&repo, &region, package_type, &config)
        .map_err(|e| CliError::Publish(format!("Build failed: {}", e)))?;

    println!("Archive created successfully!");
    println!();
    println!("  Archive: {}", result.archive.archive_name);
    println!("  Parts:   {}", result.archive.parts.len());
    println!(
        "  Size:    {}",
        format_size(result.archive.total_size as usize)
    );
    println!();
    println!("Archive parts:");
    for part in &result.archive.parts {
        println!("  {} ({})", part.filename, format_size(part.size as usize));
    }
    println!();
    println!("Next steps:");
    println!("  1. Upload archive parts to your hosting provider");
    println!(
        "  2. Run 'xearthlayer publish urls --region {} --type {} --base-url <url>' to configure URLs",
        region,
        package_type.folder_suffix()
    );

    Ok(())
}

fn run_urls(
    region: String,
    package_type: PackageTypeArg,
    base_url: String,
    verify: bool,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let package_type = PackageType::from(package_type);

    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    // Read metadata to get part filenames
    let package_dir = repo.package_dir(&region, package_type);
    let metadata = xearthlayer::publisher::read_metadata(&package_dir)
        .map_err(|e| CliError::Publish(format!("Failed to read metadata: {}", e)))?;

    if metadata.parts.is_empty() {
        return Err(CliError::Publish(
            "Package has no archive parts. Run 'publish build' first.".to_string(),
        ));
    }

    // Generate URLs from base URL
    // Get archive name and part suffixes
    let suffixes: Vec<&str> = metadata
        .parts
        .iter()
        .map(|p| {
            // Extract the suffix from the filename (e.g., "aa", "ab" from archive.tar.gz.aa)
            p.filename.rsplit('.').next().unwrap_or("")
        })
        .collect();
    let urls: Vec<String> =
        xearthlayer::publisher::generate_part_urls(&base_url, &metadata.filename, &suffixes);

    println!(
        "Configuring URLs for {} {}...",
        region.to_uppercase(),
        package_type
    );
    println!("  Base URL: {}", base_url);
    println!();

    println!("Generated URLs:");
    for url in &urls {
        println!("  {}", url);
    }
    println!();

    let result = configure_urls(&repo, &region, package_type, &urls, verify)
        .map_err(|e| CliError::Publish(format!("Failed to configure URLs: {}", e)))?;

    if verify {
        println!("URL verification:");
        println!("  Verified: {}", result.urls_verified);
        if !result.failed_urls.is_empty() {
            println!("  Failed: {}", result.failed_urls.len());
            for (url, error) in &result.failed_urls {
                println!("    {} - {}", url, error);
            }
        }
        println!();
    }

    println!("URLs configured successfully!");
    println!();
    println!("Next steps:");
    println!("  1. Upload the updated metadata file:");
    println!(
        "     {}",
        package_dir
            .join("xearthlayer_scenery_package.txt")
            .display()
    );
    println!(
        "  2. Run 'xearthlayer publish release --region {} --type {} --metadata-url <url>'",
        region,
        package_type.folder_suffix()
    );

    Ok(())
}

fn run_version(
    region: String,
    package_type: PackageTypeArg,
    bump: Option<BumpType>,
    set: Option<String>,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let package_type = PackageType::from(package_type);

    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    let package_dir = repo.package_dir(&region, package_type);
    let metadata = xearthlayer::publisher::read_metadata(&package_dir)
        .map_err(|e| CliError::Publish(format!("Failed to read metadata: {}", e)))?;

    let old_version = metadata.package_version.clone();

    let new_metadata = if let Some(bump_type) = bump {
        let bump = VersionBump::from(bump_type);
        xearthlayer::publisher::bump_package_version(&package_dir, bump)
            .map_err(|e| CliError::Publish(format!("Failed to bump version: {}", e)))?
    } else if let Some(version_str) = set {
        let version = Version::parse(&version_str)
            .map_err(|e| CliError::Publish(format!("Invalid version '{}': {}", version_str, e)))?;
        xearthlayer::publisher::update_version(&package_dir, version)
            .map_err(|e| CliError::Publish(format!("Failed to set version: {}", e)))?
    } else {
        // No bump or set specified, just show current version
        println!(
            "{} {} version: {}",
            region.to_uppercase(),
            package_type,
            old_version
        );
        return Ok(());
    };

    println!(
        "Updated {} {} version:",
        region.to_uppercase(),
        package_type
    );
    println!("  {} → {}", old_version, new_metadata.package_version);

    Ok(())
}

fn run_release(
    region: String,
    package_type: PackageTypeArg,
    metadata_url: String,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let package_type = PackageType::from(package_type);

    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    println!(
        "Releasing {} {} to library index...",
        region.to_uppercase(),
        package_type
    );
    println!();

    let result = release_package(&repo, &region, package_type, &metadata_url)
        .map_err(|e| CliError::Publish(format!("Release failed: {}", e)))?;

    println!("Package released successfully!");
    println!();
    println!("  Region:   {}", result.region.to_uppercase());
    println!("  Type:     {}", result.package_type);
    println!("  Version:  {}", result.version);
    println!("  Sequence: {}", result.sequence);
    println!();
    println!("Library index updated:");
    println!("  {}", repo.root().join(LIBRARY_FILENAME).display());
    println!();
    println!("Next steps:");
    println!("  1. Upload the library index to your CDN");
    println!("  2. Verify the package is accessible via the metadata URL");

    Ok(())
}

fn run_status(
    region: Option<String>,
    package_type: Option<PackageTypeArg>,
    repo_path: PathBuf,
) -> Result<(), CliError> {
    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    let packages = repo
        .list_packages()
        .map_err(|e| CliError::Publish(format!("Failed to list packages: {}", e)))?;

    if packages.is_empty() {
        println!("No packages in repository.");
        return Ok(());
    }

    // Filter packages
    let filtered: Vec<_> = packages
        .iter()
        .filter(|(r, t)| {
            let region_match = region.as_ref().is_none_or(|rg| r.eq_ignore_ascii_case(rg));
            let type_match = package_type.is_none_or(|pt| *t == PackageType::from(pt));
            region_match && type_match
        })
        .collect();

    if filtered.is_empty() {
        println!("No matching packages found.");
        return Ok(());
    }

    println!("Package Status");
    println!("==============");
    println!();

    for (region, pkg_type) in filtered {
        let status = get_release_status(&repo, region, *pkg_type);
        let package_dir = repo.package_dir(region, *pkg_type);

        println!("{} {}", region.to_uppercase(), pkg_type);
        println!("  Status: {}", format_status(&status));

        if let Ok(metadata) = xearthlayer::publisher::read_metadata(&package_dir) {
            println!("  Version: {}", metadata.package_version);
            println!("  Parts: {}", metadata.parts.len());

            // Validation status
            let errors = metadata.validate(ValidationContext::Release);
            if errors.is_empty() {
                println!("  Validation: OK");
            } else {
                println!("  Validation: {} issue(s)", errors.len());
                for error in errors.iter().take(3) {
                    println!("    - {}", error);
                }
                if errors.len() > 3 {
                    println!("    ... and {} more", errors.len() - 3);
                }
            }
        }
        println!();
    }

    Ok(())
}

fn run_validate(repo_path: PathBuf) -> Result<(), CliError> {
    let repo = Repository::open(&repo_path)
        .map_err(|e| CliError::Publish(format!("Failed to open repository: {}", e)))?;

    println!("Validating repository at: {}", repo.root().display());
    println!();

    validate_repository(&repo)
        .map_err(|e| CliError::Publish(format!("Validation failed: {}", e)))?;

    let packages = repo
        .list_packages()
        .map_err(|e| CliError::Publish(format!("Failed to list packages: {}", e)))?;

    println!("Packages: {}", packages.len());
    println!();
    println!("Repository is valid. No issues found.");

    Ok(())
}

// ============================================================================
// Helper Functions
// ============================================================================

fn print_scan_result(scan: &SceneryScanResult) {
    println!("Scan Results");
    println!("============");
    println!();
    println!("Tiles:  {}", scan.tiles.len());

    if !scan.tiles.is_empty() {
        println!();
        println!("Tiles found:");
        for tile in &scan.tiles {
            println!(
                "  {:+03}{:+04} - {} DSF, {} TER, {} masks",
                tile.latitude,
                tile.longitude,
                tile.dsf_files.len(),
                tile.ter_files.len(),
                tile.mask_files.len()
            );
        }
    }

    if !scan.warnings.is_empty() {
        println!();
        println!("Warnings ({}):", scan.warnings.len());
        for warning in &scan.warnings {
            println!("  - {:?}", warning);
        }
    }
}

fn print_region_suggestion(suggestion: &xearthlayer::publisher::RegionSuggestion) {
    println!("Region Suggestion");
    println!("-----------------");
    if let Some(ref region) = suggestion.region {
        println!(
            "Suggested region: {} ({})",
            region.code().to_uppercase(),
            region.name()
        );
    } else if suggestion.regions_found.is_empty() {
        println!("Could not determine region from tile coordinates.");
    } else {
        println!("Tiles span multiple regions:");
        for region in &suggestion.regions_found {
            println!("  {} ({})", region.code().to_uppercase(), region.name());
        }
        println!();
        println!("Consider processing tiles separately by region.");
    }
}

fn print_process_summary(summary: &ProcessSummary) {
    println!("Processing Summary");
    println!("------------------");
    println!("Tiles processed: {}", summary.tile_count);
    println!("DSF files:       {}", summary.dsf_count);
    println!("TER files:       {}", summary.ter_count);
    println!("Mask files:      {}", summary.mask_count);
    println!("DDS skipped:     {}", summary.dds_skipped);
}

fn print_status_short(status: &ReleaseStatus) {
    match status {
        ReleaseStatus::NotBuilt => print!("Not Built"),
        ReleaseStatus::AwaitingUrls { part_count, .. } => {
            print!("Awaiting URLs ({} parts)", part_count)
        }
        ReleaseStatus::Ready => print!("Ready"),
        ReleaseStatus::Released => print!("Released"),
    }
}

fn format_status(status: &ReleaseStatus) -> String {
    match status {
        ReleaseStatus::NotBuilt => "Not Built - run 'publish build'".to_string(),
        ReleaseStatus::AwaitingUrls {
            archive_name,
            part_count,
        } => format!(
            "Awaiting URLs - {} ({} parts) - run 'publish urls'",
            archive_name, part_count
        ),
        ReleaseStatus::Ready => "Ready - run 'publish release'".to_string(),
        ReleaseStatus::Released => "Released".to_string(),
    }
}
