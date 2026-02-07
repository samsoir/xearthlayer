//! Cache management CLI commands.

use clap::Subcommand;
use xearthlayer::cache::migrate::migrate_cache;
use xearthlayer::cache::{clear_disk_cache, disk_cache_stats};
use xearthlayer::config::{format_size, ConfigFile};

use crate::error::CliError;

/// Cache action subcommands.
#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Clear the disk cache, removing all cached tiles
    Clear,
    /// Show disk cache statistics
    Stats,
    /// Migrate flat cache files into region subdirectories for faster startup.
    ///
    /// Moves existing cache files from the flat layout into 1x1 degree DSF
    /// region subdirectories. This enables parallel scanning on startup,
    /// dramatically reducing startup time for large caches.
    ///
    /// This command is safe to run multiple times (idempotent).
    /// The cache should not be in active use during migration.
    ///
    /// Deprecated: will be removed in v0.4.0 when all caches use the region layout.
    Migrate,
}

/// Run a cache subcommand.
pub fn run(action: CacheAction) -> Result<(), CliError> {
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
        CacheAction::Migrate => {
            println!("Migrating cache at: {}", cache_dir.display());
            println!("NOTE: This command will be removed in v0.4.0.");
            println!();

            match migrate_cache(cache_dir) {
                Ok(result) => {
                    if result.files_moved == 0 {
                        println!("No flat cache files found. Cache is already migrated or empty.");
                    } else {
                        println!(
                            "Migrated {} files ({}) into {} region directories",
                            result.files_moved,
                            format_size(result.bytes_migrated as usize),
                            result.regions_created,
                        );
                        if result.files_skipped > 0 {
                            println!("Skipped {} non-cache files", result.files_skipped);
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(CliError::CacheClear(e.to_string())),
            }
        }
    }
}
