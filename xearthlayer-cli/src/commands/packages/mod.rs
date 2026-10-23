//! Package Manager CLI commands for installing and managing scenery packages.
//!
//! This module implements the Command Pattern with trait-based dependency
//! injection, providing a clean separation of concerns:
//!
//! - `traits`: Core interfaces (`Output`, `PackageManagerService`, `CommandHandler`)
//! - `services`: Concrete implementations of the traits
//! - `args`: CLI argument types and parsing (clap-derived)
//! - `handlers`: Command handlers implementing business logic
//! - `actions`: Console-free package decisions shared with the package manager TUI
//!
//! # Architecture
//!
//! Each command handler:
//! - Implements the `CommandHandler` trait
//! - Depends only on trait interfaces via `CommandContext`
//! - Can be tested in isolation with mock implementations

pub(crate) mod actions;
mod args;
mod handlers;
pub(crate) mod progress;
mod selector;
mod services;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod traits;

// Re-export public types
pub use args::PackagesCommands;
pub use handlers::{
    CheckHandler, InfoHandler, InstallHandler, ListHandler, RemoveHandler, UpdateHandler,
};
pub use services::{ConsoleInteraction, ConsoleOutput, DefaultPackageManagerService};
pub use traits::CommandHandler;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use args::{CheckArgs, InfoArgs, InstallArgs, ListArgs, RemoveArgs, UpdateArgs};
use clap::CommandFactory;
use selector::Selection;
use traits::CommandContext;
use xearthlayer::config::ConfigFile;

use crate::error::CliError;
use crate::signals::{ActiveQueue, SignalWatch};

/// Load config or return default.
fn load_config() -> ConfigFile {
    ConfigFile::load().unwrap_or_default()
}

/// Get the default installation directory.
///
/// Priority:
/// 1. packages.install_location from config
/// 2. ~/.xearthlayer/packages (default)
pub(crate) fn default_install_dir(config: &ConfigFile) -> PathBuf {
    config
        .packages
        .install_location
        .clone()
        .unwrap_or_else(xearthlayer::paths::packages_dir)
}

/// Get the Custom Scenery directory for overlay symlinks.
///
/// Priority:
/// 1. packages.custom_scenery_path from config
/// 2. xplane.scenery_dir from config
/// 3. Auto-detect from X-Plane installation
pub(crate) fn default_custom_scenery_dir(config: &ConfigFile) -> Option<PathBuf> {
    config
        .packages
        .custom_scenery_path
        .clone()
        .or_else(|| config.xplane.scenery_dir.clone())
        .or_else(|| {
            // Try auto-detect from X-Plane
            xearthlayer::config::detect_custom_scenery().ok()
        })
}

/// Get the default temporary directory.
///
/// Uses the config file's packages.temp_dir if available, otherwise falls back to
/// `~/.xearthlayer/tmp`. This avoids using the system temp directory which on many
/// Linux distributions is a tmpfs (RAM-backed), causing OOM when downloading large
/// scenery packages.
pub(crate) fn default_temp_dir(config: &ConfigFile) -> PathBuf {
    config
        .packages
        .temp_dir
        .clone()
        .unwrap_or_else(xearthlayer::paths::temp_dir)
}

/// Get the default library URL from config.
///
/// Returns an error if not configured.
fn require_library_url(cli_url: Option<String>, config: &ConfigFile) -> Result<String, CliError> {
    cli_url
        .or_else(|| config.packages.library_url.clone())
        .ok_or_else(|| {
            CliError::Config(
                "No library URL specified. Use --library-url or set library_url in config.ini [packages] section.".to_string(),
            )
        })
}

/// Whether `xearthlayer packages` with no subcommand opens the package
/// manager: only with a terminal to draw in and keys to read.
fn open_tui(stdin_tty: bool, stdout_tty: bool) -> bool {
    stdin_tty && stdout_tty
}

/// Open the package manager, and print what it did once it exits.
pub fn open_manager() -> Result<(), CliError> {
    let summary = crate::ui::packages::runner::run_manager(&load_config())?;
    println!("{summary}");
    Ok(())
}

/// Run a packages subcommand, or with none, open the package manager.
///
/// This is the main entry point for packages commands. It creates the
/// production context with real implementations and dispatches to the
/// appropriate handler. With no subcommand and no terminal, it prints the
/// `packages` help, as before the package manager existed.
pub fn run(command: Option<PackagesCommands>) -> Result<(), CliError> {
    let Some(command) = command else {
        if open_tui(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        ) {
            return open_manager();
        }
        let mut cli = crate::Cli::command();
        if let Some(packages) = cli.find_subcommand_mut("packages") {
            packages
                .print_long_help()
                .map_err(|e| CliError::Packages(format!("could not print help: {e}")))?;
        }
        return Ok(());
    };

    // Load config once for all commands
    let config = load_config();

    // Create production context
    let output = ConsoleOutput::new();
    let active = ActiveQueue::default();
    let manager = DefaultPackageManagerService::with_active_queue(active.clone());
    let interaction = ConsoleInteraction::new();
    let ctx = CommandContext::new(&output, &manager, &interaction);

    // Signals stop a running package operation cleanly; with none running
    // they keep their default action.
    let watch = SignalWatch::start(active, |notice| {
        // A closed stderr must not panic the signal thread.
        if !crate::signals::terminal_gone() {
            let _ = writeln!(std::io::stderr(), "{notice}");
        }
    })
    .map_err(|e| CliError::Packages(format!("could not install signal handlers: {e}")))?;

    // Dispatch to appropriate handler
    let result = match command {
        PackagesCommands::List {
            install_dir,
            verbose,
        } => ListHandler::execute(
            ListArgs {
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                verbose,
            },
            &ctx,
        ),

        PackagesCommands::Check {
            library_url,
            install_dir,
        } => CheckHandler::execute(
            CheckArgs {
                library_url: require_library_url(library_url, &config)?,
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
            },
            &ctx,
        ),

        PackagesCommands::Install {
            regions,
            all,
            r#type,
            library_url,
            install_dir,
            temp_dir,
        } => InstallHandler::execute(
            InstallArgs {
                selection: Selection::from_args(regions, all),
                package_type: r#type.into(),
                library_url: require_library_url(library_url, &config)?,
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                temp_dir: temp_dir.unwrap_or_else(|| default_temp_dir(&config)),
                custom_scenery_path: default_custom_scenery_dir(&config),
                auto_install_overlays: config.packages.auto_install_overlays,
                disable_overlays: config.packages.disable_overlays,
                xworld_pro: config.packages.xworld_pro,
                concurrent_downloads: config.packages.concurrent_downloads,
            },
            &ctx,
        ),

        PackagesCommands::Update {
            region,
            r#type,
            library_url,
            install_dir,
            temp_dir,
            all,
        } => UpdateHandler::execute(
            UpdateArgs {
                region,
                package_type: r#type.map(|t| t.into()),
                library_url: require_library_url(library_url, &config)?,
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                temp_dir: temp_dir.unwrap_or_else(|| default_temp_dir(&config)),
                all,
                custom_scenery_path: default_custom_scenery_dir(&config),
                disable_overlays: config.packages.disable_overlays,
                xworld_pro: config.packages.xworld_pro,
                concurrent_downloads: config.packages.concurrent_downloads,
            },
            &ctx,
        ),

        PackagesCommands::Remove {
            regions,
            all,
            r#type,
            install_dir,
            force,
        } => RemoveHandler::execute(
            RemoveArgs {
                selection: Selection::from_args(regions, all),
                package_type: r#type.into(),
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                temp_dir: default_temp_dir(&config),
                force,
                custom_scenery_path: default_custom_scenery_dir(&config),
                disable_overlays: config.packages.disable_overlays,
                xworld_pro: config.packages.xworld_pro,
            },
            &ctx,
        ),

        PackagesCommands::Info {
            region,
            r#type,
            install_dir,
        } => InfoHandler::execute(
            InfoArgs {
                region,
                package_type: r#type.into(),
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
            },
            &ctx,
        ),
    };

    match watch.received() {
        Some(signal) => Err(CliError::Interrupted(signal)),
        None => result,
    }
}

#[cfg(test)]
mod tests {
    use super::open_tui;

    #[test]
    fn the_manager_opens_only_when_stdin_and_stdout_are_terminals() {
        assert!(open_tui(true, true));
        assert!(!open_tui(true, false), "output piped");
        assert!(!open_tui(false, true), "input piped");
        assert!(!open_tui(false, false));
    }
}
