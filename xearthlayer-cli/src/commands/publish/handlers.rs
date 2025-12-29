//! Command handlers for publish subcommands.
//!
//! Each handler implements the `CommandHandler` trait and contains the
//! business logic for a single command. Handlers depend only on trait
//! interfaces, making them testable in isolation.

use semver::Version;

use super::args::{
    AddArgs, BuildArgs, CoverageArgs, InitArgs, ListArgs, ReleaseArgs, ScanArgs, StatusArgs,
    UrlsArgs, ValidateArgs, VersionArgs,
};
use super::output::{
    format_size_display, format_status, print_process_summary, print_region_suggestion,
    print_scan_result, print_status_short,
};
use super::traits::{CommandContext, CommandHandler};
use crate::error::CliError;
use xearthlayer::package::{PackageType, ValidationContext};
use xearthlayer::publisher::{
    parse_size, RepoConfig, VersionBump, DEFAULT_PART_SIZE, LIBRARY_FILENAME,
};

// ============================================================================
// Init Handler
// ============================================================================

/// Handler for the `publish init` command.
pub struct InitHandler;

impl CommandHandler for InitHandler {
    type Args = InitArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        // Parse part size
        let part_size_bytes = parse_size(&args.part_size).map_err(|e| {
            CliError::Publish(format!("Invalid part size '{}': {}", args.part_size, e))
        })?;

        // Validate part size range
        let config = RepoConfig::new(part_size_bytes)
            .map_err(|e| CliError::Publish(format!("Invalid part size: {}", e)))?;

        // Initialize repository
        let repo = ctx.publisher.init_repository(&args.path)?;

        // Write config if non-default
        if config.part_size != DEFAULT_PART_SIZE {
            ctx.publisher.write_config(repo.root(), &config)?;
        }

        ctx.output
            .println("Initialized XEarthLayer package repository at:");
        ctx.output.indented(&repo.root().display().to_string());
        ctx.output.newline();
        ctx.output.println("Repository structure:");
        ctx.output
            .indented("packages/  - Package working directories");
        ctx.output
            .indented("dist/      - Built archives for distribution");
        ctx.output
            .indented("staging/   - Temporary processing area");
        ctx.output.newline();
        ctx.output.println(&format!(
            "Archive part size: {}",
            format_size_display(config.part_size)
        ));
        ctx.output.newline();
        ctx.output.println("Next steps:");
        ctx.output.indented(
            "1. Run 'xearthlayer publish scan --source <ortho4xp_tiles>' to preview tiles",
        );
        ctx.output.indented("2. Run 'xearthlayer publish add --source <ortho4xp_tiles> --region <code>' to add a package");

        Ok(())
    }
}

// ============================================================================
// Scan Handler
// ============================================================================

/// Handler for the `publish scan` command.
pub struct ScanHandler;

impl CommandHandler for ScanHandler {
    type Args = ScanArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        let source_type = match package_type {
            PackageType::Ortho => "Ortho4XP tiles",
            PackageType::Overlay => "Ortho4XP overlays",
        };

        ctx.output.println(&format!(
            "Scanning {} at: {}",
            source_type,
            args.source.display()
        ));
        ctx.output.newline();

        let scan_result = match package_type {
            PackageType::Ortho => ctx.publisher.scan_scenery(&args.source)?,
            PackageType::Overlay => ctx.publisher.scan_overlay(&args.source)?,
        };

        print_scan_result(ctx.output, &scan_result);

        // Suggest region if tiles found
        if !scan_result.tiles.is_empty() {
            ctx.output.newline();
            let coords: Vec<(i32, i32)> = scan_result
                .tiles
                .iter()
                .map(|t| (t.latitude, t.longitude))
                .collect();
            let suggestion = ctx.publisher.analyze_tiles(&coords);
            print_region_suggestion(ctx.output, &suggestion);
        }

        Ok(())
    }
}

// ============================================================================
// Add Handler
// ============================================================================

/// Handler for the `publish add` command.
pub struct AddHandler;

impl CommandHandler for AddHandler {
    type Args = AddArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        // Parse version
        let version = Version::parse(&args.version)
            .map_err(|e| CliError::Publish(format!("Invalid version '{}': {}", args.version, e)))?;

        // Open repository
        let repo = ctx.publisher.open_repository(&args.repo)?;

        let source_type = match package_type {
            PackageType::Ortho => "Ortho4XP tiles",
            PackageType::Overlay => "Ortho4XP overlays",
        };

        ctx.output
            .println(&format!("Processing {} output...", source_type));
        ctx.output
            .indented(&format!("Source: {}", args.source.display()));
        ctx.output
            .indented(&format!("Region: {}", args.region.to_uppercase()));
        ctx.output.indented(&format!("Type:   {}", package_type));
        ctx.output.indented(&format!("Version: {}", version));
        ctx.output.newline();

        // Scan using appropriate processor
        ctx.output.println("Scanning...");
        let scan_result = match package_type {
            PackageType::Ortho => ctx.publisher.scan_scenery(&args.source)?,
            PackageType::Overlay => ctx.publisher.scan_overlay(&args.source)?,
        };

        if scan_result.tiles.is_empty() {
            return Err(CliError::Publish("No valid tiles found".to_string()));
        }

        ctx.output
            .println(&format!("Found {} tiles", scan_result.tiles.len()));
        ctx.output.newline();

        ctx.output.println("Processing into package...");
        let summary =
            ctx.publisher
                .process_tiles(&scan_result, &args.region, package_type, repo.as_ref())?;

        print_process_summary(ctx.output, &summary);

        // Generate initial metadata
        ctx.output.newline();
        ctx.output.println("Generating metadata...");
        ctx.publisher.generate_initial_metadata(
            repo.as_ref(),
            &args.region,
            package_type,
            version,
        )?;

        ctx.output.newline();
        ctx.output.println("Package created successfully!");
        ctx.output.indented(&format!(
            "Location: {}",
            repo.package_dir(&args.region, package_type).display()
        ));
        ctx.output.newline();
        ctx.output.println("Next steps:");
        ctx.output.indented(&format!(
            "1. Run 'xearthlayer publish build --region {} --type {}' to create archives",
            args.region,
            package_type.folder_suffix()
        ));

        Ok(())
    }
}

// ============================================================================
// List Handler
// ============================================================================

/// Handler for the `publish list` command.
pub struct ListHandler;

impl CommandHandler for ListHandler {
    type Args = ListArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let repo = ctx.publisher.open_repository(&args.repo)?;
        let packages = repo.list_packages()?;

        if packages.is_empty() {
            ctx.output.println("No packages in repository.");
            ctx.output.newline();
            ctx.output
                .println("Run 'xearthlayer publish add' to create a package.");
            return Ok(());
        }

        ctx.output.println("Packages in repository:");
        ctx.output.newline();

        for (region, package_type) in &packages {
            let status = ctx
                .publisher
                .get_release_status(repo.as_ref(), region, *package_type);

            ctx.output
                .print(&format!("  {} {} - ", region.to_uppercase(), package_type));
            print_status_short(ctx.output, &status);
            ctx.output.println("");

            if args.verbose {
                // Show more details
                let package_dir = repo.package_dir(region, *package_type);
                if let Ok(metadata) = ctx.publisher.read_metadata(&package_dir) {
                    ctx.output
                        .println(&format!("    Version: {}", metadata.package_version));
                    ctx.output
                        .println(&format!("    Parts:   {}", metadata.parts.len()));
                    if metadata.has_all_urls() {
                        ctx.output.println("    URLs:    Configured");
                    } else if metadata.has_parts() {
                        ctx.output.println("    URLs:    Pending");
                    }
                }
                ctx.output.newline();
            }
        }

        if !args.verbose {
            ctx.output.newline();
            ctx.output.println("Use --verbose for more details.");
        }

        Ok(())
    }
}

// ============================================================================
// Build Handler
// ============================================================================

/// Handler for the `publish build` command.
pub struct BuildHandler;

impl CommandHandler for BuildHandler {
    type Args = BuildArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        let repo = ctx.publisher.open_repository(&args.repo)?;
        let config = ctx.publisher.read_config(repo.root())?;

        ctx.output.println(&format!(
            "Building archive for {} {}...",
            args.region.to_uppercase(),
            package_type
        ));
        ctx.output.indented(&format!(
            "Part size: {}",
            format_size_display(config.part_size)
        ));
        ctx.output.newline();

        let result =
            ctx.publisher
                .build_package(repo.as_ref(), &args.region, package_type, &config)?;

        ctx.output.println("Archive created successfully!");
        ctx.output.newline();
        ctx.output
            .indented(&format!("Archive: {}", result.archive.archive_name));
        ctx.output
            .indented(&format!("Parts:   {}", result.archive.parts.len()));
        ctx.output.indented(&format!(
            "Size:    {}",
            format_size_display(result.archive.total_size)
        ));
        ctx.output.newline();
        ctx.output.println("Archive parts:");
        for part in &result.archive.parts {
            ctx.output.indented(&format!(
                "{} ({})",
                part.filename,
                format_size_display(part.size)
            ));
        }
        ctx.output.newline();
        ctx.output.println("Next steps:");
        ctx.output
            .indented("1. Upload archive parts to your hosting provider");
        ctx.output.indented(&format!(
            "2. Run 'xearthlayer publish urls --region {} --type {} --base-url <url>' to configure URLs",
            args.region,
            package_type.folder_suffix()
        ));

        Ok(())
    }
}

// ============================================================================
// Urls Handler
// ============================================================================

/// Handler for the `publish urls` command.
pub struct UrlsHandler;

impl CommandHandler for UrlsHandler {
    type Args = UrlsArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        let repo = ctx.publisher.open_repository(&args.repo)?;

        // Read metadata to get part filenames
        let package_dir = repo.package_dir(&args.region, package_type);
        let metadata = ctx.publisher.read_metadata(&package_dir)?;

        if metadata.parts.is_empty() {
            return Err(CliError::Publish(
                "Package has no archive parts. Run 'publish build' first.".to_string(),
            ));
        }

        // Generate URLs from base URL
        let suffixes: Vec<&str> = metadata
            .parts
            .iter()
            .map(|p| {
                // Extract the suffix from the filename (e.g., "aa", "ab" from archive.tar.gz.aa)
                p.filename.rsplit('.').next().unwrap_or("")
            })
            .collect();
        let urls = ctx
            .publisher
            .generate_part_urls(&args.base_url, &metadata.filename, &suffixes);

        ctx.output.println(&format!(
            "Configuring URLs for {} {}...",
            args.region.to_uppercase(),
            package_type
        ));
        ctx.output.indented(&format!("Base URL: {}", args.base_url));
        ctx.output.newline();

        ctx.output.println("Generated URLs:");
        for url in &urls {
            ctx.output.indented(url);
        }
        ctx.output.newline();

        let result = ctx.publisher.configure_urls(
            repo.as_ref(),
            &args.region,
            package_type,
            &urls,
            args.verify,
        )?;

        if args.verify {
            ctx.output.println("URL verification:");
            ctx.output
                .indented(&format!("Verified: {}", result.urls_verified));
            if !result.failed_urls.is_empty() {
                ctx.output
                    .indented(&format!("Failed: {}", result.failed_urls.len()));
                for (url, error) in &result.failed_urls {
                    ctx.output.println(&format!("    {} - {}", url, error));
                }
            }
            ctx.output.newline();
        }

        ctx.output.println("URLs configured successfully!");
        ctx.output.newline();
        ctx.output.println("Next steps:");
        ctx.output.indented("1. Upload the updated metadata file:");
        ctx.output.println(&format!(
            "     {}",
            package_dir
                .join("xearthlayer_scenery_package.txt")
                .display()
        ));
        ctx.output.indented(&format!(
            "2. Run 'xearthlayer publish release --region {} --type {} --metadata-url <url>'",
            args.region,
            package_type.folder_suffix()
        ));

        Ok(())
    }
}

// ============================================================================
// Version Handler
// ============================================================================

/// Handler for the `publish version` command.
pub struct VersionHandler;

impl CommandHandler for VersionHandler {
    type Args = VersionArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        let repo = ctx.publisher.open_repository(&args.repo)?;
        let package_dir = repo.package_dir(&args.region, package_type);
        let metadata = ctx.publisher.read_metadata(&package_dir)?;

        let old_version = metadata.package_version.clone();

        let new_metadata = if let Some(bump_type) = args.bump {
            let bump = VersionBump::from(bump_type);
            ctx.publisher.bump_package_version(&package_dir, bump)?
        } else if let Some(version_str) = args.set {
            let version = Version::parse(&version_str).map_err(|e| {
                CliError::Publish(format!("Invalid version '{}': {}", version_str, e))
            })?;
            ctx.publisher.update_version(&package_dir, version)?
        } else {
            // No bump or set specified, just show current version
            ctx.output.println(&format!(
                "{} {} version: {}",
                args.region.to_uppercase(),
                package_type,
                old_version
            ));
            return Ok(());
        };

        ctx.output.println(&format!(
            "Updated {} {} version:",
            args.region.to_uppercase(),
            package_type
        ));
        ctx.output.indented(&format!(
            "{} → {}",
            old_version, new_metadata.package_version
        ));

        Ok(())
    }
}

// ============================================================================
// Release Handler
// ============================================================================

/// Handler for the `publish release` command.
pub struct ReleaseHandler;

impl CommandHandler for ReleaseHandler {
    type Args = ReleaseArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let package_type = PackageType::from(args.package_type);

        let repo = ctx.publisher.open_repository(&args.repo)?;

        ctx.output.println(&format!(
            "Releasing {} {} to library index...",
            args.region.to_uppercase(),
            package_type
        ));
        ctx.output.newline();

        let result = ctx.publisher.release_package(
            repo.as_ref(),
            &args.region,
            package_type,
            &args.metadata_url,
        )?;

        ctx.output.println("Package released successfully!");
        ctx.output.newline();
        ctx.output
            .indented(&format!("Region:   {}", result.region.to_uppercase()));
        ctx.output
            .indented(&format!("Type:     {}", result.package_type));
        ctx.output
            .indented(&format!("Version:  {}", result.version));
        ctx.output
            .indented(&format!("Sequence: {}", result.sequence));
        ctx.output.newline();
        ctx.output.println("Library index updated:");
        ctx.output
            .indented(&repo.root().join(LIBRARY_FILENAME).display().to_string());
        ctx.output.newline();
        ctx.output.println("Next steps:");
        ctx.output
            .indented("1. Upload the library index to your CDN");
        ctx.output
            .indented("2. Verify the package is accessible via the metadata URL");

        Ok(())
    }
}

// ============================================================================
// Status Handler
// ============================================================================

/// Handler for the `publish status` command.
pub struct StatusHandler;

impl CommandHandler for StatusHandler {
    type Args = StatusArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let repo = ctx.publisher.open_repository(&args.repo)?;
        let packages = repo.list_packages()?;

        if packages.is_empty() {
            ctx.output.println("No packages in repository.");
            return Ok(());
        }

        // Filter packages
        let filtered: Vec<_> = packages
            .iter()
            .filter(|(r, t)| {
                let region_match = args
                    .region
                    .as_ref()
                    .is_none_or(|rg| r.eq_ignore_ascii_case(rg));
                let type_match = args
                    .package_type
                    .is_none_or(|pt| *t == PackageType::from(pt));
                region_match && type_match
            })
            .collect();

        if filtered.is_empty() {
            ctx.output.println("No matching packages found.");
            return Ok(());
        }

        ctx.output.println("Package Status");
        ctx.output.println("==============");
        ctx.output.newline();

        for (region, pkg_type) in filtered {
            let status = ctx
                .publisher
                .get_release_status(repo.as_ref(), region, *pkg_type);
            let package_dir = repo.package_dir(region, *pkg_type);

            ctx.output
                .println(&format!("{} {}", region.to_uppercase(), pkg_type));
            ctx.output
                .indented(&format!("Status: {}", format_status(&status)));

            if let Ok(metadata) = ctx.publisher.read_metadata(&package_dir) {
                ctx.output
                    .indented(&format!("Version: {}", metadata.package_version));
                ctx.output
                    .indented(&format!("Parts: {}", metadata.parts.len()));

                // Validation status
                let errors = metadata.validate(ValidationContext::Release);
                if errors.is_empty() {
                    ctx.output.indented("Validation: OK");
                } else {
                    ctx.output
                        .indented(&format!("Validation: {} issue(s)", errors.len()));
                    for error in errors.iter().take(3) {
                        ctx.output.println(&format!("    - {}", error));
                    }
                    if errors.len() > 3 {
                        ctx.output
                            .println(&format!("    ... and {} more", errors.len() - 3));
                    }
                }
            }
            ctx.output.newline();
        }

        Ok(())
    }
}

// ============================================================================
// Validate Handler
// ============================================================================

/// Handler for the `publish validate` command.
pub struct ValidateHandler;

impl CommandHandler for ValidateHandler {
    type Args = ValidateArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let repo = ctx.publisher.open_repository(&args.repo)?;

        ctx.output.println(&format!(
            "Validating repository at: {}",
            repo.root().display()
        ));
        ctx.output.newline();

        ctx.publisher.validate_repository(repo.as_ref())?;

        let packages = repo.list_packages()?;

        ctx.output.println(&format!("Packages: {}", packages.len()));
        ctx.output.newline();
        ctx.output.println("Repository is valid. No issues found.");

        Ok(())
    }
}

// ============================================================================
// Coverage Handler
// ============================================================================

/// Handler for the `publish coverage` command.
pub struct CoverageHandler;

impl CommandHandler for CoverageHandler {
    type Args = CoverageArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let repo = ctx.publisher.open_repository(&args.repo)?;
        let packages_dir = repo.root().join("packages");

        let format_type = if args.geojson { "GeoJSON" } else { "PNG" };
        ctx.output
            .println(&format!("Generating {} coverage map...", format_type));
        ctx.output.newline();

        let result = if args.geojson {
            ctx.publisher
                .generate_coverage_geojson(&packages_dir, &args.output)?
        } else {
            ctx.publisher.generate_coverage_map(
                &packages_dir,
                &args.output,
                args.width,
                args.height,
                args.dark,
            )?
        };

        ctx.output.println(&format!(
            "{} coverage map generated successfully!",
            format_type
        ));
        ctx.output.newline();
        ctx.output
            .indented(&format!("Output:  {}", args.output.display()));
        if !args.geojson {
            ctx.output
                .indented(&format!("Size:    {}x{}", args.width, args.height));
        }
        ctx.output
            .indented(&format!("Tiles:   {}", result.total_tiles));
        ctx.output.newline();

        ctx.output.println("Tile counts by region:");
        for (region, count) in &result.tiles_by_region {
            ctx.output
                .indented(&format!("{}: {}", region.to_uppercase(), count));
        }

        Ok(())
    }
}
