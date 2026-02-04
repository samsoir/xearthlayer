//! Service orchestrator for XEarthLayer backend.
//!
//! This module provides `ServiceOrchestrator` which coordinates the startup,
//! operation, and shutdown of all XEarthLayer backend services.
//!
//! # Architecture
//!
//! The orchestrator owns and manages:
//! - **Aircraft Position & Telemetry (APT)** - unified position aggregation
//! - **Prefetch system** - predictive tile caching
//! - **Scene tracker** - FUSE load monitoring
//! - **FUSE mounts** (via `MountManager`) - virtual filesystem
//!
//! Cache services are now managed internally by `XEarthLayerService` via `CacheLayer`,
//! which is created during `XEarthLayerService::start()`. This ensures proper metrics
//! integration: MetricsSystem is created first, then CacheLayer with metrics client,
//! so the GC daemon can report cache evictions.
//!
//! # Startup Sequence
//!
//! 1. ServiceBuilder is created (deferred service creation)
//! 2. FUSE mount triggers `XEarthLayerService::start()`:
//!    - MetricsSystem created first
//!    - CacheLayer created with metrics (GC daemon starts)
//!    - Service fully initialized
//! 3. APT module starts telemetry reception
//! 4. Prefetch system subscribes to APT telemetry
//! 5. FUSE mounts become active
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{ServiceOrchestrator, OrchestratorConfig};
//!
//! let config = OrchestratorConfig::from_config_file(...);
//! let orchestrator = ServiceOrchestrator::start(config)?;
//!
//! // Access telemetry for UI
//! let snapshot = orchestrator.telemetry_snapshot();
//!
//! // Graceful shutdown
//! orchestrator.shutdown();
//! ```

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::aircraft_position::{
    spawn_position_logger, AircraftPositionBroadcaster, SharedAircraftPosition, StateAggregator,
    TelemetryReceiver, TelemetryReceiverConfig, DEFAULT_LOG_INTERVAL,
};
use crate::executor::{DdsClient, MemoryCache};
use crate::log::TracingLogger;
use crate::manager::{
    create_consolidated_overlay, InstalledPackage, LocalPackageStore, MountManager, ServiceBuilder,
};
use crate::metrics::TelemetrySnapshot;
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::{
    load_cache, save_cache, warn_if_legacy, AdaptivePrefetchConfig, AdaptivePrefetchCoordinator,
    CacheLoadResult, CircuitBreaker, FuseRequestAnalyzer, Prefetcher, SceneryIndex,
    SceneryIndexConfig, SharedPrefetchStatus,
};
use crate::runtime::SharedRuntimeHealth;

use super::error::ServiceError;
use super::orchestrator_config::OrchestratorConfig;
use super::XEarthLayerService;

/// Handle to a running prefetch system.
pub struct PrefetchHandle {
    /// Join handle for the prefetch task.
    #[allow(dead_code)]
    handle: JoinHandle<()>,
}

/// Result of mounting consolidated ortho.
pub struct MountResult {
    /// Whether mounting succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
    /// Number of sources mounted.
    pub source_count: usize,
    /// Number of files indexed.
    pub file_count: usize,
    /// Names of patches mounted.
    pub patch_names: Vec<String>,
    /// Regions of packages mounted.
    pub package_regions: Vec<String>,
    /// Mountpoint path.
    pub mountpoint: std::path::PathBuf,
}

/// Progress updates during service initialization.
#[derive(Debug, Clone)]
pub enum StartupProgress {
    /// Scanning disk cache size.
    ScanningDiskCache,
    /// Mounting FUSE filesystem.
    Mounting {
        /// Current phase of index building.
        phase: crate::ortho_union::IndexBuildPhase,
        /// Source being processed.
        current_source: Option<String>,
        /// Number of sources completed.
        sources_complete: usize,
        /// Total sources to process.
        sources_total: usize,
        /// Files scanned so far.
        files_scanned: usize,
        /// Whether using cached index.
        using_cache: bool,
    },
    /// Creating overlay symlinks.
    CreatingOverlay,
    /// Starting APT telemetry receiver.
    StartingTelemetry,
    /// Building scenery index.
    BuildingSceneryIndex {
        /// Package being indexed.
        package_name: String,
        /// Package index (0-based).
        package_index: usize,
        /// Total packages to index.
        total_packages: usize,
        /// Tiles indexed so far.
        tiles_indexed: usize,
        /// Whether loaded from cache.
        from_cache: bool,
    },
    /// Scenery index complete.
    SceneryIndexComplete {
        /// Total tiles indexed.
        total_tiles: usize,
        /// Land tiles.
        land_tiles: usize,
        /// Sea tiles.
        sea_tiles: usize,
    },
    /// Starting prefetch system.
    StartingPrefetch,
    /// All services initialized.
    Complete,
}

/// Result of service initialization.
pub struct StartupResult {
    /// Mount result details.
    pub mount: MountResult,
    /// Overlay creation succeeded.
    pub overlay_success: bool,
    /// Overlay error message if failed.
    pub overlay_error: Option<String>,
    /// Scenery index tile count.
    pub scenery_tiles: usize,
    /// Whether scenery index was loaded from cache.
    pub scenery_from_cache: bool,
}

/// Coordinates startup and operation of all XEarthLayer backend services.
///
/// This is the main entry point for the backend daemon. It encapsulates all
/// service orchestration that was previously scattered in `run.rs`, providing
/// a clean API for the CLI/TUI layer.
///
/// # Cache Architecture
///
/// The orchestrator no longer owns cache infrastructure directly. Instead,
/// `XEarthLayerService::start()` creates the `CacheLayer` with proper metrics
/// integration. This ensures the GC daemon has access to metrics for reporting
/// cache evictions.
pub struct ServiceOrchestrator {
    /// Aircraft position provider (APT module).
    aircraft_position: SharedAircraftPosition,

    /// Prefetch status for UI display.
    prefetch_status: Arc<SharedPrefetchStatus>,

    /// Prefetch system handle.
    prefetch_handle: Option<PrefetchHandle>,

    /// Mount manager (owns FUSE mounts).
    mount_manager: MountManager,

    /// Service builder for creating services (consumed on mount).
    service_builder: Option<ServiceBuilder>,

    /// Runtime health for control plane monitoring.
    runtime_health: Option<SharedRuntimeHealth>,

    /// Maximum concurrent jobs (for UI display).
    max_concurrent_jobs: usize,

    /// FUSE request analyzer for position inference.
    fuse_analyzer: Option<Arc<FuseRequestAnalyzer>>,

    /// Scenery index for prefetching.
    scenery_index: Arc<SceneryIndex>,

    /// Master cancellation token.
    cancellation: CancellationToken,

    /// Configuration (retained for accessors).
    config: OrchestratorConfig,
}

impl ServiceOrchestrator {
    /// Start all backend services with the given configuration.
    ///
    /// This performs the complete startup sequence:
    /// 1. Starts cache services (GC daemons auto-start)
    /// 2. Creates service builder with cache bridges
    /// 3. Creates mount manager
    /// 4. Returns orchestrator ready for mounting
    ///
    /// Note: FUSE mounting is done separately via `mount_consolidated_ortho()`
    /// to allow progress callbacks for the TUI loading screen.
    pub fn start(config: OrchestratorConfig) -> Result<Self, ServiceError> {
        info!("Starting ServiceOrchestrator");

        let cancellation = CancellationToken::new();
        let prefetch_status = SharedPrefetchStatus::new();

        // Create FUSE request analyzer for position inference (if prefetch enabled)
        let fuse_analyzer = if config.prefetch_enabled() {
            Some(Arc::new(FuseRequestAnalyzer::new(
                crate::prefetch::FuseInferenceConfig::default(),
            )))
        } else {
            None
        };

        // Create service builder with disk I/O profile
        // Note: Cache is now created inside XEarthLayerService::start() via CacheLayer,
        // which ensures MetricsSystem is created FIRST, then CacheLayer with metrics.
        // This fixes the GC daemon not having metrics client for eviction reporting.
        let logger: Arc<dyn crate::log::Logger> = Arc::new(TracingLogger);
        let mut service_builder = ServiceBuilder::with_disk_io_profile(
            config.service.clone(),
            config.provider.clone(),
            logger,
            config.disk_io_profile,
        );

        // Create mount manager with Custom Scenery path
        let mount_manager = MountManager::with_scenery_path(&config.custom_scenery_path);

        // Wire load monitor for circuit breaker integration
        let load_monitor = mount_manager.load_monitor();
        service_builder = service_builder.with_load_monitor(Arc::clone(&load_monitor));

        // Wire FUSE analyzer callback for position inference
        if let Some(ref analyzer) = fuse_analyzer {
            service_builder = service_builder.with_tile_request_callback(analyzer.callback());
        }

        // Create APT module (starts later with mount)
        let (apt_broadcast_tx, _apt_broadcast_rx) = broadcast::channel(16);
        let apt_aggregator = StateAggregator::new(apt_broadcast_tx);
        let aircraft_position = SharedAircraftPosition::new(apt_aggregator);

        // Create empty scenery index (populated during mount)
        let scenery_index = Arc::new(SceneryIndex::with_defaults());

        info!("ServiceOrchestrator initialized (mount pending)");

        Ok(Self {
            aircraft_position,
            prefetch_status,
            prefetch_handle: None,
            mount_manager,
            service_builder: Some(service_builder),
            runtime_health: None,
            max_concurrent_jobs: 0,
            fuse_analyzer,
            scenery_index,
            cancellation,
            config,
        })
    }

    /// Initialize all services with a single call.
    ///
    /// This is the recommended way to start XEarthLayer. It performs the complete
    /// startup sequence in order:
    ///
    /// 1. Mount consolidated ortho (FUSE filesystem)
    /// 2. Create overlay symlinks
    /// 3. Start APT telemetry receiver
    /// 4. Build scenery index (with cache support)
    /// 5. Start prefetch system
    ///
    /// # Arguments
    ///
    /// * `store` - Local package store for discovering packages
    /// * `ortho_packages` - List of ortho packages to mount
    /// * `progress_callback` - Optional callback for progress updates (for TUI)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = orchestrator.initialize_services(
    ///     &store,
    ///     &ortho_packages,
    ///     Some(|progress| println!("{:?}", progress)),
    /// )?;
    /// ```
    pub fn initialize_services<F>(
        &mut self,
        store: &LocalPackageStore,
        ortho_packages: &[&InstalledPackage],
        progress_callback: Option<F>,
    ) -> Result<StartupResult, ServiceError>
    where
        F: Fn(StartupProgress) + Send + Sync + 'static,
    {
        // Wrap callback in Arc for sharing across phases
        let callback: Option<Arc<dyn Fn(StartupProgress) + Send + Sync>> =
            progress_callback.map(|f| Arc::new(f) as Arc<dyn Fn(StartupProgress) + Send + Sync>);

        // Note: Disk cache scanning is now handled internally by XEarthLayerService::start()
        // via CacheLayer, which scans and reports to metrics during service creation.
        // This ensures proper metrics integration without needing external coordination.

        // Phase 1: Mount consolidated ortho with progress
        // This now uses XEarthLayerService::start() which creates CacheLayer with metrics
        if let Some(ref cb) = callback {
            cb(StartupProgress::ScanningDiskCache);
        }

        let mount_result = if let Some(ref cb) = callback {
            // Convert our StartupProgress to the ortho_union IndexBuildProgress
            let cb_clone = Arc::clone(cb);
            let mount_progress = move |p: crate::ortho_union::IndexBuildProgress| {
                cb_clone(StartupProgress::Mounting {
                    phase: p.phase,
                    current_source: p.current_source,
                    sources_complete: p.sources_complete,
                    sources_total: p.sources_total,
                    files_scanned: p.files_scanned,
                    using_cache: p.using_cache,
                });
            };
            self.mount_consolidated_ortho_with_progress(store, Some(mount_progress))
        } else {
            self.mount_consolidated_ortho(store)
        };

        if !mount_result.success {
            return Err(ServiceError::IoError(std::io::Error::other(
                mount_result
                    .error
                    .clone()
                    .unwrap_or_else(|| "Mount failed".to_string()),
            )));
        }

        // Phase 2: Create overlay symlinks
        if let Some(ref cb) = callback {
            cb(StartupProgress::CreatingOverlay);
        }
        let overlay_result = create_consolidated_overlay(store, &self.config.custom_scenery_path);
        let overlay_success = overlay_result.success;
        let overlay_error = overlay_result.error.clone();
        if let Some(ref error) = overlay_result.error {
            tracing::warn!(error = %error, "Failed to create consolidated overlay");
        }

        // Phase 3: Start APT telemetry
        if let Some(ref cb) = callback {
            cb(StartupProgress::StartingTelemetry);
        }
        if let Err(e) = self.start_apt_telemetry() {
            tracing::warn!(error = %e, "Failed to start APT telemetry");
        }

        // Phase 4: Build scenery index (with cache support)
        let packages_for_index: Vec<(String, std::path::PathBuf)> = ortho_packages
            .iter()
            .map(|p| (p.region().to_string(), p.path.clone()))
            .collect();

        let (scenery_tiles, scenery_from_cache) =
            self.build_scenery_index(&packages_for_index, callback.as_ref())?;

        // Phase 5: Start prefetch
        if self.config.prefetch_enabled() {
            if let Some(ref cb) = callback {
                cb(StartupProgress::StartingPrefetch);
            }
            if let Err(e) = self.start_prefetch() {
                tracing::warn!(error = %e, "Failed to start prefetch system");
            }
        }

        // Signal completion
        if let Some(ref cb) = callback {
            cb(StartupProgress::Complete);
        }

        Ok(StartupResult {
            mount: mount_result,
            overlay_success,
            overlay_error,
            scenery_tiles,
            scenery_from_cache,
        })
    }

    /// Build the scenery index from packages, with cache support.
    fn build_scenery_index(
        &mut self,
        packages: &[(String, std::path::PathBuf)],
        callback: Option<&Arc<dyn Fn(StartupProgress) + Send + Sync>>,
    ) -> Result<(usize, bool), ServiceError> {
        // Try to load from cache first
        match load_cache(packages) {
            CacheLoadResult::Loaded {
                tiles,
                total_tiles,
                sea_tiles,
            } => {
                tracing::info!(
                    tiles = total_tiles,
                    sea = sea_tiles,
                    "Loaded scenery index from cache"
                );

                if let Some(cb) = callback {
                    cb(StartupProgress::SceneryIndexComplete {
                        total_tiles,
                        land_tiles: total_tiles - sea_tiles,
                        sea_tiles,
                    });
                }

                let index = Arc::new(SceneryIndex::from_tiles(
                    tiles,
                    SceneryIndexConfig::default(),
                ));
                self.scenery_index = index;
                return Ok((total_tiles, true));
            }
            CacheLoadResult::Stale { reason } => {
                tracing::info!(reason = %reason, "Scenery cache is stale, rebuilding");
            }
            CacheLoadResult::NotFound => {
                tracing::info!("No scenery cache found, building index");
            }
            CacheLoadResult::Invalid { error } => {
                tracing::warn!(error = %error, "Scenery cache invalid, rebuilding");
            }
        }

        // Build from scratch
        let index = Arc::new(SceneryIndex::with_defaults());
        let mut total_tiles = 0usize;

        for (idx, (region, path)) in packages.iter().enumerate() {
            if let Some(cb) = callback {
                cb(StartupProgress::BuildingSceneryIndex {
                    package_name: region.clone(),
                    package_index: idx,
                    total_packages: packages.len(),
                    tiles_indexed: total_tiles,
                    from_cache: false,
                });
            }

            match index.build_from_package(path) {
                Ok(count) => {
                    total_tiles += count;
                    tracing::debug!(
                        region = %region,
                        tiles = count,
                        "Indexed scenery package"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        region = %region,
                        error = %e,
                        "Failed to index scenery package"
                    );
                }
            }
        }

        // Save cache for next launch
        if total_tiles > 0 {
            if let Err(e) = save_cache(&index, packages) {
                tracing::warn!(error = %e, "Failed to save scenery cache");
            }
        }

        if let Some(cb) = callback {
            cb(StartupProgress::SceneryIndexComplete {
                total_tiles,
                land_tiles: index.land_tile_count(),
                sea_tiles: index.sea_tile_count(),
            });
        }

        self.scenery_index = index;
        Ok((total_tiles, false))
    }

    /// Mount consolidated ortho scenery.
    ///
    /// This mounts all ortho sources (patches + packages) into a single FUSE mount.
    /// Call this after `start()` to activate the FUSE filesystem.
    ///
    /// For TUI mode, use `mount_consolidated_ortho_with_progress()` instead.
    pub fn mount_consolidated_ortho(&mut self, store: &LocalPackageStore) -> MountResult {
        let service_builder = self
            .service_builder
            .take()
            .expect("Service builder should be set - was mount_consolidated_ortho called twice?");

        let result = self.mount_manager.mount_consolidated_ortho(
            &self.config.patches_dir,
            store,
            &service_builder,
        );

        // Wire runtime health after mount
        self.wire_runtime_health();

        MountResult {
            success: result.success,
            error: result.error,
            source_count: result.source_count,
            file_count: result.file_count,
            patch_names: result.patch_names,
            package_regions: result.package_regions,
            mountpoint: result.mountpoint,
        }
    }

    /// Mount with progress callback for TUI loading screen.
    pub fn mount_consolidated_ortho_with_progress<F>(
        &mut self,
        store: &LocalPackageStore,
        progress_callback: Option<F>,
    ) -> MountResult
    where
        F: Fn(crate::ortho_union::IndexBuildProgress) + Send + Sync + 'static,
    {
        let service_builder = self
            .service_builder
            .take()
            .expect("Service builder should be set - was mount_consolidated_ortho called twice?");

        let callback = progress_callback
            .map(|f| Arc::new(f) as crate::ortho_union::IndexBuildProgressCallback);

        let result = self.mount_manager.mount_consolidated_ortho_with_progress(
            &self.config.patches_dir,
            store,
            &service_builder,
            callback,
        );

        // Wire runtime health after mount
        self.wire_runtime_health();

        MountResult {
            success: result.success,
            error: result.error,
            source_count: result.source_count,
            file_count: result.file_count,
            patch_names: result.patch_names,
            package_regions: result.package_regions,
            mountpoint: result.mountpoint,
        }
    }

    /// Wire runtime health from mounted service.
    fn wire_runtime_health(&mut self) {
        if let Some(service) = self.mount_manager.get_service() {
            self.runtime_health = service.runtime_health();
            self.max_concurrent_jobs = service.max_concurrent_jobs();
        }
    }

    /// Start the APT telemetry receiver.
    ///
    /// This starts listening for X-Plane UDP telemetry on the configured port.
    /// Must be called after mounting to have a runtime handle available.
    pub fn start_apt_telemetry(&self) -> Result<(), ServiceError> {
        let service = self
            .mount_manager
            .get_service()
            .ok_or_else(|| ServiceError::NotStarted("No service available for APT".into()))?;

        let runtime_handle = service.runtime_handle().clone();
        let telemetry_port = self.config.prefetch.udp_port;
        let (telemetry_tx, mut telemetry_rx) = mpsc::channel(32);

        let telemetry_config = TelemetryReceiverConfig {
            port: telemetry_port,
            ..Default::default()
        };
        let receiver = TelemetryReceiver::new(telemetry_config, telemetry_tx);
        let apt_cancellation = self.cancellation.clone();
        let logger_cancellation = apt_cancellation.clone();

        // Start the UDP receiver
        runtime_handle.spawn(async move {
            tokio::select! {
                result = receiver.start() => {
                    match result {
                        Ok(Ok(())) => tracing::debug!("APT telemetry receiver stopped"),
                        Ok(Err(e)) => tracing::warn!("APT telemetry receiver error: {}", e),
                        Err(e) => tracing::warn!("APT telemetry receiver task failed: {}", e),
                    }
                }
                _ = apt_cancellation.cancelled() => {
                    tracing::debug!("APT telemetry receiver cancelled");
                }
            }
        });

        // Bridge task: forward telemetry states to APT aggregator
        let aircraft_position = self.aircraft_position.clone();
        runtime_handle.spawn(async move {
            while let Some(state) = telemetry_rx.recv().await {
                aircraft_position.receive_telemetry(state);
            }
        });

        // Periodic position logger for flight analysis (DEBUG level only)
        if tracing::enabled!(tracing::Level::DEBUG) {
            spawn_position_logger(
                &runtime_handle,
                self.aircraft_position.clone(),
                logger_cancellation,
                DEFAULT_LOG_INTERVAL,
            );
        }

        info!(port = telemetry_port, "APT telemetry receiver started");
        Ok(())
    }

    /// Start the prefetch system.
    ///
    /// This starts the prefetch daemon that predictively caches tiles
    /// based on aircraft position and heading.
    pub fn start_prefetch(&mut self) -> Result<(), ServiceError> {
        if !self.config.prefetch_enabled() {
            info!("Prefetch disabled by configuration");
            return Ok(());
        }

        let service = self
            .mount_manager
            .get_service()
            .ok_or_else(|| ServiceError::NotStarted("No service available for prefetch".into()))?;

        let dds_client = service
            .dds_client()
            .ok_or_else(|| ServiceError::NotStarted("DDS client not available".into()))?;

        let runtime_handle = service.runtime_handle().clone();

        // Try legacy adapter first, then new cache bridge architecture
        if let Some(memory_cache) = service.memory_cache_adapter() {
            self.start_prefetch_with_cache(&runtime_handle, dds_client, memory_cache)?;
        } else if let Some(memory_cache) = service.memory_cache_bridge() {
            self.start_prefetch_with_cache(&runtime_handle, dds_client, memory_cache)?;
        } else {
            tracing::warn!("Memory cache not available, prefetch disabled");
            return Ok(());
        }

        Ok(())
    }

    /// Internal helper to start prefetch with a specific memory cache type.
    fn start_prefetch_with_cache<M: MemoryCache + 'static>(
        &mut self,
        runtime_handle: &Handle,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
    ) -> Result<(), ServiceError> {
        use crate::prefetch::AircraftState as PrefetchAircraftState;

        // Create channel for prefetch telemetry data
        let (state_tx, state_rx) = mpsc::channel(32);

        // Bridge APT telemetry to prefetch channel
        let mut apt_rx = self.aircraft_position.subscribe();
        let bridge_cancel = self.cancellation.clone();
        runtime_handle.spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = bridge_cancel.cancelled() => {
                        tracing::debug!("APT-to-prefetch telemetry bridge cancelled");
                        break;
                    }

                    result = apt_rx.recv() => {
                        match result {
                            Ok(apt_state) => {
                                let prefetch_state = PrefetchAircraftState::new(
                                    apt_state.latitude,
                                    apt_state.longitude,
                                    apt_state.heading,
                                    apt_state.ground_speed,
                                    apt_state.altitude,
                                );
                                if state_tx.send(prefetch_state).await.is_err() {
                                    tracing::debug!("Prefetch channel closed");
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::debug!("APT broadcast channel closed");
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::trace!("APT-to-prefetch bridge lagged by {} messages", n);
                            }
                        }
                    }
                }
            }
        });

        // Build prefetcher
        let config = &self.config.prefetch;

        // Keep a reference to DDS client for adaptive strategy
        let dds_client_for_adaptive = Arc::clone(&dds_client);

        // Warn if a legacy strategy is configured (all now use adaptive)
        warn_if_legacy(&config.strategy);

        // Build and start the prefetcher
        let prefetcher_cancel = self.cancellation.clone();

        // Build adaptive prefetch coordinator
        let adaptive_config = AdaptivePrefetchConfig::from_prefetch_config(config);
        let mut coordinator = AdaptivePrefetchCoordinator::new(adaptive_config);

        // Wire DDS client
        coordinator = coordinator.with_dds_client(dds_client_for_adaptive);

        // Wire memory cache for tile existence checks (Bug 5 fix)
        coordinator = coordinator.with_memory_cache(memory_cache);

        // Wire circuit breaker as throttler
        let circuit_breaker = CircuitBreaker::new(
            config.circuit_breaker.clone(),
            self.mount_manager.load_monitor(),
        );
        coordinator = coordinator.with_throttler(Arc::new(circuit_breaker));

        // Wire scenery index if available
        if self.scenery_index.tile_count() > 0 {
            coordinator = coordinator.with_scenery_index(Arc::clone(&self.scenery_index));
        }

        // Wire shared status for TUI display
        coordinator = coordinator.with_shared_status(Arc::clone(&self.prefetch_status));

        let prefetcher: Box<dyn Prefetcher> = Box::new(coordinator);
        let handle = runtime_handle.spawn(async move {
            prefetcher.run(state_rx, prefetcher_cancel).await;
        });

        self.prefetch_handle = Some(PrefetchHandle { handle });
        info!(strategy = "adaptive", "Prefetch system started");

        Ok(())
    }

    /// Update the scenery index (called after building).
    pub fn set_scenery_index(&mut self, index: Arc<SceneryIndex>) {
        self.scenery_index = index;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Accessors for TUI integration
    // ─────────────────────────────────────────────────────────────────────────

    /// Get the aircraft position provider for TUI display.
    pub fn aircraft_position(&self) -> SharedAircraftPosition {
        self.aircraft_position.clone()
    }

    /// Get the prefetch status for TUI display.
    pub fn prefetch_status(&self) -> Arc<SharedPrefetchStatus> {
        Arc::clone(&self.prefetch_status)
    }

    /// Get runtime health for control plane display.
    pub fn runtime_health(&self) -> Option<SharedRuntimeHealth> {
        self.runtime_health.clone()
    }

    /// Get maximum concurrent jobs for UI display.
    pub fn max_concurrent_jobs(&self) -> usize {
        self.max_concurrent_jobs
    }

    /// Get aggregated telemetry snapshot for UI display.
    pub fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        self.mount_manager.aggregated_telemetry()
    }

    /// Get the cancellation token (for coordinating shutdown).
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Get the OrthoUnionIndex if available.
    pub fn ortho_union_index(&self) -> Option<Arc<OrthoUnionIndex>> {
        self.mount_manager.ortho_union_index()
    }

    /// Get mutable access to the mount manager.
    pub fn mount_manager(&mut self) -> &mut MountManager {
        &mut self.mount_manager
    }

    /// Get the underlying service if mounted.
    pub fn service(&self) -> Option<&XEarthLayerService> {
        self.mount_manager.get_service()
    }

    /// Get the configuration.
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }

    /// Get the Tokio runtime handle from the underlying service.
    ///
    /// Returns None if no service is mounted yet.
    pub fn runtime_handle(&self) -> Option<Handle> {
        self.mount_manager
            .get_service()
            .map(|s| s.runtime_handle().clone())
    }

    /// Get the scenery index.
    pub fn scenery_index(&self) -> &Arc<SceneryIndex> {
        &self.scenery_index
    }

    /// Get the FUSE analyzer for position inference.
    pub fn fuse_analyzer(&self) -> Option<Arc<FuseRequestAnalyzer>> {
        self.fuse_analyzer.clone()
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Shutdown
    // ─────────────────────────────────────────────────────────────────────────

    /// Gracefully shutdown all services.
    ///
    /// This shuts down services in reverse order of startup:
    /// 1. Cancel prefetch and other async tasks
    /// 2. Unmount FUSE (which drops the service and its cache layer)
    ///
    /// Note: Cache services (with GC daemons) are now owned by `XEarthLayerService`
    /// via `CacheLayer`, so they are automatically shut down when the service is dropped
    /// during FUSE unmount.
    pub fn shutdown(mut self) {
        info!("Shutting down ServiceOrchestrator");

        // 1. Cancel all async tasks
        self.cancellation.cancel();

        // 2. Unmount all FUSE filesystems
        // This drops the XEarthLayerService, which owns the CacheLayer with GC daemon
        self.mount_manager.unmount_all();
        info!("FUSE mounts unmounted (cache services shutdown via service drop)");

        info!("ServiceOrchestrator shutdown complete");
    }
}
