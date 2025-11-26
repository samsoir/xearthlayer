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

mod error;
mod runner;

use clap::{Parser, Subcommand, ValueEnum};
use error::CliError;
use runner::CliRunner;
use std::path::Path;
use xearthlayer::config::{derive_mountpoint, ConfigFile, DownloadConfig, TextureConfig};
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
    /// Google Maps satellite imagery (requires API key)
    Google,
}

impl ProviderType {
    fn to_config(&self, api_key: Option<String>) -> Result<ProviderConfig, CliError> {
        match self {
            ProviderType::Bing => Ok(ProviderConfig::bing()),
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

    /// Mount a scenery pack with passthrough for real files and on-demand DDS generation
    Mount {
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

    /// Download a single tile to a file
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

    /// Start FUSE server for on-demand texture generation
    Serve {
        /// FUSE mountpoint directory
        #[arg(long)]
        mountpoint: String,

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
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init => run_init(),
        Commands::Mount {
            source,
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            timeout,
            parallel,
            no_cache,
        } => run_mount(
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
        Commands::Serve {
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            timeout,
            parallel,
            no_cache,
        } => run_serve(
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            timeout,
            parallel,
            no_cache,
        ),
    };

    if let Err(e) = result {
        e.exit();
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

/// Initialize configuration file.
fn run_init() -> Result<(), CliError> {
    let path = ConfigFile::ensure_exists()?;
    println!("Configuration file: {}", path.display());
    println!();
    println!("Edit this file to customize XEarthLayer settings.");
    println!("CLI arguments override config file values when specified.");
    Ok(())
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
fn run_mount(
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
    runner.log_startup("mount");
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

    // Start serving with passthrough (blocks until unmount)
    service
        .serve_passthrough(&source, &mountpoint)
        .map_err(CliError::Serve)?;

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

#[allow(clippy::too_many_arguments)]
fn run_serve(
    mountpoint: String,
    provider: Option<ProviderType>,
    google_api_key: Option<String>,
    dds_format: Option<DdsCompression>,
    timeout: Option<u64>,
    parallel: Option<usize>,
    no_cache: bool,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("serve");
    let config = runner.config();

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
        .mountpoint(&mountpoint)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
        .build();

    // Print banner
    println!("XEarthLayer FUSE Server v{}", xearthlayer::VERSION);
    println!("=======================");
    println!();
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

    println!("Mounting filesystem...");
    println!("Press Ctrl+C to unmount and exit");
    println!();

    // Start serving (blocks until unmount)
    service.serve().map_err(CliError::Serve)?;

    println!("Filesystem unmounted.");
    Ok(())
}
