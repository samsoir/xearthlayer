//! Run command - mount all installed ortho packages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use xearthlayer::airport::AirportIndex;
use xearthlayer::config::{
    analyze_config, config_file_path, format_size, ConfigFile, DownloadConfig, TextureConfig,
};
use xearthlayer::log::TracingLogger;
use xearthlayer::manager::{InstalledPackage, LocalPackageStore, MountManager, ServiceBuilder};
use xearthlayer::package::PackageType;
use xearthlayer::panic as panic_handler;
use xearthlayer::prefetch::{
    load_cache, save_cache, CacheLoadResult, FuseInferenceConfig, FuseRequestAnalyzer,
    IndexingProgress, PrefetcherBuilder, PrewarmConfig, PrewarmPrefetcher,
    PrewarmProgress as LibPrewarmProgress, SceneryIndex, SceneryIndexConfig, SharedPrefetchStatus,
    TelemetryListener,
};
use xearthlayer::service::ServiceConfig;
use xearthlayer::xplane::XPlaneEnvironment;

use super::common::{resolve_dds_format, resolve_provider, DdsCompression, ProviderType};
use crate::error::CliError;
use crate::runner::CliRunner;
use crate::ui::{self, DashboardState, LoadingProgress, PrewarmProgress};

/// Arguments for the run command.
pub struct RunArgs {
    pub provider: Option<ProviderType>,
    pub google_api_key: Option<String>,
    pub mapbox_token: Option<String>,
    pub dds_format: Option<DdsCompression>,
    pub timeout: Option<u64>,
    pub parallel: Option<usize>,
    pub no_cache: bool,
    pub debug: bool,
    pub no_prefetch: bool,
    pub airport: Option<String>,
}

/// Run the run command.
pub fn run(args: RunArgs) -> Result<(), CliError> {
    // Initialize panic handler early for crash cleanup
    panic_handler::init();

    let runner = CliRunner::with_debug(args.debug)?;
    runner.log_startup("run");
    let config = runner.config();

    // Check for config upgrade needs
    check_config_upgrade_warning();

    // Get install location (where packages are stored)
    let install_location = config.packages.install_location.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".xearthlayer")
            .join("packages")
    });

    if !install_location.exists() {
        return Err(CliError::NoPackages {
            install_location: install_location.clone(),
        });
    }

    // Get Custom Scenery path (where mounts go)
    let custom_scenery_path = config
        .packages
        .custom_scenery_path
        .clone()
        .or_else(|| config.xplane.scenery_dir.clone())
        .ok_or_else(|| {
            CliError::Config(
                "No Custom Scenery path configured. \
                 Run 'xearthlayer init' or set packages.custom_scenery_path in config.ini"
                    .to_string(),
            )
        })?;

    if !custom_scenery_path.exists() {
        return Err(CliError::Config(format!(
            "Custom Scenery directory does not exist: {}\n\
             Check your configuration or run 'xearthlayer init'",
            custom_scenery_path.display()
        )));
    }

    // Discover installed packages from install_location
    let store = LocalPackageStore::new(&install_location);
    let packages = store
        .list()
        .map_err(|e| CliError::Packages(e.to_string()))?;

    // Filter to ortho packages only
    let ortho_packages: Vec<_> = packages
        .iter()
        .filter(|p| p.package_type() == PackageType::Ortho)
        .collect();

    if ortho_packages.is_empty() {
        return Err(CliError::NoPackages {
            install_location: install_location.clone(),
        });
    }

    // Resolve settings from CLI and config
    let provider_config = resolve_provider(
        args.provider,
        args.google_api_key,
        args.mapbox_token,
        config,
    )?;
    let format = resolve_dds_format(args.dds_format, config);
    let timeout_secs = args.timeout.unwrap_or(config.download.timeout);
    // Default parallel downloads is handled by DownloadConfig::default()
    let parallel_downloads = args.parallel.unwrap_or(32);

    // Build configurations
    let texture_config = TextureConfig::new(format).with_mipmap_count(5);

    let download_config = DownloadConfig::new()
        .with_timeout_secs(timeout_secs)
        .with_max_retries(3)
        .with_parallel_downloads(parallel_downloads);

    // Check if we'll use TUI (need to know before creating services)
    let use_tui = atty::is(atty::Stream::Stdout);

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!args.no_cache)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
        .pipeline(config.pipeline.clone())
        .quiet_mode(use_tui) // Disable stats logging when TUI is active
        .build();

    // Print banner only if not using TUI (TUI has its own display)
    if !use_tui {
        println!("XEarthLayer v{}", xearthlayer::VERSION);
        println!("{}", "=".repeat(40));
        println!();
        println!("Packages:       {}", install_location.display());
        println!("Custom Scenery: {}", custom_scenery_path.display());
        println!("DDS Format:     {:?}", texture_config.format());
        println!("Provider:       {}", provider_config.name());
        println!("FUSE Backend:   fuse3 (async multi-threaded)");
        println!();

        // List discovered packages
        println!("Installed ortho packages ({}):", ortho_packages.len());
        for pkg in &ortho_packages {
            println!("  {} v{}", pkg.region().to_uppercase(), pkg.version());
        }
        println!();

        // Print cache info
        if !args.no_cache {
            println!(
                "Cache: {} memory, {} disk",
                format_size(config.cache.memory_size),
                format_size(config.cache.disk_size)
            );
        } else {
            println!("Cache: Disabled");
        }
    }

    // Print prefetch status (details printed later after prefetcher starts)
    let prefetch_enabled = config.prefetch.enabled && !args.no_prefetch;
    if !use_tui && !prefetch_enabled {
        println!("Prefetch: Disabled");
    }
    if !use_tui {
        println!();
    }

    // Create mount manager with the custom scenery path as target
    let mut mount_manager = MountManager::with_scenery_path(&custom_scenery_path);

    // Create a logger for all services
    let logger: Arc<dyn xearthlayer::log::Logger> = Arc::new(TracingLogger);

    // Create FUSE request analyzer for position inference (if prefetch is enabled)
    // The analyzer is created early so its callback can be wired to services before mounting.
    // This enables FUSE-based position inference when telemetry is unavailable.
    let fuse_analyzer = if prefetch_enabled {
        Some(Arc::new(FuseRequestAnalyzer::new(
            FuseInferenceConfig::default(),
        )))
    } else {
        None
    };

    // Create service builder with shared disk I/O limiter and configured profile
    // This ensures all packages share a single limiter tuned for the storage type
    let mut service_builder = ServiceBuilder::with_disk_io_profile(
        service_config.clone(),
        provider_config.clone(),
        logger.clone(),
        config.cache.disk_io_profile,
    );

    // Wire FUSE analyzer callback to services for position inference
    if let Some(ref analyzer) = fuse_analyzer {
        service_builder = service_builder.with_tile_request_callback(analyzer.callback());
    }

    // Mount all packages
    if !use_tui {
        println!("Mounting packages to Custom Scenery...");
    }
    let results = mount_manager
        .mount_all(&store, |pkg| service_builder.build(pkg))
        .map_err(|e| CliError::Packages(e.to_string()))?;

    // Report results
    let mut success_count = 0;
    let mut failure_count = 0;

    for result in &results {
        if result.success {
            if !use_tui {
                println!(
                    "  ✓ {} → {}",
                    result.region.to_uppercase(),
                    result.mountpoint.display()
                );
            }
            success_count += 1;
        } else {
            if !use_tui {
                println!(
                    "  ✗ {}: {}",
                    result.region.to_uppercase(),
                    result.error.as_deref().unwrap_or("Unknown error")
                );
            }
            failure_count += 1;
        }
    }
    if !use_tui {
        println!();
    }

    if success_count == 0 {
        return Err(CliError::Serve(
            xearthlayer::service::ServiceError::IoError(std::io::Error::other(
                "Failed to mount any packages",
            )),
        ));
    }

    if !use_tui {
        println!(
            "Ready! {} package(s) mounted{}",
            success_count,
            if failure_count > 0 {
                format!(", {} failed", failure_count)
            } else {
                String::new()
            }
        );
        println!();
    }

    // Start prefetch system if enabled (non-TUI path only)
    // TUI path handles prefetch setup inside run_with_dashboard after index is built
    let prefetch_cancellation = CancellationToken::new();
    let shared_prefetch_status = SharedPrefetchStatus::new();
    let mut prefetch_started = false;

    if !use_tui && prefetch_enabled {
        if let Some(service) = mount_manager.get_service() {
            // Get shared memory cache adapter for cache-aware prefetching
            // This is the same adapter instance used by the pipeline
            if let Some(memory_cache) = service.memory_cache_adapter() {
                let dds_handler = service.create_prefetch_handler();
                let runtime_handle = service.runtime_handle().clone();

                // Create channels for telemetry data
                let (state_tx, state_rx) = mpsc::channel(32);

                // Start the telemetry listener
                let listener = TelemetryListener::new(config.prefetch.udp_port);
                let listener_cancel = prefetch_cancellation.clone();
                runtime_handle.spawn(async move {
                    tokio::select! {
                        result = listener.run(state_tx) => {
                            if let Err(e) = result {
                                tracing::warn!("Telemetry listener error: {}", e);
                            }
                        }
                        _ = listener_cancel.cancelled() => {
                            tracing::debug!("Telemetry listener cancelled");
                        }
                    }
                });

                // Build scenery index for scenery-aware prefetching
                // This enables exact tile lookup from .ter files instead of coordinate calculation
                let scenery_index = {
                    let index = Arc::new(SceneryIndex::with_defaults());
                    let mut total_tiles = 0usize;
                    for pkg in &ortho_packages {
                        match index.build_from_package(&pkg.path) {
                            Ok(count) => {
                                total_tiles += count;
                                tracing::debug!(
                                    region = %pkg.region(),
                                    tiles = count,
                                    "Indexed scenery package"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    region = %pkg.region(),
                                    error = %e,
                                    "Failed to index scenery package"
                                );
                            }
                        }
                    }
                    if total_tiles > 0 {
                        println!(
                            "Scenery index: {} tiles ({} land, {} sea)",
                            total_tiles,
                            index.land_tile_count(),
                            index.sea_tile_count()
                        );
                        Some(index)
                    } else {
                        tracing::warn!(
                            "No scenery tiles indexed, falling back to coordinate-based prefetch"
                        );
                        None
                    }
                };

                // Build prefetcher using the builder with configuration
                // Strategies: radial (simple), heading-aware (directional), auto (graceful degradation)
                let mut builder = PrefetcherBuilder::new()
                    .memory_cache(memory_cache)
                    .dds_handler(dds_handler)
                    .strategy(&config.prefetch.strategy)
                    .shared_status(Arc::clone(&shared_prefetch_status))
                    .cone_half_angle(config.prefetch.cone_angle)
                    .inner_radius_nm(config.prefetch.inner_radius_nm)
                    .outer_radius_nm(config.prefetch.outer_radius_nm)
                    .max_tiles_per_cycle(config.prefetch.max_tiles_per_cycle)
                    .cycle_interval_ms(config.prefetch.cycle_interval_ms)
                    .radial_radius(config.prefetch.radial_radius);

                // Wire FUSE analyzer for heading-aware/auto strategies
                // This enables FUSE-based position inference when telemetry is unavailable
                if let Some(ref analyzer) = fuse_analyzer {
                    builder = builder.with_fuse_analyzer(Arc::clone(analyzer));
                }

                // Wire scenery index for scenery-aware prefetching
                // This enables exact tile lookup instead of coordinate calculation
                if let Some(index) = scenery_index {
                    builder = builder.with_scenery_index(index);
                }

                let prefetcher = builder.build();

                // Get startup info before prefetcher is consumed
                let startup_info = prefetcher.startup_info();

                // Start the prefetcher
                let prefetcher_cancel = prefetch_cancellation.clone();
                runtime_handle.spawn(async move {
                    prefetcher.run(state_rx, prefetcher_cancel).await;
                });

                println!(
                    "Prefetch system started ({}, UDP port {})",
                    startup_info, config.prefetch.udp_port
                );
                prefetch_started = true;
            } else {
                println!("Warning: Memory cache not available, prefetch disabled");
            }
        } else {
            println!("Warning: No services available for prefetch");
        }
    }
    if !use_tui {
        println!();
    }

    // Set up signal handler for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::SeqCst);
    })
    .map_err(|e| CliError::Config(format!("Failed to set signal handler: {}", e)))?;

    // Track which cancellation token to use for cleanup
    let cleanup_cancellation = if use_tui {
        // TUI path: Start dashboard in Loading state, build index async, then start prefetcher
        // This provides immediate visual feedback during the 30+ second index build
        let ctx = TuiContext {
            mount_manager: &mut mount_manager,
            shutdown,
            config,
            prefetch_status: Arc::clone(&shared_prefetch_status),
            ortho_packages: ortho_packages.clone(),
            prefetch_enabled,
            fuse_analyzer,
            prefetch_cancellation,
            airport_icao: args.airport.clone(),
            // Derive X-Plane environment from Custom Scenery path
            xplane_env: config
                .packages
                .custom_scenery_path
                .as_ref()
                .or(config.xplane.scenery_dir.as_ref())
                .and_then(|p| XPlaneEnvironment::from_custom_scenery_path(p).ok()),
        };
        run_with_dashboard(ctx)?
    } else {
        // Fallback to simple text output for non-TTY
        run_with_simple_output(&mut mount_manager, shutdown)?;

        // Cancel prefetch system (non-TUI path handles prefetch in main function)
        if prefetch_started {
            prefetch_cancellation.cancel();
        }
        CancellationToken::new() // Dummy token, already cancelled above
    };

    // Cancel prefetch system for TUI path
    cleanup_cancellation.cancel();

    // Get final telemetry before unmounting
    let final_snapshot = mount_manager.aggregated_telemetry();

    // Unmount all packages
    mount_manager.unmount_all();

    // Print final session summary (to stdout after TUI exits)
    if final_snapshot.jobs_completed > 0 {
        ui::dashboard::print_session_summary(&final_snapshot);
    }

    println!();
    println!("All packages unmounted. Goodbye!");

    Ok(())
}

/// Context for TUI dashboard with loading state support.
struct TuiContext<'a> {
    mount_manager: &'a mut MountManager,
    shutdown: Arc<AtomicBool>,
    config: &'a ConfigFile,
    prefetch_status: Arc<SharedPrefetchStatus>,
    ortho_packages: Vec<&'a InstalledPackage>,
    prefetch_enabled: bool,
    fuse_analyzer: Option<Arc<FuseRequestAnalyzer>>,
    prefetch_cancellation: CancellationToken,
    /// Airport ICAO code for prewarm (if specified)
    airport_icao: Option<String>,
    /// X-Plane environment for apt.dat lookup and resource paths
    xplane_env: Option<XPlaneEnvironment>,
}

/// Run with TUI dashboard, starting in Loading state.
///
/// This function:
/// 1. Starts the TUI immediately in Loading state
/// 2. Spawns async SceneryIndex building with progress updates
/// 3. When indexing completes, optionally prewarm cache around airport
/// 4. Starts the prefetcher and transitions to Running
fn run_with_dashboard(ctx: TuiContext) -> Result<CancellationToken, CliError> {
    use std::time::Duration;
    use ui::{Dashboard, DashboardConfig, DashboardEvent};

    let dashboard_config = DashboardConfig {
        memory_cache_max: ctx.config.cache.memory_size,
        disk_cache_max: ctx.config.cache.disk_size,
    };

    // Create initial loading progress
    let loading_progress = LoadingProgress::new(ctx.ortho_packages.len());

    // Start dashboard in Loading state
    let initial_state = DashboardState::Loading(loading_progress);
    let mut dashboard =
        Dashboard::with_state(dashboard_config, ctx.shutdown.clone(), initial_state)
            .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?
            .with_prefetch_status(Arc::clone(&ctx.prefetch_status));

    // Wire in control plane health if available
    if let Some(service) = ctx.mount_manager.get_service() {
        let control_plane_health = service.control_plane_health();
        let max_concurrent_jobs = service.max_concurrent_jobs();
        dashboard = dashboard.with_control_plane(control_plane_health, max_concurrent_jobs);
    }

    // Get the runtime handle for spawning async tasks
    let runtime_handle = ctx
        .mount_manager
        .get_service()
        .map(|s| s.runtime_handle().clone())
        .ok_or_else(|| CliError::Config("No runtime handle available".to_string()))?;

    // Create channel for index progress updates
    let (progress_tx, mut progress_rx) = mpsc::channel::<IndexingProgress>(32);

    // Prepare package list for async indexing
    let packages_for_index: Vec<(String, PathBuf)> = ctx
        .ortho_packages
        .iter()
        .map(|p| (p.region().to_string(), p.path.clone()))
        .collect();

    // Debug: Log what packages are being indexed
    tracing::debug!(
        package_count = packages_for_index.len(),
        "Preparing to build scenery index"
    );
    for (name, path) in &packages_for_index {
        tracing::debug!(
            region = %name,
            path = %path.display(),
            terrain_exists = path.join("terrain").exists(),
            "Package for indexing"
        );
    }

    // Try to load scenery index from cache first
    let (scenery_index, cache_loaded) = match load_cache(&packages_for_index) {
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

            // Update loading progress to show cache was used
            let mut loading = LoadingProgress::new(packages_for_index.len());
            loading.tiles_indexed = total_tiles;
            loading.packages_scanned = packages_for_index.len();
            loading.current_package = "Cache loaded".to_string();
            dashboard.update_loading_progress(loading);

            // Create index from cached tiles
            let index = Arc::new(SceneryIndex::from_tiles(
                tiles,
                SceneryIndexConfig::default(),
            ));

            // Send completion signal through the channel
            let _ = progress_tx
                .try_send(IndexingProgress::Complete {
                    total: total_tiles,
                    land: total_tiles - sea_tiles,
                    sea: sea_tiles,
                })
                .ok();

            (index, true)
        }
        CacheLoadResult::Stale { reason } => {
            tracing::info!(reason = %reason, "Scenery cache is stale, rebuilding");
            (Arc::new(SceneryIndex::with_defaults()), false)
        }
        CacheLoadResult::NotFound => {
            tracing::info!("No scenery cache found, building index");
            (Arc::new(SceneryIndex::with_defaults()), false)
        }
        CacheLoadResult::Invalid { error } => {
            tracing::warn!(error = %error, "Scenery cache invalid, rebuilding");
            (Arc::new(SceneryIndex::with_defaults()), false)
        }
    };

    // If cache wasn't loaded, build index from scratch and save cache on completion
    if !cache_loaded {
        let index_for_build = Arc::clone(&scenery_index);
        let packages_for_cache = packages_for_index.clone();

        runtime_handle.spawn(async move {
            SceneryIndex::build_from_packages_with_progress(
                Arc::clone(&index_for_build),
                packages_for_index,
                progress_tx,
            )
            .await;

            // Save cache for next launch
            if let Err(e) = save_cache(&index_for_build, &packages_for_cache) {
                tracing::warn!(error = %e, "Failed to save scenery cache");
            }
        });
    }

    // Track state transitions
    let mut indexing_complete = cache_loaded; // True if loaded from cache
    let mut prewarm_active = false;
    let mut prewarm_complete = false;
    let mut prefetcher_started = false;

    // Prewarm progress channel (created on-demand when prewarm starts)
    let mut prewarm_progress_rx: Option<mpsc::Receiver<LibPrewarmProgress>> = None;
    let prewarm_cancellation = CancellationToken::new();

    // Main event loop
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    loop {
        // Poll for events
        match dashboard.poll_event() {
            Ok(Some(DashboardEvent::Quit)) => break,
            Ok(Some(DashboardEvent::Cancel)) => {
                // Cancel prewarm if active
                if prewarm_active && !prewarm_complete {
                    tracing::info!("Prewarm cancelled by user");
                    prewarm_cancellation.cancel();
                    prewarm_complete = true;
                    prewarm_active = false;
                }
            }
            Ok(None) => {}
            Err(e) => return Err(CliError::Config(format!("Dashboard error: {}", e))),
        }

        // Check for index progress updates (non-blocking)
        while let Ok(progress) = progress_rx.try_recv() {
            match progress {
                IndexingProgress::PackageStarted { name, index, total } => {
                    let mut loading = LoadingProgress::new(total);
                    loading.packages_scanned = index;
                    loading.scanning(&name);
                    dashboard.update_loading_progress(loading);
                }
                IndexingProgress::PackageCompleted { tiles, .. } => {
                    if let DashboardState::Loading(ref mut progress) = dashboard.state().clone() {
                        let mut updated = progress.clone();
                        updated.package_completed(tiles);
                        dashboard.update_loading_progress(updated);
                    }
                }
                IndexingProgress::TileProgress { tiles_indexed } => {
                    if let DashboardState::Loading(ref mut progress) = dashboard.state().clone() {
                        let mut updated = progress.clone();
                        updated.tiles_indexed = tiles_indexed;
                        dashboard.update_loading_progress(updated);
                    }
                }
                IndexingProgress::Complete { total, land, sea } => {
                    tracing::info!(
                        total = total,
                        land = land,
                        sea = sea,
                        "Scenery index complete"
                    );
                    indexing_complete = true;
                }
            }
        }

        // After indexing, transition to Running state and start prewarm in background
        if indexing_complete && !prewarm_active && !prewarm_complete && !prefetcher_started {
            // Transition to Running state immediately
            dashboard.transition_to_running();

            // Start prewarm in background if airport specified
            if let Some(ref icao) = ctx.airport_icao {
                match start_prewarm(
                    ctx.mount_manager,
                    ctx.config,
                    &scenery_index,
                    icao,
                    ctx.xplane_env.as_ref(),
                    &prewarm_cancellation,
                    &runtime_handle,
                ) {
                    Ok((rx, airport_name, total_tiles)) => {
                        tracing::info!(
                            icao = %icao,
                            airport = %airport_name,
                            tiles = total_tiles,
                            "Starting prewarm in background"
                        );
                        prewarm_progress_rx = Some(rx);
                        prewarm_active = true;

                        // Set prewarm status (displays in header while Running)
                        let prewarm_progress = PrewarmProgress::new(icao, total_tiles);
                        dashboard.update_prewarm_progress(prewarm_progress);
                    }
                    Err(e) => {
                        // Prewarm failed to start, log warning and continue
                        tracing::warn!("Prewarm skipped: {}", e);
                        prewarm_complete = true;
                    }
                }
            } else {
                // No airport specified, skip prewarm
                prewarm_complete = true;
            }

            // Start prefetcher immediately (doesn't wait for prewarm)
            if ctx.prefetch_enabled {
                prefetcher_started = start_prefetcher(
                    ctx.mount_manager,
                    ctx.config,
                    &ctx.prefetch_status,
                    &scenery_index,
                    ctx.fuse_analyzer.clone(),
                    &prewarm_cancellation,
                    &runtime_handle,
                );
            } else {
                prefetcher_started = true; // Prevent re-entry
            }
        }

        // Handle prewarm progress updates (runs in background)
        if let Some(ref mut rx) = prewarm_progress_rx {
            while let Ok(progress) = rx.try_recv() {
                match progress {
                    LibPrewarmProgress::Starting { total_tiles } => {
                        if let Some(prewarm) = dashboard.prewarm_status().cloned() {
                            let mut updated = prewarm;
                            updated.total_tiles = total_tiles;
                            dashboard.update_prewarm_progress(updated);
                        }
                    }
                    LibPrewarmProgress::TileLoaded { cache_hit } => {
                        if let Some(prewarm) = dashboard.prewarm_status().cloned() {
                            let mut updated = prewarm;
                            updated.tile_loaded(cache_hit);
                            dashboard.update_prewarm_progress(updated);
                        }
                    }
                    LibPrewarmProgress::TileFailed => {
                        // Count as loaded (progress) but not a cache hit
                        if let Some(prewarm) = dashboard.prewarm_status().cloned() {
                            let mut updated = prewarm;
                            updated.tiles_loaded += 1;
                            dashboard.update_prewarm_progress(updated);
                        }
                    }
                    LibPrewarmProgress::Complete {
                        tiles_loaded,
                        cache_hits,
                        failures,
                    } => {
                        tracing::info!(
                            tiles_loaded = tiles_loaded,
                            cache_hits = cache_hits,
                            failures = failures,
                            "Prewarm complete"
                        );
                        prewarm_complete = true;
                        prewarm_active = false;
                        dashboard.clear_prewarm_status();
                    }
                    LibPrewarmProgress::Cancelled { tiles_loaded } => {
                        tracing::info!(tiles_loaded = tiles_loaded, "Prewarm cancelled");
                        prewarm_complete = true;
                        prewarm_active = false;
                        dashboard.clear_prewarm_status();
                    }
                }
            }
        }

        // Legacy prefetcher start block - now handled above, keep for backwards compat
        if indexing_complete && prewarm_complete && !prefetcher_started {
            if ctx.prefetch_enabled {
                prefetcher_started = start_prefetcher(
                    ctx.mount_manager,
                    ctx.config,
                    &ctx.prefetch_status,
                    &scenery_index,
                    ctx.fuse_analyzer.clone(),
                    &ctx.prefetch_cancellation,
                    &runtime_handle,
                );
            } else {
                prefetcher_started = true; // Prevent re-entry
            }
            // Transition to Running state
            dashboard.transition_to_running();
        }

        // Update dashboard at tick rate
        if last_tick.elapsed() >= tick_rate {
            if dashboard.is_loading() {
                dashboard
                    .draw_loading()
                    .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;
            } else {
                let snapshot = ctx.mount_manager.aggregated_telemetry();
                dashboard
                    .draw(&snapshot)
                    .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;
            }
            last_tick = Instant::now();
        }

        // Small sleep to prevent busy-waiting
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(ctx.prefetch_cancellation)
}

/// Start the prefetcher after scenery index is built.
fn start_prefetcher(
    mount_manager: &mut MountManager,
    config: &ConfigFile,
    prefetch_status: &Arc<SharedPrefetchStatus>,
    scenery_index: &Arc<SceneryIndex>,
    fuse_analyzer: Option<Arc<FuseRequestAnalyzer>>,
    cancellation: &CancellationToken,
    runtime_handle: &Handle,
) -> bool {
    let Some(service) = mount_manager.get_service() else {
        tracing::warn!("No services available for prefetch");
        return false;
    };

    let Some(memory_cache) = service.memory_cache_adapter() else {
        tracing::warn!("Memory cache not available, prefetch disabled");
        return false;
    };

    let dds_handler = service.create_prefetch_handler();

    // Create channels for telemetry data
    let (state_tx, state_rx) = mpsc::channel(32);

    // Start the telemetry listener
    let listener = TelemetryListener::new(config.prefetch.udp_port);
    let listener_cancel = cancellation.clone();
    runtime_handle.spawn(async move {
        tokio::select! {
            result = listener.run(state_tx) => {
                if let Err(e) = result {
                    tracing::warn!("Telemetry listener error: {}", e);
                }
            }
            _ = listener_cancel.cancelled() => {
                tracing::debug!("Telemetry listener cancelled");
            }
        }
    });

    // Build prefetcher
    let mut builder = PrefetcherBuilder::new()
        .memory_cache(memory_cache)
        .dds_handler(dds_handler)
        .strategy(&config.prefetch.strategy)
        .shared_status(Arc::clone(prefetch_status))
        .cone_half_angle(config.prefetch.cone_angle)
        .inner_radius_nm(config.prefetch.inner_radius_nm)
        .outer_radius_nm(config.prefetch.outer_radius_nm)
        .max_tiles_per_cycle(config.prefetch.max_tiles_per_cycle)
        .cycle_interval_ms(config.prefetch.cycle_interval_ms)
        .radial_radius(config.prefetch.radial_radius);

    if let Some(analyzer) = fuse_analyzer {
        builder = builder.with_fuse_analyzer(analyzer);
    }

    if scenery_index.tile_count() > 0 {
        builder = builder.with_scenery_index(Arc::clone(scenery_index));
    }

    let prefetcher = builder.build();
    let prefetcher_cancel = cancellation.clone();
    runtime_handle.spawn(async move {
        prefetcher.run(state_rx, prefetcher_cancel).await;
    });

    tracing::info!(
        strategy = %config.prefetch.strategy,
        udp_port = config.prefetch.udp_port,
        "Prefetch system started"
    );

    true
}

/// Start prewarm for a given airport.
///
/// Returns the progress receiver, airport name, and total tile count on success.
fn start_prewarm(
    mount_manager: &mut MountManager,
    config: &ConfigFile,
    scenery_index: &Arc<SceneryIndex>,
    icao: &str,
    xplane_env: Option<&XPlaneEnvironment>,
    cancellation: &CancellationToken,
    runtime_handle: &Handle,
) -> Result<(mpsc::Receiver<LibPrewarmProgress>, String, usize), String> {
    // Get X-Plane environment for apt.dat lookup
    let xplane_env = xplane_env.ok_or_else(|| "X-Plane installation not detected".to_string())?;

    // Get apt.dat path
    let apt_dat_path = xplane_env.apt_dat_path().ok_or_else(|| {
        format!(
            "Airport database not found at {}",
            xplane_env.earth_nav_data_path().display()
        )
    })?;

    // Load airport index from apt.dat
    let airport_index = AirportIndex::from_apt_dat(&apt_dat_path)
        .map_err(|e| format!("Failed to load airport database: {}", e))?;

    // Look up the airport
    let airport = airport_index
        .get(icao)
        .ok_or_else(|| format!("Airport '{}' not found in apt.dat", icao))?;

    // Get prewarm radius from config
    let radius_nm = config.prewarm.radius_nm;

    // Log scenery index stats and airport coordinates for debugging
    tracing::debug!(
        airport_lat = airport.latitude,
        airport_lon = airport.longitude,
        airport_name = %airport.name,
        scenery_tiles_total = scenery_index.tile_count(),
        scenery_land_tiles = scenery_index.land_tile_count(),
        scenery_sea_tiles = scenery_index.sea_tile_count(),
        radius_nm = radius_nm,
        "Searching for tiles near airport"
    );

    // Find tiles within radius
    let tiles = scenery_index.tiles_near(airport.latitude, airport.longitude, radius_nm);

    tracing::debug!(tiles_found = tiles.len(), "Tiles found in radius");

    if tiles.is_empty() {
        return Err(format!(
            "No scenery tiles found within {}nm of {} ({}). Scenery index contains {} tiles total.",
            radius_nm,
            icao,
            airport.name,
            scenery_index.tile_count()
        ));
    }

    // Get service for DDS handler and memory cache
    let service = mount_manager
        .get_service()
        .ok_or_else(|| "No services available for prewarm".to_string())?;

    let memory_cache = service
        .memory_cache_adapter()
        .ok_or_else(|| "Memory cache not available for prewarm".to_string())?;

    let dds_handler = service.create_prefetch_handler();

    // Create prewarm config
    let prewarm_config = PrewarmConfig {
        radius_nm,
        max_concurrent: 8,
    };

    // Create the prewarm prefetcher
    let prewarm = PrewarmPrefetcher::new(
        Arc::clone(scenery_index),
        dds_handler,
        memory_cache,
        prewarm_config,
    );

    // Create progress channel
    let (progress_tx, progress_rx) = mpsc::channel(32);
    let cancel_token = cancellation.clone();
    let airport_lat = airport.latitude;
    let airport_lon = airport.longitude;
    let airport_name = airport.name.clone();
    let total_tiles = tiles.len();

    // Spawn the prewarm task
    runtime_handle.spawn(async move {
        prewarm
            .run(airport_lat, airport_lon, progress_tx, cancel_token)
            .await;
    });

    Ok((progress_rx, airport_name, total_tiles))
}

/// Run with simple text output (for non-TTY environments).
fn run_with_simple_output(
    mount_manager: &mut MountManager,
    shutdown: Arc<AtomicBool>,
) -> Result<(), CliError> {
    println!("Start X-Plane to use XEarthLayer scenery.");
    println!("Press Ctrl+C to stop.");
    println!();

    let mut last_telemetry = std::time::Instant::now();
    let telemetry_interval = std::time::Duration::from_secs(30);

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Display telemetry every 30 seconds
        if last_telemetry.elapsed() >= telemetry_interval {
            let snapshot = mount_manager.aggregated_telemetry();
            ui::dashboard::print_simple_status(&snapshot);
            last_telemetry = std::time::Instant::now();
        }
    }

    Ok(())
}

/// Display warning if configuration file needs upgrade.
///
/// Checks if the user's config.ini is missing settings from the current version
/// and displays a helpful message with instructions on how to upgrade.
fn check_config_upgrade_warning() {
    let path = config_file_path();

    // Only check if config file exists
    if !path.exists() {
        return;
    }

    match analyze_config(&path) {
        Ok(analysis) if analysis.needs_upgrade => {
            let missing_count = analysis.missing_keys.len();
            let deprecated_count = analysis.deprecated_keys.len();

            eprintln!();
            eprintln!(
                "Warning: Your configuration file is missing {} new setting(s)",
                missing_count
            );
            if deprecated_count > 0 {
                eprintln!(
                    "         and contains {} deprecated setting(s).",
                    deprecated_count
                );
            }
            eprintln!();
            eprintln!("Run 'xearthlayer config upgrade' to update your configuration.");
            eprintln!("Use 'xearthlayer config upgrade --dry-run' to preview changes first.");
            eprintln!();
        }
        Ok(_) => {} // Up to date, no message needed
        Err(e) => {
            // Log error but don't fail - config upgrade is informational
            tracing::warn!("Failed to analyze config for upgrade: {}", e);
        }
    }
}
