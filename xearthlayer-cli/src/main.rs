//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.

use clap::{Parser, Subcommand, ValueEnum};
use std::process;
use std::sync::Arc;
use tracing::{error, info};
use xearthlayer::cache::{Cache, CacheConfig, CacheSystem, NoOpCache};
use xearthlayer::config::{DownloadConfig, TextureConfig};
use xearthlayer::coord::to_tile_coords;
use xearthlayer::dds::{DdsEncoder, DdsFormat};
use xearthlayer::fuse::XEarthLayerFS;
use xearthlayer::logging::{default_log_dir, default_log_file, init_logging};
use xearthlayer::orchestrator::TileOrchestrator;
use xearthlayer::provider::{BingMapsProvider, GoogleMapsProvider, Provider, ReqwestClient};
use xearthlayer::texture::{DdsTextureEncoder, TextureEncoder};
use xearthlayer::tile::{DefaultTileGenerator, TileGenerator};

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
            // Convert CLI format to DdsFormat
            let dds_fmt = match dds_format {
                DdsCompressionFormat::Bc1 => DdsFormat::BC1,
                DdsCompressionFormat::Bc3 => DdsFormat::BC3,
            };

            // Build texture configuration
            let texture_config = TextureConfig::new(dds_fmt).with_mipmap_count(mipmap_count);

            handle_download(
                lat,
                lon,
                zoom,
                &output,
                &format,
                texture_config,
                provider,
                google_api_key,
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
            // Convert CLI format to DdsFormat
            let format = match dds_format {
                DdsCompressionFormat::Bc1 => DdsFormat::BC1,
                DdsCompressionFormat::Bc3 => DdsFormat::BC3,
            };

            // Build configuration objects from CLI arguments
            let texture_config = TextureConfig::new(format).with_mipmap_count(mipmap_count);

            let download_config = DownloadConfig::new()
                .with_timeout_secs(timeout)
                .with_max_retries(retries as u32)
                .with_parallel_downloads(parallel);

            handle_serve(
                &mountpoint,
                provider,
                google_api_key,
                texture_config,
                download_config,
                no_cache,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_download(
    lat: f64,
    lon: f64,
    zoom: u8,
    output: &str,
    format: &Option<OutputFormat>,
    texture_config: TextureConfig,
    provider: ProviderType,
    google_api_key: Option<String>,
) {
    // Initialize logging (keep guard alive for entire function)
    let _logging_guard = match init_logging(default_log_dir(), default_log_file()) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("Failed to initialize logging: {}", e);
            process::exit(1);
        }
    };

    info!("XEarthLayer v{}", xearthlayer::VERSION);
    info!("XEarthLayer CLI: download command");

    // Validate zoom level
    if !(1..=19).contains(&zoom) {
        error!("Invalid zoom level: {}", zoom);
        eprintln!("Error: Zoom level must be between 1 and 19");
        process::exit(1);
    }

    // Convert coordinates to tile
    let tile = match to_tile_coords(lat, lon, zoom) {
        Ok(t) => {
            info!(
                "Converted coordinates to tile: lat={}, lon={}, zoom={} -> row={}, col={}",
                lat, lon, zoom, t.row, t.col
            );
            t
        }
        Err(e) => {
            error!("Failed to convert coordinates: {}", e);
            eprintln!("Error converting coordinates: {}", e);
            process::exit(1);
        }
    };

    println!("Downloading tile for:");
    println!("  Location: {}, {}", lat, lon);
    println!("  Zoom: {}", zoom);
    println!(
        "  Tile: row={}, col={}, zoom={}",
        tile.row, tile.col, tile.zoom
    );
    println!();

    // Create HTTP client
    let http_client = match ReqwestClient::new() {
        Ok(client) => {
            info!("HTTP client created successfully");
            client
        }
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            eprintln!("Error creating HTTP client: {}", e);
            process::exit(1);
        }
    };

    // Create provider based on selection
    let (provider, name, max): (Arc<dyn Provider>, String, u8) = match provider {
        ProviderType::Bing => {
            let p = BingMapsProvider::new(http_client);
            let name = p.name().to_string();
            let max = p.max_zoom();
            (Arc::new(p), name, max)
        }
        ProviderType::Google => {
            let api_key = google_api_key.unwrap(); // Safe: required_if_eq

            info!("Creating Google Maps session...");
            println!("Creating Google Maps session...");
            let p = match GoogleMapsProvider::new(http_client, api_key) {
                Ok(p) => {
                    info!("Google Maps session created successfully");
                    p
                }
                Err(e) => {
                    error!("Failed to create Google Maps provider: {}", e);
                    eprintln!("Error creating Google Maps provider: {}", e);
                    eprintln!("Make sure:");
                    eprintln!("  1. Map Tiles API is enabled in Google Cloud Console");
                    eprintln!("  2. Billing is enabled for your project");
                    eprintln!("  3. Your API key is valid and unrestricted");
                    process::exit(1);
                }
            };
            let name = p.name().to_string();
            let max = p.max_zoom();
            (Arc::new(p), name, max)
        }
    };

    // Validate zoom level
    if zoom + 4 > max {
        error!(
            "Zoom level {} requires chunks at zoom {}, but {} only supports up to zoom {}",
            zoom,
            zoom + 4,
            name,
            max
        );
        eprintln!(
            "Error: Zoom level {} requires chunks at zoom {}, but {} only supports up to zoom {}",
            zoom,
            zoom + 4,
            name,
            max
        );
        eprintln!("Maximum usable zoom level for {} is {}", name, max - 4);
        process::exit(1);
    }

    info!("Using provider: {}", name);
    println!("Using provider: {}", name);

    // Use default download config for download command
    let orchestrator = TileOrchestrator::with_config(provider, DownloadConfig::default());
    download_and_save(orchestrator, &tile, output, format, texture_config);
}

fn handle_serve(
    mountpoint: &str,
    provider: ProviderType,
    google_api_key: Option<String>,
    texture_config: TextureConfig,
    download_config: DownloadConfig,
    no_cache: bool,
) {
    // Initialize logging (keep guard alive for entire function)
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

    // Check if mountpoint exists
    if !std::path::Path::new(mountpoint).exists() {
        error!("Mountpoint does not exist: {}", mountpoint);
        eprintln!("Error: Mountpoint directory does not exist: {}", mountpoint);
        eprintln!("Please create it first: mkdir -p {}", mountpoint);
        process::exit(1);
    }

    // Create HTTP client
    let http_client = match ReqwestClient::new() {
        Ok(client) => {
            info!("HTTP client created successfully");
            client
        }
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            eprintln!("Error creating HTTP client: {}", e);
            process::exit(1);
        }
    };

    // Create texture encoder from config
    let encoder: Arc<dyn TextureEncoder> = Arc::new(
        DdsTextureEncoder::new(texture_config.format())
            .with_mipmap_count(texture_config.mipmap_count()),
    );
    info!("Using texture encoder: {}", encoder.name());
    println!(
        "Encoder: {} ({} mipmaps)",
        encoder.name(),
        texture_config.mipmap_count()
    );

    // Create provider based on selection
    let (provider, provider_name): (Arc<dyn Provider>, String) = match provider {
        ProviderType::Bing => {
            let p = BingMapsProvider::new(http_client);
            let name = p.name().to_string();
            info!("Using provider: {} (no API key required)", name);
            println!("Provider: {} (no API key required)", name);
            (Arc::new(p), name)
        }
        ProviderType::Google => {
            let api_key = google_api_key.unwrap(); // Safe: required_if_eq

            info!("Creating Google Maps session...");
            println!("Creating Google Maps session...");
            let p = match GoogleMapsProvider::new(http_client, api_key) {
                Ok(p) => {
                    info!("Google Maps session created successfully");
                    p
                }
                Err(e) => {
                    error!("Failed to create Google Maps provider: {}", e);
                    eprintln!("Error creating Google Maps provider: {}", e);
                    process::exit(1);
                }
            };
            let name = p.name().to_string();
            info!("Using provider: {}", name);
            println!("Provider: {}", name);
            (Arc::new(p), name)
        }
    };
    println!();

    // Create orchestrator from config
    let orchestrator = TileOrchestrator::with_config(provider, download_config);

    // Create tile generator (combines orchestrator + encoder)
    let generator: Arc<dyn TileGenerator> =
        Arc::new(DefaultTileGenerator::new(orchestrator, encoder.clone()));
    info!("Tile generator created");

    // Create cache system or no-op cache
    let cache: Arc<dyn Cache> = if no_cache {
        info!("Caching disabled (--no-cache)");
        println!("Cache: Disabled (all tiles generated fresh)");
        Arc::new(NoOpCache::new(&provider_name))
    } else {
        let cache_config = CacheConfig::new(&provider_name);
        match CacheSystem::new(cache_config) {
            Ok(cache) => {
                info!("Cache enabled: 2GB memory, 20GB disk");
                println!("Cache: Enabled (2GB memory, 20GB disk at ~/.cache/xearthlayer)");
                Arc::new(cache)
            }
            Err(e) => {
                error!("Failed to create cache system: {}", e);
                eprintln!("Error: Failed to create cache system: {}", e);
                process::exit(1);
            }
        }
    };
    println!();

    let fs = XEarthLayerFS::new(generator, cache, texture_config.format());

    info!("Mounting FUSE filesystem at {}", mountpoint);
    println!("Mounting filesystem...");
    println!("Press Ctrl+C to unmount and exit");
    println!();

    let options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("xearthlayer".to_string()),
    ];

    match fuser::mount2(fs, mountpoint, &options) {
        Ok(_) => {
            info!("FUSE filesystem unmounted successfully");
            println!("Filesystem unmounted.");
        }
        Err(e) => {
            error!("Failed to mount FUSE filesystem: {}", e);
            eprintln!("Error mounting filesystem: {}", e);
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

fn download_and_save(
    orchestrator: TileOrchestrator,
    tile: &xearthlayer::coord::TileCoord,
    output_path: &str,
    format: &Option<OutputFormat>,
    texture_config: TextureConfig,
) {
    // Download tile
    info!("Starting download of 256 chunks in parallel");
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let image = match orchestrator.download_tile(tile) {
        Ok(img) => {
            info!(
                "Tile downloaded successfully: {}x{} pixels",
                img.width(),
                img.height()
            );
            img
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
    println!("Image size: {}x{}", image.width(), image.height());
    println!();

    // Determine output format (from flag or file extension)
    let output_format = match format {
        Some(f) => f.clone(),
        None => {
            // Auto-detect from file extension
            if output_path.to_lowercase().ends_with(".dds") {
                OutputFormat::Dds
            } else {
                OutputFormat::Jpeg
            }
        }
    };

    // Save based on format
    match output_format {
        OutputFormat::Jpeg => save_jpeg(&image, output_path),
        OutputFormat::Dds => save_dds(&image, output_path, texture_config),
    }
}

fn save_jpeg(image: &image::RgbaImage, output_path: &str) {
    info!("Encoding as JPEG (90% quality)");
    println!("Encoding as JPEG (90% quality)...");

    // Convert RGBA to RGB for JPEG
    let rgb_image = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();

    // Use JPEG encoder with quality setting
    let file = match std::fs::File::create(output_path) {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to create output file: {}", e);
            eprintln!("Error creating output file: {}", e);
            process::exit(1);
        }
    };

    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);

    match encoder.encode(
        rgb_image.as_raw(),
        rgb_image.width(),
        rgb_image.height(),
        image::ExtendedColorType::Rgb8,
    ) {
        Ok(_) => {
            let file_size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
            info!(
                "JPEG saved successfully: {} ({:.2} MB)",
                output_path,
                file_size as f64 / 1_048_576.0
            );
            println!(
                "✓ Saved successfully: {} ({:.2} MB)",
                output_path,
                file_size as f64 / 1_048_576.0
            );
        }
        Err(e) => {
            error!("Failed to encode JPEG: {}", e);
            eprintln!("Error encoding JPEG: {}", e);
            process::exit(1);
        }
    }
}

fn save_dds(image: &image::RgbaImage, output_path: &str, texture_config: TextureConfig) {
    let format = texture_config.format();
    let mipmap_count = texture_config.mipmap_count();

    info!(
        "Encoding as DDS ({:?} compression, {} mipmap levels)",
        format, mipmap_count
    );
    println!(
        "Encoding as DDS ({:?} compression, {} mipmap levels)...",
        format, mipmap_count
    );

    let encode_start = std::time::Instant::now();

    // Create encoder and encode
    let encoder = DdsEncoder::new(format).with_mipmap_count(mipmap_count);

    let dds_data = match encoder.encode(image) {
        Ok(data) => {
            info!(
                "DDS encoding completed: {} bytes in {:.2}s",
                data.len(),
                encode_start.elapsed().as_secs_f64()
            );
            data
        }
        Err(e) => {
            error!("Failed to encode DDS: {}", e);
            eprintln!("Error encoding DDS: {}", e);
            process::exit(1);
        }
    };

    let encode_elapsed = encode_start.elapsed();

    // Write to file
    match std::fs::write(output_path, &dds_data) {
        Ok(_) => {
            info!(
                "DDS saved successfully: {} ({:.2} MB, format: {:?}, mipmaps: {})",
                output_path,
                dds_data.len() as f64 / 1_048_576.0,
                format,
                mipmap_count
            );
            println!(
                "✓ Saved successfully: {} ({:.2} MB, encoded in {:.2}s)",
                output_path,
                dds_data.len() as f64 / 1_048_576.0,
                encode_elapsed.as_secs_f64()
            );
            println!("  Format: {:?}", format);
            println!("  Mipmaps: {}", mipmap_count);
            println!("  Dimensions: {}×{}", image.width(), image.height());
        }
        Err(e) => {
            error!("Failed to write DDS file: {}", e);
            eprintln!("Error writing DDS file: {}", e);
            process::exit(1);
        }
    }
}
