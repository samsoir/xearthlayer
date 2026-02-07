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
use xearthlayer::config::ConfigFile;

use crate::error::CliError;

/// Load config or return default.
fn load_config() -> ConfigFile {
    ConfigFile::load().unwrap_or_default()
}

/// Get the default installation directory.
///
/// Priority:
/// 1. packages.install_location from config
/// 2. ~/.xearthlayer/packages (default)
fn default_install_dir(config: &ConfigFile) -> PathBuf {
    config.packages.install_location.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".xearthlayer")
            .join("packages")
    })
}

/// Get the Custom Scenery directory for overlay symlinks.
///
/// Priority:
/// 1. packages.custom_scenery_path from config
/// 2. xplane.scenery_dir from config
/// 3. Auto-detect from X-Plane installation
fn default_custom_scenery_dir(config: &ConfigFile) -> Option<PathBuf> {
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
/// system temp directory.
fn default_temp_dir(config: &ConfigFile) -> PathBuf {
    config
        .packages
        .temp_dir
        .clone()
        .unwrap_or_else(|| env::temp_dir().join("xearthlayer"))
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

/// Run a packages subcommand.
///
/// This is the main entry point for packages commands. It creates the
/// production context with real implementations and dispatches to the
/// appropriate handler.
pub fn run(command: PackagesCommands) -> Result<(), CliError> {
    // Load config once for all commands
    let config = load_config();

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
            region,
            r#type,
            library_url,
            install_dir,
            temp_dir,
        } => InstallHandler::execute(
            InstallArgs {
                region,
                package_type: r#type.into(),
                library_url: require_library_url(library_url, &config)?,
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                temp_dir: temp_dir.unwrap_or_else(|| default_temp_dir(&config)),
                custom_scenery_path: default_custom_scenery_dir(&config),
                auto_install_overlays: config.packages.auto_install_overlays,
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
                install_dir: install_dir.unwrap_or_else(|| default_install_dir(&config)),
                force,
                custom_scenery_path: default_custom_scenery_dir(&config),
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
    }
}
