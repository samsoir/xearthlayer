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
//! │  │                                                         │      │
//! │  │  lit_disk_service (only with night lighting enabled)   │      │
//! │  │       └──► GC daemon (5% of the disk budget)           │      │
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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::cache::legacy::{remove_legacy_chunk_regions, LegacyChunkCleanup};
use crate::cache::{
    Cache, CacheService, DdsDiskCacheBridge, DiskCacheBridge, MemoryCacheBridge, ServiceCacheConfig,
};
use crate::config::{DEFAULT_DDS_DISK_RATIO, DEFAULT_DISK_CACHE_SIZE, DEFAULT_MEMORY_CACHE_SIZE};
use crate::metrics::MetricsClient;
use crate::service::config::ServiceConfig;
use crate::service::error::ServiceError;

/// Fixed share of `cache.disk_size` given to night lighting payloads.
pub const LIT_DISK_SHARE: f64 = 0.05;

/// Disk budgets per tier, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudgets {
    pub dds: u64,
    pub chunk: u64,
    /// `None` when night lighting is off.
    pub lit: Option<u64>,
}

/// Disk budgets per tier. Night lighting takes its fixed share first, so
/// the DDS/chunk ratio keeps its meaning and the sum never exceeds
/// `disk_size`; without night lighting the split is the one before it.
///
/// Total for any `dds_ratio`: the configuration rejects a ratio outside
/// 0.0 to 1.0, but this function does not rely on it (#401). A ratio
/// outside the range is clamped to it and NaN gives the default split, so
/// the sum of the budgets never exceeds `disk_size`.
pub fn disk_budgets(disk_size: u64, dds_ratio: f64, night_enabled: bool) -> DiskBudgets {
    let dds_ratio = if dds_ratio.is_nan() {
        DEFAULT_DDS_DISK_RATIO
    } else {
        dds_ratio.clamp(0.0, 1.0)
    };
    let lit = night_enabled.then_some((disk_size as f64 * LIT_DISK_SHARE) as u64);
    let rest = disk_size.saturating_sub(lit.unwrap_or(0));
    let dds = ((rest as f64 * dds_ratio) as u64).min(rest);
    DiskBudgets {
        dds,
        chunk: rest.saturating_sub(dds),
        lit,
    }
}

/// Cache layer encapsulating the three-tier cache hierarchy.
///
/// This struct owns all cache services and their bridge adapters,
/// providing a unified interface for cache management within the service.
///
/// The three tiers are:
/// - **Memory** (moka LRU): Fastest, serves active FUSE reads
/// - **DDS Disk** (LRU + GC): Persistent DDS tiles, avoids re-encoding
/// - **Chunk Disk** (LRU + GC): Raw imagery chunks, requires assembly + encoding
///
/// With night lighting enabled there is also a **Lit Disk** tier (5% of the
/// disk budget) holding generated lit texture payloads.
pub struct CacheLayer {
    /// `cache.directory`: the root the disk tiers live under.
    disk_dir: PathBuf,

    /// Memory cache service (owns LRU eviction).
    memory_service: CacheService,

    /// DDS disk cache service (encoded DDS tiles on disk).
    dds_disk_service: CacheService,

    /// Chunk disk cache service (raw imagery chunks on disk).
    chunk_disk_service: CacheService,

    /// Lit disk cache service (night lighting payloads); `None` when night
    /// lighting is off.
    lit_disk_service: Option<CacheService>,

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
    /// 2. Computes disk budgets: night lighting's 5% first (only when
    ///    enabled), then DDS/chunk from `dds_disk_ratio` of the rest
    /// 3. Creates DDS disk cache service at `{disk_dir}/{provider}/dds/`
    /// 4. Creates chunk disk cache service at `{disk_dir}/{provider}/chunks/`
    /// 5. When night lighting is enabled, creates the lit disk cache
    ///    service at `{disk_dir}/night/`
    /// 6. Creates bridge adapters for executor integration
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
        let budgets = disk_budgets(disk_size, dds_ratio, config.night_enabled());
        let dds_disk_budget = budgets.dds;
        let chunk_disk_budget = budgets.chunk;

        // Get cache directory with default
        let disk_dir = config
            .cache_directory()
            .cloned()
            .unwrap_or_else(crate::paths::tile_cache_dir);

        let gc_interval = Duration::from_secs(config.disk_gc_interval_secs());

        // Both ortho tiers live under the provider (#398): chunk keys carry no
        // provider, so a shared chunk tier would build one provider's tiles
        // from another's imagery.
        let dds_disk_dir = crate::cache::layout::dds_dir(&disk_dir, provider_name);
        let chunk_disk_dir = crate::cache::layout::chunk_dir(&disk_dir, provider_name);

        // 1. Start memory cache service (with metrics for self-reporting size)
        let memory_config =
            ServiceCacheConfig::memory(memory_size, None).with_metrics(metrics.clone());
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

        // 4. Start the lit disk cache service, only with night lighting on.
        // It reports no metrics: `DiskTier::Lit` has no counters yet.
        let lit_disk_service = match budgets.lit {
            Some(lit_budget) => {
                let lit_disk_dir = crate::cache::layout::lit_dir(&disk_dir);
                let lit_config = ServiceCacheConfig::disk(
                    lit_budget,
                    lit_disk_dir.clone(),
                    gc_interval,
                    "night".to_string(),
                )
                .as_lit_tier();

                let service = CacheService::start(lit_config).await.map_err(|e| {
                    ServiceError::CacheError(format!("Lit disk cache start failed: {}", e))
                })?;

                info!(
                    max_size_bytes = lit_budget,
                    directory = %lit_disk_dir.display(),
                    "Lit disk cache service started"
                );
                Some(service)
            }
            None => None,
        };

        // 5. Create bridge adapters
        let memory_bridge = Arc::new(MemoryCacheBridge::new(memory_service.cache()));
        let dds_disk_bridge = Arc::new(DdsDiskCacheBridge::new(dds_disk_service.cache()));
        let disk_bridge = Arc::new(DiskCacheBridge::with_metrics(
            chunk_disk_service.cache(),
            metrics,
        ));

        info!("CacheLayer initialized with three-tier cache bridges");

        Ok(Self {
            disk_dir,
            memory_service,
            dds_disk_service,
            chunk_disk_service,
            lit_disk_service,
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

    /// Get the lit disk cache provider for GC scheduler integration.
    ///
    /// `None` when night lighting is off.
    pub fn lit_disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.lit_disk_service
            .as_ref()
            .and_then(|s| s.disk_provider())
    }

    /// Get the raw lit disk cache for direct access.
    ///
    /// `None` when night lighting is off.
    pub fn lit_disk_cache(&self) -> Option<Arc<dyn Cache>> {
        self.lit_disk_service.as_ref().map(|s| s.cache())
    }

    /// Delete, on a blocking thread, the region directories the chunk tier
    /// left directly in `cache.directory` before it moved under its
    /// provider (#398). Nothing else in the directory is touched.
    ///
    /// Returns at once; the outcome is logged (removals at info, failures
    /// as warnings) and also returned through the handle, which callers may
    /// drop.
    pub fn spawn_legacy_chunk_cleanup(&self) -> tokio::task::JoinHandle<LegacyChunkCleanup> {
        let cache_dir = self.disk_dir.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = remove_legacy_chunk_regions(&cache_dir);
            if outcome.removed > 0 {
                info!(
                    removed = outcome.removed,
                    bytes_freed = outcome.bytes_freed,
                    directory = %cache_dir.display(),
                    "Removed legacy chunk cache regions from the cache root"
                );
            }
            if outcome.failed > 0 {
                tracing::warn!(
                    failed = outcome.failed,
                    directory = %cache_dir.display(),
                    "Some legacy chunk cache regions could not be removed"
                );
            }
            outcome
        })
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
        if let Some(lit) = self.lit_disk_service {
            lit.shutdown().await;
            info!("Lit disk cache service shut down");
        }

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
    use crate::cache::integrity::EXPECTED_DDS_SIZE;
    use crate::executor::{DdsDiskCache, DiskCache, MemoryCache};
    use bytes::Bytes;
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

    /// #398: the chunk tier lives under its provider, beside the DDS tier.
    #[tokio::test]
    async fn chunk_tier_lives_under_its_provider() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());

        let cache_layer = CacheLayer::new(&config, "Bing Maps", create_test_metrics())
            .await
            .unwrap();

        let chunk_provider = cache_layer.disk_provider().unwrap();
        assert_eq!(
            chunk_provider.directory().as_path(),
            crate::cache::layout::chunk_dir(temp_dir.path(), "Bing Maps").as_path()
        );

        cache_layer.shutdown().await;
    }

    /// #398: two providers sharing one `cache.directory` never read each
    /// other's chunks, so a provider switch cannot build tiles from the old
    /// provider's imagery.
    #[tokio::test]
    async fn two_providers_never_read_each_others_chunks() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());

        let bing = CacheLayer::new(&config, "Bing Maps", create_test_metrics())
            .await
            .unwrap();
        let go2 = CacheLayer::new(&config, "Google GO2", create_test_metrics())
            .await
            .unwrap();

        let bing_chunks = bing.disk_bridge();
        let go2_chunks = go2.disk_bridge();
        bing_chunks
            .put(100, 200, 15, 3, 4, Bytes::from_static(b"bing"))
            .await
            .unwrap();
        go2_chunks
            .put(100, 200, 15, 5, 6, Bytes::from_static(b"go2"))
            .await
            .unwrap();

        assert_eq!(
            bing_chunks.get(100, 200, 15, 3, 4).await,
            Some(Bytes::from_static(b"bing"))
        );
        assert_eq!(go2_chunks.get(100, 200, 15, 3, 4).await, None);
        assert_eq!(
            go2_chunks.get(100, 200, 15, 5, 6).await,
            Some(Bytes::from_static(b"go2"))
        );
        assert_eq!(bing_chunks.get(100, 200, 15, 5, 6).await, None);

        bing.shutdown().await;
        go2.shutdown().await;
    }

    /// #398: the root-level regions the chunk tier left behind are deleted
    /// on a blocking thread, and the tiers themselves are untouched.
    #[tokio::test]
    async fn legacy_chunk_cleanup_removes_root_regions_only() {
        let temp_dir = tempdir().unwrap();
        let legacy = temp_dir.path().join("+37-123");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("chunk_16_1_2_3_4.cache"), [0u8; 64]).unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let cache_layer = CacheLayer::new(&config, "Bing Maps", create_test_metrics())
            .await
            .unwrap();
        cache_layer
            .disk_bridge()
            .put(100, 200, 15, 0, 0, Bytes::from_static(b"kept"))
            .await
            .unwrap();

        let outcome = cache_layer.spawn_legacy_chunk_cleanup().await.unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.bytes_freed, 64);
        assert!(!legacy.exists());
        assert_eq!(
            cache_layer.disk_bridge().get(100, 200, 15, 0, 0).await,
            Some(Bytes::from_static(b"kept"))
        );

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_memory_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.memory_bridge();

        bridge.put(100, 200, 15, vec![1, 2, 3].into()).await;
        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(Bytes::from(vec![1, 2, 3])));

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn test_cache_layer_dds_disk_bridge() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let metrics = create_test_metrics();

        let cache_layer = CacheLayer::new(&config, "test", metrics).await.unwrap();
        let bridge = cache_layer.dds_disk_bridge();

        // This bridge is backed by the real DDS tier (`.as_dds_tier()`), so
        // a round trip must satisfy the DDS validator (#253) — arbitrary
        // bytes would now be discarded as corrupt and reported as a miss.
        let mut tile = vec![0u8; EXPECTED_DDS_SIZE];
        tile[0..4].copy_from_slice(b"DDS ");
        bridge.put(100, 200, 15, Bytes::from(tile.clone())).await;
        let result = bridge.get(100, 200, 15).await;
        assert_eq!(result, Some(Bytes::from(tile)));

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

        bridge
            .put(100, 200, 15, 0, 0, vec![1, 2, 3].into())
            .await
            .unwrap();
        let result = bridge.get(100, 200, 15, 0, 0).await;
        assert_eq!(result, Some(Bytes::from(vec![1, 2, 3])));

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

    #[test]
    fn night_off_leaves_the_split_as_it_was() {
        let b = disk_budgets(10_000_000, 0.6, false);
        assert_eq!(
            b,
            DiskBudgets {
                dds: 6_000_000,
                chunk: 4_000_000,
                lit: None
            }
        );
    }

    #[test]
    fn night_on_takes_five_percent_first() {
        let b = disk_budgets(10_000_000, 0.6, true);
        assert_eq!(b.lit, Some(500_000));
        assert_eq!(b.dds, 5_700_000);
        assert_eq!(b.chunk, 3_800_000);
        assert_eq!(
            b.dds + b.chunk + b.lit.unwrap(),
            10_000_000,
            "never over budget"
        );
    }

    /// #401: a ratio outside 0.0 to 1.0 overflowed `rest - dds` (a panic in
    /// debug, a chunk budget near 2^64 in release) or starved the DDS tier.
    #[test]
    fn a_ratio_outside_zero_to_one_never_exceeds_the_disk_size() {
        for night in [false, true] {
            for ratio in [
                1.5,
                -0.5,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MAX,
            ] {
                let b = disk_budgets(10_000_000, ratio, night);
                let total = b.dds as u128 + b.chunk as u128 + b.lit.unwrap_or(0) as u128;
                assert!(
                    total <= 10_000_000,
                    "ratio {ratio}, night {night}: {b:?} exceeds the disk size"
                );
            }
        }
    }

    #[test]
    fn a_ratio_outside_zero_to_one_is_clamped() {
        assert_eq!(
            disk_budgets(10_000_000, 1.5, false),
            disk_budgets(10_000_000, 1.0, false)
        );
        assert_eq!(
            disk_budgets(10_000_000, f64::INFINITY, true),
            disk_budgets(10_000_000, 1.0, true)
        );
        assert_eq!(
            disk_budgets(10_000_000, -0.5, false),
            disk_budgets(10_000_000, 0.0, false)
        );
    }

    #[test]
    fn a_nan_ratio_gives_the_default_split() {
        for night in [false, true] {
            assert_eq!(
                disk_budgets(10_000_000, f64::NAN, night),
                disk_budgets(10_000_000, DEFAULT_DDS_DISK_RATIO, night)
            );
        }
    }

    #[tokio::test]
    async fn night_off_creates_no_lit_tier_or_directory() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let cache_layer = CacheLayer::new(&config, "test", create_test_metrics())
            .await
            .unwrap();

        assert!(cache_layer.lit_disk_cache().is_none());
        assert!(cache_layer.lit_disk_provider().is_none());
        assert!(!temp_dir.path().join("night").exists());

        cache_layer.shutdown().await;
    }

    #[tokio::test]
    async fn night_on_creates_the_lit_tier_under_night() {
        let temp_dir = tempdir().unwrap();
        let config = ServiceConfig::builder()
            .cache_directory(temp_dir.path().to_path_buf())
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .cache_dds_disk_ratio(0.6)
            .disk_gc_interval(3600)
            .night_enabled(true)
            .build();
        let cache_layer = CacheLayer::new(&config, "test", create_test_metrics())
            .await
            .unwrap();

        let provider = cache_layer.lit_disk_provider().unwrap();
        assert_eq!(provider.directory(), &temp_dir.path().join("night"));
        assert_eq!(provider.max_size_bytes(), 500_000);

        let cache = cache_layer.lit_disk_cache().unwrap();
        let key = "lit:16:25312:10528";
        cache
            .set(key, Bytes::from_static(b"XELLpayload"))
            .await
            .unwrap();
        assert_eq!(
            cache.get(key).await.unwrap(),
            Some(Bytes::from_static(b"XELLpayload"))
        );

        cache_layer.shutdown().await;
    }
}
