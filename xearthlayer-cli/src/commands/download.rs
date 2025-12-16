//! Download command - download a single tile to a file.

use xearthlayer::config::{DownloadConfig, TextureConfig};
use xearthlayer::service::ServiceConfig;

use super::common::{resolve_dds_format, resolve_provider, DdsCompression, ProviderType};
use crate::error::CliError;
use crate::runner::CliRunner;

/// Arguments for the download command.
pub struct DownloadArgs {
    pub lat: f64,
    pub lon: f64,
    pub zoom: u8,
    pub output: String,
    pub dds_format: Option<DdsCompression>,
    pub provider: Option<ProviderType>,
    pub google_api_key: Option<String>,
    pub mapbox_token: Option<String>,
}

/// Run the download command.
pub fn run(args: DownloadArgs) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    runner.log_startup("download");
    let config = runner.config();

    // Resolve settings from CLI and config
    let provider_config = resolve_provider(
        args.provider,
        args.google_api_key,
        args.mapbox_token,
        config,
    )?;
    let format = resolve_dds_format(args.dds_format, config);

    // Build configurations
    let texture_config = TextureConfig::new(format).with_mipmap_count(5);

    // Caching disabled for single downloads
    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(DownloadConfig::default())
        .cache_enabled(false)
        .build();

    println!("Downloading tile for:");
    println!("  Location: {}, {}", args.lat, args.lon);
    println!("  Zoom: {}", args.zoom);
    println!();

    let service = runner.create_service(service_config, &provider_config)?;
    println!("Using provider: {}", service.provider_name());

    // Download tile
    println!("Downloading 256 chunks in parallel...");
    let start = std::time::Instant::now();

    let dds_data = service
        .download_tile(args.lat, args.lon, args.zoom)
        .map_err(CliError::from)?;

    let elapsed = start.elapsed();
    println!("Downloaded successfully in {:.2}s", elapsed.as_secs_f64());
    println!();

    // Save to file
    runner.save_dds(&args.output, &dds_data, &texture_config)?;

    Ok(())
}
