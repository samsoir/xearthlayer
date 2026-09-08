//! Cache management CLI commands.

use clap::Subcommand;
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
    }
}
