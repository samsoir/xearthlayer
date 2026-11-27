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
//! │  │  disk (None with `run --no-cache`):              │      │      │
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
//! With disk caching off (`ServiceConfig::disk_cache_enabled` false, which
//! is `run --no-cache`) only the memory tier runs: no disk tier starts, and
//! this layer creates, reads, writes and deletes nothing under
//! `cache.directory`. (The scenery and ortho union index caches, which are not
//! tiers and are not this layer's, still load and save as usual.)
//! [`CacheLayer::executor_disk_caches`] then answers
//! [`ExecutorDiskCaches::Off`], and the executor gets disk caches that
//! always miss.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{CacheLayer, ServiceConfig};
//! use xearthlayer::metrics::MetricsClient;
//!
//! let cache_layer = CacheLayer::new(&config, "bing", metrics).await?;
//!
//! // Get the caches for executor integration
//! let memory_bridge = cache_layer.memory_bridge();
//! let disk_caches = cache_layer.executor_disk_caches();
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
use crate::night::LitMemoryBudget;
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

/// The directory the disk tiers live under: `cache.directory`, else the
/// platform cache directory.
fn disk_root(config: &ServiceConfig) -> PathBuf {
    config
        .cache_directory()
        .cloned()
        .unwrap_or_else(crate::paths::tile_cache_dir)
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

/// Night lighting's share of `cache.memory_size`, in percent, taken out of
/// it once the share reaches [`LitMemoryBudget::FLOOR`] (#432).
pub const LIT_MEMORY_SHARE_PERCENT: u64 = 15;

/// The tails' share of night lighting's memory above the floor, in
/// percent; the heads take the rest.
pub const LIT_TAIL_SHARE_PERCENT: u64 = 40;

/// Memory budgets, in bytes: the ortho memory tier's and night lighting's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudgets {
    /// The ortho memory tier.
    pub ortho: u64,
    /// Night lighting's payload memory; `None` when night lighting is off.
    pub lit: Option<LitMemoryBudget>,
}

/// `percent` of `bytes`, rounded down, for any `bytes`.
fn percent_of(bytes: u64, percent: u64) -> u64 {
    (u128::from(bytes) * u128::from(percent) / 100) as u64
}

/// Memory budgets for `memory_size` (`cache.memory_size`), XEL's total
/// memory cache.
///
/// - Night lighting off: the ortho tier gets all of it.
/// - Night lighting on, its share ([`LIT_MEMORY_SHARE_PERCENT`]) below
///   [`LitMemoryBudget::FLOOR`]'s total (`memory_size` below about
///   1.67 GiB): night lighting gets the floor on top, and the ortho tier
///   still gets all of `memory_size`, as before the share.
/// - At or above it: the share is taken out of `memory_size`, the ortho
///   tier gets the rest, and the share is split [`LIT_TAIL_SHARE_PERCENT`]
///   tail, the rest head, the tail never below the floor's (so up to a
///   share of 480 MiB the tail stays at the floor's and the head takes the
///   rest). The floor's two parts sum to the share at the crossover, so
///   neither part is ever below its floor value, and both only grow with
///   `memory_size`.
///
/// Across the crossover the ortho tier falls from all of `memory_size` to
/// 85% of it: below, night lighting is added on top; above, taken out.
pub fn memory_budgets(memory_size: u64, night_enabled: bool) -> MemoryBudgets {
    if !night_enabled {
        return MemoryBudgets {
            ortho: memory_size,
            lit: None,
        };
    }
    let floor = LitMemoryBudget::FLOOR;
    let share = percent_of(memory_size, LIT_MEMORY_SHARE_PERCENT);
    if share < floor.total() {
        return MemoryBudgets {
            ortho: memory_size,
            lit: Some(floor),
        };
    }
    let tail = percent_of(share, LIT_TAIL_SHARE_PERCENT).max(floor.tail);
    MemoryBudgets {
        ortho: memory_size - share,
        lit: Some(LitMemoryBudget {
            tail,
            head: share - tail,
        }),
    }
}

/// The disk caches the executor is given: the layer's tiers, or none.
///
/// `RuntimeBuilder::build_with_cache_service` maps [`Off`](Self::Off) to
/// `NullDdsDiskCache` and `NullDiskCache`, which always miss and drop
/// writes, so demand, prefetch and the prefetch lane's cached tile probe
/// all see "not cached".
pub enum ExecutorDiskCaches {
    /// Disk caching is on: the DDS and chunk tiers' bridges.
    Persistent {
        dds: Arc<DdsDiskCacheBridge>,
        chunks: Arc<DiskCacheBridge>,
    },
    /// Disk caching is off (`run --no-cache`).
    Off,
}

/// The disk tiers, all started together or not at all.
struct DiskTiers {
    /// `cache.directory`: the root the disk tiers live under.
    cache_dir: PathBuf,

    /// DDS disk cache service (encoded DDS tiles on disk).
    dds_disk_service: CacheService,

    /// Chunk disk cache service (raw imagery chunks on disk).
    chunk_disk_service: CacheService,

    /// Lit disk cache service (night lighting payloads); `None` when night
    /// lighting is off.
    lit_disk_service: Option<CacheService>,

    /// DDS disk cache bridge (implements executor::DdsDiskCache).
    dds_disk_bridge: Arc<DdsDiskCacheBridge>,

    /// Chunk disk cache bridge (implements executor::DiskCache).
    disk_bridge: Arc<DiskCacheBridge>,
}

impl DiskTiers {
    /// Start the DDS, chunk and (with night lighting) lit disk tiers.
    async fn start(
        config: &ServiceConfig,
        provider_name: &str,
        metrics: MetricsClient,
    ) -> Result<Self, ServiceError> {
        let disk_size = config.cache_disk_size().unwrap_or(DEFAULT_DISK_CACHE_SIZE) as u64;
        let dds_ratio = config
            .cache_dds_disk_ratio()
            .unwrap_or(DEFAULT_DDS_DISK_RATIO);

        // Compute per-tier disk budgets
        let budgets = disk_budgets(disk_size, dds_ratio, config.night_enabled());
        let dds_disk_budget = budgets.dds;
        let chunk_disk_budget = budgets.chunk;

        let cache_dir = disk_root(config);

        let gc_interval = Duration::from_secs(config.disk_gc_interval_secs());

        // Both ortho tiers live under the provider (#398): chunk keys carry no
        // provider, so a shared chunk tier would build one provider's tiles
        // from another's imagery.
        let dds_disk_dir = crate::cache::layout::dds_dir(&cache_dir, provider_name);
        let chunk_disk_dir = crate::cache::layout::chunk_dir(&cache_dir, provider_name);

        // DDS disk cache service
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

        // Chunk disk cache service with metrics
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

        // The lit disk cache service, only with night lighting on.
        // It reports no metrics: `DiskTier::Lit` has no counters yet.
        let lit_disk_service = match budgets.lit {
            Some(lit_budget) => {
                let lit_disk_dir = crate::cache::layout::lit_dir(&cache_dir);
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

        let dds_disk_bridge = Arc::new(DdsDiskCacheBridge::new(dds_disk_service.cache()));
        let disk_bridge = Arc::new(DiskCacheBridge::with_metrics(
            chunk_disk_service.cache(),
            metrics,
        ));

        Ok(Self {
            cache_dir,
            dds_disk_service,
            chunk_disk_service,
            lit_disk_service,
            dds_disk_bridge,
            disk_bridge,
        })
    }

    async fn shutdown(self) {
        if let Some(lit) = self.lit_disk_service {
            lit.shutdown().await;
            info!("Lit disk cache service shut down");
        }

        self.chunk_disk_service.shutdown().await;
        info!("Chunk disk cache service shut down");

        self.dds_disk_service.shutdown().await;
        info!("DDS disk cache service shut down");
    }
}

/// Cache layer encapsulating the three-tier cache hierarchy.
///
/// This struct owns all cache services and their bridge adapters,
/// providing a unified interface for cache management within the service.
///
/// The three tiers are:
/// - **Memory** (moka, TinyLFU): Fastest, serves active FUSE reads
/// - **DDS Disk** (LRU + GC): Persistent DDS tiles, avoids re-encoding
/// - **Chunk Disk** (LRU + GC): Raw imagery chunks, requires assembly + encoding
///
/// With night lighting enabled there is also a **Lit Disk** tier (5% of the
/// disk budget) holding generated lit texture payloads.
///
/// With disk caching off (`run --no-cache`) only the memory tier exists and
/// every disk accessor answers `None`.
pub struct CacheLayer {
    /// Memory cache service (owns its eviction, TinyLFU, #405).
    memory_service: CacheService,

    /// Memory cache bridge (implements executor::MemoryCache).
    memory_bridge: Arc<MemoryCacheBridge>,

    /// Night lighting's payload memory, decided with the memory tier's;
    /// `None` when night lighting is off.
    lit_memory: Option<LitMemoryBudget>,

    /// The disk tiers; `None` when disk caching is off.
    disk: Option<DiskTiers>,
}

impl CacheLayer {
    /// Create a new cache layer with the given configuration.
    ///
    /// This method:
    /// 1. Creates memory cache service, sized by [`memory_budgets`]: all of
    ///    `cache.memory_size`, or with night lighting on and its share at or
    ///    above the floor, what night lighting's share leaves
    /// 2. When disk caching is on (`ServiceConfig::disk_cache_enabled`):
    ///    - computes disk budgets: night lighting's 5% first (only when
    ///      enabled), then DDS/chunk from `dds_disk_ratio` of the rest
    ///    - creates DDS disk cache service at `{disk_dir}/{provider}/dds/`
    ///    - creates chunk disk cache service at `{disk_dir}/{provider}/chunks/`
    ///    - when night lighting is enabled, creates the lit disk cache
    ///      service at `{disk_dir}/night/`
    /// 3. Creates bridge adapters for executor integration
    ///
    /// With disk caching off, step 2 is skipped entirely: nothing under
    /// `cache.directory` is created or read.
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
        let memory_size = config
            .cache_memory_size()
            .unwrap_or(DEFAULT_MEMORY_CACHE_SIZE) as u64;
        // Night lighting's memory comes out of `cache.memory_size` above
        // its floor (#432); `--no-cache` keeps both, as it keeps memory.
        let memory_budgets = memory_budgets(memory_size, config.night_enabled());
        match memory_budgets.lit {
            Some(lit) => info!(
                memory_size,
                ortho_memory_bytes = memory_budgets.ortho,
                lit_tail_bytes = lit.tail,
                lit_head_bytes = lit.head,
                "Memory budgets: ortho memory tier and night lighting"
            ),
            None => info!(
                memory_size,
                ortho_memory_bytes = memory_budgets.ortho,
                "Memory budgets: ortho memory tier (night lighting off)"
            ),
        }

        // Memory cache service (with metrics for self-reporting size)
        let memory_config =
            ServiceCacheConfig::memory(memory_budgets.ortho, None).with_metrics(metrics.clone());
        let memory_service = CacheService::start(memory_config)
            .await
            .map_err(|e| ServiceError::CacheError(format!("Memory cache start failed: {}", e)))?;

        info!(
            max_size_bytes = memory_budgets.ortho,
            "Memory cache service started in CacheLayer"
        );

        let disk = if config.disk_cache_enabled() {
            Some(DiskTiers::start(config, provider_name, metrics).await?)
        } else {
            info!("Disk caching off for this session (--no-cache): memory cache only");
            None
        };

        let memory_bridge = Arc::new(MemoryCacheBridge::new(memory_service.cache()));

        info!(disk_caching = disk.is_some(), "CacheLayer initialized");

        Ok(Self {
            memory_service,
            memory_bridge,
            lit_memory: memory_budgets.lit,
            disk,
        })
    }

    /// Night lighting's payload memory this session (see
    /// [`memory_budgets`]); `None` when night lighting is off.
    pub fn lit_memory_budget(&self) -> Option<LitMemoryBudget> {
        self.lit_memory
    }

    /// Whether the disk tiers run this session (`false` with `--no-cache`).
    pub fn disk_caching(&self) -> bool {
        self.disk.is_some()
    }

    /// The disk caches to hand the executor: the DDS and chunk tiers, or
    /// [`ExecutorDiskCaches::Off`] when disk caching is off.
    pub fn executor_disk_caches(&self) -> ExecutorDiskCaches {
        match &self.disk {
            Some(disk) => ExecutorDiskCaches::Persistent {
                dds: Arc::clone(&disk.dds_disk_bridge),
                chunks: Arc::clone(&disk.disk_bridge),
            },
            None => ExecutorDiskCaches::Off,
        }
    }

    /// Get the memory cache bridge for executor integration.
    ///
    /// This bridge implements `executor::MemoryCache` and can be passed
    /// to the job factory and executor daemon.
    pub fn memory_bridge(&self) -> Arc<MemoryCacheBridge> {
        Arc::clone(&self.memory_bridge)
    }

    /// Get the DDS disk cache bridge.
    ///
    /// This bridge implements `executor::DdsDiskCache` and
    /// `DdsDiskCacheChecker`. `None` when disk caching is off.
    pub fn dds_disk_bridge(&self) -> Option<Arc<DdsDiskCacheBridge>> {
        self.disk.as_ref().map(|d| Arc::clone(&d.dds_disk_bridge))
    }

    /// Get the chunk disk cache bridge.
    ///
    /// This bridge implements `executor::DiskCache`. `None` when disk
    /// caching is off.
    pub fn disk_bridge(&self) -> Option<Arc<DiskCacheBridge>> {
        self.disk.as_ref().map(|d| Arc::clone(&d.disk_bridge))
    }

    /// Get the raw memory cache for direct access.
    ///
    /// This provides access to the underlying generic cache implementation,
    /// useful for size queries or advanced operations.
    pub fn raw_memory_cache(&self) -> Arc<dyn Cache> {
        self.memory_service.cache()
    }

    /// Get the raw chunk disk cache for direct access; `None` when disk
    /// caching is off.
    pub fn raw_disk_cache(&self) -> Option<Arc<dyn Cache>> {
        self.disk.as_ref().map(|d| d.chunk_disk_service.cache())
    }

    /// Get the raw DDS disk cache for direct access; `None` when disk
    /// caching is off.
    pub fn raw_dds_disk_cache(&self) -> Option<Arc<dyn Cache>> {
        self.disk.as_ref().map(|d| d.dds_disk_service.cache())
    }

    /// Get the chunk disk cache provider for GC scheduler integration.
    ///
    /// Returns the underlying `DiskCacheProvider` which provides access to:
    /// - LRU index for cache size tracking
    /// - GC configuration (needs_gc, gc_target_size)
    /// - Cache directory path
    ///
    /// `None` when disk caching is off.
    pub fn disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.disk
            .as_ref()
            .and_then(|d| d.chunk_disk_service.disk_provider())
    }

    /// Get the DDS disk cache provider for GC scheduler integration.
    ///
    /// `None` when disk caching is off.
    pub fn dds_disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.disk
            .as_ref()
            .and_then(|d| d.dds_disk_service.disk_provider())
    }

    /// Get the lit disk cache provider for GC scheduler integration.
    ///
    /// `None` when disk caching or night lighting is off.
    pub fn lit_disk_provider(&self) -> Option<Arc<crate::cache::DiskCacheProvider>> {
        self.disk
            .as_ref()
            .and_then(|d| d.lit_disk_service.as_ref())
            .and_then(|s| s.disk_provider())
    }

    /// Get the raw lit disk cache for direct access.
    ///
    /// `None` when disk caching or night lighting is off.
    pub fn lit_disk_cache(&self) -> Option<Arc<dyn Cache>> {
        self.disk
            .as_ref()
            .and_then(|d| d.lit_disk_service.as_ref())
            .map(|s| s.cache())
    }

    /// Delete, on a blocking thread, the region directories the chunk tier
    /// left directly in `cache.directory` before it moved under its
    /// provider (#398). Nothing else in the directory is touched.
    ///
    /// Returns at once; the outcome is logged (removals at info, failures
    /// as warnings) and also returned through the handle, which callers may
    /// drop. `None`, and nothing touched, when disk caching is off.
    pub fn spawn_legacy_chunk_cleanup(
        &self,
    ) -> Option<tokio::task::JoinHandle<LegacyChunkCleanup>> {
        let cache_dir = self.disk.as_ref()?.cache_dir.clone();
        Some(tokio::task::spawn_blocking(move || {
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
        }))
    }

    /// Scan and report the initial disk cache sizes.
    ///
    /// Scans both DDS and chunk disk cache directories to count existing data.
    /// Call this during service initialization when the UI can display
    /// progress feedback.
    ///
    /// Returns the combined scanned size in bytes; 0, without touching the
    /// disk, when disk caching is off.
    pub async fn scan_disk_cache_size(&self) -> u64 {
        let Some(disk) = &self.disk else {
            return 0;
        };
        let mut total = 0u64;

        match disk.dds_disk_service.scan_initial_size().await {
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

        match disk.chunk_disk_service.scan_initial_size().await {
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
        if let Some(disk) = self.disk {
            disk.shutdown().await;
        }

        self.memory_service.shutdown().await;
        info!("Memory cache service shut down");

        info!("CacheLayer shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::integrity::EXPECTED_DDS_SIZE;
    use crate::coord::TileCoord;
    use crate::executor::{
        BlockingExecutor, ChunkDownloadError, ChunkProvider, ChunkResults, DdsDiskCache, DiskCache,
        ExecutorError, JobId, MemoryCache, NullDdsDiskCache, NullDiskCache, Task, TaskContext,
        TaskOutput, TaskResult, TextureEncodeError, TextureEncoderAsync,
    };
    use crate::tasks::chunk_image::test_chunks;
    use crate::tasks::{BuildAndCacheDdsTask, DownloadChunksTask};
    use bytes::Bytes;
    use std::future::Future;
    use std::pin::Pin;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

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

        let bing_chunks = bing.disk_bridge().unwrap();
        let go2_chunks = go2.disk_bridge().unwrap();
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
            .unwrap()
            .put(100, 200, 15, 0, 0, Bytes::from_static(b"kept"))
            .await
            .unwrap();

        let outcome = cache_layer
            .spawn_legacy_chunk_cleanup()
            .unwrap()
            .await
            .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.bytes_freed, 64);
        assert!(!legacy.exists());
        assert_eq!(
            cache_layer
                .disk_bridge()
                .unwrap()
                .get(100, 200, 15, 0, 0)
                .await,
            Some(Bytes::from_static(b"kept"))
        );

        cache_layer.shutdown().await;
    }

    fn disk_off_config(cache_dir: std::path::PathBuf) -> ServiceConfig {
        ServiceConfig::builder()
            .cache_directory(cache_dir)
            .cache_memory_size(1_000_000)
            .cache_disk_size(10_000_000)
            .cache_dds_disk_ratio(0.6)
            .disk_gc_interval(3600)
            .night_enabled(true)
            .disk_cache_enabled(false)
            .build()
    }

    /// #400: with disk caching off (`run --no-cache`) no disk tier starts,
    /// nothing is created under `cache.directory`, and the executor is
    /// handed null disk caches. The memory tier works as usual.
    #[tokio::test]
    async fn disk_off_starts_no_disk_tier_and_creates_nothing() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let cache_layer = CacheLayer::new(
            &disk_off_config(cache_dir.clone()),
            "Bing Maps",
            create_test_metrics(),
        )
        .await
        .unwrap();

        assert!(!cache_layer.disk_caching());
        assert!(matches!(
            cache_layer.executor_disk_caches(),
            ExecutorDiskCaches::Off
        ));
        assert!(cache_layer.dds_disk_bridge().is_none());
        assert!(cache_layer.disk_bridge().is_none());
        assert!(cache_layer.disk_provider().is_none());
        assert!(cache_layer.dds_disk_provider().is_none());
        assert!(cache_layer.lit_disk_provider().is_none());
        assert!(cache_layer.lit_disk_cache().is_none());
        assert_eq!(cache_layer.scan_disk_cache_size().await, 0);

        let memory = cache_layer.memory_bridge();
        memory.put(100, 200, 15, vec![1, 2, 3].into()).await;
        assert_eq!(
            memory.get(100, 200, 15).await,
            Some(Bytes::from(vec![1, 2, 3]))
        );

        assert!(
            !cache_dir.exists(),
            "nothing may be created under cache.directory"
        );
        cache_layer.shutdown().await;
    }

    /// #400: the legacy cleanup is a disk operation too, so it does not run
    /// with disk caching off.
    #[tokio::test]
    async fn disk_off_runs_no_legacy_cleanup() {
        let temp_dir = tempdir().unwrap();
        let legacy = temp_dir.path().join("+37-123");
        std::fs::create_dir_all(&legacy).unwrap();
        let cache_layer = CacheLayer::new(
            &disk_off_config(temp_dir.path().to_path_buf()),
            "Bing Maps",
            create_test_metrics(),
        )
        .await
        .unwrap();

        assert!(cache_layer.spawn_legacy_chunk_cleanup().is_none());
        assert!(legacy.exists());
        cache_layer.shutdown().await;
    }

    // ------------------------------------------------------------------
    // Tiles built through the layer's bridges
    // ------------------------------------------------------------------

    const TILE: TileCoord = TileCoord {
        row: 100,
        col: 200,
        zoom: 15,
    };

    /// Encodes every image to a well-formed, blank DDS tile.
    struct BlankDdsEncoder;
    impl TextureEncoderAsync for BlankDdsEncoder {
        fn encode_blocking(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            let mut dds = vec![0u8; EXPECTED_DDS_SIZE];
            dds[0..4].copy_from_slice(b"DDS ");
            Ok(dds)
        }
        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            EXPECTED_DDS_SIZE
        }
    }

    struct Inline;
    impl BlockingExecutor for Inline {
        fn execute_blocking<F, R>(
            &self,
            f: F,
        ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static,
        {
            Box::pin(async move { Ok(f()) })
        }
    }

    /// Waits up to two seconds for `ready` to hold.
    async fn eventually<F, Fut>(what: &str, mut ready: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !ready().await {
            assert!(tokio::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// #400: a tile generated with disk caching off reaches memory and no
    /// disk tier. The build task gets what `RuntimeBuilder` gives the
    /// executor for [`ExecutorDiskCaches::Off`]: the layer's memory bridge,
    /// `NullDdsDiskCache` and `NullDiskCache`.
    #[tokio::test]
    async fn disk_off_tile_is_cached_in_memory_only() {
        let temp_dir = tempdir().unwrap();
        let cache_layer = CacheLayer::new(
            &disk_off_config(temp_dir.path().to_path_buf()),
            "Bing Maps",
            create_test_metrics(),
        )
        .await
        .unwrap();
        assert!(matches!(
            cache_layer.executor_disk_caches(),
            ExecutorDiskCaches::Off
        ));

        let task = BuildAndCacheDdsTask::new(
            TILE,
            Arc::new(BlankDdsEncoder),
            cache_layer.memory_bridge(),
            Arc::new(NullDdsDiskCache),
            Arc::new(NullDiskCache),
            Arc::new(Inline),
        );
        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                chunks.add_success(row, col, test_chunks::jpeg());
            }
        }
        let mut ctx = TaskContext::new(JobId::new("no-cache"), CancellationToken::new());
        let mut output = TaskOutput::new();
        output.set("chunks", chunks);
        ctx.add_task_output("DownloadChunks".to_string(), output);
        task.execute(&mut ctx).await;

        let memory = cache_layer.memory_bridge();
        eventually("the tile never reached memory", || async {
            memory.get(100, 200, 15).await.is_some()
        })
        .await;
        assert_eq!(
            std::fs::read_dir(temp_dir.path()).unwrap().count(),
            0,
            "no disk tier may be written"
        );

        cache_layer.shutdown().await;
    }

    /// Answers every chunk with a usable JPEG and counts the downloads of
    /// one chunk.
    struct ImageryProvider {
        watched: (u32, u32),
        watched_downloads: std::sync::atomic::AtomicUsize,
    }
    impl ImageryProvider {
        /// Watches chunk `(row, col)` of [`TILE`].
        fn watching(row: u8, col: u8) -> Self {
            Self {
                watched: (TILE.row * 16 + row as u32, TILE.col * 16 + col as u32),
                watched_downloads: Default::default(),
            }
        }
        fn watched_downloads(&self) -> usize {
            self.watched_downloads
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl ChunkProvider for ImageryProvider {
        fn download_chunk(
            &self,
            row: u32,
            col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Result<Bytes, ChunkDownloadError>> + Send {
            if (row, col) == self.watched {
                self.watched_downloads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            async { Ok(test_chunks::jpeg()) }
        }
        fn name(&self) -> &str {
            "imagery"
        }
    }

    /// Generates [`TILE`] as a job does: download its chunks through the
    /// chunk tier, then build and cache it through the layer's bridges.
    async fn generate_tile(cache_layer: &CacheLayer, provider: &Arc<ImageryProvider>) {
        let chunk_cache = cache_layer.disk_bridge().unwrap();
        let mut ctx = TaskContext::new(JobId::new("generate"), CancellationToken::new());
        let download =
            DownloadChunksTask::new(TILE, Arc::clone(provider), Arc::clone(&chunk_cache));
        let TaskResult::SuccessWithOutput(chunks) = download.execute(&mut ctx).await else {
            panic!("the download task failed");
        };
        ctx.add_task_output("DownloadChunks".to_string(), chunks);
        let build = BuildAndCacheDdsTask::new(
            TILE,
            Arc::new(BlankDdsEncoder),
            cache_layer.memory_bridge(),
            cache_layer.dds_disk_bridge().unwrap(),
            chunk_cache,
            Arc::new(Inline),
        );
        assert!(
            matches!(
                build.execute(&mut ctx).await,
                TaskResult::SuccessWithOutput(_)
            ),
            "X-Plane gets a tile"
        );
    }

    /// #388, the issue's scenario through the production bridges: a chunk
    /// cache entry that is not an image (an error page cached before the
    /// fix, or corruption on disk) is a cache hit, so it is never
    /// downloaded again, and before the fix its tile was cached with a
    /// magenta square. Now the tile is served but not cached, the entry is
    /// discarded, and the next generation downloads the chunk and caches a
    /// complete tile.
    #[tokio::test]
    async fn a_cached_chunk_that_fails_to_decode_is_discarded_and_downloaded_again() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let cache_layer = CacheLayer::new(&config, "Bing Maps", create_test_metrics())
            .await
            .unwrap();
        let chunk_cache = cache_layer.disk_bridge().unwrap();
        chunk_cache
            .put(100, 200, 15, 3, 4, test_chunks::error_page())
            .await
            .unwrap();
        let provider = Arc::new(ImageryProvider::watching(3, 4));
        let memory = cache_layer.memory_bridge();
        let dds_disk = cache_layer.dds_disk_bridge().unwrap();

        generate_tile(&cache_layer, &provider).await;

        assert_eq!(provider.watched_downloads(), 0, "the bad chunk was a hit");
        eventually("the bad chunk was never discarded", || async {
            chunk_cache.get(100, 200, 15, 3, 4).await.is_none()
        })
        .await;
        assert!(memory.get(100, 200, 15).await.is_none());
        assert!(dds_disk.get(100, 200, 15).await.is_none());

        generate_tile(&cache_layer, &provider).await;

        assert_eq!(provider.watched_downloads(), 1, "downloaded once more");
        eventually("the complete tile never reached memory", || async {
            memory.get(100, 200, 15).await.is_some()
        })
        .await;
        eventually(
            "the complete tile never reached the DDS disk tier",
            || async { dds_disk.get(100, 200, 15).await.is_some() },
        )
        .await;

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
        let bridge = cache_layer.dds_disk_bridge().unwrap();

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
        let bridge = cache_layer.disk_bridge().unwrap();

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

    /// The root of the disk tiers: the configured directory, else the
    /// platform's. Checked without starting a layer, which would create the
    /// tiers in the user's real cache directory (it used to, as
    /// `~/.cache/xearthlayer/test/`).
    #[test]
    fn the_disk_root_is_the_configured_directory_else_the_platforms() {
        let unset = ServiceConfig::builder().build();
        assert!(unset.cache_directory().is_none());
        assert_eq!(disk_root(&unset), crate::paths::tile_cache_dir());

        let set = ServiceConfig::builder()
            .cache_directory(PathBuf::from("/somewhere/else"))
            .build();
        assert_eq!(disk_root(&set), PathBuf::from("/somewhere/else"));
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

    const MIB: u64 = 1024 * 1024;

    /// The smallest `cache.memory_size` whose share reaches the floor.
    fn crossover() -> u64 {
        let floor = LitMemoryBudget::FLOOR.total();
        // ceil(floor * 100 / 15): the share, rounded down, first reaches it.
        let m = (floor * 100).div_ceil(LIT_MEMORY_SHARE_PERCENT);
        assert!(percent_of(m - 1, LIT_MEMORY_SHARE_PERCENT) < floor);
        assert!(percent_of(m, LIT_MEMORY_SHARE_PERCENT) >= floor);
        m
    }

    #[test]
    fn night_off_gives_the_ortho_tier_all_of_the_memory_size() {
        for m in [0, 1, 512 * MIB, 8_590_000_000, u64::MAX] {
            assert_eq!(
                memory_budgets(m, false),
                MemoryBudgets {
                    ortho: m,
                    lit: None
                }
            );
        }
    }

    /// #432: below about 1.67 GiB night lighting keeps its floor on top,
    /// and the ortho tier is as it was before the share.
    #[test]
    fn below_the_crossover_night_lighting_gets_the_floor_on_top() {
        let floor = Some(LitMemoryBudget::FLOOR);
        assert_eq!(
            LitMemoryBudget::FLOOR,
            LitMemoryBudget {
                tail: 192 * MIB,
                head: 64 * MIB
            }
        );
        for m in [0, 1, 1_000_000, 512 * MIB, 1024 * MIB, crossover() - 1] {
            assert_eq!(
                memory_budgets(m, true),
                MemoryBudgets {
                    ortho: m,
                    lit: floor
                },
                "memory_size {m}"
            );
        }
    }

    #[test]
    fn at_the_crossover_the_floor_is_taken_out_of_the_memory_size() {
        let m = crossover();
        let b = memory_budgets(m, true);
        let lit = b.lit.unwrap();
        assert_eq!(lit.tail, LitMemoryBudget::FLOOR.tail);
        assert_eq!(lit.head, percent_of(m, 15) - 192 * MIB);
        assert!(lit.head >= LitMemoryBudget::FLOOR.head);
        assert_eq!(b.ortho + lit.total(), m);
    }

    /// Up to a share of 480 MiB (40% of it is the floor's tail) the tail
    /// stays at the floor's and the head takes the rest; above, 40/60.
    #[test]
    fn above_the_crossover_the_tail_is_forty_percent_never_below_the_floor() {
        let m = 3 * 1024 * MIB; // share 460.8 MiB: 40% would be 184.3 MiB
        let lit = memory_budgets(m, true).lit.unwrap();
        assert_eq!(lit.tail, 192 * MIB);
        assert_eq!(lit.head, percent_of(m, 15) - 192 * MIB);

        let m = 8_590_000_000; // the brief's example
        let b = memory_budgets(m, true);
        let lit = b.lit.unwrap();
        assert_eq!(b.ortho, 7_301_500_000);
        assert_eq!(lit.tail, 515_400_000);
        assert_eq!(lit.head, 773_100_000);
    }

    #[test]
    fn above_the_crossover_ortho_and_lit_are_the_memory_size() {
        let mut m = crossover();
        while m < u64::MAX / 3 {
            let b = memory_budgets(m, true);
            let lit = b.lit.unwrap();
            assert_eq!(b.ortho as u128 + lit.total() as u128, m as u128, "{m}");
            m = m * 3 + 7;
        }
        let b = memory_budgets(u64::MAX, true);
        let lit = b.lit.unwrap();
        assert_eq!(b.ortho as u128 + lit.total() as u128, u64::MAX as u128);
        assert_eq!(lit.tail, percent_of(percent_of(u64::MAX, 15), 40));
    }

    /// Neither part ever drops below its floor value, and neither shrinks
    /// as the memory size grows, across the crossover and 480 MiB share.
    #[test]
    fn lit_parts_never_drop_below_the_floor_and_only_grow() {
        let floor = LitMemoryBudget::FLOOR;
        let edges = [crossover(), (480 * MIB * 100).div_ceil(15)];
        let mut sizes: Vec<u64> = (0..=64).map(|i| i * 64 * MIB).collect();
        for e in edges {
            sizes.extend((e - 3)..=(e + 3));
        }
        sizes.extend([1 << 40, 1 << 50, u64::MAX / 2, u64::MAX]);
        sizes.sort_unstable();
        let mut last = floor;
        for m in sizes {
            let lit = memory_budgets(m, true).lit.unwrap();
            assert!(
                lit.tail >= floor.tail && lit.head >= floor.head,
                "{m}: {lit:?}"
            );
            assert!(
                lit.tail >= last.tail && lit.head >= last.head,
                "{m}: {lit:?} after {last:?}"
            );
            last = lit;
        }
    }

    /// The layer sizes its memory tier by the budgets, and hands night
    /// lighting its part; `--no-cache` changes neither.
    #[tokio::test]
    async fn the_memory_tier_is_what_night_lighting_leaves() {
        let m = 4 * 1024 * MIB;
        for disk in [true, false] {
            let temp_dir = tempdir().unwrap();
            let config = ServiceConfig::builder()
                .cache_directory(temp_dir.path().to_path_buf())
                .cache_memory_size(m as usize)
                .cache_disk_size(10_000_000)
                .disk_cache_enabled(disk)
                .disk_gc_interval(3600)
                .night_enabled(true)
                .build();
            let layer = CacheLayer::new(&config, "test", create_test_metrics())
                .await
                .unwrap();
            let b = memory_budgets(m, true);
            assert_eq!(layer.raw_memory_cache().max_size_bytes(), b.ortho);
            assert_eq!(layer.lit_memory_budget(), b.lit);
            assert!(b.ortho < m);
            layer.shutdown().await;
        }
    }

    #[tokio::test]
    async fn night_off_gives_the_memory_tier_all_and_night_lighting_none() {
        let temp_dir = tempdir().unwrap();
        let config = create_test_config(temp_dir.path().to_path_buf());
        let layer = CacheLayer::new(&config, "test", create_test_metrics())
            .await
            .unwrap();
        assert_eq!(layer.raw_memory_cache().max_size_bytes(), 1_000_000);
        assert_eq!(layer.lit_memory_budget(), None);
        layer.shutdown().await;
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
