//! Configuration types for the cache service.
//!
//! These types configure cache providers independently of the domain-specific
//! usage. Domain concerns (tile coordinates, metrics) are handled by decorators,
//! not by the cache configuration.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::DEFAULT_MEMORY_CACHE_SIZE;
use crate::metrics::MetricsClient;

/// Configuration for creating a cache service.
#[derive(Debug, Clone)]
pub struct ServiceCacheConfig {
    /// Maximum size in bytes.
    pub max_size_bytes: u64,

    /// Provider-specific settings.
    pub provider: ProviderConfig,

    /// Optional metrics client for reporting cache stats.
    /// For disk caches, this enables GC eviction reporting.
    pub metrics_client: Option<MetricsClient>,
}

/// Provider-specific configuration.
#[derive(Debug, Clone)]
pub enum ProviderConfig {
    /// In-memory cache using moka.
    Memory {
        /// Optional time-to-live for entries.
        /// If `None`, entries are only evicted by LRU when size limit is reached.
        ttl: Option<Duration>,
    },

    /// On-disk cache with background garbage collection.
    Disk {
        /// Directory for cache storage.
        directory: PathBuf,

        /// Interval between GC checks.
        /// The GC daemon checks if eviction is needed at this interval.
        gc_interval: Duration,

        /// Provider name for subdirectory structure.
        /// Used to create isolated cache directories per provider (e.g., "bing", "google").
        provider_name: String,
    },
}

impl ServiceCacheConfig {
    /// Create a memory cache configuration.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum memory size in bytes
    /// * `ttl` - Optional time-to-live for entries
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = ServiceCacheConfig::memory(2 * 1024 * 1024 * 1024, None); // 2 GB, no TTL
    /// ```
    pub fn memory(max_size_bytes: u64, ttl: Option<Duration>) -> Self {
        Self {
            max_size_bytes,
            provider: ProviderConfig::Memory { ttl },
            metrics_client: None,
        }
    }

    /// Create a disk cache configuration.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum disk size in bytes
    /// * `directory` - Cache directory path
    /// * `gc_interval` - Interval between GC checks
    /// * `provider_name` - Name for subdirectory structure
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = ServiceCacheConfig::disk(
    ///     40 * 1024 * 1024 * 1024, // 40 GB
    ///     PathBuf::from("/var/cache/xearthlayer"),
    ///     Duration::from_secs(60),
    ///     "bing".to_string(),
    /// );
    /// ```
    pub fn disk(
        max_size_bytes: u64,
        directory: PathBuf,
        gc_interval: Duration,
        provider_name: String,
    ) -> Self {
        Self {
            max_size_bytes,
            provider: ProviderConfig::Disk {
                directory,
                gc_interval,
                provider_name,
            },
            metrics_client: None,
        }
    }

    /// Set the metrics client for reporting cache stats.
    ///
    /// For disk caches, this enables GC eviction reporting to the metrics system.
    pub fn with_metrics(mut self, metrics: MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
        self
    }
}

impl Default for ServiceCacheConfig {
    /// Default configuration: 2 GB memory cache with no TTL.
    fn default() -> Self {
        Self::memory(DEFAULT_MEMORY_CACHE_SIZE as u64, None)
    }
}

/// Configuration specifically for disk cache providers.
///
/// This is a convenience struct that extracts disk-specific settings
/// from `ProviderConfig::Disk`.
#[derive(Debug, Clone)]
pub struct DiskProviderConfig {
    /// Cache directory path.
    pub directory: PathBuf,

    /// Maximum size in bytes.
    pub max_size_bytes: u64,

    /// Interval between GC checks.
    pub gc_interval: Duration,

    /// Provider name for subdirectory structure.
    pub provider_name: String,

    /// Optional metrics client for reporting GC eviction stats.
    pub metrics_client: Option<MetricsClient>,
}

impl DiskProviderConfig {
    /// Create from ServiceCacheConfig if it's a disk provider.
    ///
    /// # Returns
    ///
    /// `Some(DiskProviderConfig)` if the config is for a disk provider,
    /// `None` otherwise.
    ///
    /// Note: The metrics_client is set to None. Use `with_metrics()` to add one.
    pub fn from_service_config(config: &ServiceCacheConfig) -> Option<Self> {
        match &config.provider {
            ProviderConfig::Disk {
                directory,
                gc_interval,
                provider_name,
            } => Some(Self {
                directory: directory.clone(),
                max_size_bytes: config.max_size_bytes,
                gc_interval: *gc_interval,
                provider_name: provider_name.clone(),
                metrics_client: None,
            }),
            ProviderConfig::Memory { .. } => None,
        }
    }

    /// Set the metrics client for GC eviction reporting.
    pub fn with_metrics(mut self, metrics: MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_config() {
        let config = ServiceCacheConfig::memory(1_000_000, None);
        assert_eq!(config.max_size_bytes, 1_000_000);
        assert!(matches!(
            config.provider,
            ProviderConfig::Memory { ttl: None }
        ));
    }

    #[test]
    fn test_memory_config_with_ttl() {
        let ttl = Duration::from_secs(3600);
        let config = ServiceCacheConfig::memory(1_000_000, Some(ttl));
        assert!(matches!(
            config.provider,
            ProviderConfig::Memory { ttl: Some(t) } if t == Duration::from_secs(3600)
        ));
    }

    #[test]
    fn test_disk_config() {
        let config = ServiceCacheConfig::disk(
            40_000_000_000,
            PathBuf::from("/tmp/cache"),
            Duration::from_secs(60),
            "test".to_string(),
        );

        assert_eq!(config.max_size_bytes, 40_000_000_000);
        if let ProviderConfig::Disk {
            directory,
            gc_interval,
            provider_name,
        } = config.provider
        {
            assert_eq!(directory, PathBuf::from("/tmp/cache"));
            assert_eq!(gc_interval, Duration::from_secs(60));
            assert_eq!(provider_name, "test");
        } else {
            panic!("Expected Disk provider");
        }
    }

    #[test]
    fn test_default_config() {
        let config = ServiceCacheConfig::default();
        assert_eq!(config.max_size_bytes, 2 * 1024 * 1024 * 1024);
        assert!(matches!(
            config.provider,
            ProviderConfig::Memory { ttl: None }
        ));
    }

    #[test]
    fn test_disk_provider_config_from_service_config() {
        let config = ServiceCacheConfig::disk(
            1_000_000,
            PathBuf::from("/tmp"),
            Duration::from_secs(30),
            "provider".to_string(),
        );

        let disk_config = DiskProviderConfig::from_service_config(&config).unwrap();
        assert_eq!(disk_config.max_size_bytes, 1_000_000);
        assert_eq!(disk_config.directory, PathBuf::from("/tmp"));
        assert_eq!(disk_config.gc_interval, Duration::from_secs(30));
        assert_eq!(disk_config.provider_name, "provider");
    }

    #[test]
    fn test_disk_provider_config_from_memory_config() {
        let config = ServiceCacheConfig::memory(1_000_000, None);
        let disk_config = DiskProviderConfig::from_service_config(&config);
        assert!(disk_config.is_none());
    }
}
