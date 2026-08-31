//! Core types and traits for the cache system.

use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

/// Cache key uniquely identifying a cached tile.
///
/// Includes all parameters needed to reconstruct the tile:
/// provider, format, and tile coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Provider name (e.g., "bing", "google")
    pub provider: String,
    /// DDS compression format (BC1 or BC3)
    pub format: DdsFormat,
    /// Tile coordinates
    pub tile: TileCoord,
}

impl CacheKey {
    /// Create a new cache key.
    pub fn new(provider: impl Into<String>, format: DdsFormat, tile: TileCoord) -> Self {
        Self {
            provider: provider.into(),
            format,
            tile,
        }
    }
}

/// Cache-related errors.
#[derive(Debug, Error)]
pub enum CacheError {
    /// I/O error during cache operations
    #[error("Cache I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Cache size limit exceeded
    #[error("Cache size limit exceeded: current={current}, limit={limit}")]
    SizeLimitExceeded { current: usize, limit: usize },

    /// Failed to acquire lock
    #[error("Failed to acquire cache lock")]
    LockError,

    /// Invalid cache configuration
    #[error("Invalid cache configuration: {0}")]
    InvalidConfig(String),
}

/// Memory cache configuration.
#[derive(Debug, Clone)]
pub struct MemoryCacheConfig {
    /// Maximum memory size in bytes (default: 2 GB)
    pub max_size_bytes: usize,
    /// Daemon check interval in seconds (default: 10)
    pub daemon_interval_secs: u64,
}

impl Default for MemoryCacheConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
            daemon_interval_secs: 10,
        }
    }
}

/// Disk cache configuration.
#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    /// Cache directory root
    pub cache_dir: PathBuf,
    /// Maximum disk size in bytes (default: 20 GB)
    pub max_size_bytes: usize,
    /// Optional: evict tiles older than this many days
    pub max_age_days: Option<u32>,
    /// Daemon check interval in seconds (default: 60)
    pub daemon_interval_secs: u64,
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("xearthlayer");

        Self {
            cache_dir,
            max_size_bytes: 20 * 1024 * 1024 * 1024, // 20 GB
            max_age_days: None,
            daemon_interval_secs: 60,
        }
    }
}

/// Which disk cache tier a write belongs to.
///
/// Chunk-tier writes are small and frequent (~14 KB, thousands per second);
/// DDS-tier writes are large and rare (11.17 MB, a few per second). Attributing
/// bytes to a tier keeps `chunk_disk_bytes_written` honest — see issue #216.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskTier {
    /// The chunk disk cache.
    Chunk,
    /// The DDS tile disk cache.
    Dds,
}

impl DiskTier {
    /// Stable label for log fields.
    ///
    /// These exact strings already appear in disk cache log output; changing
    /// them would break existing log greps. Note the asymmetry — the chunk
    /// label is plural, the DDS label is not.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chunk => "chunks",
            Self::Dds => "dds",
        }
    }
}

impl fmt::Display for DiskTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_creation() {
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };
        let key = CacheKey::new("bing", DdsFormat::BC1, tile);

        assert_eq!(key.provider, "bing");
        assert_eq!(key.format, DdsFormat::BC1);
        assert_eq!(key.tile.row, 12754);
        assert_eq!(key.tile.col, 5279);
        assert_eq!(key.tile.zoom, 15);
    }

    #[test]
    fn test_cache_key_equality() {
        let tile1 = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };
        let tile2 = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };
        let tile3 = TileCoord {
            row: 100,
            col: 201,
            zoom: 15,
        };

        let key1 = CacheKey::new("bing", DdsFormat::BC1, tile1);
        let key2 = CacheKey::new("bing", DdsFormat::BC1, tile2);
        let key3 = CacheKey::new("bing", DdsFormat::BC1, tile3);

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_cache_key_different_providers() {
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        let key1 = CacheKey::new("bing", DdsFormat::BC1, tile);
        let key2 = CacheKey::new("google", DdsFormat::BC1, tile);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_different_formats() {
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        let key1 = CacheKey::new("bing", DdsFormat::BC1, tile);
        let key2 = CacheKey::new("bing", DdsFormat::BC3, tile);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_memory_cache_config_default() {
        let config = MemoryCacheConfig::default();
        assert_eq!(config.max_size_bytes, 2 * 1024 * 1024 * 1024); // 2 GB
        assert_eq!(config.daemon_interval_secs, 10);
    }

    #[test]
    fn test_disk_cache_config_default() {
        let config = DiskCacheConfig::default();
        assert_eq!(config.max_size_bytes, 20 * 1024 * 1024 * 1024); // 20 GB
        assert_eq!(config.daemon_interval_secs, 60);
        assert!(config.max_age_days.is_none());
        assert!(config.cache_dir.ends_with("xearthlayer"));
    }

    // These exact strings already appear in disk cache log output. Changing
    // them — including "normalising" the plural — would break existing log
    // greps and dashboard filters.
    #[test]
    fn disk_tier_labels_are_stable() {
        assert_eq!(DiskTier::Chunk.as_str(), "chunks");
        assert_eq!(DiskTier::Dds.as_str(), "dds");
        assert_eq!(format!("{}", DiskTier::Chunk), "chunks");
        assert_eq!(format!("{}", DiskTier::Dds), "dds");
    }
}
