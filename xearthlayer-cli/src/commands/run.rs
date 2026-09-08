//! Run command - mount all installed ortho packages.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use xearthlayer::config::{
    format_size, ControlPlaneSettings, DownloadConfig, PipelineSettings, TextureConfig,
};
use xearthlayer::manager::LocalPackageStore;
use xearthlayer::package::PackageType;
use xearthlayer::panic as panic_handler;
use xearthlayer::service::{
    OrchestratorConfig, ServiceConfig, ServiceOrchestrator, StartupProgress,
};
use xearthlayer::xplane::XPlaneEnvironment;

use super::common::{resolve_dds_format, resolve_provider, DdsCompression, ProviderType};
use crate::error::CliError;
use crate::tui_app::{run_headless, run_tui, TuiAppConfig};
use crate::ui;
use xearthlayer::preflight::BootstrapContext;

/// Arguments for the run command.
///
/// `--debug` and `--profile` are promoted to top-level CLI flags (see
/// `main.rs`) so they apply to every subcommand and reach the logging
/// initializer before dispatch. They are no longer carried per-command.
#[derive(Default)]
pub struct RunArgs {
    pub provider: Option<ProviderType>,
    pub google_api_key: Option<String>,
    pub mapbox_token: Option<String>,
    pub dds_format: Option<DdsCompression>,
    pub timeout: Option<u64>,
    pub parallel: Option<usize>,
    pub no_cache: bool,
    pub no_prefetch: bool,
    pub airport: Option<String>,
}

/// Run the run command.
pub fn run(args: RunArgs, ctx: &BootstrapContext) -> Result<(), CliError> {
    // Initialize panic handler early for crash cleanup
    panic_handler::init();

    // Every prerequisite below was verified before dispatch. Absence here is a
    // misconfigured registry, not a user error, so it names the check that owed
    // the value rather than degrading into a vague failure.
    let config = ctx
        .config()
        .expect("preflight check 'load-config' contributes the configuration");
    let install_location = ctx
        .install_location()
        .expect("preflight check 'packages-installed' contributes the install location")
        .clone();
    let custom_scenery_path = ctx
        .custom_scenery_path()
        .expect("preflight check 'resolve-custom-scenery' contributes the Custom Scenery path")
        .clone();

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
    // Mipmap levels are left unset so the encoder emits the full chain for the
    // texture size — a truncated chain makes X-Plane clamp sampling and band
    // sloped terrain at grazing angles.
    let texture_config = TextureConfig::new(format)
        .with_compressor(config.texture.compressor.clone())
        .with_gpu_device(config.texture.gpu_device.clone());
    let dds_format = texture_config.format();

    let download_config = DownloadConfig::new()
        .with_timeout_secs(timeout_secs)
        .with_max_retries(3)
        .with_parallel_downloads(parallel_downloads);

    // Check if we'll use TUI (need to know before creating services)
    let use_tui = atty::is(atty::Stream::Stdout);

    // Pool sizing is no longer configurable (#249) — it is derived by
    // ResourcePoolConfig::default(). These two fields keep their own defaults so
    // PipelineSettings still constructs; nothing reads them today, since
    // ServiceConfig::pipeline() has no caller.
    let pipeline_settings = PipelineSettings {
        max_http_concurrent: xearthlayer::config::default_http_concurrent(),
        max_cpu_concurrent: xearthlayer::config::default_cpu_concurrent(),
        max_prefetch_in_flight: config.pipeline.max_prefetch_in_flight,
        request_timeout_secs: config.executor.request_timeout_secs,
        max_retries: config.executor.max_retries,
        retry_base_delay_ms: config.executor.retry_base_delay_ms,
        coalesce_channel_capacity: config.pipeline.coalesce_channel_capacity,
    };

    let control_plane_settings = ControlPlaneSettings {
        max_concurrent_jobs: config.control_plane.max_concurrent_jobs,
        stall_threshold_secs: config.control_plane.stall_threshold_secs,
        health_check_interval_secs: config.control_plane.health_check_interval_secs,
        semaphore_timeout_secs: config.control_plane.semaphore_timeout_secs,
    };

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .download(download_config)
        .cache_enabled(!args.no_cache)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .cache_dds_disk_ratio(config.cache.dds_disk_ratio)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
        .pipeline(pipeline_settings)
        .control_plane(control_plane_settings)
        .quiet_mode(use_tui) // Disable stats logging when TUI is active
        .build();

    // Print banner only if not using TUI (TUI has its own display)
    if !use_tui {
        println!("XEarthLayer v{}", xearthlayer::VERSION);
        println!("{}", "=".repeat(40));
        println!();
        println!("Packages:       {}", install_location.display());
        println!("Custom Scenery: {}", custom_scenery_path.display());
        println!("DDS Format:     {:?}", dds_format);
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

    // Create ServiceOrchestrator configuration
    // This consolidates all config needed for backend services
    let orch_config = OrchestratorConfig::from_config_file(
        config,
        provider_config.clone(),
        service_config.clone(),
        custom_scenery_path.clone(),
        install_location.clone(),
        use_tui,
    );

    // Start ServiceOrchestrator - this initializes:
    // - Cache services (CacheLayer with internal GC daemon)
    // - ServiceBuilder with cache bridges
    // - MountManager with load monitor
    // - FUSE request analyzer (if prefetch enabled)
    // - APT aggregator (aircraft position provider)
    let mut orchestrator = ServiceOrchestrator::start(orch_config.clone())
        .map_err(|e| CliError::Config(format!("Failed to start service orchestrator: {}", e)))?;

    // Get shared components from orchestrator for TUI integration
    let shared_prefetch_status = orchestrator.prefetch_status();
    let aircraft_position = orchestrator.aircraft_position();

    // For non-TUI mode, initialize all services with text progress output
    if !use_tui {
        println!("Initializing services...");

        // Progress callback that prints to stdout
        let progress_callback = |progress: StartupProgress| match progress {
            StartupProgress::ScanningDiskCache => {
                println!("  Scanning disk cache...");
            }
            StartupProgress::Mounting { current_source, .. } => {
                if let Some(source) = current_source {
                    println!("  Scanning: {}", source);
                }
            }
            StartupProgress::CreatingOverlay => {
                println!("  Creating overlay symlinks...");
            }
            StartupProgress::StartingTelemetry => {
                println!("  Starting APT telemetry receiver...");
            }
            StartupProgress::BuildingSceneryIndex { package_name, .. } => {
                println!("  Indexing: {}", package_name);
            }
            StartupProgress::SceneryIndexComplete {
                total_tiles,
                land_tiles,
                sea_tiles,
            } => {
                println!(
                    "  Scenery index: {} tiles ({} land, {} sea)",
                    total_tiles, land_tiles, sea_tiles
                );
            }
            StartupProgress::StartingPrefetch => {
                println!("  Starting prefetch system...");
            }
            StartupProgress::Complete => {
                println!("  ✓ All services initialized");
            }
        };

        // Initialize all services in one call
        let result = orchestrator
            .initialize_services(&store, &ortho_packages, Some(progress_callback))
            .map_err(CliError::Serve)?;

        // Report mount results
        println!();
        println!("  ✓ zzXEL_ortho → {}", result.mount.mountpoint.display());
        println!(
            "    Sources: {} ({} patches, {} packages)",
            result.mount.source_count,
            result.mount.patch_names.len(),
            result.mount.package_regions.len()
        );
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
        let tui_config = TuiAppConfig {
            orchestrator: &mut orchestrator,
            store: &store,
            shutdown,
            config,
            prefetch_status: Arc::clone(&shared_prefetch_status),
            aircraft_position: aircraft_position.clone(),
            ortho_packages: ortho_packages.clone(),
            airport_icao: args.airport.clone(),
            // Derive X-Plane environment from Custom Scenery path
            xplane_env: config
                .packages
                .custom_scenery_path
                .as_ref()
                .or(config.xplane.scenery_dir.as_ref())
                .and_then(|p| XPlaneEnvironment::from_custom_scenery_path(p).ok()),
        };
        run_tui(tui_config)?
    } else {
        // Fallback to simple text output for non-TTY
        run_headless(&mut orchestrator, shutdown, config.general.update_check)?;
        orchestrator.cancellation()
    };

    // Cancel all systems for cleanup
    cleanup_cancellation.cancel();

    // Get final telemetry before shutdown
    let final_snapshot = orchestrator.telemetry_snapshot();

    // Graceful shutdown (unmounts FUSE, stops cache GC)
    orchestrator.shutdown();

    // Print final session summary (to stdout after TUI exits)
    ui::dashboard::print_session_summary(&final_snapshot);

    println!();
    println!("All packages unmounted. Goodbye!");

    Ok(())
}
