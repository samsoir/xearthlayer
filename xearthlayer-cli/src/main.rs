//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.
//!
//! # Architecture
//!
//! The CLI is organized into:
//! - `Cli` / `Commands`: Argument parsing (clap)
//! - `commands/*`: Individual command implementations
//! - `runner::CliRunner`: Common setup (logging, service creation)
//! - `error::CliError`: Centralized error handling with user-friendly messages
//!
//! # Configuration
//!
//! Settings are loaded from `~/.xearthlayer/config.ini` on startup.
//! CLI arguments override config file values when specified.

mod commands;
mod error;
mod runner;
mod tui_app;
mod ui;

use clap::{Parser, Subcommand};
use commands::cache::CacheAction;
use commands::common::{DdsCompression, ProviderType};
use commands::scenery_index::SceneryIndexAction;

// ============================================================================
// CLI Argument Definitions
// ============================================================================

#[derive(Parser)]
#[command(name = "xearthlayer")]
#[command(version = xearthlayer::VERSION)]
#[command(about = "Satellite imagery streaming for X-Plane", long_about = None)]
struct Cli {
    /// Subcommand to run. If omitted, defaults to 'run'.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize configuration file at ~/.xearthlayer/config.ini
    Init,

    /// Interactive setup wizard for first-time configuration
    ///
    /// Guides you through configuring XEarthLayer for your system.
    /// Detects X-Plane installation, system hardware, and recommends
    /// optimal settings based on your CPU, memory, and storage.
    Setup,

    /// Get or set configuration values
    Config {
        #[command(subcommand)]
        command: commands::config::ConfigCommands,
    },

    /// Cache management commands
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Scenery index cache management commands
    #[command(name = "scenery-index")]
    SceneryIndex {
        #[command(subcommand)]
        action: SceneryIndexAction,
    },

    /// Output system diagnostics for bug reports
    Diagnostics,

    /// Package publisher commands (create and manage scenery packages)
    Publish {
        #[command(subcommand)]
        command: commands::publish::PublishCommands,
    },

    /// Package manager commands (install and manage scenery packages)
    Packages {
        #[command(subcommand)]
        command: commands::packages::PackagesCommands,
    },

    /// Tile patches management commands (custom mesh/elevation tiles)
    ///
    /// Patches are pre-built Ortho4XP tiles with custom mesh/elevation data
    /// from airport addons. XEL generates textures dynamically for these tiles.
    Patches {
        #[command(subcommand)]
        command: commands::patches::PatchesCommands,
    },

    /// Start XEarthLayer and mount all installed ortho packages for X-Plane
    ///
    /// This is the main command for running XEarthLayer. It discovers all installed
    /// ortho packages and mounts them as FUSE filesystems in your X-Plane Custom Scenery
    /// directory. DDS textures are generated on-demand when X-Plane requests them.
    Run {
        /// Imagery provider (default: from config)
        #[arg(long, value_enum)]
        provider: Option<ProviderType>,

        /// Google Maps API key (default: from config)
        #[arg(long)]
        google_api_key: Option<String>,

        /// MapBox access token (default: from config)
        #[arg(long)]
        mapbox_token: Option<String>,

        /// DDS compression format (default: from config)
        #[arg(long, value_enum)]
        dds_format: Option<DdsCompression>,

        /// Download timeout in seconds (default: from config)
        #[arg(long)]
        timeout: Option<u64>,

        /// Maximum parallel downloads (default: from config)
        #[arg(long)]
        parallel: Option<usize>,

        /// Disable caching (always generate tiles fresh)
        #[arg(long)]
        no_cache: bool,

        /// Enable debug-level logging for troubleshooting
        #[arg(long)]
        debug: bool,

        /// Disable predictive tile prefetching
        #[arg(long)]
        no_prefetch: bool,

        /// ICAO airport code for cold-start pre-warming (e.g., LFBO, KJFK)
        ///
        /// When specified, pre-loads tiles around the airport before starting.
        /// Useful for pre-warming the cache before a flight.
        #[arg(long)]
        airport: Option<String>,
    },
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        // Default to 'run' when no subcommand is provided
        None => commands::run::run(commands::run::RunArgs::default()),

        Some(Commands::Init) => commands::init::run(),
        Some(Commands::Setup) => commands::setup::run(),
        Some(Commands::Config { command }) => commands::config::run(command),
        Some(Commands::Cache { action }) => commands::cache::run(action),
        Some(Commands::SceneryIndex { action }) => commands::scenery_index::run(action),
        Some(Commands::Diagnostics) => commands::diagnostics::run(),
        Some(Commands::Publish { command }) => commands::publish::run(command),
        Some(Commands::Packages { command }) => commands::packages::run(command),
        Some(Commands::Patches { command }) => commands::patches::run(command),
        Some(Commands::Run {
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
            debug,
            no_prefetch,
            airport,
        }) => commands::run::run(commands::run::RunArgs {
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
            debug,
            no_prefetch,
            airport,
        }),
    };

    if let Err(e) = result {
        e.exit();
    }
}
