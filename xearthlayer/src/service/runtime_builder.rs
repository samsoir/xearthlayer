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
//! cache_dir            →  DiskCacheAdapter        →  DiskCache
//! (implicit)           →  TokioExecutor           →  BlockingExecutor
//!                             ↓
//!                      DefaultDdsJobFactory
//!                             ↓
//!                      XEarthLayerRuntime
//!                             ↓
//!                         DdsClient
//! ```

use crate::cache::adapters::MemoryCacheBridge;
use crate::cache::MemoryCache;
use crate::config::DEFAULT_PREFETCH_CAPACITY_SHARE;
use crate::dds::DdsFormat;
use crate::executor::{
    AsyncProviderAdapter, DdsDiskCache, DiskCache, DiskCacheAdapter, DownloadConfig,
    ExecutorCacheAdapter, LaneSizing, NullDdsDiskCache, NullDiskCache, TextureEncoderAdapter,
    TokioExecutor,
};
use crate::jobs::DefaultDdsJobFactory;
use crate::metrics::MetricsClient;
use crate::provider::AsyncProviderType;
use crate::runtime::{RuntimeConfig, XEarthLayerRuntime};
use crate::service::cache_layer::ExecutorDiskCaches;
use crate::texture::DdsTextureEncoder;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

// Type aliases for the concrete adapter types
type ProviderAdapter = AsyncProviderAdapter<AsyncProviderType>;
type EncoderAdapter = TextureEncoderAdapter<Arc<DdsTextureEncoder>>;
/// A lane's job factory on the cache service's bridges.
type BridgeFactory<DD, D> =
    DefaultDdsJobFactory<ProviderAdapter, EncoderAdapter, MemoryCacheBridge, DD, D, TokioExecutor>;

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
    /// Texture encoder for DDS compression: the demand lane's, and the
    /// prefetch lane's unless [`with_prefetch_encoder`](Self::with_prefetch_encoder)
    /// gives that lane its own.
    encoder: Arc<DdsTextureEncoder>,
    /// The prefetch lane's encoder, when it is not `encoder` (#421: with
    /// the GPU it sends on the prefetch channel).
    prefetch_encoder: Option<Arc<DdsTextureEncoder>>,
    /// Memory cache (raw cache, will be wrapped in ExecutorCacheAdapter)
    memory_cache: Option<Arc<MemoryCache>>,
    /// Cache directory for disk cache
    cache_dir: Option<PathBuf>,
    /// Runtime configuration
    config: RuntimeConfig,
    /// Tokio runtime handle for spawning tasks
    runtime_handle: Option<tokio::runtime::Handle>,
    /// Metrics client for event-based telemetry
    metrics_client: Option<MetricsClient>,
    /// Per-chunk download behaviour handed to the job factory.
    ///
    /// Carries `executor.request_timeout_secs` and `executor.max_retries`.
    /// Defaults to the executor's own defaults so an unconfigured builder
    /// behaves exactly as it did before #249 wired these keys.
    download_config: DownloadConfig,
    /// `prefetch.capacity_share`: the prefetch lane's size relative to the
    /// demand lane. Clamped to its range when the lanes are sized.
    capacity_share: f64,
    /// `network.connections`: scales the prefetch lane's HTTP permits (#367).
    network_level: crate::config::NetworkLevel,
}

impl RuntimeBuilder {
    /// Creates a new runtime builder.
    ///
    /// # Arguments
    ///
    /// * `provider_name` - Provider name (e.g., "bing", "go2")
    /// * `format` - DDS format (only BC1 is supported, #395)
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
            prefetch_encoder: None,
            memory_cache: None,
            cache_dir: None,
            config: RuntimeConfig::default(),
            runtime_handle: None,
            metrics_client: None,
            download_config: DownloadConfig::default(),
            capacity_share: DEFAULT_PREFETCH_CAPACITY_SHARE,
            network_level: crate::config::NetworkLevel::default(),
        }
    }

    /// Gives the prefetch lane its own encoder (#421). With the GPU encoder
    /// it is the one on the prefetch channel, which the GPU worker reads
    /// only while the demand channel is empty; nothing the demand lane holds
    /// can reach it. Without this both lanes share the builder's encoder, as
    /// the CPU encoders do.
    pub fn with_prefetch_encoder(mut self, encoder: Arc<DdsTextureEncoder>) -> Self {
        self.prefetch_encoder = Some(encoder);
        self
    }

    /// The demand and prefetch lanes' encoders: `encoder`, and `prefetch`
    /// when the prefetch lane has its own.
    fn lane_encoders(
        encoder: &Arc<DdsTextureEncoder>,
        prefetch: Option<&Arc<DdsTextureEncoder>>,
    ) -> (Arc<DdsTextureEncoder>, Arc<DdsTextureEncoder>) {
        (Arc::clone(encoder), Arc::clone(prefetch.unwrap_or(encoder)))
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

    /// Sets the per-chunk download behaviour (timeout and retries).
    ///
    /// Built from `executor.request_timeout_secs` and `executor.max_retries`
    /// by `ServiceConfig::chunk_download_config`.
    pub fn with_download_config(mut self, download_config: DownloadConfig) -> Self {
        self.download_config = download_config;
        self
    }

    /// Sets `prefetch.capacity_share`, which sizes the prefetch lane.
    pub fn with_capacity_share(mut self, share: f64) -> Self {
        self.capacity_share = share;
        self
    }

    /// Sets `network.connections`, which scales the prefetch lane's HTTP
    /// permits (#367). The demand lane's arrive scaled in the download config.
    pub fn with_network_level(mut self, level: crate::config::NetworkLevel) -> Self {
        self.network_level = level;
        self
    }

    /// Both lanes' sizes: the configured share, then the network level.
    fn lane_sizing(share: f64, level: crate::config::NetworkLevel) -> LaneSizing {
        LaneSizing::from_configured_share(share).at_network_level(level)
    }

    /// The prefetch lane's download behaviour: the configured timeout and
    /// retries, but its own HTTP semaphore, so prefetch can never hold a
    /// connection permit a FUSE request is waiting for.
    fn prefetch_download_config(demand: &DownloadConfig, sizing: &LaneSizing) -> DownloadConfig {
        demand
            .clone()
            .with_http_semaphore(Arc::new(Semaphore::new(sizing.prefetch_http_permits)))
    }

    /// Sets the Tokio runtime handle for spawning tasks.
    ///
    /// This is required for the runtime to spawn background tasks.
    pub fn with_runtime_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.runtime_handle = Some(handle);
        self
    }

    /// Sets the metrics client for event-based telemetry.
    ///
    /// When provided, per-chunk download events and job lifecycle events
    /// flow to the metrics daemon for real-time dashboard updates.
    pub fn with_metrics_client(mut self, client: MetricsClient) -> Self {
        self.metrics_client = Some(client);
        self
    }

    /// Builds the XEarthLayerRuntime with disk caching enabled.
    ///
    /// This method creates the runtime with a `DiskCacheAdapter` for chunk caching.
    ///
    /// # Panics
    ///
    /// Panics if required components are not set:
    /// - `async_provider` is required
    /// - `memory_cache` is required
    /// - `cache_dir` is required (use `build_without_disk_cache` if disk cache not needed)
    /// - `runtime_handle` is required
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
        let runtime_handle = self
            .runtime_handle
            .expect("RuntimeBuilder: runtime_handle is required");

        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            memory_cache,
            &self.provider_name,
            self.format,
        ));

        // Create disk cache adapter
        let disk_cache = Arc::new(DiskCacheAdapter::new(cache_dir, &self.provider_name));

        let sizing = Self::lane_sizing(self.capacity_share, self.network_level);
        let (demand_encoder, prefetch_encoder) =
            Self::lane_encoders(&self.encoder, self.prefetch_encoder.as_ref());
        let demand_factory = Self::create_factory_with_disk_cache(
            Arc::clone(&async_provider),
            demand_encoder,
            Arc::clone(&cache_adapter),
            Arc::clone(&disk_cache),
            self.download_config.clone(),
        );
        let prefetch_factory = Self::create_factory_with_disk_cache(
            async_provider,
            prefetch_encoder,
            Arc::clone(&cache_adapter),
            disk_cache,
            Self::prefetch_download_config(&self.download_config, &sizing),
        )
        .with_cache_probe();

        let null_dds_disk = Arc::new(NullDdsDiskCache);
        XEarthLayerRuntime::with_metrics_client(
            Arc::new(demand_factory),
            Arc::new(prefetch_factory),
            cache_adapter,
            null_dds_disk,
            self.config.with_sizing(sizing),
            runtime_handle,
            self.metrics_client,
        )
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
    /// - `runtime_handle` is required
    pub fn build_without_disk_cache(self) -> XEarthLayerRuntime {
        let async_provider = self
            .async_provider
            .expect("RuntimeBuilder: async_provider is required");
        let memory_cache = self
            .memory_cache
            .expect("RuntimeBuilder: memory_cache is required");
        let runtime_handle = self
            .runtime_handle
            .expect("RuntimeBuilder: runtime_handle is required");

        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            memory_cache,
            &self.provider_name,
            self.format,
        ));

        let sizing = Self::lane_sizing(self.capacity_share, self.network_level);
        let (demand_encoder, prefetch_encoder) =
            Self::lane_encoders(&self.encoder, self.prefetch_encoder.as_ref());
        let demand_factory = Self::create_factory_without_disk_cache(
            Arc::clone(&async_provider),
            demand_encoder,
            Arc::clone(&cache_adapter),
            self.download_config.clone(),
        );
        let prefetch_factory = Self::create_factory_without_disk_cache(
            async_provider,
            prefetch_encoder,
            Arc::clone(&cache_adapter),
            Self::prefetch_download_config(&self.download_config, &sizing),
        )
        .with_cache_probe();

        let null_dds_disk = Arc::new(NullDdsDiskCache);
        XEarthLayerRuntime::with_metrics_client(
            Arc::new(demand_factory),
            Arc::new(prefetch_factory),
            cache_adapter,
            null_dds_disk,
            self.config.with_sizing(sizing),
            runtime_handle,
            self.metrics_client,
        )
    }

    /// Creates a factory with DiskCacheAdapter (no DDS disk cache).
    fn create_factory_with_disk_cache(
        async_provider: Arc<AsyncProviderType>,
        encoder: Arc<DdsTextureEncoder>,
        cache_adapter: Arc<ExecutorCacheAdapter>,
        disk_cache: Arc<DiskCacheAdapter>,
        download_config: DownloadConfig,
    ) -> DefaultDdsJobFactory<
        ProviderAdapter,
        EncoderAdapter,
        ExecutorCacheAdapter,
        NullDdsDiskCache,
        DiskCacheAdapter,
        TokioExecutor,
    > {
        let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_provider));
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));
        let dds_disk_cache = Arc::new(NullDdsDiskCache);
        let executor = Arc::new(TokioExecutor::new());

        DefaultDdsJobFactory::with_config(
            provider_adapter,
            encoder_adapter,
            cache_adapter,
            dds_disk_cache,
            disk_cache,
            executor,
            download_config,
        )
    }

    /// Creates a factory with NullDiskCache and NullDdsDiskCache.
    fn create_factory_without_disk_cache(
        async_provider: Arc<AsyncProviderType>,
        encoder: Arc<DdsTextureEncoder>,
        cache_adapter: Arc<ExecutorCacheAdapter>,
        download_config: DownloadConfig,
    ) -> DefaultDdsJobFactory<
        ProviderAdapter,
        EncoderAdapter,
        ExecutorCacheAdapter,
        NullDdsDiskCache,
        NullDiskCache,
        TokioExecutor,
    > {
        let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_provider));
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));
        let dds_disk_cache = Arc::new(NullDdsDiskCache);
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());

        DefaultDdsJobFactory::with_config(
            provider_adapter,
            encoder_adapter,
            cache_adapter,
            dds_disk_cache,
            disk_cache,
            executor,
            download_config,
        )
    }

    /// Builds the XEarthLayerRuntime using the cache service architecture.
    ///
    /// This method creates the runtime using the caches of the `CacheLayer`:
    /// its `MemoryCacheBridge`, and the disk caches it hands the executor.
    /// With [`ExecutorDiskCaches::Persistent`] those are the DDS and chunk
    /// tiers' bridges, which wrap `CacheService` instances that manage their
    /// own lifecycle. With [`ExecutorDiskCaches::Off`] (`run --no-cache`)
    /// both lanes get `NullDdsDiskCache` and `NullDiskCache`, which always
    /// miss and drop writes without touching the disk.
    ///
    /// # Arguments
    ///
    /// * `memory_bridge` - Memory cache bridge from `CacheLayer`
    /// * `disk_caches` - `CacheLayer::executor_disk_caches()`
    ///
    /// # Panics
    ///
    /// Panics if required components are not set:
    /// - `async_provider` is required
    /// - `runtime_handle` is required
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::service::RuntimeBuilder;
    ///
    /// let runtime = RuntimeBuilder::new(provider_name, format, encoder)
    ///     .with_async_provider(provider)
    ///     .with_runtime_handle(handle)
    ///     .build_with_cache_service(
    ///         cache_layer.memory_bridge(),
    ///         cache_layer.executor_disk_caches(),
    ///     );
    /// ```
    pub fn build_with_cache_service(
        self,
        memory_bridge: Arc<MemoryCacheBridge>,
        disk_caches: ExecutorDiskCaches,
    ) -> XEarthLayerRuntime {
        match disk_caches {
            ExecutorDiskCaches::Persistent { dds, chunks } => {
                self.build_with_disk_caches(memory_bridge, dds, chunks)
            }
            ExecutorDiskCaches::Off => self.build_with_disk_caches(
                memory_bridge,
                Arc::new(NullDdsDiskCache),
                Arc::new(NullDiskCache),
            ),
        }
    }

    /// Builds the runtime on the memory bridge and the given disk caches.
    fn build_with_disk_caches<DD, D>(
        self,
        memory_bridge: Arc<MemoryCacheBridge>,
        dds_disk_bridge: Arc<DD>,
        disk_bridge: Arc<D>,
    ) -> XEarthLayerRuntime
    where
        DD: DdsDiskCache + 'static,
        D: DiskCache + 'static,
    {
        let async_provider = self
            .async_provider
            .expect("RuntimeBuilder: async_provider is required");
        let runtime_handle = self
            .runtime_handle
            .expect("RuntimeBuilder: runtime_handle is required");

        let dds_disk_for_daemon = Arc::clone(&dds_disk_bridge);

        let sizing = Self::lane_sizing(self.capacity_share, self.network_level);
        let (demand_factory, prefetch_factory) = Self::lane_factories_with_bridges(
            Self::lane_encoders(&self.encoder, self.prefetch_encoder.as_ref()),
            async_provider,
            Arc::clone(&memory_bridge),
            dds_disk_bridge,
            disk_bridge,
            &self.download_config,
            &sizing,
        );

        XEarthLayerRuntime::with_metrics_client(
            Arc::new(demand_factory),
            Arc::new(prefetch_factory),
            memory_bridge,
            dds_disk_for_daemon,
            self.config.with_sizing(sizing),
            runtime_handle,
            self.metrics_client,
        )
    }

    /// The demand and prefetch lanes' factories on the cache service's
    /// bridges: each lane's encoder of `(demand, prefetch)` encoders
    /// ([`lane_encoders`](Self::lane_encoders)), and the prefetch lane's own
    /// HTTP semaphore and cache probe.
    fn lane_factories_with_bridges<DD, D>(
        (demand_encoder, prefetch_encoder): (Arc<DdsTextureEncoder>, Arc<DdsTextureEncoder>),
        async_provider: Arc<AsyncProviderType>,
        memory_bridge: Arc<MemoryCacheBridge>,
        dds_disk_bridge: Arc<DD>,
        disk_bridge: Arc<D>,
        download_config: &DownloadConfig,
        sizing: &LaneSizing,
    ) -> (BridgeFactory<DD, D>, BridgeFactory<DD, D>)
    where
        DD: DdsDiskCache,
        D: DiskCache,
    {
        let demand = Self::create_factory_with_bridges(
            Arc::clone(&async_provider),
            demand_encoder,
            Arc::clone(&memory_bridge),
            Arc::clone(&dds_disk_bridge),
            Arc::clone(&disk_bridge),
            download_config.clone(),
        );
        let prefetch = Self::create_factory_with_bridges(
            async_provider,
            prefetch_encoder,
            memory_bridge,
            dds_disk_bridge,
            disk_bridge,
            Self::prefetch_download_config(download_config, sizing),
        )
        .with_cache_probe();
        (demand, prefetch)
    }

    /// Creates a factory with bridge adapters from the cache service.
    fn create_factory_with_bridges<DD, D>(
        async_provider: Arc<AsyncProviderType>,
        encoder: Arc<DdsTextureEncoder>,
        memory_bridge: Arc<MemoryCacheBridge>,
        dds_disk_bridge: Arc<DD>,
        disk_bridge: Arc<D>,
        download_config: DownloadConfig,
    ) -> DefaultDdsJobFactory<
        ProviderAdapter,
        EncoderAdapter,
        MemoryCacheBridge,
        DD,
        D,
        TokioExecutor,
    >
    where
        DD: DdsDiskCache,
        D: DiskCache,
    {
        let provider_adapter = Arc::new(AsyncProviderAdapter::from_arc(async_provider));
        let encoder_adapter = Arc::new(TextureEncoderAdapter::new(encoder));
        let executor = Arc::new(TokioExecutor::new());

        DefaultDdsJobFactory::with_config(
            provider_adapter,
            encoder_adapter,
            memory_bridge,
            dds_disk_bridge,
            disk_bridge,
            executor,
            download_config,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MemoryCache;
    use crate::dds::DdsFormat;
    use crate::provider::{AsyncBingMapsProvider, AsyncProviderType, AsyncReqwestClient};
    use crate::texture::DdsTextureEncoder;
    use std::time::Duration;

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

    /// #249: the configured per-chunk timeout and retry count reach the
    /// executor only if every factory helper forwards them. Each helper is a
    /// separate opportunity to drop the config on the floor, so each is
    /// asserted against a value that is not the default.
    #[test]
    fn factory_without_disk_cache_forwards_the_download_config() {
        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            create_test_memory_cache(),
            "bing",
            DdsFormat::BC1,
        ));

        let factory = RuntimeBuilder::create_factory_without_disk_cache(
            create_test_provider(),
            create_test_encoder(),
            cache_adapter,
            DownloadConfig::new(Duration::from_secs(45), 7),
        );

        assert_eq!(
            factory.download_config().request_timeout,
            Duration::from_secs(45)
        );
        assert_eq!(factory.download_config().max_retries, 7);
    }

    #[test]
    fn factory_with_disk_cache_forwards_the_download_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache_adapter = Arc::new(ExecutorCacheAdapter::new(
            create_test_memory_cache(),
            "bing",
            DdsFormat::BC1,
        ));
        let disk_cache = Arc::new(DiskCacheAdapter::new(temp.path().to_path_buf(), "bing"));

        let factory = RuntimeBuilder::create_factory_with_disk_cache(
            create_test_provider(),
            create_test_encoder(),
            cache_adapter,
            disk_cache,
            DownloadConfig::new(Duration::from_secs(45), 7),
        );

        assert_eq!(
            factory.download_config().request_timeout,
            Duration::from_secs(45)
        );
        assert_eq!(factory.download_config().max_retries, 7);
    }

    #[test]
    fn download_config_defaults_to_the_executor_default() {
        // A builder nobody configures must behave exactly as before.
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, create_test_encoder());

        assert_eq!(
            builder.download_config.request_timeout,
            DownloadConfig::default().request_timeout
        );
        assert_eq!(
            builder.download_config.max_retries,
            DownloadConfig::default().max_retries
        );
    }

    #[test]
    fn the_network_level_sizes_the_prefetch_connections() {
        use crate::config::NetworkLevel;
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, create_test_encoder());
        // Defaults to the config default, as the capacity share does.
        assert_eq!(builder.network_level, NetworkLevel::High);
        assert_eq!(
            RuntimeBuilder::lane_sizing(builder.capacity_share, builder.network_level)
                .prefetch_http_permits,
            820
        );
        let builder = builder.with_network_level(NetworkLevel::Low);
        assert_eq!(
            RuntimeBuilder::lane_sizing(builder.capacity_share, builder.network_level)
                .prefetch_http_permits,
            666
        );
    }

    /// Without a prefetch encoder both lanes share the builder's (the CPU
    /// encoders); with one, the prefetch lane gets it (#421).
    #[test]
    fn the_prefetch_lane_gets_its_own_encoder_when_given_one() {
        let encoder = create_test_encoder();
        let (demand, prefetch) = RuntimeBuilder::lane_encoders(&encoder, None);
        assert!(Arc::ptr_eq(&demand, &encoder) && Arc::ptr_eq(&prefetch, &encoder));

        let own = create_test_encoder();
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, Arc::clone(&encoder))
            .with_prefetch_encoder(Arc::clone(&own));
        let (demand, prefetch) =
            RuntimeBuilder::lane_encoders(&builder.encoder, builder.prefetch_encoder.as_ref());
        assert!(Arc::ptr_eq(&demand, &encoder) && Arc::ptr_eq(&prefetch, &own));
    }

    /// The wiring test of #421: with the GPU encoder, the demand factory's
    /// encoder sends on the demand channel and the prefetch factory's on
    /// the prefetch channel, from the service's lane encoders through the
    /// factories the runtime is built with.
    #[tokio::test]
    async fn each_lane_factory_encodes_on_its_own_gpu_channel() {
        use crate::dds::gpu_channel::{gpu_lanes, GpuEncodeReply, GpuTileTiming};
        use crate::dds::gpu_health::SharedGpuHealth;
        use crate::executor::TextureEncoderAsync;

        let (channels, mut queues) = gpu_lanes(
            4,
            SharedGpuHealth::new(),
            tokio_util::sync::CancellationToken::new(),
        );
        let encoders =
            crate::service::builder::gpu_lane_encoders(channels, DdsFormat::BC1, Some(1));
        let temp_dir = tempfile::tempdir().unwrap();
        let config = crate::service::ServiceConfig::builder()
            .cache_directory(temp_dir.path().join("cache"))
            .cache_memory_size(1_000_000)
            .disk_cache_enabled(false)
            .build();
        let metrics = crate::metrics::MetricsSystem::new(&tokio::runtime::Handle::current());
        let cache_layer = crate::service::CacheLayer::new(&config, "bing", metrics.client())
            .await
            .unwrap();
        let (demand, prefetch) = RuntimeBuilder::lane_factories_with_bridges(
            encoders,
            create_test_provider(),
            cache_layer.memory_bridge(),
            Arc::new(NullDdsDiskCache),
            Arc::new(NullDiskCache),
            &DownloadConfig::default(),
            &LaneSizing::from_configured_share(DEFAULT_PREFETCH_CAPACITY_SHARE),
        );

        for (factory, lane) in [(&demand, "demand"), (&prefetch, "prefetch")] {
            // The production path: the job's awaited encode (#422).
            let encoder = Arc::clone(factory.encoder());
            let caller = tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                encoder
                    .encode(image::RgbaImage::new(4, 4), &cancel, &TokioExecutor::new())
                    .await
                    .expect("no blocking work")
            });
            let (ours, other) = match lane {
                "demand" => (&mut queues.demand, &mut queues.prefetch),
                _ => (&mut queues.prefetch, &mut queues.demand),
            };
            let request = tokio::time::timeout(Duration::from_secs(5), ours.recv())
                .await
                .unwrap_or_else(|_| panic!("the {lane} encoder sent on the {lane} channel"))
                .unwrap();
            assert!(
                other.try_recv().is_err(),
                "{lane}: nothing on the other lane"
            );
            // The worker's answer: the file it was sent, filled in place.
            let _ = request.response.send(GpuEncodeReply::Processed {
                result: Ok(request.output),
                timing: GpuTileTiming::default(),
            });
            let dds = caller.await.unwrap().expect("the reply is the tile");
            assert_eq!(dds.len(), crate::dds::layout::HEADER_LEN + 8, "{lane}");
        }
        cache_layer.shutdown().await;
    }

    #[test]
    fn capacity_share_defaults_to_the_config_default() {
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, create_test_encoder());
        assert_eq!(builder.capacity_share, DEFAULT_PREFETCH_CAPACITY_SHARE);
        let builder = builder.with_capacity_share(0.2);
        assert_eq!(builder.capacity_share, 0.2);
    }

    /// #296: prefetch keeps the configured timeout and retries but must not
    /// share the demand lane's HTTP semaphore.
    #[test]
    fn prefetch_download_config_has_its_own_http_semaphore() {
        let builder = RuntimeBuilder::new("test", DdsFormat::BC1, create_test_encoder())
            .with_download_config(DownloadConfig::new(Duration::from_secs(45), 7))
            .with_capacity_share(0.35);
        let sizing = LaneSizing::from_configured_share(builder.capacity_share);

        let prefetch = RuntimeBuilder::prefetch_download_config(&builder.download_config, &sizing);

        assert_eq!(prefetch.request_timeout, Duration::from_secs(45));
        assert_eq!(prefetch.max_retries, 7);
        assert_eq!(prefetch.http_capacity(), sizing.prefetch_http_permits);
        assert!(!Arc::ptr_eq(
            &prefetch.http_semaphore,
            &builder.download_config.http_semaphore
        ));
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
        let handle = tokio::runtime::Handle::current();

        let runtime = RuntimeBuilder::new("bing", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_memory_cache(cache)
            .with_runtime_handle(handle)
            .build_without_disk_cache();

        assert!(runtime.is_running());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn test_builder_builds_runtime_with_disk_cache() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();
        let async_provider = create_test_provider();
        let handle = tokio::runtime::Handle::current();

        // Use temp dir for test
        let temp_dir = std::env::temp_dir().join("xearthlayer_test_runtime_builder");

        let runtime = RuntimeBuilder::new("bing", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_memory_cache(cache)
            .with_cache_dir(temp_dir)
            .with_runtime_handle(handle)
            .build();

        assert!(runtime.is_running());
        runtime.shutdown().await;
    }

    /// #400: with disk caching off the cache layer hands the builder
    /// `ExecutorDiskCaches::Off`, and the runtime starts on null disk caches.
    #[tokio::test]
    async fn builds_a_runtime_with_disk_caching_off() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let config = crate::service::ServiceConfig::builder()
            .cache_directory(cache_dir.clone())
            .cache_memory_size(1_000_000)
            .disk_cache_enabled(false)
            .build();
        let metrics = crate::metrics::MetricsSystem::new(&tokio::runtime::Handle::current());
        let cache_layer = crate::service::CacheLayer::new(&config, "bing", metrics.client())
            .await
            .unwrap();

        let runtime = RuntimeBuilder::new("bing", DdsFormat::BC1, create_test_encoder())
            .with_async_provider(create_test_provider())
            .with_runtime_handle(tokio::runtime::Handle::current())
            .build_with_cache_service(
                cache_layer.memory_bridge(),
                cache_layer.executor_disk_caches(),
            );

        assert!(runtime.is_running());
        runtime.shutdown().await;
        cache_layer.shutdown().await;
        assert!(!cache_dir.exists());
    }

    #[tokio::test]
    #[should_panic(expected = "async_provider is required")]
    async fn test_builder_panics_without_provider() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();
        let handle = tokio::runtime::Handle::current();

        RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_memory_cache(cache)
            .with_runtime_handle(handle)
            .build_without_disk_cache();
    }

    #[tokio::test]
    #[should_panic(expected = "memory_cache is required")]
    async fn test_builder_panics_without_cache() {
        let encoder = create_test_encoder();
        let async_provider = create_test_provider();
        let handle = tokio::runtime::Handle::current();

        RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_runtime_handle(handle)
            .build_without_disk_cache();
    }

    #[tokio::test]
    #[should_panic(expected = "runtime_handle is required")]
    async fn test_builder_panics_without_runtime_handle() {
        let encoder = create_test_encoder();
        let cache = create_test_memory_cache();
        let async_provider = create_test_provider();

        RuntimeBuilder::new("test", DdsFormat::BC1, encoder)
            .with_async_provider(async_provider)
            .with_memory_cache(cache)
            .build_without_disk_cache();
    }
}
