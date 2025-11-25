//! Core types and traits for the cache system.

use crate::coord::TileCoord;
use crate::dds::DdsFormat;
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

/// Complete cache system configuration.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Memory cache configuration
    pub memory: MemoryCacheConfig,
    /// Disk cache configuration
    pub disk: DiskCacheConfig,
    /// Active provider name
    pub provider: String,
}

impl CacheConfig {
    /// Create a new cache configuration with the given provider.
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            memory: MemoryCacheConfig::default(),
            disk: DiskCacheConfig::default(),
            provider: provider.into(),
        }
    }

    /// Set memory cache size in bytes.
    pub fn with_memory_size(mut self, size: usize) -> Self {
        self.memory.max_size_bytes = size;
        self
    }

    /// Set disk cache size in bytes.
    pub fn with_disk_size(mut self, size: usize) -> Self {
        self.disk.max_size_bytes = size;
        self
    }

    /// Set cache directory.
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.disk.cache_dir = dir;
        self
    }

    /// Set maximum age in days for disk cache.
    pub fn with_max_age_days(mut self, days: u32) -> Self {
        self.disk.max_age_days = Some(days);
        self
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

    #[test]
    fn test_cache_config_builder() {
        let config = CacheConfig::new("bing")
            .with_memory_size(1_000_000_000)
            .with_disk_size(10_000_000_000)
            .with_cache_dir(PathBuf::from("/tmp/cache"))
            .with_max_age_days(30);

        assert_eq!(config.provider, "bing");
        assert_eq!(config.memory.max_size_bytes, 1_000_000_000);
        assert_eq!(config.disk.max_size_bytes, 10_000_000_000);
        assert_eq!(config.disk.cache_dir, PathBuf::from("/tmp/cache"));
        assert_eq!(config.disk.max_age_days, Some(30));
    }

    #[test]
    fn test_cache_config_default_values() {
        let config = CacheConfig::new("google");

        assert_eq!(config.provider, "google");
        assert_eq!(config.memory.max_size_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(config.disk.max_size_bytes, 20 * 1024 * 1024 * 1024);
    }
}
