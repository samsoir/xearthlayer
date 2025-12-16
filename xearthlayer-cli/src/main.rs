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
mod ui;

use clap::{Parser, Subcommand};
use commands::cache::CacheAction;
use commands::common::{DdsCompression, ProviderType};

// ============================================================================
// CLI Argument Definitions
// ============================================================================

#[derive(Parser)]
#[command(name = "xearthlayer")]
#[command(version = xearthlayer::VERSION)]
#[command(about = "Satellite imagery streaming for X-Plane", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize configuration file at ~/.xearthlayer/config.ini
    Init,

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

    /// Start XEarthLayer with a scenery pack (passthrough for real files, on-demand DDS generation)
    Start {
        /// Source scenery pack directory to overlay
        #[arg(long)]
        source: String,

        /// FUSE mountpoint directory (default: from config or auto-detect)
        #[arg(long)]
        mountpoint: Option<String>,

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
    },

    /// Download a single tile to a file (for testing)
    Download {
        /// Latitude in decimal degrees
        #[arg(long)]
        lat: f64,

        /// Longitude in decimal degrees
        #[arg(long)]
        lon: f64,

        /// Zoom level (12-19 for Bing, 12-22 for Google)
        #[arg(long, default_value = "16")]
        zoom: u8,

        /// Output file path (.dds extension)
        #[arg(long)]
        output: String,

        /// DDS compression format (default: from config)
        #[arg(long, value_enum)]
        dds_format: Option<DdsCompression>,

        /// Imagery provider (default: from config)
        #[arg(long, value_enum)]
        provider: Option<ProviderType>,

        /// Google Maps API key (default: from config)
        #[arg(long)]
        google_api_key: Option<String>,

        /// MapBox access token (default: from config)
        #[arg(long)]
        mapbox_token: Option<String>,
    },
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init => commands::init::run(),
        Commands::Config { command } => commands::config::run(command),
        Commands::Cache { action } => commands::cache::run(action),
        Commands::Diagnostics => commands::diagnostics::run(),
        Commands::Publish { command } => commands::publish::run(command),
        Commands::Packages { command } => commands::packages::run(command),
        Commands::Start {
            source,
            mountpoint,
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
        } => commands::start::run(commands::start::StartArgs {
            source,
            mountpoint,
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
        }),
        Commands::Download {
            lat,
            lon,
            zoom,
            output,
            dds_format,
            provider,
            google_api_key,
            mapbox_token,
        } => commands::download::run(commands::download::DownloadArgs {
            lat,
            lon,
            zoom,
            output,
            dds_format,
            provider,
            google_api_key,
            mapbox_token,
        }),
        Commands::Run {
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
        } => commands::run::run(commands::run::RunArgs {
            provider,
            google_api_key,
            mapbox_token,
            dds_format,
            timeout,
            parallel,
            no_cache,
        }),
    };

    if let Err(e) = result {
        e.exit();
    }
}
