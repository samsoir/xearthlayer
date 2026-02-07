//! Command handlers for packages CLI commands.
//!
//! Each handler implements the `CommandHandler` trait and contains the
//! business logic for its respective command.

use xearthlayer::config::format_size;
use xearthlayer::manager::{
    create_consolidated_overlay, remove_overlay_symlink, MountStatus, PackageStatus,
};
use xearthlayer::package::PackageType;

use super::args::{CheckArgs, InfoArgs, InstallArgs, ListArgs, RemoveArgs, UpdateArgs};
use super::traits::{CommandContext, CommandHandler};
use crate::error::CliError;

// ============================================================================
// List Handler
// ============================================================================

/// Handler for the `packages list` command.
pub struct ListHandler;

impl CommandHandler for ListHandler {
    type Args = ListArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let store = ctx.manager.create_store(&args.install_dir);
        let packages = ctx
            .manager
            .list_packages(&store)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        if packages.is_empty() {
            ctx.output.println("No packages installed.");
            ctx.output.newline();
            ctx.output
                .println("Use 'xearthlayer packages install' to install packages.");
            return Ok(());
        }

        ctx.output
            .header(&format!("Installed Packages ({})", packages.len()));
        ctx.output.newline();

        for package in &packages {
            let type_str = match package.package_type() {
                PackageType::Ortho => "ortho",
                PackageType::Overlay => "overlay",
            };

            if args.verbose {
                ctx.output.println(&format!(
                    "{} ({}) v{}",
                    package.region(),
                    type_str,
                    package.version()
                ));
                ctx.output
                    .indented(&format!("Path: {}", package.path.display()));
                ctx.output.indented(&format!(
                    "Size: {}",
                    format_size(package.size_bytes as usize)
                ));

                if package.package_type() == PackageType::Ortho {
                    let mount_status = match package.mount_status {
                        MountStatus::Mounted => "Mounted",
                        MountStatus::NotMounted => "Not mounted",
                        MountStatus::Unknown => "Unknown",
                    };
                    ctx.output.indented(&format!("Mount: {}", mount_status));
                }
                ctx.output.newline();
            } else {
                ctx.output.println(&format!(
                    "  {} ({}) v{} - {}",
                    package.region(),
                    type_str,
                    package.version(),
                    format_size(package.size_bytes as usize)
                ));
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
// Check Handler
// ============================================================================

/// Handler for the `packages check` command.
pub struct CheckHandler;

impl CommandHandler for CheckHandler {
    type Args = CheckArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        ctx.output.println("Checking for package updates...");
        ctx.output.newline();

        // Fetch library
        let library = ctx
            .manager
            .fetch_library(&args.library_url)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        // Check for updates
        let store = ctx.manager.create_store(&args.install_dir);
        let statuses = ctx.manager.check_updates(&store, &library);

        let mut updates_available = 0;
        let mut not_installed = 0;

        for (info, status) in &statuses {
            match status {
                PackageStatus::UpToDate => {
                    let version = info
                        .installed_version()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    ctx.output.println(&format!(
                        "  {} ({}) v{} - Up to date",
                        info.region, info.package_type, version
                    ));
                }
                PackageStatus::UpdateAvailable {
                    installed,
                    available,
                } => {
                    ctx.output.println(&format!(
                        "  {} ({}) v{} -> v{} - Update available",
                        info.region, info.package_type, installed, available
                    ));
                    updates_available += 1;
                }
                PackageStatus::NotInstalled { available } => {
                    ctx.output.println(&format!(
                        "  {} ({}) v{} - Not installed",
                        info.region, info.package_type, available
                    ));
                    not_installed += 1;
                }
                PackageStatus::Orphaned { installed } => {
                    ctx.output.println(&format!(
                        "  {} ({}) v{} - Not in library (orphaned)",
                        info.region, info.package_type, installed
                    ));
                }
            }
        }

        ctx.output.newline();

        if updates_available > 0 {
            ctx.output.println(&format!(
                "{} update(s) available. Use 'xearthlayer packages update' to update.",
                updates_available
            ));
        } else if not_installed > 0 {
            ctx.output.println(&format!(
                "{} package(s) available for installation.",
                not_installed
            ));
        } else {
            ctx.output.println("All packages are up to date.");
        }

        Ok(())
    }
}

// ============================================================================
// Shared Helpers
// ============================================================================

/// Rebuild the consolidated overlay folder after an overlay install/remove/update.
///
/// This creates a single `yzXEL_overlay/` folder in Custom Scenery containing
/// symlinks to all DSF files from installed overlay packages. Any stale per-region
/// symlinks (e.g. `yzXEL_na_overlay`) are automatically cleaned up.
fn rebuild_consolidated_overlay(
    install_dir: &std::path::Path,
    custom_scenery_path: &Option<std::path::PathBuf>,
    ctx: &CommandContext<'_>,
) {
    let Some(ref scenery_path) = custom_scenery_path else {
        ctx.output
            .println("Note: No Custom Scenery path configured. Overlay symlink not created.");
        ctx.output
            .println("Set 'custom_scenery_path' in config to enable automatic overlay creation.");
        return;
    };

    ctx.output.println("Rebuilding consolidated overlay...");
    let store = ctx.manager.create_store(install_dir);
    let result = create_consolidated_overlay(&store, scenery_path);
    if result.success {
        ctx.output.success(&format!(
            "Consolidated overlay updated: {} regions, {} files",
            result.package_count, result.file_count
        ));
    } else if let Some(error) = result.error {
        ctx.output.error(&format!(
            "Warning: Failed to create consolidated overlay: {}",
            error
        ));
    }
}

// ============================================================================
// Install Handler
// ============================================================================

/// Handler for the `packages install` command.
pub struct InstallHandler;

impl InstallHandler {
    /// Install an overlay package for auto_install_overlays feature.
    fn install_overlay_for_region(
        region: &str,
        library: &xearthlayer::package::PackageLibrary,
        args: &InstallArgs,
        ctx: &CommandContext<'_>,
    ) {
        // Find overlay entry in library
        let overlay_entry = library.entries.iter().find(|e| {
            e.title.to_lowercase() == region.to_lowercase()
                && e.package_type == PackageType::Overlay
        });

        let Some(overlay_entry) = overlay_entry else {
            return; // No overlay available for this region
        };

        ctx.output.newline();
        ctx.output.println(&format!(
            "Auto-installing overlay package for {}...",
            region
        ));

        // Fetch and install overlay
        let metadata = match ctx.manager.fetch_metadata(&overlay_entry.metadata_url) {
            Ok(m) => m,
            Err(e) => {
                ctx.output
                    .error(&format!("Failed to fetch overlay metadata: {}", e));
                return;
            }
        };

        ctx.output.println(&format!(
            "Overlay package: {} v{}",
            metadata.title, metadata.package_version
        ));
        ctx.output.println(&format!(
            "Parts: {} ({})",
            metadata.parts.len(),
            metadata.filename
        ));
        ctx.output.newline();

        // Install with progress callback
        let progress_callback = ctx.output.create_progress_callback();
        let result = match ctx.manager.install_package(
            &metadata,
            &args.install_dir,
            &args.temp_dir,
            Some(progress_callback),
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.output
                    .error(&format!("Failed to install overlay package: {}", e));
                return;
            }
        };

        ctx.output.progress_done();
        ctx.output.success(&format!(
            "Installed overlay {} v{} to {}",
            result.region,
            result.version,
            result.install_path.display()
        ));

        // Rebuild consolidated overlay to include the newly installed overlay
        rebuild_consolidated_overlay(&args.install_dir, &args.custom_scenery_path, ctx);
    }
}

impl CommandHandler for InstallHandler {
    type Args = InstallArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        ctx.output.println(&format!(
            "Installing {} ({})...",
            args.region, args.package_type
        ));
        ctx.output.newline();

        // Fetch library to find the package
        ctx.output.println("Fetching library index...");
        let library = ctx
            .manager
            .fetch_library(&args.library_url)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        // Find the package in the library
        let entry = library
            .entries
            .iter()
            .find(|e| {
                e.title.to_lowercase() == args.region.to_lowercase()
                    && e.package_type == args.package_type
            })
            .ok_or_else(|| {
                CliError::Packages(format!(
                    "Package {} ({}) not found in library",
                    args.region, args.package_type
                ))
            })?;

        // Fetch package metadata
        ctx.output.println("Fetching package metadata...");
        let metadata = ctx
            .manager
            .fetch_metadata(&entry.metadata_url)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        ctx.output.println(&format!(
            "Package: {} v{}",
            metadata.title, metadata.package_version
        ));
        ctx.output.println(&format!(
            "Parts: {} ({})",
            metadata.parts.len(),
            metadata.filename
        ));
        ctx.output.newline();

        // Install the package with progress reporting
        let progress_callback = ctx.output.create_progress_callback();
        let result = ctx.manager.install_package(
            &metadata,
            &args.install_dir,
            &args.temp_dir,
            Some(progress_callback),
        )?;

        ctx.output.progress_done();
        ctx.output.success(&format!(
            "Installed {} ({}) v{} to {}",
            result.region,
            result.package_type,
            result.version,
            result.install_path.display()
        ));
        ctx.output.println(&format!(
            "Downloaded {} bytes, extracted {} files",
            format_size(result.bytes_downloaded as usize),
            result.files_extracted
        ));

        // Rebuild consolidated overlay for overlay packages
        if args.package_type == PackageType::Overlay {
            ctx.output.newline();
            rebuild_consolidated_overlay(&args.install_dir, &args.custom_scenery_path, ctx);
        }

        // Auto-install overlay when installing ortho (if enabled)
        if args.package_type == PackageType::Ortho && args.auto_install_overlays {
            Self::install_overlay_for_region(&args.region, &library, &args, ctx);
        }

        Ok(())
    }
}

// ============================================================================
// Update Handler
// ============================================================================

/// Handler for the `packages update` command.
pub struct UpdateHandler;

impl CommandHandler for UpdateHandler {
    type Args = UpdateArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        ctx.output.println("Checking for updates...");
        ctx.output.newline();

        // Fetch library
        let library = ctx
            .manager
            .fetch_library(&args.library_url)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        // Check for updates
        let store = ctx.manager.create_store(&args.install_dir);
        let statuses = ctx.manager.check_updates(&store, &library);

        // Filter to updates only, and optionally by region/type
        let updates: Vec<_> = statuses
            .into_iter()
            .filter(|(info, status)| {
                // Only updates
                if !matches!(status, PackageStatus::UpdateAvailable { .. }) {
                    return false;
                }

                // Filter by region if specified
                if let Some(ref region) = args.region {
                    if info.region.to_lowercase() != region.to_lowercase() {
                        return false;
                    }
                }

                // Filter by type if specified
                if let Some(pkg_type) = args.package_type {
                    if info.package_type != pkg_type {
                        return false;
                    }
                }

                true
            })
            .collect();

        if updates.is_empty() {
            ctx.output.println("No updates available.");
            return Ok(());
        }

        // Show what will be updated
        ctx.output
            .println(&format!("{} update(s) available:", updates.len()));
        for (info, status) in &updates {
            if let PackageStatus::UpdateAvailable {
                installed,
                available,
            } = status
            {
                ctx.output.println(&format!(
                    "  {} ({}) v{} -> v{}",
                    info.region, info.package_type, installed, available
                ));
            }
        }
        ctx.output.newline();

        // Confirm unless --all is specified
        if !args.all {
            if !ctx.interaction.confirm("Proceed with update?") {
                ctx.output.println("Update cancelled.");
                return Ok(());
            }
            ctx.output.newline();
        }

        // Perform updates
        let mut success_count = 0;
        let mut error_count = 0;
        let mut overlay_updated = false;

        for (info, _) in updates {
            ctx.output.println(&format!(
                "Updating {} ({})...",
                info.region, info.package_type
            ));

            // Find the entry in the library
            let entry = library.entries.iter().find(|e| {
                e.title.to_lowercase() == info.region.to_lowercase()
                    && e.package_type == info.package_type
            });

            let entry = match entry {
                Some(e) => e,
                None => {
                    ctx.output.error(&format!(
                        "Package {} ({}) not found in library",
                        info.region, info.package_type
                    ));
                    error_count += 1;
                    continue;
                }
            };

            // Fetch metadata and install
            let metadata = match ctx.manager.fetch_metadata(&entry.metadata_url) {
                Ok(m) => m,
                Err(e) => {
                    ctx.output
                        .error(&format!("Failed to fetch metadata: {}", e));
                    error_count += 1;
                    continue;
                }
            };

            let progress_callback = ctx.output.create_progress_callback();
            match ctx.manager.install_package(
                &metadata,
                &args.install_dir,
                &args.temp_dir,
                Some(progress_callback),
            ) {
                Ok(result) => {
                    ctx.output.progress_done();
                    ctx.output.success(&format!(
                        "Updated {} ({}) to v{}",
                        result.region, result.package_type, result.version
                    ));
                    if result.package_type == PackageType::Overlay {
                        overlay_updated = true;
                    }
                    success_count += 1;
                }
                Err(e) => {
                    ctx.output.progress_done();
                    ctx.output.error(&format!("Failed to update: {}", e));
                    error_count += 1;
                }
            }
        }

        ctx.output.newline();
        ctx.output.println(&format!(
            "Update complete: {} succeeded, {} failed",
            success_count, error_count
        ));

        // Rebuild consolidated overlay if any overlay packages were updated
        if overlay_updated {
            ctx.output.newline();
            rebuild_consolidated_overlay(&args.install_dir, &args.custom_scenery_path, ctx);
        }

        Ok(())
    }
}

// ============================================================================
// Remove Handler
// ============================================================================

/// Handler for the `packages remove` command.
pub struct RemoveHandler;

impl CommandHandler for RemoveHandler {
    type Args = RemoveArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let store = ctx.manager.create_store(&args.install_dir);

        // Check if package exists
        let package = ctx
            .manager
            .get_package(&store, &args.region, args.package_type)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        ctx.output.println(&format!(
            "Package: {} ({}) v{}",
            package.region(),
            package.package_type(),
            package.version()
        ));
        ctx.output
            .println(&format!("Path: {}", package.path.display()));
        ctx.output.println(&format!(
            "Size: {}",
            format_size(package.size_bytes as usize)
        ));
        ctx.output.newline();

        // Check mount status for ortho packages
        if package.package_type() == PackageType::Ortho
            && package.mount_status == MountStatus::Mounted
        {
            return Err(CliError::Packages(
                "Cannot remove a mounted package. Unmount it first.".to_string(),
            ));
        }

        // Confirm unless --force
        if !args.force
            && !ctx
                .interaction
                .confirm("Are you sure you want to remove this package?")
        {
            ctx.output.println("Removal cancelled.");
            return Ok(());
        }

        // Clean up stale per-region overlay symlink if one exists
        if args.package_type == PackageType::Overlay {
            if let Some(ref custom_scenery_path) = args.custom_scenery_path {
                match remove_overlay_symlink(&args.region, custom_scenery_path) {
                    Ok(true) => {
                        tracing::info!(region = %args.region, "Removed stale per-region overlay symlink");
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(region = %args.region, error = %e, "Failed to remove per-region overlay symlink");
                    }
                }
            }
        }

        // Remove the package
        ctx.output.println("Removing package...");
        ctx.manager
            .remove_package(&store, &args.region, args.package_type)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        ctx.output
            .success(&format!("Removed {} ({})", args.region, args.package_type));

        // Rebuild consolidated overlay after removing an overlay package
        if args.package_type == PackageType::Overlay {
            rebuild_consolidated_overlay(&args.install_dir, &args.custom_scenery_path, ctx);
        }

        Ok(())
    }
}

// ============================================================================
// Info Handler
// ============================================================================

/// Handler for the `packages info` command.
pub struct InfoHandler;

impl CommandHandler for InfoHandler {
    type Args = InfoArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let store = ctx.manager.create_store(&args.install_dir);

        let package = ctx
            .manager
            .get_package(&store, &args.region, args.package_type)
            .map_err(|e| CliError::Packages(e.to_string()))?;

        ctx.output.header(&format!(
            "{} ({}) v{}",
            package.region(),
            package.package_type(),
            package.version()
        ));
        ctx.output.newline();

        ctx.output.println("Package Details:");
        ctx.output
            .indented(&format!("Title: {}", package.metadata.title));
        ctx.output.indented(&format!(
            "Type: {}",
            match package.package_type() {
                PackageType::Ortho => "Orthophoto (satellite imagery)",
                PackageType::Overlay => "Overlay (roads, buildings)",
            }
        ));
        ctx.output
            .indented(&format!("Version: {}", package.version()));
        ctx.output
            .indented(&format!("Mountpoint: {}", package.metadata.mountpoint));
        ctx.output.newline();

        ctx.output.println("Installation:");
        ctx.output
            .indented(&format!("Path: {}", package.path.display()));
        ctx.output.indented(&format!(
            "Size: {}",
            format_size(package.size_bytes as usize)
        ));
        ctx.output.newline();

        if package.package_type() == PackageType::Ortho {
            ctx.output.println("Mount Status:");
            let mount_status = match package.mount_status {
                MountStatus::Mounted => "Mounted (FUSE filesystem active)",
                MountStatus::NotMounted => "Not mounted",
                MountStatus::Unknown => "Unknown",
            };
            ctx.output.indented(mount_status);
            ctx.output.newline();
        }

        ctx.output.println("Archive Parts:");
        for part in &package.metadata.parts {
            ctx.output
                .indented(&format!("{} ({})", part.filename, part.checksum));
        }

        Ok(())
    }
}
