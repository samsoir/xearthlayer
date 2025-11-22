//! XEarthLayer CLI - Command-line interface
//!
//! This binary provides a command-line interface to the XEarthLayer library.

use clap::Parser;
use std::process;
use xearthlayer::coord::to_tile_coords;
use xearthlayer::orchestrator::TileOrchestrator;
use xearthlayer::provider::{BingMapsProvider, Provider, ReqwestClient};

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

    /// Zoom level (1-15 for Bing Maps, default: 15)
    #[arg(long, default_value = "15")]
    zoom: u8,

    /// Output file path (JPEG format)
    #[arg(long)]
    output: String,
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

    // Create HTTP client and provider
    let http_client = match ReqwestClient::new() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Error creating HTTP client: {}", e);
            process::exit(1);
        }
    };

    let provider = BingMapsProvider::new(http_client);

    // Validate zoom level - chunks are downloaded at zoom+4
    // so we need zoom+4 <= provider's max_zoom
    if args.zoom + 4 > provider.max_zoom() {
        eprintln!(
            "Error: Zoom level {} requires chunks at zoom {}, but {} only supports up to zoom {}",
            args.zoom,
            args.zoom + 4,
            provider.name(),
            provider.max_zoom()
        );
        eprintln!(
            "Maximum usable zoom level for {} is {}",
            provider.name(),
            provider.max_zoom() - 4
        );
        process::exit(1);
    }

    // Create orchestrator (30s timeout, 3 retries per chunk, 32 parallel downloads)
    let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);

    // Download tile
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let image = match orchestrator.download_tile(&tile) {
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

    // Save as JPEG with 90% quality
    println!("Saving to {}...", args.output);

    // Convert RGBA to RGB for JPEG
    let rgb_image = image::DynamicImage::ImageRgba8(image).to_rgb8();

    // Use JPEG encoder with quality setting
    let file = match std::fs::File::create(&args.output) {
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
            println!("Saved successfully with 90% quality!");
        }
        Err(e) => {
            eprintln!("Error encoding JPEG: {}", e);
            process::exit(1);
        }
    }
}
