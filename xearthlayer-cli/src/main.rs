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

mod error;
mod runner;

use clap::{Parser, Subcommand, ValueEnum};
use error::CliError;
use runner::CliRunner;
use std::path::Path;
use xearthlayer::config::{derive_mountpoint, DownloadConfig, TextureConfig};
use xearthlayer::dds::DdsFormat;
use xearthlayer::provider::ProviderConfig;
use xearthlayer::service::ServiceConfig;

// ============================================================================
// CLI Argument Definitions
// ============================================================================

#[derive(Debug, Clone, ValueEnum)]
enum ProviderType {
    /// Bing Maps aerial imagery (no API key required)
    Bing,
    /// Google Maps satellite imagery (requires API key)
    Google,
}

impl ProviderType {
    fn to_config(&self, api_key: Option<String>) -> ProviderConfig {
        match self {
            ProviderType::Bing => ProviderConfig::bing(),
            ProviderType::Google => {
                // Safe: clap's required_if_eq ensures API key is present for Google
                ProviderConfig::google(api_key.unwrap())
            }
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
    /// Mount a scenery pack with passthrough for real files and on-demand DDS generation
    Mount {
        /// Source scenery pack directory to overlay
        #[arg(long)]
        source: String,

        /// FUSE mountpoint directory. If not specified, automatically detects
        /// X-Plane 12 Custom Scenery folder and uses the source pack name.
        #[arg(long)]
        mountpoint: Option<String>,

        /// Imagery provider to use
        #[arg(long, value_enum, default_value = "bing")]
        provider: ProviderType,

        /// Google Maps API key (required when using --provider google)
        #[arg(long, required_if_eq("provider", "google"))]
        google_api_key: Option<String>,

        /// DDS compression format (BC1 or BC3)
        #[arg(long, value_enum, default_value = "bc1")]
        dds_format: DdsCompression,

        /// Number of mipmap levels for DDS (default: 5 for 4096→256)
        #[arg(long, default_value = "5")]
        mipmap_count: usize,

        /// Download timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,

        /// Number of retry attempts per chunk
        #[arg(long, default_value = "3")]
        retries: usize,

        /// Maximum parallel downloads
        #[arg(long, default_value = "32")]
        parallel: usize,

        /// Disable caching (always generate tiles fresh - useful for testing)
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

        /// DDS compression format (BC1 or BC3)
        #[arg(long, value_enum, default_value = "bc1")]
        dds_format: DdsCompression,

        /// Number of mipmap levels for DDS (default: 5 for 4096→256)
        #[arg(long, default_value = "5")]
        mipmap_count: usize,

        /// Imagery provider to use
        #[arg(long, value_enum, default_value = "bing")]
        provider: ProviderType,

        /// Google Maps API key (required when using --provider google)
        #[arg(long, required_if_eq("provider", "google"))]
        google_api_key: Option<String>,
    },

    /// Start FUSE server for on-demand texture generation
    Serve {
        /// FUSE mountpoint directory
        #[arg(long)]
        mountpoint: String,

        /// Imagery provider to use
        #[arg(long, value_enum, default_value = "bing")]
        provider: ProviderType,

        /// Google Maps API key (required when using --provider google)
        #[arg(long, required_if_eq("provider", "google"))]
        google_api_key: Option<String>,

        /// DDS compression format (BC1 or BC3)
        #[arg(long, value_enum, default_value = "bc1")]
        dds_format: DdsCompression,

        /// Number of mipmap levels for DDS (default: 5 for 4096→256)
        #[arg(long, default_value = "5")]
        mipmap_count: usize,

        /// Download timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,

        /// Number of retry attempts per chunk
        #[arg(long, default_value = "3")]
        retries: usize,

        /// Maximum parallel downloads
        #[arg(long, default_value = "32")]
        parallel: usize,

        /// Disable caching (always generate tiles fresh - useful for testing)
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
        Commands::Mount {
            source,
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            mipmap_count,
            timeout,
            retries,
            parallel,
            no_cache,
        } => run_mount(
            source,
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            mipmap_count,
            timeout,
            retries,
            parallel,
            no_cache,
        ),
        Commands::Download {
            lat,
            lon,
            zoom,
            output,
            dds_format,
            mipmap_count,
            provider,
            google_api_key,
        } => run_download(
            lat,
            lon,
            zoom,
            output,
            dds_format,
            mipmap_count,
            provider,
            google_api_key,
        ),
        Commands::Serve {
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            mipmap_count,
            timeout,
            retries,
            parallel,
            no_cache,
        } => run_serve(
            mountpoint,
            provider,
            google_api_key,
            dds_format,
            mipmap_count,
            timeout,
            retries,
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

#[allow(clippy::too_many_arguments)]
fn run_mount(
    source: String,
    mountpoint: Option<String>,
    provider: ProviderType,
    google_api_key: Option<String>,
    dds_format: DdsCompression,
    mipmap_count: usize,
    timeout: u64,
    retries: usize,
    parallel: usize,
    no_cache: bool,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("mount");

    // Determine mountpoint: use provided value or derive from X-Plane installation
    let mountpoint = match mountpoint {
        Some(mp) => mp,
        None => {
            // Try to derive from X-Plane installation
            let source_path = Path::new(&source);
            match derive_mountpoint(source_path) {
                Ok(mp) => {
                    println!("Auto-detected X-Plane 12 Custom Scenery directory");
                    mp.to_string_lossy().to_string()
                }
                Err(e) => {
                    return Err(CliError::Config(format!(
                        "Could not determine mountpoint: {}. \
                         Please specify --mountpoint explicitly or ensure X-Plane 12 is installed.",
                        e
                    )));
                }
            }
        }
    };

    // Build configurations
    let texture_config = TextureConfig::new(dds_format.into()).with_mipmap_count(mipmap_count);
    let provider_config = provider.to_config(google_api_key);

    let download_config = DownloadConfig::new()
        .with_timeout_secs(timeout)
        .with_max_retries(retries as u32)
        .with_parallel_downloads(parallel);

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!no_cache)
        .build();

    // Print banner
    println!("XEarthLayer Passthrough Mount v{}", xearthlayer::VERSION);
    println!("================================");
    println!();
    println!("Source:     {}", source);
    println!("Mountpoint: {}", mountpoint);
    println!("DDS Format: {:?}", texture_config.format());
    println!("Mipmap Levels: {}", texture_config.mipmap_count());
    println!();

    let service = runner.create_service(service_config, &provider_config)?;

    // Print service info
    if service.cache_enabled() {
        println!("Cache: Enabled (2GB memory, 20GB disk at ~/.cache/xearthlayer)");
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
    dds_format: DdsCompression,
    mipmap_count: usize,
    provider: ProviderType,
    google_api_key: Option<String>,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("download");

    // Build configurations
    let texture_config = TextureConfig::new(dds_format.into()).with_mipmap_count(mipmap_count);
    let provider_config = provider.to_config(google_api_key);

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
    provider: ProviderType,
    google_api_key: Option<String>,
    dds_format: DdsCompression,
    mipmap_count: usize,
    timeout: u64,
    retries: usize,
    parallel: usize,
    no_cache: bool,
) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("serve");

    // Build configurations
    let texture_config = TextureConfig::new(dds_format.into()).with_mipmap_count(mipmap_count);
    let provider_config = provider.to_config(google_api_key);

    let download_config = DownloadConfig::new()
        .with_timeout_secs(timeout)
        .with_max_retries(retries as u32)
        .with_parallel_downloads(parallel);

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!no_cache)
        .mountpoint(&mountpoint)
        .build();

    // Print banner
    println!("XEarthLayer FUSE Server v{}", xearthlayer::VERSION);
    println!("=======================");
    println!();
    println!("Mountpoint: {}", mountpoint);
    println!("DDS Format: {:?}", texture_config.format());
    println!("Mipmap Levels: {}", texture_config.mipmap_count());
    println!();

    let service = runner.create_service(service_config, &provider_config)?;

    // Print service info
    if service.cache_enabled() {
        println!("Cache: Enabled (2GB memory, 20GB disk at ~/.cache/xearthlayer)");
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
