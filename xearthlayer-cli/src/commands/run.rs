//! Run command - mount all installed ortho packages.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use xearthlayer::airport::AirportIndex;
use xearthlayer::cache::{run_eviction_daemon, DiskCacheConfig};
use xearthlayer::config::{
    analyze_config, config_file_path, format_size, ConfigFile, DownloadConfig, TextureConfig,
};
use xearthlayer::log::TracingLogger;
use xearthlayer::manager::{
    create_consolidated_overlay, InstalledPackage, LocalPackageStore, MountManager, ServiceBuilder,
};
use xearthlayer::package::PackageType;
use xearthlayer::panic as panic_handler;
use xearthlayer::prefetch::{
    load_cache, save_cache, CacheLoadResult, CircuitBreakerConfig, FuseInferenceConfig,
    FuseRequestAnalyzer, IndexingProgress, PrefetcherBuilder, PrewarmConfig, PrewarmPrefetcher,
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
#[derive(Default)]
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

    // Check for first-run scenario: no config file and no packages directory
    // This provides a friendly welcome message instead of confusing errors
    let config_path = xearthlayer::config::config_file_path();
    if !config_path.exists() {
        let default_packages_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".xearthlayer")
            .join("packages");

        if !default_packages_dir.exists() {
            // First-run scenario: show welcome message and exit cleanly
            return Err(CliError::NeedsSetup);
        }
    }

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

    // Wire load monitor for circuit breaker integration
    // This allows the circuit breaker to see aggregate load across all mounted packages
    let load_monitor = mount_manager.load_monitor();
    service_builder = service_builder.with_load_monitor(Arc::clone(&load_monitor));

    // Determine patches directory
    let patches_dir = if config.patches.enabled {
        config.patches.directory.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".xearthlayer")
                .join("patches")
        })
    } else {
        // Use a non-existent path when patches are disabled
        PathBuf::from("/nonexistent")
    };

    // For TUI mode, mounting happens inside run_with_dashboard() with progress callback
    // For non-TUI mode, mount here before continuing
    if !use_tui {
        println!("Mounting consolidated ortho scenery...");

        let consolidated_result =
            mount_manager.mount_consolidated_ortho(&patches_dir, &store, &service_builder);

        if !consolidated_result.success {
            let error_msg = consolidated_result
                .error
                .as_deref()
                .unwrap_or("Unknown error");
            return Err(CliError::Serve(
                xearthlayer::service::ServiceError::IoError(std::io::Error::other(format!(
                    "Failed to mount consolidated ortho: {}",
                    error_msg
                ))),
            ));
        }

        // Report consolidated mount results
        println!(
            "  ✓ zzXEL_ortho → {}",
            consolidated_result.mountpoint.display()
        );
        println!(
            "    Sources: {} ({} patches, {} packages)",
            consolidated_result.source_count,
            consolidated_result.patch_names.len(),
            consolidated_result.package_regions.len()
        );
        println!("    Files: {}", consolidated_result.file_count);

        // List patches if present
        if !consolidated_result.patch_names.is_empty() {
            println!("    Patches:");
            for name in &consolidated_result.patch_names {
                println!("      • {}", name);
            }
        }

        // List packages
        if !consolidated_result.package_regions.is_empty() {
            println!("    Packages:");
            for region in &consolidated_result.package_regions {
                println!("      • {}", region.to_uppercase());
            }
        }
        println!();

        // Create consolidated overlay symlinks
        let overlay_result = create_consolidated_overlay(&store, &custom_scenery_path);
        if overlay_result.success && overlay_result.package_count > 0 {
            println!(
                "  ✓ yzXEL_overlay → {} ({} DSF files from {} packages)",
                overlay_result.path.display(),
                overlay_result.file_count,
                overlay_result.package_count
            );
            println!();
        } else if let Some(ref error) = overlay_result.error {
            tracing::warn!(error = %error, "Failed to create consolidated overlay");
            println!("Warning: Failed to create consolidated overlay: {}", error);
            println!();
        }

        println!(
            "Ready! Consolidated ortho mount active ({} sources)",
            consolidated_result.source_count
        );
        println!();
    }

    // Start disk cache eviction daemon (shared between TUI and non-TUI paths)
    // This runs in the background and evicts oldest files when cache exceeds limit
    let eviction_cancellation = CancellationToken::new();
    if !args.no_cache {
        if let Some(service) = mount_manager.get_service() {
            let eviction_config = DiskCacheConfig {
                cache_dir: config.cache.directory.clone(),
                max_size_bytes: config.cache.disk_size,
                daemon_interval_secs: 60,
                max_age_days: None,
            };
            let eviction_cancel = eviction_cancellation.clone();
            let runtime_handle = service.runtime_handle().clone();
            runtime_handle.spawn(async move {
                run_eviction_daemon(eviction_config, eviction_cancel).await;
            });
            tracing::debug!(
                max_size = config.cache.disk_size,
                "Disk cache eviction daemon started"
            );
        }
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
                let dds_client = service
                    .dds_client()
                    .expect("DDS client should be available");
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
                    .dds_client(dds_client)
                    .strategy(&config.prefetch.strategy)
                    .shared_status(Arc::clone(&shared_prefetch_status))
                    .cone_half_angle(config.prefetch.cone_angle)
                    .inner_radius_nm(config.prefetch.inner_radius_nm)
                    .outer_radius_nm(config.prefetch.outer_radius_nm)
                    .radial_radius(config.prefetch.radial_radius)
                    .max_tiles_per_cycle(config.prefetch.max_tiles_per_cycle)
                    .cycle_interval_ms(config.prefetch.cycle_interval_ms)
                    // Wire circuit breaker throttler to pause prefetch during high X-Plane load
                    .with_circuit_breaker_throttler(
                        Arc::clone(&load_monitor),
                        CircuitBreakerConfig {
                            threshold_jobs_per_sec: config.prefetch.circuit_breaker_threshold,
                            open_duration: Duration::from_millis(
                                config.prefetch.circuit_breaker_open_ms,
                            ),
                            half_open_duration: Duration::from_secs(
                                config.prefetch.circuit_breaker_half_open_secs,
                            ),
                        },
                    );

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
        // TUI path: Start dashboard in Loading state, mount with progress, then start prefetcher
        // This provides immediate visual feedback during the potentially long index build
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
            // Mount params for mounting inside dashboard with progress
            mount_params: MountParams {
                patches_dir,
                store: &store,
                service_builder,
            },
        };
        run_with_dashboard(ctx, &custom_scenery_path)?
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

    // Cancel disk cache eviction daemon
    eviction_cancellation.cancel();

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
    /// Parameters for mounting (moved here so TUI can show progress)
    mount_params: MountParams<'a>,
}

/// Parameters for mounting consolidated ortho.
struct MountParams<'a> {
    patches_dir: PathBuf,
    store: &'a LocalPackageStore,
    service_builder: ServiceBuilder,
}

/// Run with TUI dashboard, starting in Loading state.
///
/// This function:
/// 1. Starts the TUI immediately in Loading state for OrthoUnionIndex building
/// 2. Mounts consolidated ortho with progress updates
/// 3. Creates overlay symlinks
/// 4. Builds SceneryIndex for prefetching
/// 5. When indexing completes, optionally prewarm cache around airport
/// 6. Starts the prefetcher and transitions to Running
fn run_with_dashboard(
    ctx: TuiContext,
    custom_scenery_path: &Path,
) -> Result<CancellationToken, CliError> {
    use std::sync::Mutex;
    use std::time::Duration;
    use ui::{Dashboard, DashboardConfig, DashboardEvent, LoadingPhase};
    use xearthlayer::ortho_union::{IndexBuildPhase, IndexBuildProgress};

    let dashboard_config = DashboardConfig {
        memory_cache_max: ctx.config.cache.memory_size,
        disk_cache_max: ctx.config.cache.disk_size,
    };

    // Create initial loading progress for OrthoUnionIndex building
    let loading_progress = LoadingProgress::new(ctx.ortho_packages.len());

    // Start dashboard in Loading state
    let initial_state = DashboardState::Loading(loading_progress);
    let mut dashboard =
        Dashboard::with_state(dashboard_config, ctx.shutdown.clone(), initial_state)
            .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?
            .with_prefetch_status(Arc::clone(&ctx.prefetch_status));

    // Draw initial loading screen immediately
    dashboard
        .draw_loading()
        .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;

    // Phase 1: Mount consolidated ortho with progress callback
    // We use a shared state to communicate progress from the callback to the dashboard
    let progress_state = Arc::new(Mutex::new(LoadingProgress::new(ctx.ortho_packages.len())));
    let progress_state_clone = Arc::clone(&progress_state);

    let progress_callback: xearthlayer::ortho_union::IndexBuildProgressCallback =
        Arc::new(move |progress: IndexBuildProgress| {
            let mut state = progress_state_clone.lock().unwrap();
            state.phase = match progress.phase {
                IndexBuildPhase::Discovering => LoadingPhase::Discovering,
                IndexBuildPhase::CheckingCache => LoadingPhase::CheckingCache,
                IndexBuildPhase::Scanning => LoadingPhase::Scanning,
                IndexBuildPhase::Merging => LoadingPhase::Merging,
                IndexBuildPhase::SavingCache => LoadingPhase::SavingCache,
                IndexBuildPhase::Complete => LoadingPhase::Complete,
            };
            state.current_package = progress.current_source.clone().unwrap_or_default();
            state.packages_scanned = progress.sources_complete;
            state.total_packages = progress.sources_total;
            state.tiles_indexed = progress.files_scanned;
            state.using_cache = progress.using_cache;
        });

    // Mount with progress - run in a separate thread so we can update dashboard
    // Use oneshot channel to get result back
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let patches_dir = &ctx.mount_params.patches_dir;
    let service_builder = &ctx.mount_params.service_builder;
    let progress_state_for_draw = Arc::clone(&progress_state);

    // The store reference is borrowed, so we need to do the mount call here
    // and spawn a thread that just signals completion
    let mount_result = std::thread::scope(|s| {
        // Spawn mount thread within scope
        let mount_handle = s.spawn(|| {
            let result = ctx.mount_manager.mount_consolidated_ortho_with_progress(
                patches_dir,
                ctx.mount_params.store,
                service_builder,
                Some(progress_callback),
            );
            let _ = result_tx.send(result);
        });

        // Update dashboard while mount is in progress
        let tick_rate = Duration::from_millis(50);
        let mount_result = loop {
            // Check for result (non-blocking)
            match result_rx.try_recv() {
                Ok(result) => break result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Not ready yet - update dashboard and wait
                    let current_progress = progress_state_for_draw.lock().unwrap().clone();
                    dashboard.update_loading_progress(current_progress);
                    if let Err(e) = dashboard.draw_loading() {
                        tracing::warn!(error = %e, "Failed to draw loading screen");
                    }
                    std::thread::sleep(tick_rate);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Sender dropped without sending - wait for thread and check error
                    mount_handle.join().expect("Mount thread panicked");
                    panic!("Mount thread completed without sending result");
                }
            }
        };

        // Wait for mount thread to finish (should already be done)
        mount_handle.join().expect("Mount thread panicked");
        mount_result
    });

    if !mount_result.success {
        let error_msg = mount_result.error.as_deref().unwrap_or("Unknown error");
        return Err(CliError::Serve(
            xearthlayer::service::ServiceError::IoError(std::io::Error::other(format!(
                "Failed to mount consolidated ortho: {}",
                error_msg
            ))),
        ));
    }

    tracing::info!(
        sources = mount_result.source_count,
        files = mount_result.file_count,
        "Consolidated ortho mounted successfully"
    );

    // Create consolidated overlay symlinks
    let overlay_result = create_consolidated_overlay(ctx.mount_params.store, custom_scenery_path);
    if let Some(ref error) = overlay_result.error {
        tracing::warn!(error = %error, "Failed to create consolidated overlay");
    }

    // Wire in runtime health now that service is available
    if let Some(service) = ctx.mount_manager.get_service() {
        if let Some(runtime_health) = service.runtime_health() {
            let max_concurrent_jobs = service.max_concurrent_jobs();
            dashboard = dashboard.with_runtime_health(runtime_health, max_concurrent_jobs);
        }
    }

    // Get the runtime handle for spawning async tasks
    let runtime_handle = ctx
        .mount_manager
        .get_service()
        .map(|s| s.runtime_handle().clone())
        .ok_or_else(|| CliError::Config("No runtime handle available".to_string()))?;

    // Phase 2: Now build SceneryIndex for prefetching (update progress display)

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
                    LibPrewarmProgress::TileSubmitted => {
                        // Tile was submitted for prefetch (not a cache hit)
                        if let Some(prewarm) = dashboard.prewarm_status().cloned() {
                            let mut updated = prewarm;
                            updated.tile_loaded(false);
                            dashboard.update_prewarm_progress(updated);
                        }
                    }
                    LibPrewarmProgress::TileCached => {
                        // Tile was already cached (counts as cache hit)
                        if let Some(prewarm) = dashboard.prewarm_status().cloned() {
                            let mut updated = prewarm;
                            updated.tile_loaded(true);
                            dashboard.update_prewarm_progress(updated);
                        }
                    }
                    LibPrewarmProgress::Complete {
                        tiles_submitted,
                        cache_hits,
                    } => {
                        tracing::info!(
                            tiles_submitted = tiles_submitted,
                            cache_hits = cache_hits,
                            "Prewarm complete"
                        );
                        prewarm_complete = true;
                        prewarm_active = false;
                        dashboard.clear_prewarm_status();
                    }
                    LibPrewarmProgress::Cancelled { tiles_submitted } => {
                        tracing::info!(tiles_submitted = tiles_submitted, "Prewarm cancelled");
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

    let dds_client = service
        .dds_client()
        .expect("DDS client should be available");

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
        .dds_client(dds_client)
        .strategy(&config.prefetch.strategy)
        .shared_status(Arc::clone(prefetch_status))
        .cone_half_angle(config.prefetch.cone_angle)
        .inner_radius_nm(config.prefetch.inner_radius_nm)
        .outer_radius_nm(config.prefetch.outer_radius_nm)
        .radial_radius(config.prefetch.radial_radius)
        .max_tiles_per_cycle(config.prefetch.max_tiles_per_cycle)
        .cycle_interval_ms(config.prefetch.cycle_interval_ms)
        // Wire circuit breaker throttler to pause prefetch during high X-Plane load
        .with_circuit_breaker_throttler(
            mount_manager.load_monitor(),
            CircuitBreakerConfig {
                threshold_jobs_per_sec: config.prefetch.circuit_breaker_threshold,
                open_duration: Duration::from_millis(config.prefetch.circuit_breaker_open_ms),
                half_open_duration: Duration::from_secs(
                    config.prefetch.circuit_breaker_half_open_secs,
                ),
            },
        );

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

    let dds_client = service
        .dds_client()
        .expect("DDS client should be available");

    // Create prewarm config
    let prewarm_config = PrewarmConfig {
        radius_nm,
        batch_size: 50,
    };

    // Create the prewarm prefetcher
    let prewarm = PrewarmPrefetcher::new(
        Arc::clone(scenery_index),
        dds_client,
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
