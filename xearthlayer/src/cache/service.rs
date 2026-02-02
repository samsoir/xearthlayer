//! Cache service lifecycle management.
//!
//! `CacheService` wraps a cache provider with proper lifecycle management,
//! including startup and shutdown coordination. This provides a clean API
//! for application code to create and manage cache services.
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::cache::{CacheService, ServiceCacheConfig, ProviderConfig};
//! use std::time::Duration;
//!
//! // Start a memory cache
//! let memory = CacheService::start(ServiceCacheConfig::memory(2_000_000_000, None)).await?;
//!
//! // Start a disk cache
//! let disk = CacheService::start(ServiceCacheConfig::disk(
//!     40_000_000_000,
//!     PathBuf::from("/var/cache/myapp"),
//!     Duration::from_secs(60),
//!     "myapp".to_string(),
//! )).await?;
//!
//! // Use the caches
//! memory.cache().set("key", vec![1, 2, 3]).await?;
//! disk.cache().set("key", vec![4, 5, 6]).await?;
//!
//! // Shutdown gracefully
//! memory.shutdown().await;
//! disk.shutdown().await;
//! ```

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cache::config::{DiskProviderConfig, ProviderConfig, ServiceCacheConfig};
use crate::cache::providers::{DiskCacheProvider, MemoryCacheProvider};
use crate::cache::traits::{Cache, ServiceCacheError};

/// A running cache service that can be shut down.
///
/// This struct wraps a cache provider and provides lifecycle management.
/// The service should be shut down gracefully using the `shutdown()` method
/// to ensure background tasks (like GC daemons) are properly stopped.
pub struct CacheService {
    /// The cache provider.
    cache: Arc<dyn Cache>,

    /// Provider type for shutdown handling.
    provider_type: ProviderType,

    /// Reference to disk provider for shutdown (if applicable).
    disk_provider: Option<Arc<DiskCacheProvider>>,

    /// Cancellation token for service-level shutdown.
    _shutdown: CancellationToken,
}

/// Internal enum to track provider type for shutdown.
enum ProviderType {
    Memory,
    Disk,
}

impl CacheService {
    /// Start a new cache service with the given configuration.
    ///
    /// This creates and starts the appropriate cache provider based on
    /// the configuration. For disk caches, this also starts the GC daemon.
    ///
    /// # Arguments
    ///
    /// * `config` - Cache configuration specifying provider and limits
    ///
    /// # Returns
    ///
    /// A running `CacheService` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to start (e.g., disk directory
    /// cannot be created).
    pub async fn start(config: ServiceCacheConfig) -> Result<Self, ServiceCacheError> {
        let shutdown = CancellationToken::new();

        match config.provider {
            ProviderConfig::Memory { ttl } => {
                let provider = MemoryCacheProvider::new(config.max_size_bytes, ttl);
                let cache = Arc::new(provider);

                info!(
                    max_bytes = config.max_size_bytes,
                    has_ttl = ttl.is_some(),
                    "Memory cache service started"
                );

                Ok(Self {
                    cache,
                    provider_type: ProviderType::Memory,
                    disk_provider: None,
                    _shutdown: shutdown,
                })
            }

            ProviderConfig::Disk {
                directory,
                gc_interval,
                provider_name,
            } => {
                let disk_config = DiskProviderConfig {
                    directory: directory.clone(),
                    max_size_bytes: config.max_size_bytes,
                    gc_interval,
                    provider_name: provider_name.clone(),
                    metrics_client: config.metrics_client,
                };

                let provider = DiskCacheProvider::start(disk_config).await?;
                let cache: Arc<dyn Cache> = Arc::clone(&provider) as Arc<dyn Cache>;

                info!(
                    dir = %directory.display(),
                    max_bytes = config.max_size_bytes,
                    gc_interval_secs = gc_interval.as_secs(),
                    provider = %provider_name,
                    "Disk cache service started"
                );

                Ok(Self {
                    cache,
                    provider_type: ProviderType::Disk,
                    disk_provider: Some(provider),
                    _shutdown: shutdown,
                })
            }
        }
    }

    /// Get a reference to the cache for creating clients.
    ///
    /// The returned `Arc<dyn Cache>` can be cloned and shared across
    /// multiple tasks. It provides the generic cache interface for
    /// domain decorators to wrap.
    pub fn cache(&self) -> Arc<dyn Cache> {
        Arc::clone(&self.cache)
    }

    /// Shutdown the cache service gracefully.
    ///
    /// For disk caches, this stops the GC daemon and waits for any
    /// in-progress operations to complete. For memory caches, this
    /// is a no-op (moka handles cleanup automatically).
    ///
    /// This method consumes the service to prevent further use after shutdown.
    pub async fn shutdown(self) {
        match self.provider_type {
            ProviderType::Memory => {
                info!("Memory cache service shutdown (no-op)");
            }
            ProviderType::Disk => {
                if let Some(provider) = self.disk_provider {
                    provider.shutdown().await;
                }
            }
        }
    }

    /// Check if this is a memory cache service.
    pub fn is_memory(&self) -> bool {
        matches!(self.provider_type, ProviderType::Memory)
    }

    /// Check if this is a disk cache service.
    pub fn is_disk(&self) -> bool {
        matches!(self.provider_type, ProviderType::Disk)
    }

    /// Get the underlying disk cache provider if this is a disk cache.
    ///
    /// Returns `None` for memory caches.
    ///
    /// This is useful for accessing disk-specific functionality like
    /// the LRU index for garbage collection scheduling.
    pub fn disk_provider(&self) -> Option<Arc<DiskCacheProvider>> {
        self.disk_provider.clone()
    }

    /// Scan the initial disk cache size.
    ///
    /// For disk caches, this scans the cache directory to get an accurate
    /// count of existing cached data. This is useful for metrics display
    /// to show accurate initial cache size.
    ///
    /// For memory caches, this returns 0 (memory starts empty).
    ///
    /// This method should be called during service initialization, after
    /// the cache service has started but before the main UI loop, to allow
    /// for progress feedback during the potentially slow disk scan.
    pub async fn scan_initial_size(&self) -> Result<u64, ServiceCacheError> {
        match self.provider_type {
            ProviderType::Memory => {
                // Memory cache always starts empty
                Ok(0)
            }
            ProviderType::Disk => {
                if let Some(ref provider) = self.disk_provider {
                    provider.scan_initial_size().await
                } else {
                    Ok(0)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_memory_service_start() {
        let config = ServiceCacheConfig::memory(1_000_000, None);
        let service = CacheService::start(config).await.unwrap();

        assert!(service.is_memory());
        assert!(!service.is_disk());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_memory_service_with_ttl() {
        let config = ServiceCacheConfig::memory(1_000_000, Some(Duration::from_secs(60)));
        let service = CacheService::start(config).await.unwrap();

        assert!(service.is_memory());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_service_start() {
        let temp_dir = TempDir::new().unwrap();
        let config = ServiceCacheConfig::disk(
            1_000_000,
            temp_dir.path().to_path_buf(),
            Duration::from_secs(3600),
            "test".to_string(),
        );

        let service = CacheService::start(config).await.unwrap();

        assert!(service.is_disk());
        assert!(!service.is_memory());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_service_cache_operations() {
        let config = ServiceCacheConfig::memory(1_000_000, None);
        let service = CacheService::start(config).await.unwrap();
        let cache = service.cache();

        // Set and get
        cache.set("key1", vec![1, 2, 3]).await.unwrap();
        let value = cache.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![1, 2, 3]));

        // Contains
        assert!(cache.contains("key1").await.unwrap());
        assert!(!cache.contains("nonexistent").await.unwrap());

        // Delete
        let deleted = cache.delete("key1").await.unwrap();
        assert!(deleted);
        assert!(!cache.contains("key1").await.unwrap());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_service_shared_cache() {
        let config = ServiceCacheConfig::memory(1_000_000, None);
        let service = CacheService::start(config).await.unwrap();

        let cache1 = service.cache();
        let cache2 = service.cache();

        // Both references should work on the same cache
        cache1.set("key1", vec![1]).await.unwrap();
        let value = cache2.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![1]));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_service_creates_directory() {
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().join("subdir").join("cache");

        let config = ServiceCacheConfig::disk(
            1_000_000,
            cache_dir.clone(),
            Duration::from_secs(3600),
            "test".to_string(),
        );

        let service = CacheService::start(config).await.unwrap();

        // Directory should have been created
        assert!(cache_dir.exists());

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_service_operations() {
        let temp_dir = TempDir::new().unwrap();
        let config = ServiceCacheConfig::disk(
            1_000_000,
            temp_dir.path().to_path_buf(),
            Duration::from_secs(3600),
            "test".to_string(),
        );

        let service = CacheService::start(config).await.unwrap();
        let cache = service.cache();

        // Set and get
        cache.set("key1", vec![1, 2, 3]).await.unwrap();
        let value = cache.get("key1").await.unwrap();
        assert_eq!(value, Some(vec![1, 2, 3]));

        service.shutdown().await;
    }

    #[tokio::test]
    async fn test_default_config() {
        let config = ServiceCacheConfig::default();
        let service = CacheService::start(config).await.unwrap();

        assert!(service.is_memory());
        assert_eq!(service.cache().max_size_bytes(), 2 * 1024 * 1024 * 1024);

        service.shutdown().await;
    }
}
