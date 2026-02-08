//! TUI Application module for XEarthLayer CLI.
//!
//! This module contains the TUI (Terminal User Interface) application logic,
//! separated from the command-line argument parsing and service orchestration.
//!
//! # Architecture
//!
//! - `run_tui()` - Interactive TUI application with dashboard and event loop
//! - `run_headless()` - Simple headless mode for non-TTY environments
//! - `TuiAppConfig` - Configuration struct for TUI initialization
//!
//! The `run.rs` command acts as a thin front controller that:
//! 1. Loads and validates configuration
//! 2. Creates the `ServiceOrchestrator`
//! 3. Delegates to `run_tui()` or `run_headless()`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use xearthlayer::aircraft_position::SharedAircraftPosition;
use xearthlayer::config::ConfigFile;
use xearthlayer::manager::{InstalledPackage, LocalPackageStore};
use xearthlayer::prefetch::{PrewarmHandle, SharedPrefetchStatus};
use xearthlayer::service::{PrewarmOrchestrator, ServiceOrchestrator, StartupProgress};
use xearthlayer::xplane::XPlaneEnvironment;

use crate::error::CliError;
use crate::ui::{
    self, Dashboard, DashboardConfig, DashboardEvent, DashboardState, LoadingPhase,
    LoadingProgress, PrewarmProgress,
};

/// Configuration for starting the TUI application.
pub struct TuiAppConfig<'a> {
    /// Service orchestrator for all backend services.
    pub orchestrator: &'a mut ServiceOrchestrator,
    /// Local package store for package discovery.
    pub store: &'a LocalPackageStore,
    /// Shutdown signal from signal handler.
    pub shutdown: Arc<AtomicBool>,
    /// Configuration file.
    pub config: &'a ConfigFile,
    /// Prefetch status for UI display.
    pub prefetch_status: Arc<SharedPrefetchStatus>,
    /// Unified aircraft position provider (APT module).
    pub aircraft_position: SharedAircraftPosition,
    /// Ortho packages to mount.
    pub ortho_packages: Vec<&'a InstalledPackage>,
    /// Airport ICAO code for prewarm (if specified).
    pub airport_icao: Option<String>,
    /// X-Plane environment for apt.dat lookup and resource paths.
    pub xplane_env: Option<XPlaneEnvironment>,
}

/// Run the TUI application with dashboard.
///
/// This function:
/// 1. Starts the TUI immediately in Loading state
/// 2. Initializes all services via ServiceOrchestrator with progress updates
/// 3. When complete, optionally prewarm cache around airport
/// 4. Enters main event loop for Running state
///
/// Returns the cancellation token for cleanup coordination.
pub fn run_tui(config: TuiAppConfig) -> Result<CancellationToken, CliError> {
    let TuiAppConfig {
        orchestrator,
        store,
        shutdown,
        config: cfg,
        prefetch_status,
        aircraft_position,
        ortho_packages,
        airport_icao,
        xplane_env,
    } = config;

    let dashboard_config = DashboardConfig {
        memory_cache_max: cfg.cache.memory_size,
        disk_cache_max: cfg.cache.disk_size,
        provider_name: cfg.provider.provider_type.clone(),
    };

    // Create initial loading progress
    let loading_progress = LoadingProgress::new(ortho_packages.len());

    // Start dashboard in Loading state
    let initial_state = DashboardState::Loading(loading_progress);
    let mut dashboard = Dashboard::with_state(dashboard_config, shutdown.clone(), initial_state)
        .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?
        .with_prefetch_status(Arc::clone(&prefetch_status))
        .with_aircraft_position(aircraft_position.clone());

    // Draw initial loading screen immediately
    dashboard
        .draw_loading()
        .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;

    // Channel for progress updates from initialization
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();

    // Run initialization in a scoped thread so we can update dashboard
    // Use a Mutex to store the result for thread-safe access
    let init_result: std::sync::Mutex<Option<Result<_, xearthlayer::service::ServiceError>>> =
        std::sync::Mutex::new(None);

    std::thread::scope(|s| {
        // Spawn initialization thread - borrows from outer scope (scoped threads allow this)
        let progress_tx_clone = progress_tx.clone();
        let init_result_ref = &init_result;
        s.spawn(|| {
            // Progress callback needs move to own progress_tx_clone
            let progress_callback = move |progress: StartupProgress| {
                let _ = progress_tx_clone.send(progress);
            };

            let result =
                orchestrator.initialize_services(store, &ortho_packages, Some(progress_callback));
            *init_result_ref.lock().unwrap() = Some(result);
        });

        // Update dashboard while initialization is in progress
        let tick_rate = Duration::from_millis(50);
        loop {
            // Process progress updates
            while let Ok(progress) = progress_rx.try_recv() {
                update_loading_progress(&mut dashboard, &progress);
            }

            // Check if initialization is complete
            if init_result.lock().unwrap().is_some() {
                break;
            }

            // Not ready yet - draw dashboard and wait
            if let Err(e) = dashboard.draw_loading() {
                tracing::warn!(error = %e, "Dashboard draw error during init");
            }
            std::thread::sleep(tick_rate);
        }
    });

    // Check initialization result
    tracing::info!("Checking initialization result");
    let result = init_result
        .into_inner()
        .unwrap()
        .ok_or_else(|| CliError::Config("Initialization failed".to_string()))?;
    let _startup_result = result.map_err(CliError::Serve)?;
    tracing::info!("Initialization completed successfully");

    // Wire in runtime health for TUI display
    if let Some(runtime_health) = orchestrator.runtime_health() {
        dashboard = dashboard.with_runtime_health(runtime_health);
    }

    // Wire in tile progress tracker for active tile display
    if let Some(tile_progress_tracker) = orchestrator.tile_progress_tracker() {
        dashboard = dashboard.with_tile_progress_tracker(tile_progress_tracker);
    }

    // Transition to Running state
    tracing::info!("Transitioning to Running state");
    dashboard.transition_to_running();

    // Debug: Check shutdown flag
    tracing::info!(shutdown = %shutdown.load(Ordering::SeqCst), "Shutdown flag status before event loop");

    // Get runtime handle for prewarm (from the orchestrator's service, not Handle::current())
    let runtime_handle = orchestrator
        .runtime_handle()
        .expect("Runtime handle should be available after initialization");

    // Track prewarm handle (None when not active or complete)
    let mut prewarm_handle: Option<PrewarmHandle> = None;

    // Start prewarm if airport specified
    if let Some(ref icao) = airport_icao {
        let prewarm_config = orchestrator.config().prewarm.clone();
        match PrewarmOrchestrator::start(
            orchestrator,
            icao,
            xplane_env.as_ref(),
            &aircraft_position,
            &prewarm_config,
            &runtime_handle,
        ) {
            Ok(result) => {
                tracing::info!(
                    icao = %icao,
                    airport = %result.airport_name,
                    tiles = result.tile_count,
                    "Starting prewarm in background"
                );
                prewarm_handle = Some(result.handle);

                // Initialize dashboard prewarm display with actual tile count
                let prewarm_progress = PrewarmProgress::new(icao, result.tile_count);
                dashboard.update_prewarm_progress(prewarm_progress);
            }
            Err(e) => {
                tracing::warn!("Prewarm skipped: {}", e);
            }
        }
    }

    // Main event loop
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();
    tracing::info!("Entering main event loop");

    // Drain any stale terminal events before entering the loop
    // (prevents buffered input from causing immediate exit)
    while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = crossterm::event::read();
        tracing::debug!("Drained stale terminal event");
    }

    loop {
        // Poll for keyboard events
        match dashboard.poll_event() {
            Ok(Some(DashboardEvent::Quit)) => {
                tracing::info!("Received Quit event - exiting loop");
                break;
            }
            Ok(Some(DashboardEvent::Cancel)) => {
                // Cancel prewarm if active
                if let Some(ref handle) = prewarm_handle {
                    if !handle.is_cancelled() {
                        tracing::info!("Prewarm cancelled by user");
                        handle.cancel();
                    }
                }
            }
            Ok(None) => {}
            Err(e) => return Err(CliError::Config(format!("Dashboard error: {}", e))),
        }

        // Update prewarm status from handle (authoritative source)
        if let Some(ref handle) = prewarm_handle {
            let status = handle.status();

            // Update dashboard with current status
            let mut prewarm_progress = dashboard
                .prewarm_status()
                .cloned()
                .unwrap_or_else(|| PrewarmProgress::new(&status.icao, status.total));

            prewarm_progress.tiles_loaded = status.completed + status.cache_hits + status.disk_hits;
            prewarm_progress.total_tiles = status.total;
            prewarm_progress.cache_hits = status.cache_hits;

            // Check if prewarm just completed (transition to complete state)
            if status.is_complete && !prewarm_progress.is_complete() {
                prewarm_progress.mark_complete(status.was_cancelled);
                if status.was_cancelled {
                    tracing::info!(
                        completed = status.completed,
                        failed = status.failed,
                        "Prewarm cancelled"
                    );
                } else {
                    tracing::info!(
                        completed = status.completed,
                        cache_hits = status.cache_hits,
                        disk_hits = status.disk_hits,
                        failed = status.failed,
                        "Prewarm complete"
                    );
                }
                prewarm_handle = None; // Drop handle, stop polling
            }

            dashboard.update_prewarm_progress(prewarm_progress);
        }

        // Clear prewarm status 5 seconds after completion
        if let Some(prewarm) = dashboard.prewarm_status() {
            if let Some(elapsed) = prewarm.time_since_completion() {
                if elapsed >= Duration::from_secs(5) {
                    dashboard.clear_prewarm_status();
                }
            }
        }

        // Update dashboard at tick rate
        if last_tick.elapsed() >= tick_rate {
            let snapshot = orchestrator.telemetry_snapshot();
            match dashboard.draw(&snapshot) {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(error = %e, "Dashboard draw error");
                    return Err(CliError::Config(format!("Dashboard draw error: {}", e)));
                }
            }
            last_tick = Instant::now();
        }

        // Small sleep to prevent busy-waiting
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(orchestrator.cancellation())
}

/// Update dashboard loading progress based on StartupProgress.
fn update_loading_progress(dashboard: &mut Dashboard, progress: &StartupProgress) {
    match progress {
        StartupProgress::ScanningDiskCache => {
            // Update phase to show disk cache scanning
            let loading = LoadingProgress {
                phase: LoadingPhase::ScanningDiskCache,
                current_package: "Disk cache".to_string(),
                ..Default::default()
            };
            dashboard.update_loading_progress(loading);
        }
        StartupProgress::Mounting {
            phase,
            current_source,
            sources_complete,
            sources_total,
            files_scanned,
            using_cache,
        } => {
            let mut loading = LoadingProgress::new(*sources_total);
            loading.phase = match phase {
                xearthlayer::ortho_union::IndexBuildPhase::Discovering => LoadingPhase::Discovering,
                xearthlayer::ortho_union::IndexBuildPhase::CheckingCache => {
                    LoadingPhase::CheckingCache
                }
                xearthlayer::ortho_union::IndexBuildPhase::Scanning => LoadingPhase::Scanning,
                xearthlayer::ortho_union::IndexBuildPhase::Merging => LoadingPhase::Merging,
                xearthlayer::ortho_union::IndexBuildPhase::SavingCache => LoadingPhase::SavingCache,
                xearthlayer::ortho_union::IndexBuildPhase::Complete => LoadingPhase::Complete,
            };
            loading.current_package = current_source.clone().unwrap_or_default();
            loading.packages_scanned = *sources_complete;
            loading.tiles_indexed = *files_scanned;
            loading.using_cache = *using_cache;
            dashboard.update_loading_progress(loading);
        }
        StartupProgress::CreatingOverlay => {
            // Keep current progress, just log
            tracing::debug!("Creating overlay symlinks");
        }
        StartupProgress::StartingTelemetry => {
            tracing::debug!("Starting APT telemetry");
        }
        StartupProgress::BuildingSceneryIndex {
            package_name,
            package_index,
            total_packages,
            tiles_indexed,
            ..
        } => {
            let mut loading = LoadingProgress::new(*total_packages);
            loading.phase = LoadingPhase::Scanning;
            loading.current_package = format!("Indexing: {}", package_name);
            loading.packages_scanned = *package_index;
            loading.tiles_indexed = *tiles_indexed;
            dashboard.update_loading_progress(loading);
        }
        StartupProgress::SceneryIndexComplete {
            total_tiles,
            land_tiles,
            sea_tiles,
        } => {
            tracing::info!(
                total = total_tiles,
                land = land_tiles,
                sea = sea_tiles,
                "Scenery index complete"
            );
        }
        StartupProgress::StartingPrefetch => {
            tracing::debug!("Starting prefetch system");
        }
        StartupProgress::Complete => {
            tracing::info!("All services initialized");
        }
    }
}

/// Run in headless mode (non-TTY environments).
///
/// This is a simple wait loop that displays periodic telemetry stats
/// until the shutdown signal is received.
pub fn run_headless(
    orchestrator: &mut ServiceOrchestrator,
    shutdown: Arc<AtomicBool>,
) -> Result<(), CliError> {
    println!("Start X-Plane to use XEarthLayer scenery.");
    println!("Press Ctrl+C to stop.");
    println!();

    let mut last_telemetry = Instant::now();
    let telemetry_interval = Duration::from_secs(30);

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));

        // Display telemetry every 30 seconds
        if last_telemetry.elapsed() >= telemetry_interval {
            let snapshot = orchestrator.telemetry_snapshot();
            ui::dashboard::print_simple_status(&snapshot);
            last_telemetry = Instant::now();
        }
    }

    Ok(())
}
