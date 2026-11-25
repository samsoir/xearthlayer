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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use xearthlayer::aircraft_position::SharedAircraftPosition;
use xearthlayer::config::ConfigFile;
use xearthlayer::executor::LaneSizing;
use xearthlayer::manager::{InstalledPackage, LocalPackageStore};
use xearthlayer::prefetch::{PrewarmHandle, SharedPrefetchStatus};
use xearthlayer::service::{PrewarmOrchestrator, ServiceOrchestrator, StartupProgress};
use xearthlayer::system::CompressorKind;
use xearthlayer::update::{RemoteUpdateChecker, UpdateChecker, UpdateInfo};
use xearthlayer::xplane::XPlaneEnvironment;

use crate::error::CliError;
use crate::logging_init::{log_file_for, LogTarget};
use crate::stderr_detour::StderrDetour;
use crate::ui::symbols::SymbolPreference;
use crate::ui::{
    self, Dashboard, DashboardConfig, DashboardEvent, DashboardState, EncoderStatus, FeatureStatus,
    LoadingPhase, LoadingProgress, PrefetchLaneConfig, PrewarmProgress, XworldProStatus,
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
    /// X-World Pro support, from the preflight detection.
    pub xworld_pro: XworldProStatus,
    /// Warnings the startup checks printed before the dashboard took the screen.
    pub startup_warnings: usize,
}

/// Point stderr at the flight log for the dashboard's lifetime, or at
/// `/dev/null` when the log cannot be opened. `None`, with a warning in the
/// log, when neither can be: the dashboard still runs, as it always did.
fn detour_stderr(cfg: &ConfigFile) -> Option<StderrDetour> {
    let log = log_file_for(LogTarget::Flight, cfg).map(|(path, _)| path);
    let sinks = log
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(Path::new("/dev/null")));
    for sink in sinks {
        match StderrDetour::to(sink) {
            Ok(detour) => return Some(detour),
            Err(e) => {
                tracing::warn!(sink = %sink.display(), error = %e, "stderr stays on the terminal")
            }
        }
    }
    None
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
        xworld_pro,
        startup_warnings,
    } = config;

    let dds_disk_max = (cfg.cache.disk_size as f64 * cfg.cache.dds_disk_ratio) as usize;
    let chunk_disk_max = cfg.cache.disk_size.saturating_sub(dds_disk_max);
    let dashboard_config = DashboardConfig {
        memory_cache_max: cfg.cache.memory_size,
        dds_disk_cache_max: dds_disk_max,
        chunk_disk_cache_max: chunk_disk_max,
        disk_caching: orchestrator.config().disk_cache_enabled(),
        provider_name: cfg.provider.provider_type.clone(),
        // Same derivation the runtime builder uses, so the group the panel
        // shows is the group the lane enforces.
        prefetch_lane: PrefetchLaneConfig {
            group: LaneSizing::from_configured_share(cfg.prefetch.capacity_share)
                .prefetch_tile_generation,
            generation_timeout: Duration::from_secs(cfg.generation.timeout),
        },
        symbols: SymbolPreference::from_config(&cfg.tui.symbols),
    };

    // Create initial loading progress
    let loading_progress = LoadingProgress::new(ortho_packages.len());

    // Nothing written to stderr may reach the screen while the dashboard owns
    // it (#358): C libraries write there past `tracing`, and a panic message
    // would be lost with the alternate screen. Declared before the dashboard
    // so it is restored after the terminal is.
    let _stderr = detour_stderr(cfg);

    // Start dashboard in Loading state
    let initial_state = DashboardState::Loading(loading_progress);
    let mut dashboard = Dashboard::with_state(dashboard_config, shutdown.clone(), initial_state)
        .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?
        .with_prefetch_status(Arc::clone(&prefetch_status))
        .with_aircraft_position(aircraft_position.clone())
        .with_sim_state(orchestrator.sim_state());

    // Draw initial loading screen immediately
    dashboard
        .draw_loading()
        .map_err(|e| CliError::Config(format!("Dashboard draw error: {}", e)))?;

    // Channel for progress updates from initialization
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();

    // Run initialization in a scoped thread so the loading screen keeps
    // drawing. A panic there is carried to this thread (#418): it unwinds
    // through `run`, whose orchestrator unmounts as it drops.
    let result = std::thread::scope(|s| {
        // Spawn initialization thread - borrows from outer scope (scoped threads allow this)
        let progress_tx_clone = progress_tx.clone();
        let init = s.spawn(|| {
            // Progress callback needs move to own progress_tx_clone
            let progress_callback = move |progress: StartupProgress| {
                let _ = progress_tx_clone.send(progress);
            };

            orchestrator.initialize_services(store, &ortho_packages, Some(progress_callback))
        });

        // Update dashboard while initialization is in progress
        wait_for_init(init, Duration::from_millis(50), || {
            while let Ok(progress) = progress_rx.try_recv() {
                update_loading_progress(&mut dashboard, &progress);
            }
            if let Err(e) = dashboard.draw_loading() {
                tracing::warn!(error = %e, "Dashboard draw error during init");
            }
        })
    });

    // Check initialization result
    tracing::info!("Checking initialization result");
    let _startup_result = result.map_err(CliError::Serve)?;
    tracing::info!("Initialization completed successfully");

    // Wire in runtime health for TUI display
    if let Some(runtime_health) = orchestrator.runtime_health() {
        dashboard = dashboard.with_runtime_health(runtime_health);
    }

    // The GPU encoder's health, read every tick: a GPU that fails during
    // the session turns the encoder line amber (#420).
    if let Some(gpu_health) = orchestrator.gpu_health() {
        dashboard = dashboard.with_gpu_health(gpu_health);
    }

    // Wire in tile progress tracker for active tile display
    if let Some(tile_progress_tracker) = orchestrator.tile_progress_tracker() {
        dashboard = dashboard.with_tile_progress_tracker(tile_progress_tracker);
    }

    // Key features' state for the Status pane: what the service actually
    // selected and resolved, not what the configuration asked for.
    dashboard.set_feature_status(FeatureStatus {
        encoder: match (orchestrator.gpu_adapter(), orchestrator.gpu_warning()) {
            (Some(adapter), Some(_)) => EncoderStatus::GpuSharedWithSim(adapter.clone()),
            (Some(adapter), None) => EncoderStatus::Gpu(adapter.clone()),
            // No adapter means a CPU encoder: a GPU that fails to start
            // stops the service rather than falling back.
            (None, _) => match CompressorKind::parse(&cfg.texture.compressor) {
                Some(CompressorKind::Software) => EncoderStatus::Software,
                _ => EncoderStatus::Ispc,
            },
        },
        night: orchestrator.night_lighting(),
        xworld_pro,
        startup_warnings,
    });

    // Transition to Running state
    tracing::info!("Transitioning to Running state");
    dashboard.transition_to_running();

    // Debug: Check shutdown flag
    tracing::info!(shutdown = %shutdown.load(Ordering::SeqCst), "Shutdown flag status before event loop");

    // Get runtime handle for prewarm (from the orchestrator's service, not Handle::current())
    let runtime_handle = orchestrator
        .runtime_handle()
        .expect("Runtime handle should be available after initialization");

    // Spawn background update check (non-blocking, once per day via disk cache)
    let update_rx = if cfg.general.update_check {
        let (tx, rx) = std::sync::mpsc::channel::<Option<UpdateInfo>>();
        let current_version = xearthlayer::VERSION.to_string();
        runtime_handle.spawn_blocking(move || {
            let checker = RemoteUpdateChecker::new();
            match checker.check(&current_version) {
                Ok(info) => {
                    let _ = tx.send(info);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Update check failed (non-fatal)");
                    let _ = tx.send(None);
                }
            }
        });
        Some(rx)
    } else {
        None
    };

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
            Err(e) if crate::signals::terminal_gone() => {
                // SIGHUP: the terminal is gone, so its errors are expected.
                tracing::info!(error = %e, "Terminal gone, leaving the dashboard");
                break;
            }
            Err(e) => return Err(CliError::Config(format!("Dashboard error: {}", e))),
        }

        // Update prewarm status from handle (authoritative source)
        if let Some(ref handle) = prewarm_handle {
            let status = handle.status();

            // Update dashboard with current status
            let mut prewarm_progress = dashboard
                .prewarm_status()
                .cloned()
                .unwrap_or_else(|| PrewarmProgress::new(&status.icao, status.to_generate));

            prewarm_progress.tiles_loaded = status.completed;
            prewarm_progress.total_tiles = status.to_generate;
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
                        patch_skipped = status.patch_skipped,
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

        // Check for update check result (non-blocking)
        if let Some(ref rx) = update_rx {
            if let Ok(Some(info)) = rx.try_recv() {
                tracing::info!(%info, "Update available");
                dashboard.set_update_info(info);
            }
        }

        // Update dashboard at tick rate
        if last_tick.elapsed() >= tick_rate {
            let snapshot = orchestrator.telemetry_snapshot();
            match dashboard.draw(&snapshot) {
                Ok(()) => {}
                Err(e) if crate::signals::terminal_gone() => {
                    tracing::info!(error = %e, "Terminal gone, leaving the dashboard");
                    break;
                }
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

/// Wait for the startup thread, calling `on_tick` every `tick` until it
/// finishes and once more after it returned, and return what it returned.
///
/// A panic on the startup thread is resumed on this one. Waiting for a result
/// the thread would never deliver left the loading screen up forever with the
/// scenery mounted, deaf to every signal (the shutdown flag is read only once
/// the dashboard runs), and killable only with `SIGKILL`. Carried here, the
/// panic unwinds through `run`, whose orchestrator unmounts as it drops
/// (#418).
fn wait_for_init<T>(
    init: std::thread::ScopedJoinHandle<'_, T>,
    tick: Duration,
    mut on_tick: impl FnMut(),
) -> T {
    while !init.is_finished() {
        on_tick();
        std::thread::sleep(tick);
    }
    match init.join() {
        // The join orders everything the thread sent (its final `Complete`)
        // before this last tick; `is_finished` alone does not.
        Ok(value) => {
            on_tick();
            value
        }
        Err(panic) => std::panic::resume_unwind(panic),
    }
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
    update_check: bool,
) -> Result<(), CliError> {
    // Background update check for headless mode
    if update_check {
        let current_version = xearthlayer::VERSION.to_string();
        std::thread::spawn(move || {
            let checker = RemoteUpdateChecker::new();
            match checker.check(&current_version) {
                Ok(Some(info)) => {
                    tracing::info!(%info, "Update available");
                    eprintln!("{}", info);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "Update check failed (non-fatal)");
                }
            }
        });
    }

    println!("Start X-Plane to use XEarthLayer scenery.");
    println!("Press Ctrl+C to stop.");
    println!();

    let mut last_telemetry = Instant::now();
    let telemetry_interval = Duration::from_secs(30);

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));

        // Display telemetry every 30 seconds, unless SIGHUP took the
        // terminal during the sleep: `println!` panics on the dead pipe.
        if last_telemetry.elapsed() >= telemetry_interval && !crate::signals::terminal_gone() {
            let snapshot = orchestrator.telemetry_snapshot();
            ui::dashboard::print_simple_status(&snapshot);
            last_telemetry = Instant::now();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Run `body` on a thread of its own and give up after `limit`, so a test
    /// of a wait that hangs fails rather than hanging the suite.
    fn within<T: Send + 'static>(limit: Duration, body: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(body());
        });
        outcome
            .recv_timeout(limit)
            .expect("the wait did not end: a panicking startup thread was waited on forever")
    }

    #[test]
    fn the_startup_result_is_returned_once_the_thread_finishes() {
        let ticks = within(Duration::from_secs(10), || {
            let mut ticks = 0;
            let value = std::thread::scope(|s| {
                let init = s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(30));
                    7
                });
                wait_for_init(init, Duration::from_millis(1), || ticks += 1)
            });
            assert_eq!(value, 7);
            ticks
        });

        assert!(ticks > 0, "the loading screen is drawn while startup runs");
    }

    #[test]
    fn progress_sent_just_before_the_thread_finishes_still_reaches_a_tick() {
        // The startup thread sends `Complete` as its last act; a tick after
        // it finishes must still drain it, or "All services initialized"
        // never reaches the log.
        let last = within(Duration::from_secs(10), || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut last = None;
            std::thread::scope(|s| {
                let init = s.spawn(move || tx.send("complete").unwrap());
                // Finished before the wait starts: the loop never ticks.
                while !init.is_finished() {
                    std::thread::yield_now();
                }
                wait_for_init(init, Duration::from_millis(1), || {
                    while let Ok(progress) = rx.try_recv() {
                        last = Some(progress);
                    }
                })
            });
            last
        });

        assert_eq!(last, Some("complete"));
    }

    #[test]
    fn a_panic_on_the_startup_thread_is_carried_to_the_waiting_thread() {
        // #418: the loop waited for a result a panicked thread never sends.
        let caught = within(Duration::from_secs(10), || {
            std::panic::catch_unwind(|| {
                std::thread::scope(|s| {
                    let init = s.spawn(|| -> u32 { panic!("mount failed during startup") });
                    wait_for_init(init, Duration::from_millis(1), || {})
                })
            })
        });

        let payload = caught.expect_err("the panic reaches the waiting thread");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"mount failed during startup")
        );
    }
}
