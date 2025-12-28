//! Run command - mount all installed ortho packages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use xearthlayer::config::{
    analyze_config, config_file_path, format_size, ConfigFile, DownloadConfig, TextureConfig,
};
use xearthlayer::log::TracingLogger;
use xearthlayer::manager::{LocalPackageStore, MountManager, ServiceBuilder};
use xearthlayer::package::PackageType;
use xearthlayer::panic as panic_handler;
use xearthlayer::prefetch::{
    FuseInferenceConfig, FuseRequestAnalyzer, PrefetcherBuilder, SceneryIndex,
    SharedPrefetchStatus, TelemetryListener,
};
use xearthlayer::service::ServiceConfig;

use super::common::{resolve_dds_format, resolve_provider, DdsCompression, ProviderType};
use crate::error::CliError;
use crate::runner::CliRunner;
use crate::ui;

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

    // Print banner
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

    // Print prefetch status (details printed later after prefetcher starts)
    let prefetch_enabled = config.prefetch.enabled && !args.no_prefetch;
    if !prefetch_enabled {
        println!("Prefetch: Disabled");
    }
    println!();

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
    println!("Mounting packages to Custom Scenery...");
    let results = mount_manager
        .mount_all(&store, |pkg| service_builder.build(pkg))
        .map_err(|e| CliError::Packages(e.to_string()))?;

    // Report results
    let mut success_count = 0;
    let mut failure_count = 0;

    for result in &results {
        if result.success {
            println!(
                "  ✓ {} → {}",
                result.region.to_uppercase(),
                result.mountpoint.display()
            );
            success_count += 1;
        } else {
            println!(
                "  ✗ {}: {}",
                result.region.to_uppercase(),
                result.error.as_deref().unwrap_or("Unknown error")
            );
            failure_count += 1;
        }
    }
    println!();

    if success_count == 0 {
        return Err(CliError::Serve(
            xearthlayer::service::ServiceError::IoError(std::io::Error::other(
                "Failed to mount any packages",
            )),
        ));
    }

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

    // Start prefetch system if enabled
    let prefetch_cancellation = CancellationToken::new();
    let shared_prefetch_status = SharedPrefetchStatus::new();
    let mut prefetch_started = false;

    if prefetch_enabled {
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
                if let Some(analyzer) = fuse_analyzer {
                    builder = builder.with_fuse_analyzer(analyzer);
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
    println!();

    // Set up signal handler for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::SeqCst);
    })
    .map_err(|e| CliError::Config(format!("Failed to set signal handler: {}", e)))?;

    if use_tui {
        // Run with TUI dashboard
        run_with_dashboard(
            &mut mount_manager,
            shutdown,
            config,
            Arc::clone(&shared_prefetch_status),
        )?;
    } else {
        // Fallback to simple text output for non-TTY
        run_with_simple_output(&mut mount_manager, shutdown)?;
    }

    // Cancel prefetch system
    if prefetch_started {
        prefetch_cancellation.cancel();
    }

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

/// Run with TUI dashboard.
fn run_with_dashboard(
    mount_manager: &mut MountManager,
    shutdown: Arc<AtomicBool>,
    config: &ConfigFile,
    prefetch_status: Arc<SharedPrefetchStatus>,
) -> Result<(), CliError> {
    use ui::{Dashboard, DashboardConfig, DashboardEvent};

    let dashboard_config = DashboardConfig {
        memory_cache_max: config.cache.memory_size,
        disk_cache_max: config.cache.disk_size,
    };

    let mut dashboard = Dashboard::new(dashboard_config, shutdown.clone())
        .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?
        .with_prefetch_status(prefetch_status);

    // Wire in control plane health if available
    if let Some(service) = mount_manager.get_service() {
        let control_plane_health = service.control_plane_health();
        let max_concurrent_jobs = service.max_concurrent_jobs();
        dashboard = dashboard.with_control_plane(control_plane_health, max_concurrent_jobs);
    }

    // Main event loop
    let tick_rate = std::time::Duration::from_millis(100);
    let mut last_tick = std::time::Instant::now();

    loop {
        // Poll for events
        if let Some(DashboardEvent::Quit) = dashboard
            .poll_event()
            .map_err(|e| CliError::Config(format!("Dashboard error: {}", e)))?
        {
            break;
        }

        // Update dashboard at tick rate
        if last_tick.elapsed() >= tick_rate {
            let snapshot = mount_manager.aggregated_telemetry();
            dashboard
                .draw(&snapshot)
                .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;
            last_tick = std::time::Instant::now();
        }

        // Small sleep to prevent busy-waiting
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    Ok(())
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
