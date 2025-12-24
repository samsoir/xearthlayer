//! DDS handler builder for the async pipeline.
//!
//! This module provides a builder for creating DDS handlers that wire up
//! all pipeline components (provider, encoder, caches, executor) and return
//! a handler suitable for use with `Fuse3PassthroughFS`.
//!
//! # Architecture
//!
//! The builder creates a handler that processes DDS generation requests through:
//!
//! ```text
//! DdsRequest → Provider (download chunks) → Encoder (compress) → DdsResponse
//!                  ↓                              ↓
//!            Memory Cache                    Disk Cache
//! ```
//!
//! # Provider Selection
//!
//! When an async provider is available (preferred), uses `AsyncProviderAdapter`
//! which avoids `spawn_blocking` for HTTP calls, preventing thread pool exhaustion.
//! Falls back to sync provider with `spawn_blocking` if async is unavailable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::cache::MemoryCache;
use crate::config::{
    default_cpu_concurrent, default_http_concurrent, default_prefetch_in_flight,
    DEFAULT_COALESCE_CHANNEL_CAPACITY, DEFAULT_MAX_CONCURRENT_DOWNLOADS, DEFAULT_MAX_RETRIES,
    DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_RETRY_BASE_DELAY_MS,
};
use crate::dds::DdsFormat;
use crate::fuse::DdsHandler;
use crate::pipeline::adapters::{
    AsyncProviderAdapter, MemoryCacheAdapter, NullDiskCache, ParallelDiskCache, ProviderAdapter,
    TextureEncoderAdapter,
};
use crate::pipeline::control_plane::PipelineControlPlane;
use crate::pipeline::{
    create_dds_handler_with_control_plane, ConcurrencyLimiter, PipelineConfig, TokioExecutor,
};
use crate::provider::{AsyncProviderType, Provider};
use crate::telemetry::PipelineMetrics;
use crate::texture::DdsTextureEncoder;

/// Builder for creating DDS handlers.
///
/// Encapsulates the complex wiring of pipeline components and provides
/// a clean API for handler creation with different configurations.
///
/// # Example
///
/// ```ignore
/// let memory_cache = Arc::new(MemoryCache::new(2 * 1024 * 1024 * 1024));
/// let handler = DdsHandlerBuilder::new()
///     .with_async_provider(async_provider)
///     .with_disk_cache("/path/to/cache", "bing")
///     .with_memory_cache(Arc::clone(&memory_cache))
///     .with_format(DdsFormat::BC1)
///     .with_mipmap_count(5)
///     .with_timeout(Duration::from_secs(30))
///     .with_metrics(metrics)
///     .build(runtime_handle);
/// ```
pub struct DdsHandlerBuilder {
    /// Async provider (preferred - non-blocking I/O)
    async_provider: Option<Arc<AsyncProviderType>>,
    /// Sync provider (fallback - uses spawn_blocking)
    sync_provider: Option<Arc<dyn Provider>>,
    /// Provider name for cache paths
    provider_name: String,
    /// Disk cache directory (None = no disk cache)
    cache_dir: Option<PathBuf>,
    /// Shared memory cache (raw cache for size queries)
    memory_cache: Option<Arc<MemoryCache>>,
    /// Shared memory cache adapter (implements pipeline::MemoryCache trait)
    memory_cache_adapter: Option<Arc<MemoryCacheAdapter>>,
    /// Shared disk I/O limiter (None = local limiter per cache)
    disk_io_limiter: Option<Arc<ConcurrencyLimiter>>,
    /// DDS compression format
    dds_format: DdsFormat,
    /// Number of mipmap levels
    mipmap_count: usize,
    /// Request timeout
    timeout: Duration,
    /// Max retries for failed requests
    max_retries: u32,
    /// Max concurrent downloads per tile
    max_concurrent_downloads: usize,
    /// Global maximum concurrent HTTP requests
    max_global_http_requests: usize,
    /// Maximum concurrent prefetch jobs in flight
    max_prefetch_in_flight: usize,
    /// Base delay in milliseconds for retry backoff
    retry_base_delay_ms: u64,
    /// Broadcast channel capacity for request coalescing
    coalesce_channel_capacity: usize,
    /// Maximum concurrent CPU-bound operations
    max_cpu_concurrent: usize,
    /// Pipeline telemetry metrics
    metrics: Option<Arc<PipelineMetrics>>,
    /// Pipeline control plane for job management and health monitoring
    control_plane: Option<Arc<PipelineControlPlane>>,
}

impl DdsHandlerBuilder {
    /// Create a new builder with default settings.
    pub fn new(provider_name: &str) -> Self {
        Self {
            async_provider: None,
            sync_provider: None,
            provider_name: provider_name.to_string(),
            cache_dir: None,
            memory_cache: None,
            memory_cache_adapter: None,
            disk_io_limiter: None,
            dds_format: DdsFormat::BC1,
            mipmap_count: 5,
            timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
            max_concurrent_downloads: DEFAULT_MAX_CONCURRENT_DOWNLOADS,
            max_global_http_requests: default_http_concurrent(),
            max_prefetch_in_flight: default_prefetch_in_flight(),
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            coalesce_channel_capacity: DEFAULT_COALESCE_CHANNEL_CAPACITY,
            max_cpu_concurrent: default_cpu_concurrent(),
            metrics: None,
            control_plane: None,
        }
    }

    /// Set the async provider (preferred - non-blocking I/O).
    ///
    /// When set, the handler will use `AsyncProviderAdapter` which performs
    /// HTTP calls asynchronously, avoiding thread pool exhaustion under load.
    pub fn with_async_provider(mut self, provider: Arc<AsyncProviderType>) -> Self {
        self.async_provider = Some(provider);
        self
    }

    /// Set the sync provider (fallback).
    ///
    /// Used when async provider is not available. HTTP calls are executed
    /// via `spawn_blocking`, which may exhaust the thread pool under heavy load.
    pub fn with_sync_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.sync_provider = Some(provider);
        self
    }

    /// Enable disk caching at the specified directory.
    ///
    /// When a shared disk I/O limiter is set via `with_disk_io_limiter()`,
    /// the cache will use it for coordinated concurrency limiting across
    /// all cache instances. Otherwise, a local limiter is created.
    pub fn with_disk_cache(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    /// Set a shared disk I/O concurrency limiter.
    ///
    /// When multiple packages are mounted, sharing a single limiter across
    /// all disk cache instances prevents the combined I/O from overwhelming
    /// the system. This is the recommended configuration for production use.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let disk_io_limiter = Arc::new(ConcurrencyLimiter::with_defaults("global_disk_io"));
    ///
    /// // Share the limiter across multiple handlers
    /// let handler1 = DdsHandlerBuilder::new("bing")
    ///     .with_disk_cache(cache_dir.clone())
    ///     .with_disk_io_limiter(Arc::clone(&disk_io_limiter))
    ///     .build(handle.clone());
    ///
    /// let handler2 = DdsHandlerBuilder::new("google")
    ///     .with_disk_cache(cache_dir)
    ///     .with_disk_io_limiter(disk_io_limiter)
    ///     .build(handle);
    /// ```
    pub fn with_disk_io_limiter(mut self, limiter: Arc<ConcurrencyLimiter>) -> Self {
        self.disk_io_limiter = Some(limiter);
        self
    }

    /// Set the shared memory cache for tile-level caching.
    ///
    /// The cache should be wrapped in `Arc` and shared across the application
    /// to allow telemetry to query actual cache sizes.
    ///
    /// Note: If you also call `with_memory_cache_adapter()`, the adapter will
    /// be used and this raw cache is only kept for compatibility.
    pub fn with_memory_cache(mut self, cache: Arc<MemoryCache>) -> Self {
        self.memory_cache = Some(cache);
        self
    }

    /// Set a pre-created memory cache adapter.
    ///
    /// This allows sharing the same adapter instance between the pipeline
    /// and other components (e.g., the prefetcher). When set, this adapter
    /// is used directly instead of creating a new one from the raw cache.
    pub fn with_memory_cache_adapter(mut self, adapter: Arc<MemoryCacheAdapter>) -> Self {
        self.memory_cache_adapter = Some(adapter);
        self
    }

    /// Set the DDS compression format (BC1 or BC3).
    pub fn with_format(mut self, format: DdsFormat) -> Self {
        self.dds_format = format;
        self
    }

    /// Set the number of mipmap levels to generate.
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = count;
        self
    }

    /// Set the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the maximum retries for failed requests.
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Set the maximum concurrent downloads per tile.
    pub fn with_max_concurrent_downloads(mut self, count: usize) -> Self {
        self.max_concurrent_downloads = count;
        self
    }

    /// Set the pipeline metrics for telemetry.
    pub fn with_metrics(mut self, metrics: Arc<PipelineMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Set the global maximum concurrent HTTP requests.
    pub fn with_max_global_http_requests(mut self, count: usize) -> Self {
        self.max_global_http_requests = count;
        self
    }

    /// Set the maximum concurrent prefetch jobs in flight.
    pub fn with_max_prefetch_in_flight(mut self, count: usize) -> Self {
        self.max_prefetch_in_flight = count;
        self
    }

    /// Set the base delay for retry backoff in milliseconds.
    pub fn with_retry_base_delay_ms(mut self, ms: u64) -> Self {
        self.retry_base_delay_ms = ms;
        self
    }

    /// Set the coalesce channel capacity.
    pub fn with_coalesce_channel_capacity(mut self, capacity: usize) -> Self {
        self.coalesce_channel_capacity = capacity;
        self
    }

    /// Set the maximum concurrent CPU-bound operations.
    pub fn with_max_cpu_concurrent(mut self, count: usize) -> Self {
        self.max_cpu_concurrent = count;
        self
    }

    /// Set the control plane for job management and health monitoring.
    ///
    /// When a control plane is set, the handler will:
    /// - Submit all work through the control plane's `submit()` method
    /// - Report stage transitions to the control plane for tracking
    /// - Respect job-level concurrency limits
    /// - Enable stall detection and recovery
    ///
    /// This is the recommended configuration for production use.
    pub fn with_control_plane(mut self, control_plane: Arc<PipelineControlPlane>) -> Self {
        self.control_plane = Some(control_plane);
        self
    }

    /// Build the DDS handler.
    ///
    /// # Arguments
    ///
    /// * `runtime_handle` - Handle to the Tokio runtime for spawning tasks
    ///
    /// # Panics
    ///
    /// Panics if neither async nor sync provider is set.
    pub fn build(self, runtime_handle: Handle) -> DdsHandler {
        // Create texture encoder
        let encoder = DdsTextureEncoder::new(self.dds_format).with_mipmap_count(self.mipmap_count);
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));

        // Use pre-created adapter if available, otherwise create from raw cache
        let memory_cache_adapter = match &self.memory_cache_adapter {
            Some(adapter) => {
                // Use the shared adapter instance (preferred - enables sharing with prefetcher)
                Arc::clone(adapter)
            }
            None => Arc::new(match &self.memory_cache {
                Some(cache) => {
                    // Create adapter from the shared cache
                    MemoryCacheAdapter::new(Arc::clone(cache), &self.provider_name, self.dds_format)
                }
                None => {
                    // Minimal cache when disabled
                    MemoryCacheAdapter::new(
                        Arc::new(MemoryCache::new(0)),
                        &self.provider_name,
                        self.dds_format,
                    )
                }
            }),
        };

        // Create executor
        let executor = Arc::new(TokioExecutor::new());

        // Create pipeline config
        let pipeline_config = PipelineConfig {
            request_timeout: self.timeout,
            max_retries: self.max_retries,
            dds_format: self.dds_format,
            mipmap_count: self.mipmap_count,
            max_concurrent_downloads: self.max_concurrent_downloads,
            max_global_http_requests: self.max_global_http_requests,
            max_prefetch_in_flight: self.max_prefetch_in_flight,
            retry_base_delay_ms: self.retry_base_delay_ms,
            coalesce_channel_capacity: self.coalesce_channel_capacity,
            max_cpu_concurrent: self.max_cpu_concurrent,
        };

        // Clone fields before the match to avoid partial move
        let cache_dir = self.cache_dir.clone();
        let async_provider = self.async_provider.clone();
        let sync_provider = self.sync_provider.clone();

        // Build handler based on available provider and cache configuration
        match (async_provider, sync_provider, cache_dir) {
            // Async provider with disk cache (PREFERRED)
            (Some(async_prov), _, Some(dir)) => {
                let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_prov));
                let disk_cache = match &self.disk_io_limiter {
                    Some(limiter) => Arc::new(ParallelDiskCache::with_shared_limiter(
                        dir,
                        &self.provider_name,
                        Arc::clone(limiter),
                    )),
                    None => Arc::new(ParallelDiskCache::with_defaults(dir, &self.provider_name)),
                };
                self.build_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    "async provider + parallel disk cache",
                )
            }
            // Async provider without disk cache (PREFERRED)
            (Some(async_prov), _, None) => {
                let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_prov));
                let disk_cache = Arc::new(NullDiskCache);
                self.build_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    "async provider, no disk cache",
                )
            }
            // Sync provider with disk cache (FALLBACK)
            (None, Some(sync_prov), Some(dir)) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(sync_prov));
                let disk_cache = match &self.disk_io_limiter {
                    Some(limiter) => Arc::new(ParallelDiskCache::with_shared_limiter(
                        dir,
                        &self.provider_name,
                        Arc::clone(limiter),
                    )),
                    None => Arc::new(ParallelDiskCache::with_defaults(dir, &self.provider_name)),
                };
                tracing::warn!("Using sync provider (may exhaust thread pool under load)");
                self.build_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    "sync provider + disk cache",
                )
            }
            // Sync provider without disk cache (FALLBACK)
            (None, Some(sync_prov), None) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(sync_prov));
                let disk_cache = Arc::new(NullDiskCache);
                tracing::warn!("Using sync provider (may exhaust thread pool under load)");
                self.build_handler(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    "sync provider, no disk cache",
                )
            }
            // No provider configured
            (None, None, _) => {
                panic!(
                    "DdsHandlerBuilder requires either async_provider or sync_provider to be set"
                )
            }
        }
    }

    /// Internal helper to build the handler with control plane.
    ///
    /// # Panics
    ///
    /// Panics if control plane is not set. Use `with_control_plane()` before calling `build()`.
    #[allow(clippy::too_many_arguments)]
    fn build_handler<P, E, M, D, X>(
        &self,
        provider: Arc<P>,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        disk_cache: Arc<D>,
        executor: Arc<X>,
        config: PipelineConfig,
        runtime_handle: Handle,
        description: &str,
    ) -> DdsHandler
    where
        P: crate::pipeline::ChunkProvider + 'static,
        E: crate::pipeline::TextureEncoderAsync + 'static,
        M: crate::pipeline::MemoryCache + 'static,
        D: crate::pipeline::DiskCache + 'static,
        X: crate::pipeline::BlockingExecutor + 'static,
    {
        let control_plane = self
            .control_plane
            .as_ref()
            .expect("DdsHandlerBuilder requires control_plane to be set via with_control_plane()");

        tracing::info!(
            description = description,
            max_concurrent_jobs = control_plane.max_concurrent_jobs(),
            "DDS handler with control plane"
        );

        create_dds_handler_with_control_plane(
            Arc::clone(control_plane),
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
            runtime_handle,
            self.metrics.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_default_values() {
        let builder = DdsHandlerBuilder::new("bing");
        assert_eq!(builder.provider_name, "bing");
        assert_eq!(builder.dds_format, DdsFormat::BC1);
        assert_eq!(builder.mipmap_count, 5);
        assert_eq!(
            builder.timeout,
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)
        );
        assert_eq!(builder.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(
            builder.max_concurrent_downloads,
            DEFAULT_MAX_CONCURRENT_DOWNLOADS
        );
        assert_eq!(builder.max_global_http_requests, default_http_concurrent());
        assert_eq!(builder.max_prefetch_in_flight, default_prefetch_in_flight());
        assert_eq!(builder.retry_base_delay_ms, DEFAULT_RETRY_BASE_DELAY_MS);
        assert_eq!(
            builder.coalesce_channel_capacity,
            DEFAULT_COALESCE_CHANNEL_CAPACITY
        );
        assert_eq!(builder.max_cpu_concurrent, default_cpu_concurrent());
        assert!(builder.async_provider.is_none());
        assert!(builder.sync_provider.is_none());
        assert!(builder.cache_dir.is_none());
        assert!(builder.memory_cache.is_none());
        assert!(builder.metrics.is_none());
    }

    #[test]
    fn test_builder_with_format() {
        let builder = DdsHandlerBuilder::new("google").with_format(DdsFormat::BC3);
        assert_eq!(builder.dds_format, DdsFormat::BC3);
    }

    #[test]
    fn test_builder_with_mipmap_count() {
        let builder = DdsHandlerBuilder::new("bing").with_mipmap_count(3);
        assert_eq!(builder.mipmap_count, 3usize);
    }

    #[test]
    fn test_builder_with_timeout() {
        let builder = DdsHandlerBuilder::new("bing").with_timeout(Duration::from_secs(60));
        assert_eq!(builder.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_builder_with_max_retries() {
        let builder = DdsHandlerBuilder::new("bing").with_max_retries(5);
        assert_eq!(builder.max_retries, 5);
    }

    #[test]
    fn test_builder_with_disk_cache() {
        let builder = DdsHandlerBuilder::new("bing").with_disk_cache(PathBuf::from("/tmp/cache"));
        assert_eq!(builder.cache_dir, Some(PathBuf::from("/tmp/cache")));
    }

    #[test]
    fn test_builder_with_memory_cache() {
        let cache = Arc::new(MemoryCache::new(1024 * 1024));
        let builder = DdsHandlerBuilder::new("bing").with_memory_cache(cache);
        assert!(builder.memory_cache.is_some());
    }

    #[test]
    fn test_builder_with_metrics() {
        let metrics = Arc::new(PipelineMetrics::new());
        let builder = DdsHandlerBuilder::new("bing").with_metrics(metrics);
        assert!(builder.metrics.is_some());
    }

    #[test]
    fn test_builder_chaining() {
        let metrics = Arc::new(PipelineMetrics::new());
        let builder = DdsHandlerBuilder::new("google")
            .with_format(DdsFormat::BC3)
            .with_mipmap_count(4)
            .with_timeout(Duration::from_secs(45))
            .with_max_retries(2)
            .with_max_concurrent_downloads(128)
            .with_disk_cache(PathBuf::from("/cache"))
            .with_metrics(metrics);

        assert_eq!(builder.provider_name, "google");
        assert_eq!(builder.dds_format, DdsFormat::BC3);
        assert_eq!(builder.mipmap_count, 4);
        assert_eq!(builder.timeout, Duration::from_secs(45));
        assert_eq!(builder.max_retries, 2);
        assert_eq!(builder.max_concurrent_downloads, 128);
        assert_eq!(builder.cache_dir, Some(PathBuf::from("/cache")));
        assert!(builder.metrics.is_some());
    }

    #[test]
    #[should_panic(expected = "requires either async_provider or sync_provider")]
    fn test_builder_panics_without_provider() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let builder = DdsHandlerBuilder::new("bing");
        builder.build(runtime.handle().clone());
    }
}
