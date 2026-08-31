//! SceneryIndex cache management CLI commands.
//!
//! Provides commands to manage the scenery index cache outside of the run command:
//! - `update`: Rebuild the index from installed packages
//! - `clear`: Delete the cache file
//! - `status`: Show cache status and tile counts

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Subcommand;
use xearthlayer::config::{format_size, ConfigFile};
use xearthlayer::manager::LocalPackageStore;
use xearthlayer::package::PackageType;
use xearthlayer::prefetch::{
    cache_status, save_cache, scenery_cache_path, SceneryIndex, SceneryIndexConfig,
};

use crate::error::CliError;

/// SceneryIndex cache subcommands.
#[derive(Debug, Subcommand)]
pub enum SceneryIndexAction {
    /// Rebuild the scenery index from installed packages
    Update,
    /// Delete the scenery index cache file
    Clear,
    /// Show scenery index cache status
    Status,
}

/// Run a scenery-index subcommand.
pub fn run(action: SceneryIndexAction) -> Result<(), CliError> {
    match action {
        SceneryIndexAction::Update => run_update(),
        SceneryIndexAction::Clear => run_clear(),
        SceneryIndexAction::Status => run_status(),
    }
}

/// Discover ortho packages from the install location.
fn discover_ortho_packages(config: &ConfigFile) -> Result<Vec<(String, PathBuf)>, CliError> {
    // Get install location with default fallback
    let install_location = config.packages.install_location.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".xearthlayer")
            .join("packages")
    });

    let store = LocalPackageStore::new(&install_location);

    let packages = store
        .list()
        .map_err(|e| CliError::SceneryIndex(e.to_string()))?;

    // Filter to ortho packages and convert to (name, path) tuples
    let ortho_packages: Vec<(String, PathBuf)> = packages
        .into_iter()
        .filter(|p| p.package_type() == PackageType::Ortho)
        .map(|p| (p.region().to_string(), p.path))
        .collect();

    Ok(ortho_packages)
}

/// Rebuild the scenery index from installed packages.
fn run_update() -> Result<(), CliError> {
    let config = ConfigFile::load().unwrap_or_default();
    let packages = discover_ortho_packages(&config)?;

    if packages.is_empty() {
        return Err(CliError::SceneryIndex(
            "No ortho packages found. Install packages first with 'xearthlayer packages install'."
                .into(),
        ));
    }

    println!("Building scenery index from {} packages...", packages.len());

    // Build index with progress output
    let index = SceneryIndex::new(SceneryIndexConfig::default());
    for (name, path) in &packages {
        print!("  Indexing {}... ", name);
        io::stdout().flush().ok();

        match index.build_from_package(path) {
            Ok(stats) if stats.failed > 0 => {
                println!(
                    "{} tiles ({} .ter files failed to parse)",
                    stats.parsed, stats.failed
                )
            }
            Ok(stats) => println!("{} tiles", stats.parsed),
            Err(e) => println!("error: {}", e),
        }
    }

    // Save cache
    println!("Saving cache...");
    save_cache(&index, &packages).map_err(|e| CliError::SceneryIndex(e.to_string()))?;

    let land_tiles = index.tile_count() - index.sea_tile_count();
    println!(
        "Done. {} tiles indexed ({} land, {} sea)",
        index.tile_count(),
        land_tiles,
        index.sea_tile_count()
    );

    Ok(())
}

/// Delete the scenery index cache file.
fn run_clear() -> Result<(), CliError> {
    let path = scenery_cache_path();

    if !path.exists() {
        println!("No scenery index cache found at: {}", path.display());
        return Ok(());
    }

    std::fs::remove_file(&path)
        .map_err(|e| CliError::SceneryIndex(format!("Failed to delete cache: {}", e)))?;

    println!("Deleted scenery index cache: {}", path.display());
    Ok(())
}

/// Show scenery index cache status.
fn run_status() -> Result<(), CliError> {
    let path = scenery_cache_path();
    println!("Scenery index cache: {}", path.display());

    if !path.exists() {
        println!("  Status: Not found");
        println!();
        println!("Run 'xearthlayer scenery-index update' to build the cache.");
        return Ok(());
    }

    // Get file size
    let file_size = std::fs::metadata(&path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);

    // Read cache header
    match cache_status() {
        Ok(status) => {
            let land_tiles = status.total_tiles - status.sea_tiles;

            println!("  Status: Valid");
            println!("  Version: {}", status.version);
            println!("  Packages: {}", status.package_count);
            for pkg in &status.packages {
                println!("    - {} ({} files)", pkg.name, pkg.terrain_file_count);
            }
            println!("  Total tiles: {}", status.total_tiles);
            println!("  Land tiles: {}", land_tiles);
            println!("  Sea tiles: {}", status.sea_tiles);
            println!("  File size: {}", format_size(file_size));

            let expected: usize = status.packages.iter().map(|p| p.terrain_file_count).sum();
            if expected > 0 {
                let pct = (status.total_tiles as f64 / expected as f64) * 100.0;
                println!(
                    "  Indexed:   {} of {} .ter files ({:.1}%)",
                    status.total_tiles, expected, pct
                );
                if status.total_tiles < expected {
                    println!(
                        "  WARNING:   {} .ter files did not produce a tile",
                        expected - status.total_tiles
                    );
                }
            }
        }
        Err(e) => {
            println!("  Status: Invalid");
            println!("  Error: {}", e);
            println!();
            println!("Run 'xearthlayer scenery-index update' to rebuild the cache.");
        }
    }

    Ok(())
}
