//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.

use clap::{Parser, Subcommand, ValueEnum};
use std::process;
use tracing::{error, info};
use xearthlayer::config::{DownloadConfig, TextureConfig};
use xearthlayer::dds::DdsFormat;
use xearthlayer::logging::{default_log_dir, default_log_file, init_logging};
use xearthlayer::provider::ProviderConfig;
use xearthlayer::service::{ServiceConfig, XEarthLayerService};

#[derive(Debug, Clone, ValueEnum)]
enum ProviderType {
    /// Bing Maps aerial imagery (no API key required)
    Bing,
    /// Google Maps satellite imagery (requires API key)
    Google,
}

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    /// JPEG image format (90% quality)
    Jpeg,
    /// DDS texture format for X-Plane
    Dds,
}

#[derive(Debug, Clone, ValueEnum)]
enum DdsCompressionFormat {
    /// BC1/DXT1 compression (4:1, best for opaque textures)
    Bc1,
    /// BC3/DXT5 compression (4:1, with full alpha channel)
    Bc3,
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
    /// Download a single tile to a file
    Download {
        /// Latitude in decimal degrees
        #[arg(long)]
        lat: f64,

        /// Longitude in decimal degrees
        #[arg(long)]
        lon: f64,

        /// Zoom level (max: 15 for Bing, 18 for Google)
        #[arg(long, default_value = "15")]
        zoom: u8,

        /// Output file path (auto-detects format from extension: .jpg/.dds)
        #[arg(long)]
        output: String,

        /// Output format (auto-detected from file extension if not specified)
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,

        /// DDS compression format (BC1 or BC3)
        #[arg(long, value_enum, default_value = "bc1")]
        dds_format: DdsCompressionFormat,

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
        dds_format: DdsCompressionFormat,

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

/// Convert CLI provider type and optional API key to ProviderConfig.
fn to_provider_config(provider: &ProviderType, api_key: Option<String>) -> ProviderConfig {
    match provider {
        ProviderType::Bing => ProviderConfig::bing(),
        ProviderType::Google => {
            // Safe: clap's required_if_eq ensures API key is present for Google
            ProviderConfig::google(api_key.unwrap())
        }
    }
}

/// Convert CLI DDS format to library DdsFormat.
fn to_dds_format(format: &DdsCompressionFormat) -> DdsFormat {
    match format {
        DdsCompressionFormat::Bc1 => DdsFormat::BC1,
        DdsCompressionFormat::Bc3 => DdsFormat::BC3,
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Download {
            lat,
            lon,
            zoom,
            output,
            format,
            dds_format,
            mipmap_count,
            provider,
            google_api_key,
        } => {
            // Build configurations
            let texture_config =
                TextureConfig::new(to_dds_format(&dds_format)).with_mipmap_count(mipmap_count);
            let provider_config = to_provider_config(&provider, google_api_key);

            handle_download(
                lat,
                lon,
                zoom,
                &output,
                &format,
                texture_config,
                provider_config,
            );
        }
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
        } => {
            // Build configurations
            let texture_config =
                TextureConfig::new(to_dds_format(&dds_format)).with_mipmap_count(mipmap_count);

            let download_config = DownloadConfig::new()
                .with_timeout_secs(timeout)
                .with_max_retries(retries as u32)
                .with_parallel_downloads(parallel);

            let provider_config = to_provider_config(&provider, google_api_key);

            handle_serve(
                &mountpoint,
                provider_config,
                texture_config,
                download_config,
                no_cache,
            );
        }
    }
}

fn handle_download(
    lat: f64,
    lon: f64,
    zoom: u8,
    output: &str,
    format: &Option<OutputFormat>,
    texture_config: TextureConfig,
    provider_config: ProviderConfig,
) {
    // Initialize logging
    let _logging_guard = match init_logging(default_log_dir(), default_log_file()) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("Failed to initialize logging: {}", e);
            process::exit(1);
        }
    };

    info!("XEarthLayer v{}", xearthlayer::VERSION);
    info!("XEarthLayer CLI: download command");

    println!("Downloading tile for:");
    println!("  Location: {}, {}", lat, lon);
    println!("  Zoom: {}", zoom);
    println!();

    // Log provider type
    if provider_config.requires_api_key() {
        info!("Creating {} session...", provider_config.name());
        println!("Creating {} session...", provider_config.name());
    }

    // Build service configuration (caching disabled for single download)
    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(DownloadConfig::default())
        .cache_enabled(false)
        .build();

    // Create service
    let service = match XEarthLayerService::new(service_config, provider_config.clone()) {
        Ok(s) => {
            info!("Service created successfully");
            s
        }
        Err(e) => {
            error!("Failed to create service: {}", e);
            eprintln!("Error creating service: {}", e);
            if provider_config.requires_api_key() {
                eprintln!("Make sure:");
                eprintln!("  1. Map Tiles API is enabled in Google Cloud Console");
                eprintln!("  2. Billing is enabled for your project");
                eprintln!("  3. Your API key is valid and unrestricted");
            }
            process::exit(1);
        }
    };

    info!("Using provider: {}", service.provider_name());
    println!("Using provider: {}", service.provider_name());

    // Download tile using service
    info!("Starting tile download");
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let dds_data = match service.download_tile(lat, lon, zoom) {
        Ok(data) => {
            info!(
                "Tile downloaded and encoded successfully: {} bytes",
                data.len()
            );
            data
        }
        Err(e) => {
            error!("Failed to download tile: {}", e);
            eprintln!("Error downloading tile: {}", e);
            process::exit(1);
        }
    };

    let elapsed = start.elapsed();
    info!("Download completed in {:.2}s", elapsed.as_secs_f64());
    println!("Downloaded successfully in {:.2}s", elapsed.as_secs_f64());
    println!();

    // Determine output format and save
    let output_format = match format {
        Some(f) => f.clone(),
        None => {
            if output.to_lowercase().ends_with(".dds") {
                OutputFormat::Dds
            } else {
                OutputFormat::Jpeg
            }
        }
    };

    match output_format {
        OutputFormat::Jpeg => {
            // For JPEG, we need to decode the DDS and re-encode as JPEG
            // This is less efficient but maintains the simpler service API
            save_dds_as_jpeg(&dds_data, output);
        }
        OutputFormat::Dds => {
            save_dds_data(&dds_data, output, texture_config);
        }
    }
}

fn handle_serve(
    mountpoint: &str,
    provider_config: ProviderConfig,
    texture_config: TextureConfig,
    download_config: DownloadConfig,
    no_cache: bool,
) {
    // Initialize logging
    let _logging_guard = match init_logging(default_log_dir(), default_log_file()) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("Failed to initialize logging: {}", e);
            process::exit(1);
        }
    };

    info!("XEarthLayer v{}", xearthlayer::VERSION);
    info!("XEarthLayer CLI: serve command");
    info!("Starting FUSE server");
    println!("XEarthLayer FUSE Server v{}", xearthlayer::VERSION);
    println!("=======================");
    println!();
    println!("Mountpoint: {}", mountpoint);
    println!("DDS Format: {:?}", texture_config.format());
    println!("Mipmap Levels: {}", texture_config.mipmap_count());
    println!();

    // Log provider type
    if provider_config.requires_api_key() {
        info!("Creating {} session...", provider_config.name());
        println!("Creating {} session...", provider_config.name());
    }

    // Build service configuration
    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!no_cache)
        .mountpoint(mountpoint)
        .build();

    // Create service
    let service = match XEarthLayerService::new(service_config, provider_config) {
        Ok(s) => {
            info!("Service created successfully");
            s
        }
        Err(e) => {
            error!("Failed to create service: {}", e);
            eprintln!("Error creating service: {}", e);
            process::exit(1);
        }
    };

    // Print service info
    if service.cache_enabled() {
        info!("Cache enabled: 2GB memory, 20GB disk");
        println!("Cache: Enabled (2GB memory, 20GB disk at ~/.cache/xearthlayer)");
    } else {
        info!("Caching disabled (--no-cache)");
        println!("Cache: Disabled (all tiles generated fresh)");
    }
    println!(
        "Provider: {}{}",
        service.provider_name(),
        if !service.cache_enabled() {
            ""
        } else {
            " (no API key required)"
        }
    );
    println!();

    info!("Mounting FUSE filesystem at {}", mountpoint);
    println!("Mounting filesystem...");
    println!("Press Ctrl+C to unmount and exit");
    println!();

    // Start serving
    match service.serve() {
        Ok(_) => {
            info!("FUSE filesystem unmounted successfully");
            println!("Filesystem unmounted.");
        }
        Err(e) => {
            error!("Failed to serve: {}", e);
            eprintln!("Error: {}", e);
            eprintln!();
            eprintln!("Common issues:");
            eprintln!("  1. FUSE not installed: sudo apt install fuse (Linux)");
            eprintln!("  2. Permissions: You may need to add your user to 'fuse' group");
            eprintln!(
                "  3. Mountpoint in use: Try unmounting with: fusermount -u {}",
                mountpoint
            );
            process::exit(1);
        }
    }
}

/// Save DDS data directly to file.
fn save_dds_data(dds_data: &[u8], output_path: &str, texture_config: TextureConfig) {
    let format = texture_config.format();
    let mipmap_count = texture_config.mipmap_count();

    info!(
        "Saving DDS ({:?} compression, {} mipmap levels)",
        format, mipmap_count
    );
    println!(
        "Saving DDS ({:?} compression, {} mipmap levels)...",
        format, mipmap_count
    );

    match std::fs::write(output_path, dds_data) {
        Ok(_) => {
            info!(
                "DDS saved successfully: {} ({:.2} MB)",
                output_path,
                dds_data.len() as f64 / 1_048_576.0
            );
            println!(
                "✓ Saved successfully: {} ({:.2} MB)",
                output_path,
                dds_data.len() as f64 / 1_048_576.0
            );
            println!("  Format: {:?}", format);
            println!("  Mipmaps: {}", mipmap_count);
        }
        Err(e) => {
            error!("Failed to write DDS file: {}", e);
            eprintln!("Error writing DDS file: {}", e);
            process::exit(1);
        }
    }
}

/// Save DDS data as JPEG (requires decoding DDS first).
/// Note: This is a simplified implementation that re-downloads and encodes as JPEG.
fn save_dds_as_jpeg(dds_data: &[u8], output_path: &str) {
    info!("Converting to JPEG format");
    println!("Note: DDS to JPEG conversion not fully implemented.");
    println!("Saving raw DDS data instead. Use --format dds for best results.");

    // For now, just save the DDS data with a warning
    // A full implementation would decode DDS and re-encode as JPEG
    let dds_path = if output_path.ends_with(".jpg") || output_path.ends_with(".jpeg") {
        output_path.replace(".jpg", ".dds").replace(".jpeg", ".dds")
    } else {
        format!("{}.dds", output_path)
    };

    match std::fs::write(&dds_path, dds_data) {
        Ok(_) => {
            info!("DDS saved to: {}", dds_path);
            println!(
                "✓ Saved as DDS: {} ({:.2} MB)",
                dds_path,
                dds_data.len() as f64 / 1_048_576.0
            );
        }
        Err(e) => {
            error!("Failed to write file: {}", e);
            eprintln!("Error writing file: {}", e);
            process::exit(1);
        }
    }
}
