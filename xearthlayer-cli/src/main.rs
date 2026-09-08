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
mod logging_init;
mod preflight;
mod runner;
mod tui_app;
mod ui;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use commands::cache::CacheAction;
use commands::common::{DdsCompression, ProviderType};
use commands::scenery_index::SceneryIndexAction;
use logging_init::init_cli_logging;
use xearthlayer::config::ConfigFile;

// ============================================================================
// CLI Argument Definitions
// ============================================================================

#[derive(Parser)]
#[command(name = "xearthlayer")]
#[command(version = xearthlayer::VERSION)]
#[command(about = "Satellite imagery streaming for X-Plane", long_about = None)]
struct Cli {
    /// Enable debug-level logging for the xearthlayer crate.
    ///
    /// Applies to every subcommand. Equivalent to setting
    /// `RUST_LOG=info,xearthlayer=debug`. Use this when filing bug reports
    /// or debugging download / cache issues.
    #[arg(long, global = true)]
    debug: bool,

    /// Enable Chrome Trace profiling output (writes trace to logs/).
    ///
    /// Applies to every subcommand. Requires the `profiling` feature.
    #[arg(long, global = true)]
    profile: bool,

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

    /// Move configuration and resources between layouts
    Migrate {
        #[command(subcommand)]
        action: Option<commands::migrate::MigrateAction>,

        /// Show what would change without changing anything
        #[arg(long)]
        dry_run: bool,
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

/// The command being run, as the preflight checks name it.
///
/// Exhaustive on purpose: a new subcommand must decide what it is called here
/// before it compiles, rather than silently inheriting another command's
/// prerequisites.
fn command_name(command: &Option<Commands>) -> &'static str {
    match command {
        // No subcommand defaults to `run`, so it gets `run`'s prerequisites.
        None | Some(Commands::Run { .. }) => "run",
        Some(Commands::Init) => "init",
        Some(Commands::Setup) => "setup",
        Some(Commands::Config { .. }) => "config",
        Some(Commands::Cache { .. }) => "cache",
        Some(Commands::Migrate { .. }) => "migrate",
        Some(Commands::SceneryIndex { .. }) => "scenery-index",
        Some(Commands::Diagnostics) => "diagnostics",
        Some(Commands::Publish { .. }) => "publish",
        Some(Commands::Packages { .. }) => "packages",
        Some(Commands::Patches { .. }) => "patches",
    }
}

/// The airport requested on the command line, if any.
fn requested_airport(command: &Option<Commands>) -> Option<String> {
    match command {
        Some(Commands::Run { airport, .. }) => airport.clone(),
        _ => None,
    }
}

fn main() -> ExitCode {
    // First, before anything allocates in earnest, and before any thread is
    // created — the arena ceiling only bounds arenas not yet made.
    //
    // Two glibc parameters, both from issue #227:
    //
    // - mmap threshold: glibc adapts it upward past the 10.66 MiB DDS tile
    //   size after a single allocate/free cycle, after which tiles come from
    //   arenas and are never returned on free. Pinning costs ~0.16 ms per tile
    //   and bounded arena retention at 4.6 MB against 191.5 MB in a 64-thread
    //   benchmark.
    // - arena ceiling: glibc's default of 8 x ncores lets every burst recruit
    //   fresh arenas, each keeping its own high-water mark forever. An
    //   11-hour flight reached a 6,091 MB arena holding 1,002 MB of live data,
    //   still growing at hour 10.75. Capped, the same route settled at
    //   3,916 MB within six minutes and held to the byte.
    //
    // Either can be overridden with MALLOC_MMAP_THRESHOLD_ / MALLOC_ARENA_MAX
    // or GLIBC_TUNABLES; an explicit user setting always wins.
    xearthlayer::metrics::configure_allocator();

    let cli = Cli::parse();

    // Initialize logging once for every subcommand. Falls back to default
    // config (and therefore the default log path) if the user's config
    // file is missing or unparseable; that keeps first-run scenarios and
    // misconfigured installs from running with no diagnostic output. (#194)
    let config_for_logging = ConfigFile::load().unwrap_or_default();
    let _logging_guard = match init_cli_logging(&config_for_logging, cli.debug, cli.profile) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!(
                "warning: failed to initialize logging ({}); continuing without log file",
                e
            );
            None
        }
    };

    // Prerequisites run before dispatch, not inside `run`. Both `run` and the
    // setup wizard independently answer "does this installation exist" from the
    // same state, so a check that lived in `run` alone would leave `setup`
    // treating an existing user as new and discarding their settings.
    let mut ctx = xearthlayer::preflight::BootstrapContext::new(
        command_name(&cli.command),
        requested_airport(&cli.command),
    );

    let result = preflight::enforce(&mut ctx).and_then(|()| match cli.command {
        // Default to 'run' when no subcommand is provided
        None => commands::run::run(commands::run::RunArgs::default(), &ctx),

        Some(Commands::Init) => commands::init::run(),
        Some(Commands::Setup) => commands::setup::run(),
        Some(Commands::Config { command }) => commands::config::run(command),
        Some(Commands::Cache { action }) => commands::cache::run(action),
        Some(Commands::Migrate { action, dry_run }) => commands::migrate::run(action, dry_run),
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
            no_prefetch,
            airport,
        }) => commands::run::run(
            commands::run::RunArgs {
                provider,
                google_api_key,
                mapbox_token,
                dds_format,
                timeout,
                parallel,
                no_cache,
                no_prefetch,
                airport,
            },
            &ctx,
        ),
    });

    // Returning ExitCode (rather than calling process::exit) lets the
    // _logging_guard drop normally, which forces tracing-appender's
    // background writer to flush. Calling process::exit here would
    // truncate the log on every failed run — exactly the symptom #194
    // was filed to prevent. See CliError::report.
    let exit_code = match result {
        Ok(()) => 0u8,
        Err(e) => e.report(),
    };
    ExitCode::from(exit_code)
}
