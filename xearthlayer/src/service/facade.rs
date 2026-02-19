//! XEarthLayer service facade implementation.

use super::builder::{self, AsyncProviderComponents, CacheComponents, ProviderComponents};
use super::cache_layer::CacheLayer;
use super::config::ServiceConfig;
use super::error::ServiceError;
use super::fuse_mount::{FuseMountConfig, FuseMountService};
use super::runtime_builder::RuntimeBuilder;
use crate::cache::adapters::{DiskCacheBridge, MemoryCacheBridge};
use crate::cache::{disk_cache_stats, GcSchedulerDaemon, MemoryCache};
use crate::config::DiskIoProfile;
use crate::executor::{DdsClient, MemoryCacheAdapter};
use crate::fuse::{MountHandle, SpawnedMountHandle};
use crate::log::Logger;
use crate::metrics::{MetricsSystem, TelemetrySnapshot, TuiReporter};
use crate::prefetch::{FuseLoadMonitor, TileRequestCallback};
use crate::provider::ProviderConfig;
use crate::runtime::{SharedRuntimeHealth, SharedTileProgressTracker, XEarthLayerRuntime};
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// High-level facade for XEarthLayer operations.
///
/// Encapsulates all component creation and wiring, providing a simplified
/// API for serving satellite imagery tiles via FUSE.
///
/// # Runtime Management
///
/// The service can be created with either a default runtime or an injected handle:
///
/// ```ignore
/// // Option 1: Default runtime (convenience)
/// let service = XEarthLayerService::new(config, provider_config, logger)?;
///
/// // Option 2: Injected runtime handle (for DI/testing)
/// let runtime = Runtime::new()?;
/// let service = XEarthLayerService::with_runtime(
///     config, provider_config, logger, runtime.handle().clone()
/// )?;
/// ```
///
/// # Example
///
/// ```ignore
/// use xearthlayer::service::{XEarthLayerService, ServiceConfig};
/// use xearthlayer::provider::ProviderConfig;
///
/// // Create service with default configuration
/// let config = ServiceConfig::default();
/// let service = XEarthLayerService::new(config, ProviderConfig::bing(), logger)?;
///
/// // Mount FUSE filesystem
/// let handle = service.mount_package_async(package_path).await?;
/// ```
pub struct XEarthLayerService {
    /// Service configuration
    config: ServiceConfig,
    /// Provider name (for cache and logging)
    provider_name: String,
    /// Provider's maximum supported zoom level
    max_zoom: u8,
    /// Logger for diagnostic output
    logger: Arc<dyn Logger>,
    // -------------------------------------------------------------------------
    // RAII Fields: Kept alive for ownership semantics, not read after construction.
    // Dropping these would stop background threads/resources.
    // Prefixed with underscore to indicate intentional ownership-only storage.
    // -------------------------------------------------------------------------
    /// Owned Tokio runtime (when created via `new()`).
    _owned_runtime: Option<Runtime>,
    /// Cache layer with internal GC daemon (when created via `start()`).
    /// Use `shutdown_cache()` for graceful shutdown.
    cache_layer: Option<CacheLayer>,

    /// Handle to the Tokio runtime
    runtime_handle: Handle,

    /// Texture encoder for DDS generation
    dds_encoder: Arc<DdsTextureEncoder>,
    /// Shared memory cache for tile-level caching
    memory_cache: Option<Arc<MemoryCache>>,
    /// Shared memory cache adapter (implements executor::MemoryCache trait)
    memory_cache_adapter: Option<Arc<MemoryCacheAdapter>>,
    /// Metrics system for event-based telemetry
    metrics_system: Option<MetricsSystem>,
    /// XEarthLayer runtime (job executor daemon)
    xearthlayer_runtime: Option<XEarthLayerRuntime>,
    /// DDS client for requesting tile generation
    dds_client: Option<Arc<dyn DdsClient>>,
    /// Tile request callback for FUSE-based position inference
    tile_request_callback: Option<TileRequestCallback>,
    /// Load monitor for circuit breaker integration.
    /// When set, records FUSE-originated requests for aggregate load tracking.
    load_monitor: Option<Arc<dyn FuseLoadMonitor>>,
    /// Memory cache bridge from new cache service architecture.
    /// Used by prefetch system when cache bridges are enabled.
    memory_cache_bridge: Option<Arc<MemoryCacheBridge>>,
    /// GC scheduler daemon handle (when started via `start()`).
    /// Kept alive for ownership - dropping would stop the scheduler.
    gc_scheduler_handle: Option<JoinHandle<()>>,
    /// GC scheduler shutdown token for graceful shutdown.
    gc_scheduler_shutdown: Option<CancellationToken>,
}

impl XEarthLayerService {
    /// Create a new XEarthLayer service with a default Tokio runtime.
    ///
    /// This is a convenience constructor that creates its own runtime internally
    /// with default SSD disk profile settings.
    /// For advanced use cases or testing, use [`with_runtime`] instead.
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub fn new(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
    ) -> Result<Self, ServiceError> {
        Self::with_disk_profile(config, provider_config, logger, DiskIoProfile::default())
    }

    /// Create a new XEarthLayer service with a Tokio runtime tuned for the disk profile.
    ///
    /// This constructor configures the Tokio runtime's blocking thread pool
    /// based on the storage profile:
    /// - **HDD**: Conservative blocking threads (seek-bound operations)
    /// - **SSD**: Moderate blocking threads (SATA queue depth)
    /// - **NVMe**: Higher blocking threads (multiple queues)
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    /// * `disk_profile` - Storage profile for tuning (use Auto for detection)
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub fn with_disk_profile(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        disk_profile: DiskIoProfile,
    ) -> Result<Self, ServiceError> {
        // Get CPU count for worker threads
        const DEFAULT_CPU_FALLBACK: usize = 4;
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(DEFAULT_CPU_FALLBACK);

        // Get max blocking threads based on disk profile
        let max_blocking_threads = disk_profile.max_blocking_threads();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_cpus)
            .max_blocking_threads(max_blocking_threads)
            .enable_all()
            .thread_name("xearthlayer-tokio")
            .build()
            .map_err(|e| ServiceError::RuntimeError(format!("failed to create runtime: {}", e)))?;

        tracing::info!(
            worker_threads = num_cpus,
            max_blocking_threads = max_blocking_threads,
            disk_profile = %disk_profile,
            "Created Tokio runtime with configured thread pools"
        );

        let handle = runtime.handle().clone();

        Self::build(config, provider_config, logger, handle, Some(runtime))
    }

    /// Create a new XEarthLayer service with a provided runtime handle.
    ///
    /// Use this constructor when you want to control the runtime lifecycle
    /// externally, or for testing with injected runtimes.
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    /// * `runtime_handle` - Handle to an existing Tokio runtime
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub fn with_runtime(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
    ) -> Result<Self, ServiceError> {
        Self::build(config, provider_config, logger, runtime_handle, None)
    }

    /// Create a new XEarthLayer service with cache bridges from the new cache service.
    ///
    /// This constructor uses the new `CacheService` architecture where:
    /// - `MemoryCacheBridge` implements `executor::MemoryCache`
    /// - `DiskCacheBridge` implements `executor::DiskCache` with **internal GC daemon**
    ///
    /// This eliminates the need for external GC daemon management and ensures
    /// garbage collection runs regardless of how the application is started
    /// (TUI vs non-TUI modes).
    ///
    /// # Arguments
    ///
    /// * `config` - Service configuration
    /// * `provider_config` - Provider-specific configuration
    /// * `logger` - Logger implementation
    /// * `runtime_handle` - Handle to an existing Tokio runtime
    /// * `memory_bridge` - Memory cache bridge from `CacheLayer`
    /// * `disk_bridge` - Disk cache bridge from `CacheLayer`
    ///
    /// # Note
    ///
    /// This method is primarily for internal use. Prefer using
    /// [`XEarthLayerService::start()`] which handles cache lifecycle automatically.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::service::XEarthLayerService;
    /// use xearthlayer::cache::adapters::{MemoryCacheBridge, DiskCacheBridge};
    ///
    /// let service = XEarthLayerService::with_cache_bridges(
    ///     service_config,
    ///     provider_config,
    ///     logger,
    ///     runtime_handle,
    ///     memory_bridge,
    ///     disk_bridge,
    /// )?;
    /// ```
    pub fn with_cache_bridges(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
        memory_bridge: Arc<MemoryCacheBridge>,
        disk_bridge: Arc<DiskCacheBridge>,
    ) -> Result<Self, ServiceError> {
        Self::build_with_bridges(
            config,
            provider_config,
            logger,
            runtime_handle,
            None,
            memory_bridge,
            disk_bridge,
        )
    }

    /// Start a new XEarthLayerService with integrated cache and metrics.
    ///
    /// This is the recommended way to create a service. It ensures:
    /// 1. MetricsSystem is created first
    /// 2. CacheLayer is created with MetricsClient (enables GC reporting)
    /// 3. All components are properly wired
    ///
    /// The service owns the CacheLayer, ensuring GC continues running
    /// for the lifetime of the service.
    ///
    /// # Note
    ///
    /// This async method uses the current runtime via `Handle::current()`.
    /// Use `start_with_owned_runtime()` if you need the service to own
    /// its own runtime (e.g., when calling from a sync context).
    pub async fn start(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
    ) -> Result<Self, ServiceError> {
        // Use the current runtime - caller is responsible for keeping it alive
        let runtime_handle = Handle::current();

        tracing::info!("Creating XEarthLayerService using current runtime");

        // 1. Create MetricsSystem FIRST (so we can inject client into caches)
        let metrics_system = MetricsSystem::new(&runtime_handle);
        let metrics_client = metrics_system.client();

        // 2. Create async provider (no sync provider needed - legacy pipeline removed)
        let AsyncProviderComponents {
            async_provider,
            name: provider_name,
            max_zoom,
        } = builder::create_async_provider(&provider_config).await?;

        // 3. Create CacheLayer WITH metrics for GC reporting
        let cache_layer = CacheLayer::new(&config, &provider_name, metrics_client.clone()).await?;

        // 4. Scan initial disk cache size and report to metrics
        let initial_disk_size = cache_layer.scan_disk_cache_size().await;
        metrics_client.disk_cache_initial_size(initial_disk_size);

        tracing::info!(
            initial_disk_size_bytes = initial_disk_size,
            "Disk cache initial size scanned"
        );

        // 5. Create texture encoder
        let dds_encoder = builder::create_encoder(&config);

        // 6. Create XEarthLayer runtime with cache bridges
        // Clone metrics_client before passing to RuntimeBuilder (we need it for GC daemon)
        let gc_metrics_client = metrics_client.clone();

        let xel_runtime = RuntimeBuilder::new(
            &provider_name,
            config.texture().format(),
            Arc::clone(&dds_encoder),
        )
        .with_async_provider(Arc::clone(&async_provider))
        .with_runtime_handle(runtime_handle.clone())
        .with_metrics_client(metrics_client)
        .build_with_cache_service(cache_layer.memory_bridge(), cache_layer.disk_bridge());

        let dds_client = xel_runtime.dds_client();

        // 7. Create and spawn GC scheduler daemon
        let (gc_scheduler_handle, gc_scheduler_shutdown) = if let Some(disk_provider) =
            cache_layer.disk_provider()
        {
            let gc_shutdown = CancellationToken::new();
            let job_submitter = xel_runtime.job_submitter();
            let lru_index = disk_provider.lru_index();
            let cache_dir = disk_provider.directory().to_path_buf();
            let max_size = disk_provider.max_size_bytes();

            let gc_daemon = GcSchedulerDaemon::new(lru_index, cache_dir, max_size, job_submitter)
                .with_metrics(gc_metrics_client);

            let gc_shutdown_clone = gc_shutdown.clone();
            let gc_handle = runtime_handle.spawn(async move {
                gc_daemon.run(gc_shutdown_clone).await;
            });

            tracing::info!("GC scheduler daemon started");
            (Some(gc_handle), Some(gc_shutdown))
        } else {
            tracing::warn!("No disk provider available, GC scheduler not started");
            (None, None)
        };

        tracing::info!(
            provider = %provider_name,
            "XEarthLayerService started with integrated cache and metrics"
        );

        Ok(Self {
            config,
            provider_name,
            max_zoom,
            logger,
            _owned_runtime: None, // Caller owns the runtime
            runtime_handle,
            dds_encoder,
            memory_cache: None,
            memory_cache_adapter: None,
            metrics_system: Some(metrics_system),
            xearthlayer_runtime: Some(xel_runtime),
            dds_client: Some(dds_client),
            tile_request_callback: None,
            load_monitor: None,
            memory_cache_bridge: Some(cache_layer.memory_bridge()),
            cache_layer: Some(cache_layer),
            gc_scheduler_handle,
            gc_scheduler_shutdown,
        })
    }

    /// Internal builder that does the actual construction.
    ///
    /// This method delegates to focused builder functions in the `builder` module
    /// for each component, keeping the overall flow clear and each piece testable.
    fn build(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
        owned_runtime: Option<Runtime>,
    ) -> Result<Self, ServiceError> {
        // 1. Create providers
        let ProviderComponents {
            sync_provider: _,
            async_provider,
            name: provider_name,
            max_zoom,
        } = builder::create_providers(&provider_config, &runtime_handle)?;

        // 2. Create texture encoder
        let dds_encoder = builder::create_encoder(&config);

        // 3. Create cache components
        let CacheComponents {
            memory_cache,
            cache_dir,
        } = builder::create_cache(&config, &provider_name)?;

        // 4. Create shared memory cache adapter (used by prefetcher for cache checks)
        let memory_cache_adapter = memory_cache.as_ref().map(|cache| {
            Arc::new(MemoryCacheAdapter::new(
                Arc::clone(cache),
                &provider_name,
                config.texture().format(),
            ))
        });

        // 5. Create metrics system for event-based telemetry
        let metrics_system = MetricsSystem::new(&runtime_handle);

        // 6. Scan existing disk cache in background to initialize size metrics
        // This avoids blocking service creation with potentially slow directory walk
        if let Some(ref cache_dir_path) = cache_dir {
            let cache_path = cache_dir_path.clone();
            let metrics_client = metrics_system.client();
            runtime_handle.spawn(async move {
                let path = cache_path;
                let result = tokio::task::spawn_blocking(move || disk_cache_stats(&path)).await;
                match result {
                    Ok(Ok((_files, bytes))) => {
                        metrics_client.disk_cache_initial_size(bytes);
                        tracing::debug!(
                            bytes = bytes,
                            "Disk cache initial size scanned (background)"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "Failed to scan disk cache size");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Disk cache scan task panicked");
                    }
                }
            });
        }

        // 7. Create XEarthLayer runtime with job executor daemon
        // Note: The runtime is created lazily when needed (when async_provider is available)
        let (xearthlayer_runtime, dds_client) = if let Some(ref async_prov) = async_provider {
            if memory_cache.is_some() {
                let runtime = RuntimeBuilder::new(
                    &provider_name,
                    config.texture().format(),
                    Arc::clone(&dds_encoder),
                )
                .with_async_provider(Arc::clone(async_prov))
                .with_memory_cache(
                    memory_cache
                        .clone()
                        .unwrap_or_else(|| Arc::new(MemoryCache::new(0))),
                )
                .with_cache_dir(
                    cache_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/tmp/xearthlayer")),
                )
                .with_runtime_handle(runtime_handle.clone())
                .with_metrics_client(metrics_system.client()) // Wire metrics for dashboard UI
                .build();

                let client = runtime.dds_client();
                (Some(runtime), Some(client))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        Ok(Self {
            config,
            provider_name,
            max_zoom,
            logger,
            _owned_runtime: owned_runtime,
            runtime_handle,
            dds_encoder,
            memory_cache,
            memory_cache_adapter,
            metrics_system: Some(metrics_system),
            xearthlayer_runtime,
            dds_client,
            tile_request_callback: None,
            load_monitor: None,
            memory_cache_bridge: None,
            cache_layer: None,
            gc_scheduler_handle: None,
            gc_scheduler_shutdown: None,
        })
    }

    /// Build the service using cache bridges from the new cache service architecture.
    ///
    /// This method is similar to `build()` but uses `MemoryCacheBridge` and `DiskCacheBridge`
    /// instead of the legacy cache system. The key difference is that:
    /// - The runtime is created using `RuntimeBuilder::build_with_cache_service()`
    /// - No external GC daemon is needed (DiskCacheBridge has internal GC)
    #[allow(clippy::too_many_arguments)]
    fn build_with_bridges(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
        runtime_handle: Handle,
        owned_runtime: Option<Runtime>,
        memory_bridge: Arc<MemoryCacheBridge>,
        disk_bridge: Arc<DiskCacheBridge>,
    ) -> Result<Self, ServiceError> {
        // 1. Create providers
        let ProviderComponents {
            sync_provider: _,
            async_provider,
            name: provider_name,
            max_zoom,
        } = builder::create_providers(&provider_config, &runtime_handle)?;

        // 2. Create texture encoder
        let dds_encoder = builder::create_encoder(&config);

        // 3. Create metrics system
        let metrics_system = MetricsSystem::new(&runtime_handle);

        // 4. Create XEarthLayer runtime with cache bridges
        let (xearthlayer_runtime, dds_client) = if let Some(ref async_prov) = async_provider {
            let runtime = RuntimeBuilder::new(
                &provider_name,
                config.texture().format(),
                Arc::clone(&dds_encoder),
            )
            .with_async_provider(Arc::clone(async_prov))
            .with_runtime_handle(runtime_handle.clone())
            .with_metrics_client(metrics_system.client())
            .build_with_cache_service(Arc::clone(&memory_bridge), Arc::clone(&disk_bridge));

            let client = runtime.dds_client();
            (Some(runtime), Some(client))
        } else {
            (None, None)
        };

        tracing::info!(
            provider = %provider_name,
            "XEarthLayerService created with cache bridges (internal GC enabled)"
        );

        Ok(Self {
            config,
            provider_name,
            max_zoom,
            logger,
            _owned_runtime: owned_runtime,
            runtime_handle,
            dds_encoder,
            // Legacy cache fields are None when using bridges
            memory_cache: None,
            memory_cache_adapter: None,
            metrics_system: Some(metrics_system),
            xearthlayer_runtime,
            dds_client,
            tile_request_callback: None,
            load_monitor: None,
            // Store bridge for prefetch system access
            memory_cache_bridge: Some(memory_bridge),
            cache_layer: None,
            gc_scheduler_handle: None,
            gc_scheduler_shutdown: None,
        })
    }

    /// Get the provider name.
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Get the provider's maximum zoom level.
    pub fn max_zoom(&self) -> u8 {
        self.max_zoom
    }

    /// Check if caching is enabled.
    pub fn cache_enabled(&self) -> bool {
        self.config.cache_enabled()
    }

    /// Get the service configuration.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Get the runtime handle.
    pub fn runtime_handle(&self) -> &Handle {
        &self.runtime_handle
    }

    /// Get a snapshot of pipeline telemetry metrics.
    ///
    /// Returns a point-in-time copy of all pipeline metrics, including:
    /// - Job counts (submitted, completed, failed, coalesced)
    /// - Download statistics (chunks, bytes, throughput)
    /// - Cache hit rates (memory and disk)
    /// - Cache sizes (memory and disk)
    /// - Timing information
    ///
    /// The snapshot is safe to use for display without blocking the pipeline.
    ///
    /// Get a telemetry snapshot for dashboard display.
    ///
    /// Note: Memory cache size is updated by tasks when they write to the cache,
    /// providing accurate real-time data. This method only reads the current state.
    pub fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        // Generate snapshot using TuiReporter
        match &self.metrics_system {
            Some(system) => {
                let reporter = TuiReporter::new();
                system.snapshot(&reporter)
            }
            None => TelemetrySnapshot::default(),
        }
    }

    /// Get the metrics client for external metric emission.
    ///
    /// Returns None if the metrics system is not initialized.
    /// This allows external components to emit metrics events.
    pub fn metrics_client(&self) -> Option<crate::metrics::MetricsClient> {
        self.metrics_system.as_ref().map(|s| s.client())
    }

    /// Check if the XEarthLayer runtime is running.
    ///
    /// Returns true if the job executor daemon is running and accepting requests.
    pub fn is_runtime_running(&self) -> bool {
        self.xearthlayer_runtime
            .as_ref()
            .map(|r| r.is_running())
            .unwrap_or(false)
    }

    /// Get the maximum concurrent jobs configured for the executor.
    ///
    /// This is used by the dashboard to display capacity utilization.
    /// Returns the configured value from the control plane settings.
    pub fn max_concurrent_jobs(&self) -> usize {
        self.config.control_plane().max_concurrent_jobs
    }

    /// Get the runtime health monitor for dashboard display.
    ///
    /// Returns `None` if the runtime is not yet started or health tracking
    /// is not available. The TUI can handle this gracefully.
    ///
    /// TODO: Wire up to actual runtime health tracking.
    pub fn runtime_health(&self) -> Option<SharedRuntimeHealth> {
        // Not yet implemented - will be connected during TUI update
        None
    }

    /// Get the tile progress tracker for TUI display.
    ///
    /// Returns the shared tile progress tracker from the runtime,
    /// which tracks active tile generation with task completion counts.
    ///
    /// Returns `None` if the runtime is not yet started.
    pub fn tile_progress_tracker(&self) -> Option<SharedTileProgressTracker> {
        self.xearthlayer_runtime
            .as_ref()
            .map(|r| r.tile_progress_tracker())
    }

    /// Get the DDS format used by this service.
    pub fn dds_format(&self) -> crate::dds::DdsFormat {
        self.config.texture().format()
    }

    /// Get the raw memory cache for size queries.
    ///
    /// Returns a reference to the shared memory cache, if enabled.
    /// For prefetch operations that need to check cache contents,
    /// use `memory_cache_adapter()` instead.
    pub fn memory_cache(&self) -> Option<Arc<MemoryCache>> {
        self.memory_cache.clone()
    }

    /// Get the shared memory cache adapter.
    ///
    /// Returns the adapter that implements `executor::MemoryCache` trait.
    /// This is the same adapter instance used by the executor daemon, ensuring
    /// the prefetcher sees the same cached tiles.
    ///
    /// Returns `None` if caching is disabled.
    pub fn memory_cache_adapter(&self) -> Option<Arc<MemoryCacheAdapter>> {
        self.memory_cache_adapter.clone()
    }

    /// Get the memory cache bridge from the new cache service architecture.
    ///
    /// Returns the `MemoryCacheBridge` that implements `executor::MemoryCache` trait.
    /// This is available when the service is created with cache bridges
    /// (via `with_cache_bridges()` constructor).
    ///
    /// Returns `None` if using legacy cache system.
    pub fn memory_cache_bridge(&self) -> Option<Arc<MemoryCacheBridge>> {
        self.memory_cache_bridge.clone()
    }

    /// Shutdown the cache layer gracefully.
    ///
    /// This stops the GC daemon and flushes pending operations.
    /// Only has effect when created via `start()` constructor; otherwise no-op.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut service = XEarthLayerService::start(config, provider_config, logger, disk_profile).await?;
    /// // ... use service ...
    /// service.shutdown_cache().await;
    /// ```
    pub async fn shutdown_cache(&mut self) {
        // Shutdown GC scheduler first (before shutting down cache)
        if let Some(shutdown_token) = self.gc_scheduler_shutdown.take() {
            tracing::info!("Shutting down GC scheduler");
            shutdown_token.cancel();

            // Wait for GC scheduler task to complete
            if let Some(handle) = self.gc_scheduler_handle.take() {
                match handle.await {
                    Ok(()) => tracing::info!("GC scheduler shut down cleanly"),
                    Err(e) => tracing::warn!(error = %e, "GC scheduler task failed"),
                }
            }
        }

        // Then shutdown cache layer
        if let Some(cache_layer) = self.cache_layer.take() {
            cache_layer.shutdown().await;
        }
    }

    /// Get the DDS client for requesting tile generation.
    ///
    /// Returns the `DdsClient` that FUSE handlers use to request DDS generation
    /// from the job executor daemon. This is the modern architecture that replaces
    /// the legacy `DdsHandler` callback pattern.
    ///
    /// Returns `None` if the async provider is not configured (sync-only mode).
    pub fn dds_client(&self) -> Option<Arc<dyn DdsClient>> {
        self.dds_client.clone()
    }

    /// Get the executor's resource pools for utilization monitoring.
    ///
    /// Returns `None` if the runtime has not been started yet.
    /// Used by the circuit breaker to trip on pool saturation.
    pub fn resource_pools(&self) -> Option<Arc<crate::executor::ResourcePools>> {
        self.xearthlayer_runtime
            .as_ref()
            .map(|r| r.resource_pools())
    }

    /// Set the tile request callback for FUSE-based position inference.
    ///
    /// When set, this callback is invoked for each DDS tile request received
    /// via FUSE. The `FuseRequestAnalyzer` uses these requests to infer
    /// aircraft position and heading when telemetry is unavailable.
    ///
    /// This should be called before mounting the filesystem.
    ///
    /// # Arguments
    ///
    /// * `callback` - The callback to invoke for each tile request
    pub fn set_tile_request_callback(&mut self, callback: TileRequestCallback) {
        self.tile_request_callback = Some(callback);
    }

    /// Set the load monitor for circuit breaker integration.
    ///
    /// When set, the load monitor's `record_request()` is called for each
    /// FUSE-originated request. This enables the circuit breaker to track
    /// aggregate load across all mounted packages.
    pub fn set_load_monitor(&mut self, monitor: Arc<dyn FuseLoadMonitor>) {
        self.load_monitor = Some(monitor);
    }

    /// Set the owned Tokio runtime.
    ///
    /// When the service is created via `start()`, the caller's runtime is used
    /// but the service doesn't own it. Call this method to transfer ownership
    /// of a runtime to the service, ensuring it stays alive for the service's
    /// lifetime.
    ///
    /// This is typically called by the `MountManager` after creating the service.
    pub fn set_owned_runtime(&mut self, runtime: Runtime) {
        self._owned_runtime = Some(runtime);
    }

    /// Set the shared memory cache.
    ///
    /// When multiple packages are mounted, sharing a single memory cache across
    /// all services ensures the configured memory limit is respected globally,
    /// not per-package. Without this, mounting N packages could use N times
    /// the configured memory limit.
    ///
    /// This should be called before mounting the filesystem.
    ///
    /// # Arguments
    ///
    /// * `cache` - The shared memory cache
    /// * `adapter` - The shared memory cache adapter (wraps the cache with provider/format context)
    pub fn set_shared_memory_cache(
        &mut self,
        cache: Arc<MemoryCache>,
        adapter: Arc<MemoryCacheAdapter>,
    ) {
        self.memory_cache = Some(cache);
        self.memory_cache_adapter = Some(adapter);
    }

    /// Calculate the expected DDS file size based on encoder configuration.
    ///
    /// Returns the expected file size for a standard 4096×4096 DDS texture
    /// with the configured format and mipmap levels.
    pub fn expected_dds_size(&self) -> usize {
        self.dds_encoder.expected_size(4096, 4096)
    }

    /// Create a mount configuration for FUSE filesystem.
    ///
    /// # Panics
    ///
    /// Panics if the DdsClient is not initialized (requires async provider).
    fn create_mount_config(&self) -> FuseMountConfig {
        let client = self
            .dds_client
            .as_ref()
            .expect("DdsClient not initialized - async provider required");

        let mut config = FuseMountConfig::new(Arc::clone(client), self.expected_dds_size())
            .with_timeout(Duration::from_secs(
                self.config.generation_timeout().unwrap_or(30),
            ))
            .with_logger(Arc::clone(&self.logger));

        // Wire tile request callback for FUSE-based position inference
        if let Some(ref callback) = self.tile_request_callback {
            config = config.with_tile_request_callback(callback.clone());
        }

        config
    }

    /// Start the passthrough FUSE filesystem server using fuse3 (async multi-threaded).
    ///
    /// Uses the fuse3 library which runs all FUSE operations asynchronously on the
    /// Tokio runtime, enabling true parallel I/O processing. This is optimized for
    /// high-concurrency scenarios like X-Plane scene loading.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `MountHandle` that keeps the filesystem mounted. When dropped, the
    /// filesystem is automatically unmounted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source directory doesn't exist
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    pub async fn serve_passthrough_fuse3(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<MountHandle, ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3(&config, source_dir, mountpoint).await
    }

    /// Start the passthrough FUSE filesystem server using fuse3 (synchronous wrapper).
    ///
    /// This is a convenience wrapper around `serve_passthrough_fuse3` that blocks
    /// until the filesystem is unmounted. For async code, use `serve_passthrough_fuse3`
    /// directly.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Note
    ///
    /// This method blocks until the filesystem is unmounted (e.g., via Ctrl+C
    /// or `fusermount -u`).
    pub fn serve_passthrough_fuse3_blocking(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<(), ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3_blocking(
            &config,
            source_dir,
            mountpoint,
            &self.runtime_handle,
        )
    }

    /// Start the passthrough FUSE filesystem server using fuse3 as a background task.
    ///
    /// This spawns the fuse3 mount as a background Tokio task, returning a handle
    /// that can be safely stored and dropped outside of an async context. This is
    /// the recommended method for use with `MountManager`.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `SpawnedMountHandle` that keeps the filesystem mounted. The handle can be
    /// dropped safely from any context (async or sync) - it will use `fusermount -u`
    /// as a fallback for cleanup if needed.
    pub async fn serve_passthrough_fuse3_spawned(
        &self,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<SpawnedMountHandle, ServiceError> {
        let config = self.create_mount_config();
        FuseMountService::mount_fuse3_spawned(&config, source_dir, mountpoint).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsFormat;

    // Note: Most XEarthLayerService tests require network access or mocks.
    // Unit tests here focus on configuration and validation logic.

    #[test]
    fn test_invalid_zoom_too_low() {
        // We can't easily test without a real service, but we can test the error type
        let err = ServiceError::InvalidZoom {
            zoom: 0,
            min: 1,
            max: 19,
        };
        assert!(err.to_string().contains("0"));
    }

    #[test]
    fn test_invalid_zoom_too_high() {
        let err = ServiceError::InvalidZoom {
            zoom: 25,
            min: 1,
            max: 19,
        };
        assert!(err.to_string().contains("25"));
    }

    #[test]
    fn test_invalid_coordinates_error() {
        let err = ServiceError::InvalidCoordinates {
            lat: 91.0,
            lon: 0.0,
            reason: "latitude must be between -90 and 90".to_string(),
        };
        assert!(err.to_string().contains("91"));
    }

    #[test]
    fn test_service_config_default() {
        let config = ServiceConfig::default();
        assert!(config.cache_enabled());
        assert_eq!(config.texture().format(), DdsFormat::BC1);
        assert_eq!(config.texture().mipmap_count(), 5);
    }

    #[test]
    fn test_config_error() {
        let err = ServiceError::ConfigError("No mountpoint".to_string());
        assert!(err.to_string().contains("Configuration error"));
        assert!(err.to_string().contains("No mountpoint"));
    }

    #[test]
    fn test_runtime_error() {
        let err = ServiceError::RuntimeError("failed to spawn".to_string());
        assert!(err.to_string().contains("Runtime error"));
        assert!(err.to_string().contains("failed to spawn"));
    }

    /// Regression test: `start()` must be callable from within a `block_on()` context.
    ///
    /// This test verifies that `XEarthLayerService::start()` does NOT try to create
    /// its own Tokio runtime, which would cause a panic with "Cannot start a runtime
    /// from within a runtime".
    ///
    /// The fix ensures `start()` uses `Handle::current()` instead of creating a new runtime.
    ///
    /// Note: This test runs the service creation in a separate thread with its own
    /// runtime to avoid Tokio's "Cannot drop a runtime in a context where blocking
    /// is not allowed" error during test cleanup. The separate runtime properly
    /// isolates the test from the `#[tokio::test]` runtime.
    #[test]
    fn test_start_callable_from_block_on() {
        use crate::log::TracingLogger;
        use crate::provider::ProviderConfig;
        use std::thread;
        use tempfile::tempdir;

        // Run in a separate thread with its own runtime to avoid tokio test cleanup issues
        let handle = thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let temp_dir = tempdir().unwrap();

                let config = ServiceConfig::builder()
                    .cache_directory(temp_dir.path().to_path_buf())
                    .build();

                // This would panic with "Cannot start a runtime from within a runtime"
                // if start() tries to create its own runtime internally
                let result = XEarthLayerService::start(
                    config,
                    ProviderConfig::bing(),
                    Arc::new(TracingLogger),
                )
                .await;

                // We expect this to succeed (or fail for other reasons like network)
                // but NOT panic due to nested runtime
                match result {
                    Ok(mut service) => {
                        // Clean shutdown
                        service.shutdown_cache().await;
                    }
                    Err(e) => {
                        // Network errors are fine, nested runtime panic is not
                        tracing::debug!("Service creation failed (expected in CI): {}", e);
                    }
                }
            });
        });

        // If the thread panics, it will be due to nested runtime, which is what we're testing
        handle
            .join()
            .expect("test thread panicked - nested runtime issue");
    }
}
