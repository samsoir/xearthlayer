//! Command handlers for packages CLI commands.
//!
//! Each handler implements the `CommandHandler` trait and contains the
//! business logic for its respective command.

use std::path::{Path, PathBuf};

use xearthlayer::config::format_size;
use xearthlayer::manager::{InstallResult, InstalledPackage, LocalPackageStore, PackageStatus};
use xearthlayer_package::{LibraryEntry, PackageType};

use super::actions::{self, RunActive};
use super::args::{CheckArgs, InfoArgs, InstallArgs, ListArgs, RemoveArgs, UpdateArgs};
use super::selector::{resolve, Selection};
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
    selection: &Selection,
    package_type: PackageType,
) -> Result<Vec<InstalledPackage>, CliError> {
    let installed = ctx
        .manager
        .list_packages(store)
        .map_err(|e| CliError::Packages(e.to_string()))?
        .into_iter()
        .filter(|p| p.package_type() == package_type)
        .collect();
    selection.select(
        installed,
        |p| p.region(),
        &format!("No {package_type} packages are installed"),
    )
}

impl From<RunActive> for CliError {
    fn from(run: RunActive) -> Self {
        CliError::Packages(run.message())
    }
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

/// Rebuild the consolidated overlay folder after an overlay install/remove/update
/// and show what happened.
fn rebuild_consolidated_overlay(
    install_dir: &Path,
    custom_scenery_path: &Option<PathBuf>,
    disable_overlays: bool,
    xworld_pro: bool,
    ctx: &CommandContext<'_>,
) {
    let report = actions::rebuild_overlays(
        ctx.manager,
        install_dir,
        custom_scenery_path.as_deref(),
        disable_overlays,
        xworld_pro,
    );
    for line in &report.lines {
        ctx.output.println(line);
    }
    match report.outcome {
        Some(outcome) if report.failed => ctx.output.error(&outcome),
        Some(outcome) => ctx.output.success(&outcome),
        None => {}
    }
}

// ============================================================================
// Install Handler
// ============================================================================

/// Handler for the `packages install` command.
pub struct InstallHandler;

impl InstallHandler {
    /// Report each package's outcome, rebuild the consolidated overlay once
    /// if any overlay installed, and fail the command if any package failed.
    ///
    /// Packages succeed or fail independently: an overlay still installs
    /// when its ortho package fails.
    fn report(
        outcomes: Vec<(&LibraryEntry, Result<InstallResult, CliError>)>,
        args: &InstallArgs,
        running: bool,
        ctx: &CommandContext<'_>,
    ) -> Result<(), CliError> {
        let total = outcomes.len();
        let mut failures = Vec::new();
        let mut overlay_installed = false;
        for (entry, result) in outcomes {
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
                    if running {
                        ctx.output
                            .indented(&format!("Note: {}", actions::RESTART_NOTE));
                    }
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
            _ if total == 1 => Err(failures.remove(0).1),
            failed => {
                for (entry, e) in &failures {
                    ctx.output
                        .error(&format!("{} ({}): {}", entry.title, entry.package_type, e));
                }
                Err(CliError::Packages(format!(
                    "{} of {} packages failed to install",
                    failed, total
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
            args.selection.describe(),
            args.package_type
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
        let entries = args.selection.select(
            offered,
            |e| e.title.as_str(),
            &format!("The library offers no {} packages", args.package_type),
        )?;

        if entries.len() > 1 {
            let verb = match args.selection {
                Selection::All => "selects",
                Selection::Regions(_) => "matches",
            };
            ctx.output.println(&format!(
                "{} {verb} {} packages ({}):",
                args.selection.describe(),
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

        let plan = actions::plan_install(&entries, &library, args.auto_install_overlays);
        // While `run` serves the packages, an install over an installed
        // package is an update in disguise: refuse it, package by package.
        // New packages install, and `run` picks them up on its next start.
        let refusal = actions::check_replacement(ctx.manager).err();
        let mut allowed: Vec<_> = plan.iter().copied().enumerate().collect();
        let mut outcomes = Vec::new();
        if let Some(run) = &refusal {
            let store = ctx.manager.create_store(&args.install_dir);
            let installed = ctx
                .manager
                .list_packages(&store)
                .map_err(|e| CliError::Packages(e.to_string()))?;
            let refused;
            (refused, allowed) = allowed.into_iter().partition(|(_, entry)| {
                installed.iter().any(|p| {
                    p.region().eq_ignore_ascii_case(&entry.title)
                        && p.package_type() == entry.package_type
                })
            });
            outcomes.extend(
                refused
                    .into_iter()
                    .map(|(position, entry)| (position, entry, Err(run.clone().into()))),
            );
        }

        let results = install_entries(
            &allowed.iter().map(|&(_, entry)| entry).collect::<Vec<_>>(),
            &args.install_dir,
            &args.temp_dir,
            args.concurrent_downloads,
            ctx,
        );
        outcomes.extend(
            allowed
                .into_iter()
                .zip(results)
                .map(|((position, entry), result)| (position, entry, result)),
        );
        // Report in plan order, refused and installed alike.
        outcomes.sort_by_key(|&(position, ..)| position);
        let outcomes = outcomes
            .into_iter()
            .map(|(_, entry, result)| (entry, result))
            .collect();
        Self::report(outcomes, &args, refusal.is_some(), ctx)
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
        actions::check_replacement(ctx.manager)?;
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
        actions::check_removal(ctx.manager)?;
        let store = ctx.manager.create_store(&args.install_dir);
        let packages = select_installed(ctx, &store, &args.selection, args.package_type)?;

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

            // Only now: a refused remove (e.g. the queue lock is held) leaves the
            // package installed, and it must keep its overlay symlink.
            actions::after_removal(
                region,
                args.package_type,
                args.custom_scenery_path.as_deref(),
            );

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
        let selection = Selection::Regions(vec![args.region.clone()]);
        let packages = select_installed(ctx, &store, &selection, args.package_type)?;

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
        remove_selection(
            manager,
            answer,
            Selection::Regions(vec![region.to_string()]),
            force,
        )
    }

    fn remove_selection(
        manager: &FakeManager,
        answer: bool,
        selection: Selection,
        force: bool,
    ) -> Run {
        run(manager, answer, |ctx| {
            RemoveHandler::execute(
                RemoveArgs {
                    selection,
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
                    selection: Selection::Regions(vec!["NA".to_string()]),
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
    fn a_running_instance_refuses_the_whole_remove_batch() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["NA-CANADA-WEST", "NA-CANADA-EAST"])
            .with_running_instance(4242);

        let run = remove(&manager, true, "NA-CANADA", true);

        let message = packages_error(run.result);
        assert!(message.contains("pid 4242"), "{message}");
        assert!(
            message.contains("stop it before updating or removing packages"),
            "{message}"
        );
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

    fn regions(codes: &[&str]) -> Selection {
        Selection::Regions(codes.iter().map(|c| c.to_string()).collect())
    }

    #[test]
    fn remove_of_several_regions_removes_everything_they_select_after_one_prompt() {
        let manager = FakeManager::default().with_installed(
            PackageType::Ortho,
            &["NA-CANADA-WEST", "NA-CANADA-EAST", "EU-WEST", "AF-SOUTH"],
        );

        let run = remove_selection(&manager, true, regions(&["AF", "NA-CANADA"]), false);

        run.result.unwrap();
        assert_eq!(
            manager.removals(),
            ["NA-CANADA-WEST", "NA-CANADA-EAST", "AF-SOUTH"]
        );
        assert_eq!(
            run.interaction.prompts(),
            ["Are you sure you want to remove these 3 packages?"]
        );
    }

    #[test]
    fn remove_with_one_unmatched_region_removes_nothing() {
        let manager =
            FakeManager::default().with_installed(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"]);

        let run = remove_selection(&manager, true, regions(&["EU", "XX"]), true);

        assert!(packages_error(run.result).contains("'XX'"));
        assert!(manager.removals().is_empty());
    }

    #[test]
    fn remove_all_removes_every_installed_package_of_the_type_after_one_prompt() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_installed(PackageType::Overlay, &["EU-WEST"]);

        let run = remove_selection(&manager, true, Selection::All, false);

        run.result.unwrap();
        assert_eq!(manager.removals(), ["EU-WEST", "AF-SOUTH"]);
        assert_eq!(
            run.interaction.prompts(),
            ["Are you sure you want to remove these 2 packages?"]
        );
    }

    #[test]
    fn remove_all_with_force_does_not_prompt() {
        let manager =
            FakeManager::default().with_installed(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"]);

        let run = remove_selection(&manager, false, Selection::All, true);

        run.result.unwrap();
        assert_eq!(manager.removals(), ["EU-WEST", "AF-SOUTH"]);
        assert!(run.interaction.prompts().is_empty());
    }

    #[test]
    fn remove_all_refuses_while_run_is_serving_packages() {
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_running_instance(7);

        let run = remove_selection(&manager, true, Selection::All, true);

        assert!(packages_error(run.result).contains("XEarthLayer is running (pid 7)"));
        assert!(manager.removals().is_empty());
    }

    #[test]
    fn remove_all_with_nothing_installed_is_an_error() {
        let manager = FakeManager::default();

        let run = remove_selection(&manager, true, Selection::All, true);

        assert!(packages_error(run.result).contains("No ortho packages are installed"));
    }

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
        install_selection(
            manager,
            answer,
            Selection::Regions(vec![region.to_string()]),
            PackageType::Ortho,
            auto_overlays,
        )
    }

    fn install_selection(
        manager: &FakeManager,
        answer: bool,
        selection: Selection,
        package_type: PackageType,
        auto_overlays: bool,
    ) -> Run {
        run(manager, answer, |ctx| {
            InstallHandler::execute(
                InstallArgs {
                    selection,
                    package_type,
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

    #[test]
    fn install_of_several_regions_installs_everything_they_select_in_one_batch() {
        let manager = FakeManager::default().with_library(
            PackageType::Ortho,
            &["NA-GREENLAND", "EU-WEST", "AF-NORTH", "AF-SOUTH"],
        );

        let run = install_selection(
            &manager,
            true,
            regions(&["AF", "NA-G"]),
            PackageType::Ortho,
            false,
        );

        run.result.unwrap();
        assert_eq!(
            manager.batches(),
            [vec![
                "NA-GREENLAND (ortho)".to_string(),
                "AF-NORTH (ortho)".to_string(),
                "AF-SOUTH (ortho)".to_string(),
            ]]
        );
        assert_eq!(run.interaction.prompts(), ["Proceed with install?"]);
        assert!(run
            .output
            .text()
            .contains("AF, NA-G matches 3 packages (ortho):"));
    }

    #[test]
    fn install_with_one_unmatched_region_installs_nothing() {
        let manager =
            FakeManager::default().with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"]);

        let run = install_selection(
            &manager,
            true,
            regions(&["EU", "XX"]),
            PackageType::Ortho,
            false,
        );

        assert!(packages_error(run.result).contains("'XX'"));
        assert!(manager.installs().is_empty());
    }

    #[test]
    fn install_all_installs_every_library_package_of_the_type_after_confirmation() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_library(PackageType::Overlay, &["EU-WEST"]);

        let run = install_selection(&manager, true, Selection::All, PackageType::Ortho, false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["EU-WEST", "AF-SOUTH"]);
        assert_eq!(run.interaction.prompts(), ["Proceed with install?"]);
        assert!(run
            .output
            .text()
            .contains("--all selects 2 packages (ortho):"));
    }

    #[test]
    fn install_all_respects_the_package_type() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_library(PackageType::Overlay, &["EU-WEST"]);

        let run = install_selection(&manager, true, Selection::All, PackageType::Overlay, false);

        run.result.unwrap();
        assert_eq!(manager.batches(), [vec!["EU-WEST (overlay)".to_string()]]);
    }

    #[test]
    fn install_all_brings_overlays_along_when_auto_install_is_on() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_library(PackageType::Overlay, &["EU-WEST"]);

        let run = install_selection(&manager, true, Selection::All, PackageType::Ortho, true);

        run.result.unwrap();
        assert_eq!(
            manager.batches(),
            [vec![
                "EU-WEST (ortho)".to_string(),
                "EU-WEST (overlay)".to_string(),
                "AF-SOUTH (ortho)".to_string(),
            ]]
        );
    }

    #[test]
    fn declining_install_all_installs_nothing() {
        let manager =
            FakeManager::default().with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"]);

        let run = install_selection(&manager, false, Selection::All, PackageType::Ortho, false);

        run.result.unwrap();
        assert!(manager.installs().is_empty());
    }

    #[test]
    fn install_all_from_a_library_without_the_type_is_an_error() {
        let manager = FakeManager::default().with_library(PackageType::Ortho, &["EU-WEST"]);

        let run = install_selection(&manager, true, Selection::All, PackageType::Overlay, false);

        assert!(packages_error(run.result).contains("The library offers no overlay packages"));
    }

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

    #[test]
    fn a_running_instance_refuses_an_update_and_installs_nothing() {
        let manager = FakeManager::default()
            .with_update(PackageType::Ortho, "NA-CANADA-WEST")
            .with_running_instance(4242);

        let run = update(&manager, "NA-CANADA");

        let message = packages_error(run.result);
        assert!(message.contains("pid 4242"), "{message}");
        assert!(
            message.contains("stop it before updating or removing packages"),
            "{message}"
        );
        assert!(manager.batches().is_empty());
    }

    // ------------------------------------------------------------------------
    // install while `xearthlayer run` is serving packages
    // ------------------------------------------------------------------------

    #[test]
    fn a_running_instance_still_installs_packages_that_are_not_installed() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .with_running_instance(4242);

        let run = install(&manager, true, "NA-G", false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["NA-GREENLAND"]);
        let text = run.output.text();
        assert!(text.contains("restart xearthlayer run to use it"), "{text}");
    }

    #[test]
    fn a_running_instance_refuses_a_package_that_is_already_installed() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .with_installed(PackageType::Ortho, &["NA-GREENLAND"])
            .with_running_instance(4242);

        let run = install(&manager, true, "NA-G", false);

        let message = packages_error(run.result);
        assert!(
            message.contains("XEarthLayer is running (pid 4242)"),
            "{message}"
        );
        assert!(
            message.contains("stop it before updating or removing packages"),
            "{message}"
        );
        assert!(manager.installs().is_empty());
    }

    /// Installed metadata may carry a legacy lower-case title; it is the
    /// same package folder, so it is still refused while `run` serves it.
    #[test]
    fn a_running_instance_refuses_an_installed_package_whose_title_differs_in_case() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .with_installed(PackageType::Ortho, &["na-greenland"])
            .with_running_instance(4242);

        let run = install(&manager, true, "NA-G", false);

        let message = packages_error(run.result);
        assert!(
            message.contains("XEarthLayer is running (pid 4242)"),
            "{message}"
        );
        assert!(manager.installs().is_empty());
    }

    #[test]
    fn a_running_instance_refuses_per_package_and_the_rest_of_the_batch_continues() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-CANADA-WEST", "NA-CANADA-EAST"])
            .with_installed(PackageType::Ortho, &["NA-CANADA-WEST"])
            .with_running_instance(4242);

        let run = install(&manager, true, "NA-CANADA", false);

        assert_eq!(manager.installs(), ["NA-CANADA-EAST"]);
        let text = run.output.text();
        assert!(text.contains("Installed NA-CANADA-EAST"), "{text}");
        assert!(
            text.contains("NA-CANADA-WEST (ortho): ")
                && text.contains("XEarthLayer is running (pid 4242)"),
            "{text}"
        );
        assert!(text.contains("restart xearthlayer run to use it"), "{text}");
        assert!(packages_error(run.result).contains("1 of 2"));
    }

    #[test]
    fn without_a_running_instance_an_installed_package_installs_again() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["NA-GREENLAND"])
            .with_installed(PackageType::Ortho, &["NA-GREENLAND"]);

        let run = install(&manager, true, "NA-G", false);

        run.result.unwrap();
        assert_eq!(manager.installs(), ["NA-GREENLAND"]);
        assert!(!run.output.text().contains("restart xearthlayer run"));
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
