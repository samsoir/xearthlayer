//! Package Manager CLI commands for installing and managing scenery packages.
//!
//! This module implements the Command Pattern with trait-based dependency
//! injection, providing a clean separation of concerns:
//!
//! - `traits`: Core interfaces (`Output`, `PackageManagerService`, `CommandHandler`)
//! - `services`: Concrete implementations of the traits
//! - `args`: CLI argument types and parsing (clap-derived)
//! - `handlers`: Command handlers implementing business logic
//!
//! # Architecture
//!
//! Each command handler:
//! - Implements the `CommandHandler` trait
//! - Depends only on trait interfaces via `CommandContext`
//! - Can be tested in isolation with mock implementations

mod args;
mod handlers;
mod services;
mod traits;

// Re-export public types
pub use args::PackagesCommands;
pub use handlers::{
    CheckHandler, InfoHandler, InstallHandler, ListHandler, RemoveHandler, UpdateHandler,
};
pub use services::{ConsoleInteraction, ConsoleOutput, DefaultPackageManagerService};
pub use traits::CommandHandler;

use std::env;
use std::path::PathBuf;

use args::{CheckArgs, InfoArgs, InstallArgs, ListArgs, RemoveArgs, UpdateArgs};
use traits::CommandContext;

use crate::error::CliError;

/// Get the default installation directory.
///
/// Uses the config file's scenery_dir if available, otherwise falls back to
/// a sensible default.
fn default_install_dir() -> PathBuf {
    // Try to load from config
    if let Ok(config) = xearthlayer::config::ConfigFile::load() {
        if let Some(ref scenery_dir) = config.xplane.scenery_dir {
            return scenery_dir.clone();
        }
    }

    // Fall back to current directory
    PathBuf::from(".")
}

/// Get the default temporary directory.
fn default_temp_dir() -> PathBuf {
    env::temp_dir().join("xearthlayer")
}

/// Run a packages subcommand.
///
/// This is the main entry point for packages commands. It creates the
/// production context with real implementations and dispatches to the
/// appropriate handler.
pub fn run(command: PackagesCommands) -> Result<(), CliError> {
    // Create production context
    let output = ConsoleOutput::new();
    let manager = DefaultPackageManagerService::new();
    let interaction = ConsoleInteraction::new();
    let ctx = CommandContext::new(&output, &manager, &interaction);

    // Dispatch to appropriate handler
    match command {
        PackagesCommands::List {
            install_dir,
            verbose,
        } => ListHandler::execute(
            ListArgs {
                install_dir: install_dir.unwrap_or_else(default_install_dir),
                verbose,
            },
            &ctx,
        ),

        PackagesCommands::Check {
            library_url,
            install_dir,
        } => CheckHandler::execute(
            CheckArgs {
                library_url,
                install_dir: install_dir.unwrap_or_else(default_install_dir),
            },
            &ctx,
        ),

        PackagesCommands::Install {
            region,
            r#type,
            library_url,
            install_dir,
            temp_dir,
        } => InstallHandler::execute(
            InstallArgs {
                region,
                package_type: r#type.into(),
                library_url,
                install_dir: install_dir.unwrap_or_else(default_install_dir),
                temp_dir: temp_dir.unwrap_or_else(default_temp_dir),
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
                library_url,
                install_dir: install_dir.unwrap_or_else(default_install_dir),
                temp_dir: temp_dir.unwrap_or_else(default_temp_dir),
                all,
            },
            &ctx,
        ),

        PackagesCommands::Remove {
            region,
            r#type,
            install_dir,
            force,
        } => RemoveHandler::execute(
            RemoveArgs {
                region,
                package_type: r#type.into(),
                install_dir: install_dir.unwrap_or_else(default_install_dir),
                force,
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
                install_dir: install_dir.unwrap_or_else(default_install_dir),
            },
            &ctx,
        ),
    }
}
