//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.

use clap::{Parser, ValueEnum};
use std::process;
use xearthlayer::coord::to_tile_coords;
use xearthlayer::dds::{DdsEncoder, DdsFormat};
use xearthlayer::orchestrator::TileOrchestrator;
use xearthlayer::provider::{BingMapsProvider, GoogleMapsProvider, Provider, ReqwestClient};

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
#[command(about = "Download satellite imagery tiles for X-Plane", long_about = None)]
struct Args {
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
}

fn main() {
    let args = Args::parse();

    // Validate zoom level
    if args.zoom < 1 || args.zoom > 19 {
        eprintln!("Error: Zoom level must be between 1 and 19");
        process::exit(1);
    }

    // Convert coordinates to tile
    let tile = match to_tile_coords(args.lat, args.lon, args.zoom) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error converting coordinates: {}", e);
            process::exit(1);
        }
    };

    println!("Downloading tile for:");
    println!("  Location: {}, {}", args.lat, args.lon);
    println!("  Zoom: {}", args.zoom);
    println!(
        "  Tile: row={}, col={}, zoom={}",
        tile.row, tile.col, tile.zoom
    );
    println!();

    // Create HTTP client
    let http_client = match ReqwestClient::new() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Error creating HTTP client: {}", e);
            process::exit(1);
        }
    };

    // Create provider based on selection
    match args.provider {
        ProviderType::Bing => {
            let provider = BingMapsProvider::new(http_client);
            let name = provider.name().to_string();
            let max = provider.max_zoom();

            // Validate zoom level - chunks are downloaded at zoom+4
            if args.zoom + 4 > max {
                eprintln!(
                    "Error: Zoom level {} requires chunks at zoom {}, but {} only supports up to zoom {}",
                    args.zoom,
                    args.zoom + 4,
                    name,
                    max
                );
                eprintln!("Maximum usable zoom level for {} is {}", name, max - 4);
                process::exit(1);
            }

            println!("Using provider: {} (no API key required)", name);

            // Create orchestrator (30s timeout, 3 retries per chunk, 32 parallel downloads)
            let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
            download_and_save(
                orchestrator,
                &tile,
                &args.output,
                &args.format,
                &args.dds_format,
                args.mipmap_count,
            );
        }
        ProviderType::Google => {
            let api_key = args.google_api_key.clone().unwrap(); // Safe: required_if_eq

            println!("Creating Google Maps session...");
            let provider = match GoogleMapsProvider::new(http_client, api_key) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error creating Google Maps provider: {}", e);
                    eprintln!("Make sure:");
                    eprintln!("  1. Map Tiles API is enabled in Google Cloud Console");
                    eprintln!("  2. Billing is enabled for your project");
                    eprintln!("  3. Your API key is valid and unrestricted");
                    process::exit(1);
                }
            };

            let name = provider.name().to_string();
            let max = provider.max_zoom();

            // Validate zoom level
            if args.zoom + 4 > max {
                eprintln!(
                    "Error: Zoom level {} requires chunks at zoom {}, but {} only supports up to zoom {}",
                    args.zoom,
                    args.zoom + 4,
                    name,
                    max
                );
                eprintln!("Maximum usable zoom level for {} is {}", name, max - 4);
                process::exit(1);
            }

            println!("Using provider: {} (session created successfully)", name);

            // Create orchestrator
            let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
            download_and_save(
                orchestrator,
                &tile,
                &args.output,
                &args.format,
                &args.dds_format,
                args.mipmap_count,
            );
        }
    }
}

fn download_and_save<P: Provider + 'static>(
    orchestrator: TileOrchestrator<P>,
    tile: &xearthlayer::coord::TileCoord,
    output_path: &str,
    format: &Option<OutputFormat>,
    dds_format: &DdsCompressionFormat,
    mipmap_count: usize,
) {
    // Download tile
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let image = match orchestrator.download_tile(tile) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("Error downloading tile: {}", e);
            process::exit(1);
        }
    };

    let elapsed = start.elapsed();

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
        OutputFormat::Dds => save_dds(&image, output_path, dds_format, mipmap_count),
    }
}

fn save_jpeg(image: &image::RgbaImage, output_path: &str) {
    println!("Encoding as JPEG (90% quality)...");

    // Convert RGBA to RGB for JPEG
    let rgb_image = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();

    // Use JPEG encoder with quality setting
    let file = match std::fs::File::create(output_path) {
        Ok(f) => f,
        Err(e) => {
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
            println!(
                "✓ Saved successfully: {} ({:.2} MB)",
                output_path,
                file_size as f64 / 1_048_576.0
            );
        }
        Err(e) => {
            eprintln!("Error encoding JPEG: {}", e);
            process::exit(1);
        }
    }
}

fn save_dds(
    image: &image::RgbaImage,
    output_path: &str,
    dds_format: &DdsCompressionFormat,
    mipmap_count: usize,
) {
    let format = match dds_format {
        DdsCompressionFormat::Bc1 => DdsFormat::BC1,
        DdsCompressionFormat::Bc3 => DdsFormat::BC3,
    };

    println!(
        "Encoding as DDS ({:?} compression, {} mipmap levels)...",
        format, mipmap_count
    );

    let encode_start = std::time::Instant::now();

    // Create encoder and encode
    let encoder = DdsEncoder::new(format).with_mipmap_count(mipmap_count);

    let dds_data = match encoder.encode(image) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Error encoding DDS: {}", e);
            process::exit(1);
        }
    };

    let encode_elapsed = encode_start.elapsed();

    // Write to file
    match std::fs::write(output_path, &dds_data) {
        Ok(_) => {
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
            eprintln!("Error writing DDS file: {}", e);
            process::exit(1);
        }
    }
}
