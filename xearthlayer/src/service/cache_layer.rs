//! Cache layer for service-owned caches.
//!
//! `CacheLayer` encapsulates the three-tier cache hierarchy with metrics
//! integration, providing a clean interface for service-level cache management.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │                      XEarthLayerService                          │
//! │                                                                   │
//! │  ┌────────────────────────────────────────────────────────┐      │
//! │  │                      CacheLayer                         │      │
//! │  │                                                         │      │
//! │  │  memory_service ──────► MemoryCacheBridge ──────┐      │      │
//! │  │                                                  │      │      │
//! │  │  dds_disk_service ────► DdsDiskCacheBridge ─────┼──► Executor │
//! │  │       │                                          │      │      │
//! │  │       └──► GC daemon (DDS budget)               │      │      │
//! │  │                                                  │      │      │
//! │  │  chunk_disk_service ──► DiskCacheBridge ────────┘      │      │
//! │  │       │                                                 │      │
//! │  │       └──► GC daemon (chunk budget)                    │      │
//! │  └────────────────────────────────────────────────────────┘      │
//! └──────────────────────────────────────────────────────────────────┘
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
//! let dds_disk_bridge = cache_layer.dds_disk_bridge();
//! let disk_bridge = cache_layer.disk_bridge();
//!
//! // Later: graceful shutdown
//! cache_layer.shutdown().await;
//! ```

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::cache::{
    Cache, CacheService, DdsDiskCacheBridge, DiskCacheBridge, MemoryCacheBridge, ServiceCacheConfig,
};
use crate::config::{DEFAULT_DDS_DISK_RATIO, DEFAULT_DISK_CACHE_SIZE, DEFAULT_MEMORY_CACHE_SIZE};
use crate::metrics::MetricsClient;
use crate::service::config::ServiceConfig;
use crate::service::error::ServiceError;

/// Cache layer encapsulating the three-tier cache hierarchy.
///
/// This struct owns all cache services and their bridge adapters,
/// providing a unified interface for cache management within the service.
///
/// The three tiers are:
/// - **Memory** (moka LRU): Fastest, serves active FUSE reads
/// - **DDS Disk** (LRU + GC): Persistent DDS tiles, avoids re-encoding
/// - **Chunk Disk** (LRU + GC): Raw imagery chunks, requires assembly + encoding
pub struct CacheLayer {
    /// Memory cache service (owns LRU eviction).
    memory_service: CacheService,

    /// DDS disk cache service (encoded DDS tiles on disk).
    dds_disk_service: CacheService,

    /// Chunk disk cache service (raw imagery chunks on disk).
    chunk_disk_service: CacheService,

    /// Memory cache bridge (implements executor::MemoryCache).
    memory_bridge: Arc<MemoryCacheBridge>,

    /// DDS disk cache bridge (implements executor::DdsDiskCache).
    dds_disk_bridge: Arc<DdsDiskCacheBridge>,

    /// Chunk disk cache bridge (implements executor::DiskCache).
    disk_bridge: Arc<DiskCacheBridge>,
}

impl CacheLayer {
    /// Create a new cache layer with the given configuration.
    ///
    /// This method:
    /// 1. Creates memory cache service
    /// 2. Computes DDS/chunk disk budgets from `disk_size * dds_disk_ratio`
    /// 3. Creates DDS disk cache service at `{disk_dir}/{provider}/dds/`
    /// 4. Creates chunk disk cache service at `{disk_dir}/{provider}/`
    /// 5. Creates bridge adapters for executor integration
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
        let dds_ratio = config
            .cache_dds_disk_ratio()
            .unwrap_or(DEFAULT_DDS_DISK_RATIO);

        // Compute per-tier disk budgets
        let dds_disk_budget = (disk_size as f64 * dds_ratio) as u64;
        let chunk_disk_budget = disk_size - dds_disk_budget;

        // Get cache directory with default
        let disk_dir = config.cache_directory().cloned().unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("xearthlayer")
        });

        let gc_interval = Duration::from_secs(config.disk_gc_interval_secs());

        // DDS disk goes into a dedicated subdirectory
        let dds_disk_dir = disk_dir.join(provider_name).join("dds");
        // Chunks keep the existing provider directory layout
        let chunk_disk_dir = disk_dir.clone();

        // 1. Start memory cache service
        let memory_config = ServiceCacheConfig::memory(memory_size, None);
        let memory_service = CacheService::start(memory_config)
            .await
            .map_err(|e| ServiceError::CacheError(format!("Memory cache start failed: {}", e)))?;

        info!(
            max_size_bytes = memory_size,
            "Memory cache service started in CacheLayer"
        );

        // 2. Start DDS disk cache service
        let dds_disk_config = ServiceCacheConfig::disk(
            dds_disk_budget,
            dds_disk_dir.clone(),
            gc_interval,
            "dds".to_string(),
        )
        .as_dds_tier()
        .with_metrics(metrics.clone());

        let dds_disk_service = CacheService::start(dds_disk_config)
            .await
            .map_err(|e| ServiceError::CacheError(format!("DDS disk cache start failed: {}", e)))?;

        info!(
            max_size_bytes = dds_disk_budget,
            directory = %dds_disk_dir.display(),
            "DDS disk cache service started (ratio={dds_ratio})"
        );

        // 3. Start chunk disk cache service with metrics
        let chunk_disk_config = ServiceCacheConfig::disk(
            chunk_disk_budget,
            chunk_disk_dir.clone(),
            gc_interval,
            provider_name.to_string(),
        )
        .with_metrics(metrics.clone());

        let chunk_disk_service = CacheService::start(chunk_disk_config).await.map_err(|e| {
            ServiceError::CacheError(format!("Chunk disk cache start failed: {}", e))
        })?;

        info!(
            max_size_bytes = chunk_disk_budget,
            directory = %chunk_disk_dir.display(),
            gc_interval_secs = gc_interval.as_secs(),
            provider = %provider_name,
            "Chunk disk cache service started in CacheLayer"
        );

        // 4. Create bridge adapters
        let memory_bridge = Arc::new(MemoryCacheBridge::new(memory_service.cache()));
        let dds_disk_bridge = Arc::new(DdsDiskCacheBridge::new(dds_disk_service.cache()));
        let disk_bridge = Arc::new(DiskCacheBridge::with_metrics(
            chunk_disk_service.cache(),
            metrics,
        ));

        info!("CacheLayer initialized with three-tier cache bridges");

        Ok(Self {
            memory_service,
            dds_disk_service,
            chunk_disk_service,
            memory_bridge,
            dds_disk_bridge,
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

    /// Get the DDS disk cache bridge for executor integration.
    ///
    /// This bridge implements `executor::DdsDiskCache` and can be passed
    /// to the executor daemon for the read path and to the build task for writes.
    pub fn dds_disk_bridge(&self) -> Arc<DdsDiskCacheBridge> {
        Arc::clone(&self.dds_disk_bridge)
    }

    /// Get the chunk disk cache bridge for executor integration.
    ///
    /// This bridge implements `executor::DiskCache` and can be passed
    /// to the job factory for chunk download caching.
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

    /// Get the raw chunk disk cache for direct access.
    pub fn raw_disk_cache(&self) -> Arc<dyn Cache> {
        self.chunk_disk_service.cache()
    }

    /// Get the raw DDS disk cache for direct access.
    pub fn raw_dds_disk_cache(&self) -> Arc<dyn Cache> {
        self.dds_disk_service.cache()
    }

    /// Get the chunk disk cache provider for GC scheduler integration.
    ///
    /// Returns the underlying `DiskCacheProvider` which provides access to:
    /// - LRU index for cache size tracking
    /// - GC configuration (needs_gc, gc_target_size)
    /// - Cache directory path
    ///
    /// Returns `None` if the disk service is not a disk cache (shouldn't happen).
    pub fn disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.chunk_disk_service.disk_provider()
    }

    /// Get the DDS disk cache provider for GC scheduler integration.
    ///
    /// Returns the underlying `DiskCacheProvider` for the DDS tier.
    pub fn dds_disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.dds_disk_service.disk_provider()
    }

    /// Scan and report the initial disk cache sizes.
    ///
    /// Scans both DDS and chunk disk cache directories to count existing data.
    /// Call this during service initialization when the UI can display
    /// progress feedback.
    ///
    /// Returns the combined scanned size in bytes.
    pub async fn scan_disk_cache_size(&self) -> u64 {
        let mut total = 0u64;

        match self.dds_disk_service.scan_initial_size().await {
            Ok(size) => {
                info!(
                    initial_size_bytes = size,
                    "DDS disk cache initial size scanned"
                );
                total += size;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to scan DDS disk cache size");
            }
        }

        match self.chunk_disk_service.scan_initial_size().await {
            Ok(size) => {
                info!(
                    initial_size_bytes = size,
                    "Chunk disk cache initial size scanned"
                );
                total += size;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to scan chunk disk cache size");
            }
        }

        total
    }

    /// Shutdown the cache layer gracefully.
    ///
    /// This shuts down cache services in reverse order of startup,
    /// ensuring all pending operations complete and GC daemons stop.
    pub async fn shutdown(self) {
        info!("Shutting down CacheLayer");

        // Shutdown in reverse order
        self.chunk_disk_service.shutdown().await;
        info!("Chunk disk cache service shut down");

        self.dds_disk_service.shutdown().await;
        info!("DDS disk cache service shut down");

        self.memory_service.shutdown().await;
        info!("Memory cache service shut down");

        info!("CacheLayer shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{DdsDiskCache, DiskCache, MemoryCache};
    use tempfile::tempdir;

    fn create_test_config(cache_dir: std::path::PathBuf) -> ServiceConfig {
        ServiceConfig::builder()
            .cache_directory(cache_dir)
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .cache_dds_disk_ratio(0.6)
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
    async fn test_cache_layer_budget_calculation() {
        let temp_dir = tempdir().unwrap();
        let config = ServiceConfig::builder()
            .cache_directory(temp_dir.path().to_path_buf())
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .cache_dds_disk_ratio(0.6)
            .disk_gc_interval(3600)
            .build();
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();

        // DDS disk: 60% of 10MB = 6MB
        let dds_provider = cache_layer.dds_disk_provider().unwrap();
        assert_eq!(dds_provider.max_size_bytes(), 6_000_000);

        // Chunk disk: 40% of 10MB = 4MB
        let chunk_provider = cache_layer.disk_provider().unwrap();
        assert_eq!(chunk_provider.max_size_bytes(), 4_000_000);

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_dds_disk_directory() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "bing", metrics).await.unwrap();

        // DDS disk should be under {cache_dir}/bing/dds/
        let dds_provider = cache_layer.dds_disk_provider().unwrap();
        let expected_dds_dir = temp_dir.path().join("bing").join("dds");
        assert_eq!(dds_provider.directory(), &expected_dds_dir);

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_memory_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.memory_bridge();

        bridge.put(100, 200, 15, vec![1, 2, 3]).await;
        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(vec![1, 2, 3]));

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_dds_disk_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.dds_disk_bridge();

        bridge.put(100, 200, 15, vec![1, 2, 3]).await;
        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(vec![1, 2, 3]));

        assert!(bridge.contains(100, 200, 15).await);
        assert!(!bridge.contains(999, 999, 15).await);

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_disk_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.disk_bridge();

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

        // Should return 0 for empty caches
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
