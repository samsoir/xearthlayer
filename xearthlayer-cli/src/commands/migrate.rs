//! Migration CLI commands.
//!
//! A namespace rather than a single command. Migrations are transient by
//! nature: each exists to move users off an old state and should then be
//! deleted. Scattering them into their domains is how one gets forgotten, which
//! is what happened to `cache migrate`, deprecated for removal in v0.4.0 and
//! still shipping. One place to look is also one place to prune, and it makes
//! the healthy state, which is empty, visible.

use clap::Subcommand;

use crate::error::CliError;
use crate::preflight;
use xearthlayer::preflight::{Preflight, RunOutcome, Runner, Status};
use xearthlayer::{config::ConfigFile, paths};

/// Migration action subcommands.
#[derive(Debug, Subcommand)]
pub enum MigrateAction {
    /// Move configuration and resources to the standard directories for this
    /// system.
    ///
    /// Runs automatically on first start after upgrading. Run it here to
    /// review resource locations, which is what happens when the automatic run
    /// had no terminal to ask in.
    Layout,

    /// Show which migrations have run and which are pending.
    Status,
}

/// Run a migration subcommand.
///
/// No action runs every applicable migration, which today is the layout.
pub fn run(action: Option<MigrateAction>, dry_run: bool) -> Result<(), CliError> {
    match action {
        Some(MigrateAction::Status) => status(),
        Some(MigrateAction::Layout) | None => layout(dry_run),
    }
}

/// Build a registry holding only the layout migration.
fn layout_registry() -> Runner<xearthlayer::preflight::BootstrapContext> {
    let mut runner = Runner::new();
    runner.register(Box::new(preflight::migrate::LayoutMigration::on_demand(
        preflight::legacy_install_dir(),
        Box::new(paths::layout_snapshot()),
    )));
    runner
}

fn layout(dry_run: bool) -> Result<(), CliError> {
    let mut registry = layout_registry();
    let mut ctx = xearthlayer::preflight::BootstrapContext::new("migrate", None);

    if dry_run {
        // Report mode *is* the dry run. Inspection is pure, so running the
        // registry without remediating cannot change anything, which is a
        // stronger guarantee than a separate code path that promises not to.
        for (check, status) in registry.report(&ctx) {
            match status {
                Status::Satisfied => println!("{}: nothing to do", check),
                Status::Warning(message) => println!("{}: {}", check, message),
                Status::Unsatisfied { reason, .. } => println!("{}: {}", check, reason),
            }
        }
        return Ok(());
    }

    match registry
        .enforce(&mut ctx)
        .map_err(preflight::remediation_to_cli_error)?
    {
        RunOutcome::AllSatisfied => Ok(()),
        RunOutcome::Failed {
            check,
            reason,
            hint,
        } => Err(preflight::to_cli_error(&check, reason, hint, &ctx)),
    }
}

fn status() -> Result<(), CliError> {
    let config = ConfigFile::load().unwrap_or_default();
    let legacy = preflight::legacy_install_dir();

    println!("Layout");
    println!("  Version in force: {}", paths::LAYOUT_VERSION);
    println!("  Configuration says: {}", config.general.layout_version);
    println!("  Configuration file: {}", paths::config_file().display());
    println!(
        "  Pre-0.5.0 directory: {}",
        if legacy.exists() {
            format!("{} (still present)", legacy.display())
        } else {
            "absent".to_string()
        }
    );
    println!();

    let migration =
        preflight::migrate::LayoutMigration::on_demand(legacy, Box::new(paths::layout_snapshot()));
    let ctx = xearthlayer::preflight::BootstrapContext::new("migrate", None);
    match migration.inspect(&ctx) {
        Status::Satisfied => println!("  Pending: none"),
        _ => println!("  Pending: layout (run 'xearthlayer migrate layout')"),
    }
    Ok(())
}
