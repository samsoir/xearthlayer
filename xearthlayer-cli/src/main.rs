//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.
//!
//! # Architecture
//!
//! The CLI is organized into:
//! - `Cli` / `Commands`: Argument parsing (clap)
//! - `CliRunner`: Common setup (logging, service creation)
//! - `CliError`: Centralized error handling with user-friendly messages
//!
//! # Configuration
//!
//! Settings are loaded from `~/.xearthlayer/config.ini` on startup.
//! CLI arguments override config file values when specified.

mod commands;
mod error;
mod runner;

use clap::{Parser, Subcommand, ValueEnum};
use error::CliError;
use runner::CliRunner;
use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use xearthlayer::cache::{clear_disk_cache, disk_cache_stats};
use xearthlayer::config::{
    derive_mountpoint, detect_scenery_dir, format_size, ConfigFile, DownloadConfig,
    SceneryDetectionResult, TextureConfig,
};
use xearthlayer::dds::DdsFormat;
use xearthlayer::provider::ProviderConfig;
use xearthlayer::service::ServiceConfig;

// ============================================================================
// CLI Argument Definitions
// ============================================================================

#[derive(Debug, Clone, ValueEnum, PartialEq)]
enum ProviderType {
    /// Bing Maps aerial imagery (no API key required)
    Bing,
    /// Google Maps via public tile servers (no API key required, same as Ortho4XP GO2)
    Go2,
    /// Google Maps official API (requires API key, has usage limits)
    Google,
}

impl ProviderType {
    fn to_config(&self, api_key: Option<String>) -> Result<ProviderConfig, CliError> {
        match self {
            ProviderType::Bing => Ok(ProviderConfig::bing()),
            ProviderType::Go2 => Ok(ProviderConfig::go2()),
            ProviderType::Google => {
                let key = api_key.ok_or_else(|| {
                    CliError::Config(
                        "Google Maps provider requires an API key. \
                         Set google_api_key in config.ini or use --google-api-key"
                            .to_string(),
                    )
                })?;
                Ok(ProviderConfig::google(key))
            }
        }
    }

    /// Parse from config file string.
    fn from_config_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "bing" => Some(ProviderType::Bing),
            "go2" => Some(ProviderType::Go2),
            "google" => Some(ProviderType::Google),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum DdsCompression {
    /// BC1/DXT1 compression (4:1, best for opaque textures)
    Bc1,
    /// BC3/DXT5 compression (4:1, with full alpha channel)
    Bc3,
}

impl From<DdsCompression> for DdsFormat {
    fn from(compression: DdsCompression) -> Self {
        match compression {
            DdsCompression::Bc1 => DdsFormat::BC1,
            DdsCompression::Bc3 => DdsFormat::BC3,
        }
    }
}

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

    /// Cache management commands
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

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
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Clear the disk cache, removing all cached tiles
    Clear,
    /// Show disk cache statistics
    Stats,
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init => run_init(),
        Commands::Cache { action } => run_cache(action),
        Commands::Publish { command } => commands::publish::run(command),
        Commands::Packages { command } => commands::packages::run(command),
        Commands::Start {
            source,
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            timeout,
            parallel,
            no_cache,
        } => run_start(
            source,
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            timeout,
            parallel,
            no_cache,
        ),
        Commands::Download {
            lat,
            lon,
            zoom,
            output,
            dds_format,
            provider,
            google_api_key,
        } => run_download(lat, lon, zoom, output, dds_format, provider, google_api_key),
    };

    if let Err(e) = result {
        e.exit();
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

/// Prompt user to select from multiple X-Plane installations.
fn prompt_xplane_selection(paths: &[PathBuf]) -> Option<PathBuf> {
    println!("Multiple X-Plane 12 installations detected:");
    for (i, path) in paths.iter().enumerate() {
        println!("  [{}] {}", i + 1, path.display());
    }
    println!();
    print!(
        "Select installation (1-{}), or press Enter to skip: ",
        paths.len()
    );
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return None;
    }

    let input = input.trim();
    if input.is_empty() {
        println!("Skipped.");
        return None;
    }

    match input.parse::<usize>() {
        Ok(choice) if choice >= 1 && choice <= paths.len() => {
            let selected = paths[choice - 1].clone();
            println!("Selected: {}", selected.display());
            Some(selected)
        }
        _ => {
            println!("Invalid selection.");
            None
        }
    }
}

/// Initialize configuration file.
fn run_init() -> Result<(), CliError> {
    // Detect X-Plane scenery directory
    let scenery_dir = match detect_scenery_dir() {
        SceneryDetectionResult::NotFound => {
            println!("X-Plane 12 installation not detected.");
            println!("You can set scenery_dir manually in the config file.");
            println!();
            None
        }
        SceneryDetectionResult::Single(path) => {
            println!("Detected X-Plane 12 Custom Scenery:");
            println!("  {}", path.display());
            println!();
            Some(path)
        }
        SceneryDetectionResult::Multiple(paths) => {
            let selected = prompt_xplane_selection(&paths);
            println!();
            selected
        }
    };

    // Load existing config or create default, then update scenery_dir
    let mut config = ConfigFile::load().unwrap_or_default();
    if config.xplane.scenery_dir.is_none() {
        config.xplane.scenery_dir = scenery_dir;
    }
    config.save()?;

    let path = xearthlayer::config::config_file_path();
    println!("Configuration file: {}", path.display());
    println!();
    println!("Edit this file to customize XEarthLayer settings.");
    println!("CLI arguments override config file values when specified.");
    Ok(())
}

/// Handle cache management commands.
fn run_cache(action: CacheAction) -> Result<(), CliError> {
    let config = ConfigFile::load().unwrap_or_default();
    let cache_dir = &config.cache.directory;

    match action {
        CacheAction::Clear => {
            println!("Clearing disk cache at: {}", cache_dir.display());

            match clear_disk_cache(cache_dir) {
                Ok(result) => {
                    println!(
                        "Deleted {} files, freed {}",
                        result.files_deleted,
                        format_size(result.bytes_freed as usize)
                    );
                    Ok(())
                }
                Err(e) => Err(CliError::CacheClear(e.to_string())),
            }
        }
        CacheAction::Stats => {
            println!("Disk cache: {}", cache_dir.display());

            match disk_cache_stats(cache_dir) {
                Ok((files, bytes)) => {
                    println!("  Files: {}", files);
                    println!("  Size:  {}", format_size(bytes as usize));
                    Ok(())
                }
                Err(e) => Err(CliError::CacheStats(e.to_string())),
            }
        }
    }
}

/// Resolve provider settings from CLI args and config.
fn resolve_provider(
    cli_provider: Option<ProviderType>,
    cli_api_key: Option<String>,
    config: &ConfigFile,
) -> Result<ProviderConfig, CliError> {
    // CLI takes precedence, then config
    let provider = cli_provider
        .or_else(|| ProviderType::from_config_str(&config.provider.provider_type))
        .unwrap_or(ProviderType::Bing);

    let api_key = cli_api_key.or_else(|| config.provider.google_api_key.clone());

    provider.to_config(api_key)
}

/// Resolve DDS format from CLI args and config.
fn resolve_dds_format(cli_format: Option<DdsCompression>, config: &ConfigFile) -> DdsFormat {
    cli_format
        .map(DdsFormat::from)
        .unwrap_or(config.texture.format)
}

#[allow(clippy::too_many_arguments)]
fn run_start(
    source: String,
    mountpoint: Option<String>,
    provider: Option<ProviderType>,
    google_api_key: Option<String>,
    dds_format: Option<DdsCompression>,
    timeout: Option<u64>,
    parallel: Option<usize>,
    no_cache: bool,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("start");
    let config = runner.config();

    // Determine mountpoint: CLI > config > auto-detect
    let mountpoint = match mountpoint {
        Some(mp) => mp,
        None => {
            // Try config scenery_dir first
            if let Some(ref scenery_dir) = config.xplane.scenery_dir {
                let source_path = Path::new(&source);
                let pack_name = source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "scenery".to_string());
                scenery_dir.join(&pack_name).to_string_lossy().to_string()
            } else {
                // Fall back to auto-detection
                let source_path = Path::new(&source);
                match derive_mountpoint(source_path) {
                    Ok(mp) => {
                        println!("Auto-detected X-Plane 12 Custom Scenery directory");
                        mp.to_string_lossy().to_string()
                    }
                    Err(e) => {
                        return Err(CliError::Config(format!(
                            "Could not determine mountpoint: {}. \
                             Set scenery_dir in config.ini or use --mountpoint.",
                            e
                        )));
                    }
                }
            }
        }
    };

    // Resolve settings from CLI and config
    let provider_config = resolve_provider(provider, google_api_key, config)?;
    let format = resolve_dds_format(dds_format, config);
    let timeout_secs = timeout.unwrap_or(config.download.timeout);
    let parallel_downloads = parallel.unwrap_or(config.download.parallel);

    // Build configurations
    let texture_config = TextureConfig::new(format).with_mipmap_count(5);

    let download_config = DownloadConfig::new()
        .with_timeout_secs(timeout_secs)
        .with_max_retries(3)
        .with_parallel_downloads(parallel_downloads);

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!no_cache)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
        .build();

    // Print banner
    println!("XEarthLayer Passthrough Mount v{}", xearthlayer::VERSION);
    println!("================================");
    println!();
    println!("Source:     {}", source);
    println!("Mountpoint: {}", mountpoint);
    println!("DDS Format: {:?}", texture_config.format());
    println!();

    let service = runner.create_service(service_config, &provider_config)?;

    // Print service info
    if service.cache_enabled() {
        println!(
            "Cache: Enabled ({} memory, {} disk)",
            xearthlayer::config::format_size(config.cache.memory_size),
            xearthlayer::config::format_size(config.cache.disk_size)
        );
    } else {
        println!("Cache: Disabled (all tiles generated fresh)");
    }
    println!("Provider: {}", service.provider_name());
    println!();

    println!("Mounting passthrough filesystem...");
    println!("  Real files: Passed through from source");
    println!("  DDS files:  Generated on-demand");
    println!();
    println!("Press Ctrl+C to unmount and exit");
    println!();

    // Set up signal handler for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    ctrlc::set_handler(move || {
        println!();
        println!("Received shutdown signal, unmounting...");
        shutdown_clone.store(true, Ordering::SeqCst);
    })
    .map_err(|e| CliError::Config(format!("Failed to set signal handler: {}", e)))?;

    // Start serving with passthrough in background
    // The BackgroundSession auto-unmounts when dropped
    let _session = service
        .serve_passthrough_background(&source, &mountpoint)
        .map_err(CliError::Serve)?;

    // Wait for shutdown signal
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Session drops here, triggering unmount
    println!("Filesystem unmounted.");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_download(
    lat: f64,
    lon: f64,
    zoom: u8,
    output: String,
    dds_format: Option<DdsCompression>,
    provider: Option<ProviderType>,
    google_api_key: Option<String>,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("download");
    let config = runner.config();

    // Resolve settings from CLI and config
    let provider_config = resolve_provider(provider, google_api_key, config)?;
    let format = resolve_dds_format(dds_format, config);

    // Build configurations
    let texture_config = TextureConfig::new(format).with_mipmap_count(5);

    // Caching disabled for single downloads
    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(DownloadConfig::default())
        .cache_enabled(false)
        .build();

    println!("Downloading tile for:");
    println!("  Location: {}, {}", lat, lon);
    println!("  Zoom: {}", zoom);
    println!();

    let service = runner.create_service(service_config, &provider_config)?;
    println!("Using provider: {}", service.provider_name());

    // Download tile
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let dds_data = service
        .download_tile(lat, lon, zoom)
        .map_err(CliError::from)?;

    let elapsed = start.elapsed();
    println!("Downloaded successfully in {:.2}s", elapsed.as_secs_f64());
    println!();

    // Save to file
    runner.save_dds(&output, &dds_data, &texture_config)?;

    Ok(())
}
