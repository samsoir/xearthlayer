//! Builder for creating XEarthLayerRuntime with proper adapter wiring.
//!
//! This module handles the complex wiring needed to connect the service-level
//! components (providers, encoders, caches) to the job executor framework.
//!
//! # Architecture
//!
//! ```text
//! Service Components          Adapters                  Job Framework
//! ─────────────────          ────────                  ─────────────
//! AsyncProviderType    →  AsyncProviderAdapter    →  ChunkProvider
//! DdsTextureEncoder    →  TextureEncoderAdapter   →  TextureEncoderAsync
//! cache::MemoryCache   →  ExecutorCacheAdapter    →  DaemonMemoryCache
//! cache_dir            →  ParallelDiskCache       →  DiskCache
//! (implicit)           →  TokioExecutor           →  BlockingExecutor
//!                             ↓
//!                      DefaultDdsJobFactory
//!                             ↓
//!                      XEarthLayerRuntime
//!                             ↓
//!                         DdsClient
//! ```

use crate::cache::MemoryCache;
use crate::dds::DdsFormat;
use crate::executor::ExecutorCacheAdapter;
use crate::jobs::DefaultDdsJobFactory;
use crate::pipeline::adapters::{
    AsyncProviderAdapter, NullDiskCache, ParallelDiskCache, TextureEncoderAdapter,
};
use crate::pipeline::{StorageConcurrencyLimiter, TokioExecutor};
use crate::provider::AsyncProviderType;
use crate::runtime::{RuntimeConfig, XEarthLayerRuntime};
use crate::texture::DdsTextureEncoder;
use std::path::PathBuf;
use std::sync::Arc;

// Type aliases for the concrete adapter types
type ProviderAdapter = AsyncProviderAdapter<AsyncProviderType>;
type EncoderAdapter = TextureEncoderAdapter<Arc<DdsTextureEncoder>>;

/// Builder for creating an XEarthLayerRuntime with the service's dependencies.
///
/// This builder collects all the necessary components and creates the properly
/// wired runtime with adapters that bridge service types to job framework traits.
pub struct RuntimeBuilder {
    /// Provider name for cache directory hierarchy
    provider_name: String,
    /// DDS format for cache key generation
    format: DdsFormat,
    /// Async provider for chunk downloads
    async_provider: Option<Arc<AsyncProviderType>>,
    /// Texture encoder for DDS compression
    encoder: Arc<DdsTextureEncoder>,
    /// Memory cache (raw cache, will be wrapped in ExecutorCacheAdapter)
    memory_cache: Option<Arc<MemoryCache>>,
    /// Cache directory for disk cache
    cache_dir: Option<PathBuf>,
    /// Shared disk I/O limiter (for global limiting across packages)
    disk_io_limiter: Option<Arc<StorageConcurrencyLimiter>>,
    /// Runtime configuration
    config: RuntimeConfig,
}

impl RuntimeBuilder {
    /// Creates a new runtime builder.
    ///
    /// # Arguments
    ///
    /// * `provider_name` - Provider name (e.g., "bing", "go2")
    /// * `format` - DDS format (BC1 or BC3)
    /// * `encoder` - Texture encoder for DDS compression
    pub fn new(
        provider_name: impl Into<String>,
        format: DdsFormat,
        encoder: Arc<DdsTextureEncoder>,
    ) -> Self {
        Self {
            provider_name: provider_name.into(),
            format,
            async_provider: None,
            encoder,
            memory_cache: None,
            cache_dir: None,
            disk_io_limiter: None,
            config: RuntimeConfig::default(),
        }
    }

    /// Sets the async provider for chunk downloads.
    pub fn with_async_provider(mut self, provider: Arc<AsyncProviderType>) -> Self {
        self.async_provider = Some(provider);
        self
    }

    /// Sets the memory cache.
    ///
    /// The cache will be wrapped in an `ExecutorCacheAdapter` that implements
    /// `DaemonMemoryCache` for the executor daemon.
    pub fn with_memory_cache(mut self, cache: Arc<MemoryCache>) -> Self {
        self.memory_cache = Some(cache);
        self
    }

    /// Sets the cache directory for disk cache.
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = Some(dir);
        self
    }

    /// Sets the shared disk I/O limiter.
    ///
    /// When multiple packages are mounted, sharing a single limiter coordinates
    /// disk I/O across all cache instances.
    pub fn with_disk_io_limiter(mut self, limiter: Arc<StorageConcurrencyLimiter>) -> Self {
        self.disk_io_limiter = Some(limiter);
        self
    }

    /// Sets the runtime configuration.
    pub fn with_config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    /// Builds the XEarthLayerRuntime with disk caching enabled.
    ///
    /// This method creates the runtime with a `ParallelDiskCache` for chunk caching.
    ///
    /// # Panics
    ///
    /// Panics if required components are not set:
    /// - `async_provider` is required
    /// - `memory_cache` is required
    /// - `cache_dir` is required (use `build_without_disk_cache` if disk cache not needed)
    pub fn build(self) -> XEarthLayerRuntime {
        let async_provider = self
            .async_provider
            .expect("RuntimeBuilder: async_provider is required");
        let memory_cache = self
            .memory_cache
            .expect("RuntimeBuilder: memory_cache is required");
        let cache_dir = self.cache_dir.expect(
            "RuntimeBuilder: cache_dir is required (use build_without_disk_cache if not needed)",
        );

        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            memory_cache,
            &self.provider_name,
            self.format,
        ));

        // Create disk cache
        let disk_cache = if let Some(ref limiter) = self.disk_io_limiter {
            Arc::new(ParallelDiskCache::with_shared_limiter(
                cache_dir,
                &self.provider_name,
                Arc::clone(limiter),
            ))
        } else {
            let default_concurrent = std::thread::available_parallelism()
                .map(|n| (n.get() * 16).min(256))
                .unwrap_or(64);
            Arc::new(ParallelDiskCache::new(
                cache_dir,
                &self.provider_name,
                default_concurrent,
            ))
        };

        let factory = Self::create_factory_with_disk_cache(
            async_provider,
            Arc::clone(&self.encoder),
            Arc::clone(&cache_adapter),
            disk_cache,
        );

        XEarthLayerRuntime::new(factory, cache_adapter, self.config)
    }

    /// Builds the XEarthLayerRuntime without disk caching.
    ///
    /// This method creates the runtime with a `NullDiskCache` that performs no I/O.
    /// Useful for testing or scenarios where chunk persistence is not needed.
    ///
    /// # Panics
    ///
    /// Panics if required components are not set:
    /// - `async_provider` is required
    /// - `memory_cache` is required
    pub fn build_without_disk_cache(self) -> XEarthLayerRuntime {
        let async_provider = self
            .async_provider
            .expect("RuntimeBuilder: async_provider is required");
        let memory_cache = self
            .memory_cache
            .expect("RuntimeBuilder: memory_cache is required");

        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            memory_cache,
            &self.provider_name,
            self.format,
        ));

        let factory = Self::create_factory_without_disk_cache(
            async_provider,
            Arc::clone(&self.encoder),
            Arc::clone(&cache_adapter),
        );

        XEarthLayerRuntime::new(factory, cache_adapter, self.config)
    }

    /// Creates a factory with ParallelDiskCache.
    fn create_factory_with_disk_cache(
        async_provider: Arc<AsyncProviderType>,
        encoder: Arc<DdsTextureEncoder>,
        cache_adapter: Arc<ExecutorCacheAdapter>,
        disk_cache: Arc<ParallelDiskCache>,
    ) -> Arc<
        DefaultDdsJobFactory<
            ProviderAdapter,
            EncoderAdapter,
            ExecutorCacheAdapter,
            ParallelDiskCache,
            TokioExecutor,
        >,
    > {
        let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_provider));
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));
        let executor = Arc::new(TokioExecutor::new());

        Arc::new(DefaultDdsJobFactory::new(
            provider_adapter,
            encoder_adapter,
            cache_adapter,
            disk_cache,
            executor,
        ))
    }

    /// Creates a factory with NullDiskCache.
    fn create_factory_without_disk_cache(
        async_provider: Arc<AsyncProviderType>,
        encoder: Arc<DdsTextureEncoder>,
        cache_adapter: Arc<ExecutorCacheAdapter>,
    ) -> Arc<
        DefaultDdsJobFactory<
            ProviderAdapter,
            EncoderAdapter,
            ExecutorCacheAdapter,
            NullDiskCache,
            TokioExecutor,
        >,
    > {
        let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_provider));
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());

        Arc::new(DefaultDdsJobFactory::new(
            provider_adapter,
            encoder_adapter,
            cache_adapter,
            disk_cache,
            executor,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MemoryCache;
    use crate::dds::DdsFormat;
    use crate::provider::{AsyncBingMapsProvider, AsyncProviderType, AsyncReqwestClient};
    use crate::texture::DdsTextureEncoder;

    fn create_test_encoder() -> Arc<DdsTextureEncoder> {
        Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1))
    }

    fn create_test_memory_cache() -> Arc<MemoryCache> {
        Arc::new(MemoryCache::new(1024 * 1024)) // 1MB
    }

    /// Creates a test provider using the real HTTP client.
    /// The client is only constructed, not used - no network calls are made.
    fn create_test_provider() -> Arc<AsyncProviderType> {
        let http_client = AsyncReqwestClient::new().expect("Failed to create HTTP client");
        let provider = AsyncBingMapsProvider::new(http_client);
        Arc::new(AsyncProviderType::Bing(provider))
    }

    #[test]
    fn test_builder_creation() {
        let encoder = create_test_encoder();
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, encoder);

        assert_eq!(builder.provider_name, "test");
        assert!(builder.async_provider.is_none());
        assert!(builder.memory_cache.is_none());
    }

    #[test]
    fn test_builder_with_cache_dir() {
        let encoder = create_test_encoder();
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_cache_dir(PathBuf::from("/tmp/cache"));

        assert!(builder.cache_dir.is_some());
    }

    #[tokio::test]
    async fn test_builder_builds_runtime_without_disk_cache() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();
        let async_provider = create_test_provider();

        let runtime = RuntimeBuilder::new("bing", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_memory_cache(cache)
            .build_without_disk_cache();

        assert!(runtime.is_running());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn test_builder_builds_runtime_with_disk_cache() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();
        let async_provider = create_test_provider();

        // Use temp dir for test
        let temp_dir = std::env::temp_dir().join("xearthlayer_test_runtime_builder");

        let runtime = RuntimeBuilder::new("bing", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_memory_cache(cache)
            .with_cache_dir(temp_dir)
            .build();

        assert!(runtime.is_running());
        runtime.shutdown().await;
    }

    #[test]
    #[should_panic(expected = "async_provider is required")]
    fn test_builder_panics_without_provider() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();

        RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_memory_cache(cache)
            .build_without_disk_cache();
    }

    #[test]
    #[should_panic(expected = "memory_cache is required")]
    fn test_builder_panics_without_cache() {
        let encoder = create_test_encoder();
        let async_provider = create_test_provider();

        RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .build_without_disk_cache();
    }
}
