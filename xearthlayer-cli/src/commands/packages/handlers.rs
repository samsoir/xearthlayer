//! Command handlers for packages CLI commands.
//!
//! Each handler implements the `CommandHandler` trait and contains the
//! business logic for its respective command.

use std::path::Path;

use xearthlayer::config::format_size;
use xearthlayer::manager::{
    create_consolidated_overlay, create_consolidated_overlay_except, remove_overlay_symlink,
    InstallResult, InstalledPackage, LocalPackageStore, MountStatus, PackageStatus,
};
use xearthlayer::xworld_pro::{detect, overlay_plan, OverlayCoverage, OverlayPlan};
use xearthlayer_package::{LibraryEntry, PackageLibrary, PackageType};

use super::args::{CheckArgs, InfoArgs, InstallArgs, ListArgs, RemoveArgs, UpdateArgs};
use super::selector::resolve;
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

/// Installed packages of `package_type` selected by a region `selector`.
///
/// Used by every command that acts on what is installed, so a selector that
/// matches nothing lists the installed codes rather than the library's.
fn select_installed(
    ctx: &CommandContext<'_>,
    store: &LocalPackageStore,
    selector: &str,
    package_type: PackageType,
) -> Result<Vec<InstalledPackage>, CliError> {
    let installed = ctx
        .manager
        .list_packages(store)
        .map_err(|e| CliError::Packages(e.to_string()))?
        .into_iter()
        .filter(|p| p.package_type() == package_type)
        .collect();
    resolve(selector, installed, |p| p.region())
}

/// Fetch each entry's metadata, then install every package whose metadata
/// arrived as one batch, so each installs while the next downloads (#292).
/// Shared by `install` and `update`. Returns one result per entry, in order.
fn install_entries(
    entries: &[&LibraryEntry],
    install_dir: &Path,
    temp_dir: &Path,
    concurrent_downloads: usize,
    ctx: &CommandContext<'_>,
) -> Vec<Result<InstallResult, CliError>> {
    ctx.output.println("Fetching package metadata...");
    let mut batch = Vec::new();
    let fetch_failures: Vec<Option<CliError>> = entries
        .iter()
        .map(
            |entry| match ctx.manager.fetch_metadata(&entry.metadata_url) {
                Ok(metadata) => {
                    ctx.output.println(&format!(
                        "Package: {} v{}",
                        metadata.title, metadata.package_version
                    ));
                    ctx.output.println(&format!(
                        "Parts: {} ({})",
                        metadata.parts.len(),
                        metadata.filename
                    ));
                    batch.push(metadata);
                    None
                }
                Err(e) => Some(CliError::Packages(e.to_string())),
            },
        )
        .collect();
    ctx.output.newline();

    let mut installed = ctx
        .manager
        .install_packages(&batch, install_dir, temp_dir, concurrent_downloads)
        .into_iter();
    fetch_failures
        .into_iter()
        .map(|failure| match failure {
            None => installed.next().expect("one result per package"),
            Some(e) => Err(e),
        })
        .collect()
}

/// Rebuild the consolidated overlay folder after an overlay install/remove/update.
///
/// This creates a single `yzXEL_overlay/` folder in Custom Scenery containing
/// symlinks to all DSF files from installed overlay packages. Any stale per-region
/// symlinks (e.g. `yzXEL_na_overlay`) are automatically cleaned up.
fn rebuild_consolidated_overlay(
    install_dir: &std::path::Path,
    custom_scenery_path: &Option<std::path::PathBuf>,
    disable_overlays: bool,
    xworld_pro: bool,
    ctx: &CommandContext<'_>,
) {
    if disable_overlays {
        // Per `packages.disable_overlays` the consolidated overlay is suppressed
        // at runtime. We deliberately leave any existing folder alone here; the
        // next `xearthlayer run` removes it during startup. Issue #152.
        ctx.output.println(
            "Note: packages.disable_overlays = true; skipping consolidated overlay rebuild. \
             The folder will be removed on next 'xearthlayer run'.",
        );
        return;
    }
    let Some(ref scenery_path) = custom_scenery_path else {
        ctx.output
            .println("Note: No Custom Scenery path configured. Overlay symlink not created.");
        ctx.output
            .println("Set 'custom_scenery_path' in config to enable automatic overlay creation.");
        return;
    };

    ctx.output.println("Rebuilding consolidated overlay...");
    let store = ctx.manager.create_store(install_dir);
    // The same choice `xearthlayer run` makes at startup.
    let coverage = xworld_pro
        .then(|| OverlayCoverage::from_detection(&detect(scenery_path)))
        .filter(|c| !c.is_empty());
    let result = match overlay_plan(disable_overlays, coverage.as_ref()) {
        OverlayPlan::Except(coverage) => {
            let result = create_consolidated_overlay_except(&store, scenery_path, &|r: &str| {
                coverage.suppresses(r)
            });
            ctx.output.println(&format!(
                "X-World Pro active ({}): left out {} overlay regions",
                coverage.describe(),
                result.excluded_regions.len()
            ));
            result
        }
        _ => create_consolidated_overlay(&store, scenery_path),
    };
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
    /// The packages to install: each selected entry, followed by its
    /// region's overlay when auto-install is on and the library has one.
    fn plan<'a>(
        entries: &[&'a LibraryEntry],
        library: &'a PackageLibrary,
        auto_install_overlays: bool,
    ) -> Vec<&'a LibraryEntry> {
        entries
            .iter()
            .flat_map(|&entry| {
                let overlay = (auto_install_overlays && entry.package_type == PackageType::Ortho)
                    .then(|| library.find(&entry.title, PackageType::Overlay))
                    .flatten();
                std::iter::once(entry).chain(overlay)
            })
            .collect()
    }

    /// Report each package's outcome, rebuild the consolidated overlay once
    /// if any overlay installed, and fail the command if any package failed.
    ///
    /// Packages succeed or fail independently: an overlay still installs
    /// when its ortho package fails.
    fn report(
        plan: &[&LibraryEntry],
        results: Vec<Result<InstallResult, CliError>>,
        args: &InstallArgs,
        ctx: &CommandContext<'_>,
    ) -> Result<(), CliError> {
        let mut failures = Vec::new();
        let mut overlay_installed = false;
        for (entry, result) in plan.iter().zip(results) {
            match result {
                Ok(result) => {
                    ctx.output.success(&format!(
                        "Installed {} ({}) v{} to {}",
                        result.region,
                        result.package_type,
                        result.version,
                        result.install_path.display()
                    ));
                    ctx.output.println(&format!(
                        "Downloaded {}, extracted {} files",
                        format_size(result.bytes_downloaded as usize),
                        result.files_extracted
                    ));
                    overlay_installed |= result.package_type == PackageType::Overlay;
                }
                Err(e) => failures.push((entry, e)),
            }
        }

        if overlay_installed {
            ctx.output.newline();
            rebuild_consolidated_overlay(
                &args.install_dir,
                &args.custom_scenery_path,
                args.disable_overlays,
                args.xworld_pro,
                ctx,
            );
        }

        match failures.len() {
            0 => Ok(()),
            _ if plan.len() == 1 => Err(failures.remove(0).1),
            failed => {
                for (entry, e) in &failures {
                    ctx.output
                        .error(&format!("{} ({}): {}", entry.title, entry.package_type, e));
                }
                Err(CliError::Packages(format!(
                    "{} of {} packages failed to install",
                    failed,
                    plan.len()
                )))
            }
        }
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

        let offered: Vec<&LibraryEntry> = library
            .entries
            .iter()
            .filter(|e| e.package_type == args.package_type)
            .collect();
        let entries = resolve(&args.region, offered, |e| e.title.as_str())?;

        if entries.len() > 1 {
            ctx.output.println(&format!(
                "{} matches {} packages ({}):",
                args.region,
                entries.len(),
                args.package_type
            ));
            for entry in &entries {
                ctx.output
                    .indented(&format!("{} v{}", entry.title, entry.version));
            }
            ctx.output.newline();
            if !ctx.interaction.confirm("Proceed with install?") {
                ctx.output.println("Install cancelled.");
                return Ok(());
            }
        }

        let plan = Self::plan(&entries, &library, args.auto_install_overlays);
        let results = install_entries(
            &plan,
            &args.install_dir,
            &args.temp_dir,
            args.concurrent_downloads,
            ctx,
        );
        Self::report(&plan, results, &args, ctx)
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

        // Installed packages of the requested type, narrowed by the region
        // selector when one is given, then only those with an update.
        let installed: Vec<_> = statuses
            .into_iter()
            .filter(|(info, status)| {
                status.is_installed() && args.package_type.is_none_or(|t| t == info.package_type)
            })
            .collect();
        let selected = match args.region {
            Some(ref selector) => resolve(selector, installed, |(info, _)| info.region.as_str())?,
            None => installed,
        };
        let updates: Vec<_> = selected
            .into_iter()
            .filter(|(_, status)| status.has_update())
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

        let mut success_count = 0;
        let mut error_count = 0;
        let mut overlay_updated = false;

        let mut entries = Vec::new();
        for (info, _) in &updates {
            match library.find(&info.region, info.package_type) {
                Some(entry) => entries.push(entry),
                None => {
                    ctx.output.error(&format!(
                        "Package {} ({}) not found in library",
                        info.region, info.package_type
                    ));
                    error_count += 1;
                }
            }
        }

        let results = install_entries(
            &entries,
            &args.install_dir,
            &args.temp_dir,
            args.concurrent_downloads,
            ctx,
        );
        for (entry, result) in entries.iter().zip(results) {
            match result {
                Ok(result) => {
                    ctx.output.success(&format!(
                        "Updated {} ({}) to v{}",
                        result.region, result.package_type, result.version
                    ));
                    overlay_updated |= result.package_type == PackageType::Overlay;
                    success_count += 1;
                }
                Err(e) => {
                    ctx.output.error(&format!(
                        "Failed to update {} ({}): {}",
                        entry.title, entry.package_type, e
                    ));
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
            rebuild_consolidated_overlay(
                &args.install_dir,
                &args.custom_scenery_path,
                args.disable_overlays,
                args.xworld_pro,
                ctx,
            );
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
        let packages = select_installed(ctx, &store, &args.region, args.package_type)?;

        // Phase one: show the whole plan and validate all of it, so a batch
        // never deletes some packages and then fails on a later one.
        for package in &packages {
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
        }

        if let Some(mounted) = packages.iter().find(|p| {
            p.package_type() == PackageType::Ortho && p.mount_status == MountStatus::Mounted
        }) {
            return Err(CliError::Packages(format!(
                "Cannot remove a mounted package ({}). Unmount it first.",
                mounted.region()
            )));
        }

        // Phase two: one confirmation for the whole set, unless --force.
        let prompt = match packages.len() {
            1 => "Are you sure you want to remove this package?".to_string(),
            n => format!("Are you sure you want to remove these {n} packages?"),
        };
        if !args.force && !ctx.interaction.confirm(&prompt) {
            ctx.output.println("Removal cancelled.");
            return Ok(());
        }

        for package in &packages {
            let region = package.region();

            ctx.output.println("Removing package...");
            ctx.manager
                .remove_package(&store, region, args.package_type, &args.temp_dir)
                .map_err(|e| CliError::Packages(e.to_string()))?;

            // Clean up stale per-region overlay symlink if one exists. Only now:
            // a refused remove (e.g. the queue lock is held) leaves the package
            // installed, and it must keep its symlink.
            if args.package_type == PackageType::Overlay {
                if let Some(ref custom_scenery_path) = args.custom_scenery_path {
                    match remove_overlay_symlink(region, custom_scenery_path) {
                        Ok(true) => {
                            tracing::info!(region = %region, "Removed stale per-region overlay symlink");
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(region = %region, error = %e, "Failed to remove per-region overlay symlink");
                        }
                    }
                }
            }

            ctx.output
                .success(&format!("Removed {} ({})", region, args.package_type));
        }

        // Rebuild consolidated overlay after removing overlay packages
        if args.package_type == PackageType::Overlay {
            rebuild_consolidated_overlay(
                &args.install_dir,
                &args.custom_scenery_path,
                args.disable_overlays,
                args.xworld_pro,
                ctx,
            );
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
        let packages = select_installed(ctx, &store, &args.region, args.package_type)?;

        for (i, package) in packages.iter().enumerate() {
            if i > 0 {
                ctx.output.newline();
            }
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
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        install_dir, FakeManager, RecordingOutput, ScriptedInteraction,
    };
    use super::*;

    fn packages_error(result: Result<(), CliError>) -> String {
        match result {
            Err(CliError::Packages(message)) => message,
            other => panic!("expected a packages error, got {other:?}"),
        }
    }

    /// What a handler returned, and what the user was asked and shown.
    struct Run {
        result: Result<(), CliError>,
        interaction: ScriptedInteraction,
        output: RecordingOutput,
    }

    /// Execute a handler against `manager`, answering every prompt with `answer`.
    fn run(
        manager: &FakeManager,
        answer: bool,
        execute: impl FnOnce(&CommandContext<'_>) -> Result<(), CliError>,
    ) -> Run {
        let interaction = ScriptedInteraction::answering(answer);
        let output = RecordingOutput::default();
        let result = execute(&CommandContext::new(&output, manager, &interaction));
        Run {
            result,
            interaction,
            output,
        }
    }

    // ------------------------------------------------------------------------
    // remove
    // ------------------------------------------------------------------------

    fn remove(manager: &FakeManager, answer: bool, region: &str, force: bool) -> Run {
        run(manager, answer, |ctx| {
            RemoveHandler::execute(
                RemoveArgs {
                    region: region.to_string(),
                    package_type: PackageType::Ortho,
                    install_dir: install_dir(),
                    temp_dir: std::path::PathBuf::from("/tmp/unused"),
                    force,
                    custom_scenery_path: None,
                    disable_overlays: false,
                    xworld_pro: false,
                },
                ctx,
            )
        })
    }

    #[test]
    fn remove_resolves_a_prefix_against_installed_packages() {
        let manager = FakeManager::default().with_installed(
            PackageType::Ortho,
            &["NA-CANADA-WEST", "NA-CANADA-EAST", "EU-WEST"],
        );

        let run = remove(&manager, true, "NA-CANADA", false);

        run.result.unwrap();
        assert_eq!(manager.removals(), ["NA-CANADA-WEST", "NA-CANADA-EAST"]);
    }

    /// Remove the overlay "NA" with its per-region symlink in place; returns
    /// the run and whether the symlink is still there afterwards.
    fn remove_overlay_with_symlink(manager: &FakeManager) -> (Run, bool) {
        let scenery = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let link = scenery.path().join(xearthlayer_package::package_mountpoint(
            "NA",
            PackageType::Overlay,
        ));
        std::os::unix::fs::symlink(target.path(), &link).unwrap();

        let run = run(manager, true, |ctx| {
            RemoveHandler::execute(
                RemoveArgs {
                    region: "NA".to_string(),
                    package_type: PackageType::Overlay,
                    install_dir: install_dir(),
                    temp_dir: std::path::PathBuf::from("/tmp/unused"),
                    force: true,
                    custom_scenery_path: Some(scenery.path().to_path_buf()),
                    disable_overlays: false,
                    xworld_pro: false,
                },
                ctx,
            )
        });
        (run, link.symlink_metadata().is_ok())
    }

    #[test]
    fn a_refused_remove_leaves_the_overlay_symlink_in_place() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Overlay, &["NA"])
            .refusing_removal_of("NA");

        let (run, symlink_survives) = remove_overlay_with_symlink(&manager);

        assert!(run.result.is_err());
        assert!(manager.removals().is_empty());
        assert!(
            symlink_survives,
            "the symlink of a package that stayed installed must survive"
        );
    }

    #[test]
    fn a_completed_overlay_remove_clears_its_symlink() {
        let manager = FakeManager::default().with_installed(PackageType::Overlay, &["NA"]);

        let (run, symlink_survives) = remove_overlay_with_symlink(&manager);

        run.result.unwrap();
        assert_eq!(manager.removals(), ["NA"]);
        assert!(!symlink_survives);
    }

    #[test]
    fn remove_considers_only_the_requested_package_type() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["EU-WEST"])
            .with_installed(PackageType::Overlay, &["EU-WEST", "EU-EAST"]);

        let run = remove(&manager, true, "EU", true);

        run.result.unwrap();
        assert_eq!(manager.removals(), ["EU-WEST"]);
    }

    #[test]
    fn remove_asks_once_for_the_whole_set_and_a_no_removes_nothing() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["NA-CANADA-WEST", "NA-CANADA-EAST"]);

        let run = remove(&manager, false, "NA-CANADA", false);

        run.result.unwrap();
        assert!(manager.removals().is_empty());
        assert_eq!(
            run.interaction.prompts(),
            ["Are you sure you want to remove these 2 packages?"]
        );
    }

    #[test]
    fn remove_of_a_single_match_keeps_the_original_prompt() {
        let manager = FakeManager::default().with_installed(PackageType::Ortho, &["NA-GREENLAND"]);

        let run = remove(&manager, false, "NA-GREENLAND", false);

        assert_eq!(
            run.interaction.prompts(),
            ["Are you sure you want to remove this package?"]
        );
    }

    #[test]
    fn one_mounted_package_aborts_the_batch_before_anything_is_removed() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["NA-CANADA-WEST"])
            .with_mounted("NA-CANADA-EAST");

        let run = remove(&manager, true, "NA-CANADA", true);

        let message = packages_error(run.result);
        assert!(message.contains("NA-CANADA-EAST"), "{message}");
        assert!(manager.removals().is_empty());
        assert!(run.interaction.prompts().is_empty());
    }

    #[test]
    fn remove_with_no_match_lists_installed_codes_not_library_codes() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["AF-NORTH"])
            .with_installed(PackageType::Ortho, &["EU-WEST"]);

        let run = remove(&manager, true, "NA", true);

        let message = packages_error(run.result);
        assert!(message.contains("Available: EU-WEST"), "{message}");
        assert!(!message.contains("AF-NORTH"), "{message}");
    }

    // ------------------------------------------------------------------------
    // info
    // ------------------------------------------------------------------------

    fn info(manager: &FakeManager, region: &str) -> Run {
        run(manager, true, |ctx| {
            InfoHandler::execute(
                InfoArgs {
                    region: region.to_string(),
                    package_type: PackageType::Ortho,
                    install_dir: install_dir(),
                },
                ctx,
            )
        })
    }

    #[test]
    fn info_shows_every_matching_package() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["AS-RUSSIA-WEST", "AS-RUSSIA-EAST"]);

        let run = info(&manager, "as-russia");

        run.result.unwrap();
        let text = run.output.text();
        assert!(text.contains("AS-RUSSIA-WEST (ortho) v1.0.0"), "{text}");
        assert!(text.contains("AS-RUSSIA-EAST (ortho) v1.0.0"), "{text}");
    }

    #[test]
    fn info_with_no_match_lists_installed_codes() {
        let manager = FakeManager::default().with_installed(PackageType::Ortho, &["EU-WEST"]);

        let run = info(&manager, "NA");

        assert!(packages_error(run.result).contains("Available: EU-WEST"));
    }

    // ------------------------------------------------------------------------
    // install
    // ------------------------------------------------------------------------

    fn install(manager: &FakeManager, answer: bool, region: &str, auto_overlays: bool) -> Run {
        run(manager, answer, |ctx| {
            InstallHandler::execute(
                InstallArgs {
                    region: region.to_string(),
                    package_type: PackageType::Ortho,
                    library_url: String::new(),
                    install_dir: install_dir(),
                    temp_dir: install_dir(),
                    custom_scenery_path: None,
                    auto_install_overlays: auto_overlays,
                    disable_overlays: false,
                    xworld_pro: false,
                    concurrent_downloads: 1,
                },
                ctx,
            )
        })
    }

    #[test]
    fn install_of_a_single_match_does_not_prompt() {
        let manager = FakeManager::default().with_library(PackageType::Ortho, &["NA-GREENLAND"]);

        let run = install(&manager, false, "NA-G", false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["NA-GREENLAND"]);
        assert!(run.interaction.prompts().is_empty());
    }

    #[test]
    fn install_of_many_matches_lists_them_and_installs_each_after_confirmation() {
        let manager = FakeManager::default().with_library(
            PackageType::Ortho,
            &["NA-CANADA-WEST", "NA-CANADA-EAST", "EU-WEST"],
        );

        let run = install(&manager, true, "NA-CANADA", false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["NA-CANADA-WEST", "NA-CANADA-EAST"]);
        assert_eq!(run.interaction.prompts(), ["Proceed with install?"]);
        let text = run.output.text();
        assert!(
            text.contains("NA-CANADA matches 2 packages (ortho):"),
            "{text}"
        );
        assert!(text.contains("NA-CANADA-EAST v1.0.0"), "{text}");
    }

    #[test]
    fn declining_a_multi_install_installs_nothing() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-CANADA-WEST", "NA-CANADA-EAST"]);

        let run = install(&manager, false, "NA-CANADA", false);

        run.result.unwrap();
        assert!(manager.installs().is_empty());
    }

    #[test]
    fn install_considers_only_the_requested_package_type() {
        let manager = FakeManager::default()
            .with_library(PackageType::Overlay, &["EU-WEST"])
            .with_library(PackageType::Ortho, &["EU-EAST"]);

        let run = install(&manager, false, "EU", false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["EU-EAST"]);
        assert!(run.interaction.prompts().is_empty());
    }

    #[test]
    fn install_with_no_match_lists_library_codes_not_installed_codes() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["AF-NORTH"])
            .with_installed(PackageType::Ortho, &["EU-WEST"]);

        let message = packages_error(install(&manager, true, "NA", false).result);

        assert!(message.contains("Available: AF-NORTH"), "{message}");
        assert!(!message.contains("EU-WEST"), "{message}");
    }

    #[test]
    fn a_failed_install_does_not_stop_the_rest_of_the_batch() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-CANADA-WEST", "NA-CANADA-EAST"])
            .failing_on("NA-CANADA-WEST", PackageType::Ortho);

        let run = install(&manager, true, "NA-CANADA", false);

        assert_eq!(manager.installs(), ["NA-CANADA-WEST", "NA-CANADA-EAST"]);
        let message = packages_error(run.result);
        assert!(message.contains("1 of 2"), "{message}");
    }

    #[test]
    fn auto_installed_overlays_follow_each_resolved_package() {
        let canada = ["NA-CANADA-WEST", "NA-CANADA-EAST"];
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &canada)
            .with_library(PackageType::Overlay, &canada);

        let run = install(&manager, true, "NA-CANADA", true);

        run.result.unwrap();
        assert_eq!(
            manager.installs(),
            [
                "NA-CANADA-WEST",
                "NA-CANADA-WEST",
                "NA-CANADA-EAST",
                "NA-CANADA-EAST"
            ]
        );
    }

    #[test]
    fn install_with_auto_overlays_installs_every_package_in_one_batch() {
        // One batch, so each package installs while the next downloads (#292).
        let canada = ["NA-CANADA-WEST", "NA-CANADA-EAST"];
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &canada)
            .with_library(PackageType::Overlay, &canada);

        install(&manager, true, "NA-CANADA", true).result.unwrap();

        assert_eq!(
            manager.batches(),
            [[
                "NA-CANADA-WEST (ortho)",
                "NA-CANADA-WEST (overlay)",
                "NA-CANADA-EAST (ortho)",
                "NA-CANADA-EAST (overlay)",
            ]]
        );
    }

    #[test]
    fn an_overlay_still_installs_when_its_ortho_fails() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .with_library(PackageType::Overlay, &["NA-GREENLAND"])
            .failing_on("NA-GREENLAND", PackageType::Ortho);

        let run = install(&manager, true, "NA-GREENLAND", true);

        let text = run.output.text();
        assert!(text.contains("Installed NA-GREENLAND (overlay)"), "{text}");
        assert!(packages_error(run.result).contains("1 of 2"));
    }

    #[test]
    fn a_single_failed_install_reports_its_own_error() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .failing_on("NA-GREENLAND", PackageType::Ortho);

        let message = packages_error(install(&manager, true, "NA-GREENLAND", false).result);

        assert!(message.contains("NA-GREENLAND (ortho) failed"), "{message}");
    }

    // ------------------------------------------------------------------------
    // update
    // ------------------------------------------------------------------------

    fn update(manager: &FakeManager, region: &str) -> Run {
        run(manager, true, |ctx| {
            UpdateHandler::execute(
                UpdateArgs {
                    region: Some(region.to_string()),
                    package_type: None,
                    library_url: String::new(),
                    install_dir: install_dir(),
                    temp_dir: install_dir(),
                    all: true,
                    custom_scenery_path: None,
                    disable_overlays: false,
                    xworld_pro: false,
                    concurrent_downloads: 1,
                },
                ctx,
            )
        })
    }

    #[test]
    fn update_with_a_prefix_updates_every_matching_package() {
        let manager = FakeManager::default()
            .with_update(PackageType::Ortho, "NA-CANADA-WEST")
            .with_update(PackageType::Ortho, "NA-CANADA-EAST")
            .with_update(PackageType::Ortho, "EU-WEST");

        update(&manager, "NA-CANADA").result.unwrap();

        assert_eq!(manager.installs(), ["NA-CANADA-WEST", "NA-CANADA-EAST"]);
    }

    #[test]
    fn update_with_no_installed_match_lists_installed_codes() {
        let manager = FakeManager::default()
            .with_update(PackageType::Ortho, "EU-WEST")
            .with_library(PackageType::Ortho, &["AF-NORTH"]);

        let message = packages_error(update(&manager, "AF").result);

        assert!(message.contains("Available: EU-WEST"), "{message}");
        assert!(!message.contains("AF-NORTH"), "{message}");
    }

    #[test]
    fn update_installs_every_update_in_one_batch() {
        let manager = FakeManager::default()
            .with_update(PackageType::Ortho, "NA-CANADA-WEST")
            .with_update(PackageType::Ortho, "NA-CANADA-EAST");

        update(&manager, "NA-CANADA").result.unwrap();

        assert_eq!(
            manager.batches(),
            [["NA-CANADA-WEST (ortho)", "NA-CANADA-EAST (ortho)"]]
        );
    }

    #[test]
    fn a_failed_update_does_not_stop_the_others() {
        let manager = FakeManager::default()
            .with_update(PackageType::Ortho, "NA-CANADA-WEST")
            .with_update(PackageType::Ortho, "NA-CANADA-EAST")
            .failing_on("NA-CANADA-WEST", PackageType::Ortho);

        let run = update(&manager, "NA-CANADA");

        run.result.unwrap();
        assert_eq!(manager.installs(), ["NA-CANADA-WEST", "NA-CANADA-EAST"]);
        let text = run.output.text();
        assert!(text.contains("Failed to update NA-CANADA-WEST"), "{text}");
        assert!(text.contains("1 succeeded, 1 failed"), "{text}");
    }

    // ------------------------------------------------------------------------
    // consolidated overlay rebuild (X-World Pro)
    // ------------------------------------------------------------------------

    fn overlay_package(packages: &Path, region: &str) {
        let folder = xearthlayer_package::package_mountpoint(region, PackageType::Overlay);
        let dir = packages.join(&folder);
        let grid = dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&grid).unwrap();
        std::fs::write(
            grid.join(if region == "na" {
                "+33-119.dsf"
            } else {
                "+34-118.dsf"
            }),
            "dsf",
        )
        .unwrap();
        std::fs::write(
            dir.join("xearthlayer_scenery_package.txt"),
            format!(
                "REGIONAL SCENERY PACKAGE\n1.0.0\n{}  1.0.0\n2024-01-01T00:00:00Z\nY\n{folder}\n\
                 test.tar.gz\n1\n\nabc123  test.tar.gz  http://example.com/test.tar.gz\n",
                region.to_uppercase()
            ),
        )
        .unwrap();
    }

    /// Rebuild with an installed NA and EU overlay and X-World Pro Europe
    /// enabled in `scenery_packs.ini`; returns what the user saw and the
    /// regions left in `yzXEL_overlay`.
    fn rebuild(disable_overlays: bool, xworld_pro: bool) -> (String, bool) {
        let packages = tempfile::tempdir().unwrap();
        overlay_package(packages.path(), "na");
        overlay_package(packages.path(), "eu");
        let scenery = tempfile::tempdir().unwrap();
        let cs = scenery.path().join("Custom Scenery");
        std::fs::create_dir_all(cs.join("simHeaven_X-WORLD-Pro_Europe-09-scenery")).unwrap();
        std::fs::write(
            cs.join("scenery_packs.ini"),
            "SCENERY_PACK Custom Scenery/simHeaven_X-WORLD-Pro_Europe-09-scenery/\n",
        )
        .unwrap();
        let manager = FakeManager::default();
        let out = run(&manager, true, |ctx| {
            rebuild_consolidated_overlay(
                packages.path(),
                &Some(cs.clone()),
                disable_overlays,
                xworld_pro,
                ctx,
            );
            Ok(())
        });
        let folder = cs.join("yzXEL_overlay");
        let eu_linked = std::fs::read_dir(folder.join("Earth nav data/+30-120"))
            .map(|d| {
                d.flatten()
                    .any(|f| f.file_name().to_string_lossy().contains("+34-118"))
            })
            .unwrap_or(false);
        (out.output.text(), eu_linked)
    }

    #[test]
    fn package_commands_leave_x_world_pro_continents_out() {
        let (text, eu_linked) = rebuild(false, true);
        assert!(!eu_linked, "{text}");
        assert!(text.contains("X-World Pro active (Europe)"), "{text}");
        assert!(text.contains("left out 1 overlay regions"), "{text}");
        assert!(
            text.contains("Consolidated overlay updated: 1 regions"),
            "{text}"
        );
    }

    #[test]
    fn package_commands_build_everything_with_x_world_pro_off() {
        let (text, eu_linked) = rebuild(false, false);
        assert!(eu_linked, "{text}");
        assert!(
            text.contains("Consolidated overlay updated: 2 regions"),
            "{text}"
        );
    }

    #[test]
    fn disable_overlays_still_wins_over_x_world_pro() {
        let (text, _) = rebuild(true, true);
        assert!(
            text.contains("skipping consolidated overlay rebuild"),
            "{text}"
        );
        assert!(!text.contains("X-World Pro active"), "{text}");
    }
}
