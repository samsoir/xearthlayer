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
use crate::dds::DdsFormat;
use crate::fuse::DdsHandler;
use crate::pipeline::adapters::{
    AsyncProviderAdapter, MemoryCacheAdapter, NullDiskCache, ParallelDiskCache, ProviderAdapter,
    TextureEncoderAdapter,
};
use crate::pipeline::{create_dds_handler_with_metrics, PipelineConfig, TokioExecutor};
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
    /// Shared memory cache (None = minimal/disabled)
    memory_cache: Option<Arc<MemoryCache>>,
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
    /// Pipeline telemetry metrics
    metrics: Option<Arc<PipelineMetrics>>,
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
            dds_format: DdsFormat::BC1,
            mipmap_count: 5,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            max_concurrent_downloads: 256,
            metrics: None,
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
    /// Uses `ParallelDiskCache` with semaphore-limited concurrency (default 64
    /// concurrent reads) to avoid overwhelming the filesystem.
    pub fn with_disk_cache(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    /// Set the shared memory cache for tile-level caching.
    ///
    /// The cache should be wrapped in `Arc` and shared across the application
    /// to allow telemetry to query actual cache sizes.
    pub fn with_memory_cache(mut self, cache: Arc<MemoryCache>) -> Self {
        self.memory_cache = Some(cache);
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

        // Create memory cache adapter using the shared cache
        let memory_cache_adapter = Arc::new(match self.memory_cache {
            Some(cache) => {
                // Use the shared cache directly
                MemoryCacheAdapter::new(cache, &self.provider_name, self.dds_format)
            }
            None => {
                // Minimal cache when disabled
                MemoryCacheAdapter::new(
                    Arc::new(MemoryCache::new(0)),
                    &self.provider_name,
                    self.dds_format,
                )
            }
        });

        // Create executor
        let executor = Arc::new(TokioExecutor::new());

        // Create pipeline config
        let pipeline_config = PipelineConfig {
            request_timeout: self.timeout,
            max_retries: self.max_retries,
            dds_format: self.dds_format,
            mipmap_count: self.mipmap_count,
            max_concurrent_downloads: self.max_concurrent_downloads,
            max_global_http_requests: PipelineConfig::default_global_http_requests(),
        };

        // Build handler based on available provider and cache configuration
        match (self.async_provider, self.sync_provider, self.cache_dir) {
            // Async provider with disk cache (PREFERRED)
            (Some(async_prov), _, Some(dir)) => {
                let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_prov));
                let disk_cache =
                    Arc::new(ParallelDiskCache::with_defaults(dir, &self.provider_name));
                tracing::info!("DDS handler: async provider + parallel disk cache");
                create_dds_handler_with_metrics(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    self.metrics,
                )
            }
            // Async provider without disk cache (PREFERRED)
            (Some(async_prov), _, None) => {
                let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_prov));
                let disk_cache = Arc::new(NullDiskCache);
                tracing::info!("DDS handler: async provider, no disk cache");
                create_dds_handler_with_metrics(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    self.metrics,
                )
            }
            // Sync provider with disk cache (FALLBACK)
            (None, Some(sync_prov), Some(dir)) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(sync_prov));
                let disk_cache =
                    Arc::new(ParallelDiskCache::with_defaults(dir, &self.provider_name));
                tracing::warn!("DDS handler: sync provider + disk cache (may exhaust thread pool)");
                create_dds_handler_with_metrics(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    self.metrics,
                )
            }
            // Sync provider without disk cache (FALLBACK)
            (None, Some(sync_prov), None) => {
                let provider_adapter = Arc::new(ProviderAdapter::new(sync_prov));
                let disk_cache = Arc::new(NullDiskCache);
                tracing::warn!(
                    "DDS handler: sync provider, no disk cache (may exhaust thread pool)"
                );
                create_dds_handler_with_metrics(
                    provider_adapter,
                    encoder_adapter,
                    memory_cache_adapter,
                    disk_cache,
                    executor,
                    pipeline_config,
                    runtime_handle,
                    self.metrics,
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
        assert_eq!(builder.timeout, Duration::from_secs(30));
        assert_eq!(builder.max_retries, 3);
        assert_eq!(builder.max_concurrent_downloads, 256);
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
