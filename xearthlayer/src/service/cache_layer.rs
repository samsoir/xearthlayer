//! Cache layer for service-owned caches.
//!
//! `CacheLayer` encapsulates both memory and disk cache services with metrics
//! integration, providing a clean interface for service-level cache management.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     XEarthLayerService                       │
//! │                                                              │
//! │  ┌─────────────────────────────────────────────────────┐    │
//! │  │                    CacheLayer                        │    │
//! │  │                                                      │    │
//! │  │  memory_service ──────► MemoryCacheBridge ──┐       │    │
//! │  │                                             │       │    │
//! │  │  disk_service ────────► DiskCacheBridge ────┼──► Executor
//! │  │       │                                     │       │    │
//! │  │       └──► GC daemon (internal)            │       │    │
//! │  └─────────────────────────────────────────────────────┘    │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{CacheLayer, ServiceConfig};
//! use xearthlayer::metrics::MetricsClient;
//!
//! let cache_layer = CacheLayer::new(&config, "bing", metrics).await?;
//!
//! // Get bridges for executor integration
//! let memory_bridge = cache_layer.memory_bridge();
//! let disk_bridge = cache_layer.disk_bridge();
//!
//! // Later: graceful shutdown
//! cache_layer.shutdown().await;
//! ```

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::cache::{Cache, CacheService, DiskCacheBridge, MemoryCacheBridge, ServiceCacheConfig};
use crate::config::{DEFAULT_DISK_CACHE_SIZE, DEFAULT_MEMORY_CACHE_SIZE};
use crate::metrics::MetricsClient;
use crate::service::config::ServiceConfig;
use crate::service::error::ServiceError;

/// Cache layer encapsulating memory and disk cache services.
///
/// This struct owns both cache services and their bridge adapters,
/// providing a unified interface for cache management within the service.
pub struct CacheLayer {
    /// Memory cache service (owns LRU eviction).
    memory_service: CacheService,

    /// Disk cache service (owns GC daemon).
    disk_service: CacheService,

    /// Memory cache bridge (implements executor::MemoryCache).
    memory_bridge: Arc<MemoryCacheBridge>,

    /// Disk cache bridge (implements executor::DiskCache).
    disk_bridge: Arc<DiskCacheBridge>,
}

impl CacheLayer {
    /// Create a new cache layer with the given configuration.
    ///
    /// This method:
    /// 1. Creates memory cache service using `ServiceCacheConfig::memory()`
    /// 2. Creates disk cache service using `ServiceCacheConfig::disk()` with metrics
    /// 3. Creates bridge adapters with metrics for executor integration
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration with cache settings
    /// * `provider_name` - Provider name for disk cache subdirectory
    /// * `metrics` - Metrics client for cache statistics
    ///
    /// # Errors
    ///
    /// Returns an error if any cache service fails to start.
    pub async fn new(
        config: &ServiceConfig,
        provider_name: &str,
        metrics: MetricsClient,
    ) -> Result<Self, ServiceError> {
        // Get cache sizes with defaults
        let memory_size = config
            .cache_memory_size()
            .unwrap_or(DEFAULT_MEMORY_CACHE_SIZE) as u64;
        let disk_size = config.cache_disk_size().unwrap_or(DEFAULT_DISK_CACHE_SIZE) as u64;

        // Get cache directory with default
        let disk_dir = config.cache_directory().cloned().unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("xearthlayer")
        });

        let gc_interval = Duration::from_secs(config.disk_gc_interval_secs());

        // 1. Start memory cache service
        let memory_config = ServiceCacheConfig::memory(memory_size, None);
        let memory_service = CacheService::start(memory_config)
            .await
            .map_err(|e| ServiceError::CacheError(format!("Memory cache start failed: {}", e)))?;

        info!(
            max_size_bytes = memory_size,
            "Memory cache service started in CacheLayer"
        );

        // 2. Start disk cache service with metrics
        let disk_config = ServiceCacheConfig::disk(
            disk_size,
            disk_dir.clone(),
            gc_interval,
            provider_name.to_string(),
        )
        .with_metrics(metrics.clone());

        let disk_service = CacheService::start(disk_config)
            .await
            .map_err(|e| ServiceError::CacheError(format!("Disk cache start failed: {}", e)))?;

        info!(
            max_size_bytes = disk_size,
            directory = %disk_dir.display(),
            gc_interval_secs = gc_interval.as_secs(),
            provider = %provider_name,
            "Disk cache service started in CacheLayer with internal GC daemon"
        );

        // 3. Create bridge adapters with metrics
        let memory_bridge = Arc::new(MemoryCacheBridge::with_metrics(
            memory_service.cache(),
            metrics.clone(),
        ));

        let disk_bridge = Arc::new(DiskCacheBridge::with_metrics(disk_service.cache(), metrics));

        info!("CacheLayer initialized with memory and disk cache bridges");

        Ok(Self {
            memory_service,
            disk_service,
            memory_bridge,
            disk_bridge,
        })
    }

    /// Get the memory cache bridge for executor integration.
    ///
    /// This bridge implements `executor::MemoryCache` and can be passed
    /// to the job factory and executor daemon.
    pub fn memory_bridge(&self) -> Arc<MemoryCacheBridge> {
        Arc::clone(&self.memory_bridge)
    }

    /// Get the disk cache bridge for executor integration.
    ///
    /// This bridge implements `executor::DiskCache` and can be passed
    /// to the job factory and executor daemon.
    pub fn disk_bridge(&self) -> Arc<DiskCacheBridge> {
        Arc::clone(&self.disk_bridge)
    }

    /// Get the raw memory cache for direct access.
    ///
    /// This provides access to the underlying generic cache implementation,
    /// useful for size queries or advanced operations.
    pub fn raw_memory_cache(&self) -> Arc<dyn Cache> {
        self.memory_service.cache()
    }

    /// Get the raw disk cache for direct access.
    pub fn raw_disk_cache(&self) -> Arc<dyn Cache> {
        self.disk_service.cache()
    }

    /// Get the disk cache provider for GC scheduler integration.
    ///
    /// Returns the underlying `DiskCacheProvider` which provides access to:
    /// - LRU index for cache size tracking
    /// - GC configuration (needs_gc, gc_target_size)
    /// - Cache directory path
    ///
    /// Returns `None` if the disk service is not a disk cache (shouldn't happen).
    pub fn disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.disk_service.disk_provider()
    }

    /// Scan and report the initial disk cache size.
    ///
    /// This scans the disk cache directory to count existing cached data.
    /// Call this during service initialization when the UI can display
    /// progress feedback.
    ///
    /// Returns the scanned size in bytes.
    pub async fn scan_disk_cache_size(&self) -> u64 {
        match self.disk_service.scan_initial_size().await {
            Ok(size) => {
                info!(initial_size_bytes = size, "Disk cache initial size scanned");
                size
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to scan disk cache size");
                0
            }
        }
    }

    /// Shutdown the cache layer gracefully.
    ///
    /// This shuts down cache services in reverse order of startup,
    /// ensuring all pending operations complete and GC daemons stop.
    pub async fn shutdown(self) {
        info!("Shutting down CacheLayer");

        // Shutdown in reverse order
        self.disk_service.shutdown().await;
        info!("Disk cache service shut down");

        self.memory_service.shutdown().await;
        info!("Memory cache service shut down");

        info!("CacheLayer shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{DiskCache, MemoryCache};
    use tempfile::tempdir;

    fn create_test_config(cache_dir: std::path::PathBuf) -> ServiceConfig {
        ServiceConfig::builder()
            .cache_directory(cache_dir)
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .disk_gc_interval(3600)
            .build()
    }

    fn create_test_metrics() -> MetricsClient {
        let runtime_handle = tokio::runtime::Handle::current();
        let metrics_system = crate::metrics::MetricsSystem::new(&runtime_handle);
        metrics_system.client()
    }

    #[tokio::test]
    async fn test_cache_layer_start_and_shutdown() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();

        // Verify services are running (size should be 0 initially)
        assert_eq!(cache_layer.raw_memory_cache().size_bytes(), 0);

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_memory_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.memory_bridge();

        // Use the bridge
        bridge.put(100, 200, 15, vec![1, 2, 3]).await;
        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(vec![1, 2, 3]));

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_disk_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.disk_bridge();

        // Use the bridge
        bridge.put(100, 200, 15, 0, 0, vec![1, 2, 3]).await.unwrap();
        let result = bridge.get(100, 200, 15, 0, 0).await;
        assert_eq!(result, Some(vec![1, 2, 3]));

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_scan_disk_size() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();

        // Should return 0 for empty cache
        let size = cache_layer.scan_disk_cache_size().await;
        assert_eq!(size, 0);

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_default_directories() {
        // Test with config that has no cache directory set
        let config = ServiceConfig::builder()
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .disk_gc_interval(3600)
            .build();
        let metrics = create_test_metrics();

        // This should use the default cache directory
        let result = CacheLayer::new(&config, "test", metrics).await;

        // It may fail if the default directory doesn't exist or isn't writable,
        // but it shouldn't panic
        if let Ok(cache_layer) = result {
            cache_layer.shutdown().await;
        }
    }
}
