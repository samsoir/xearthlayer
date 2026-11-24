//! Run command - mount all installed ortho packages.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use xearthlayer::config::{format_size, ControlPlaneSettings, TextureConfig};
use xearthlayer::dds::DdsFormat;
use xearthlayer::manager::LocalPackageStore;
use xearthlayer::panic as panic_handler;
use xearthlayer::service::{
    OrchestratorConfig, ServiceConfig, ServiceOrchestrator, StartupProgress,
};
use xearthlayer::xplane::XPlaneEnvironment;
use xearthlayer_package::PackageType;

use super::common::{resolve_provider, ProviderType};
use crate::error::CliError;
use crate::signals::{terminal_gone, Signal, SignalWatch};
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
    pub timeout: Option<u64>,
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
    let chunk_request_timeout_secs =
        resolve_chunk_timeout_secs(args.timeout, config.executor.request_timeout_secs);

    // Build configurations
    // Mipmap levels are left unset so the encoder emits the full chain for the
    // texture size — a truncated chain makes X-Plane clamp sampling and band
    // sloped terrain at grazing angles.
    // BC1 is the only format XEarthLayer encodes (#395).
    let texture_config = TextureConfig::new(DdsFormat::BC1)
        .with_compressor(config.texture.compressor.clone())
        .with_gpu_device(config.texture.gpu_device.clone());
    let dds_format = texture_config.format();

    // Check if we'll use TUI (need to know before creating services)
    let use_tui = atty::is(atty::Stream::Stdout);

    let control_plane_settings = ControlPlaneSettings {
        stall_threshold_secs: config.control_plane.stall_threshold_secs,
        health_check_interval_secs: config.control_plane.health_check_interval_secs,
        semaphore_timeout_secs: config.control_plane.semaphore_timeout_secs,
    };

    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .chunk_request_timeout_secs(chunk_request_timeout_secs)
        .chunk_max_retries(config.executor.max_retries)
        .prefetch_capacity_share(config.prefetch.capacity_share)
        .network_level(config.network.connections)
        .disk_cache_enabled(!args.no_cache)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .cache_dds_disk_ratio(config.cache.dds_disk_ratio)
        .night_enabled(config.night.enabled)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
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

        println!(
            "{}",
            cache_banner(
                config.cache.memory_size,
                config.cache.disk_size,
                args.no_cache
            )
        );
    }

    // Print prefetch status (details printed later after prefetcher starts)
    let prefetch_off =
        prefetch_off_reason(config.prefetch.enabled, args.no_prefetch, args.no_cache);
    if !use_tui {
        if let Some(line) = prefetch_banner(prefetch_off) {
            println!("{line}");
        }
    }
    if !use_tui {
        println!();
    }

    // Create ServiceOrchestrator configuration
    // This consolidates all config needed for backend services
    let mut orch_config = OrchestratorConfig::from_config_file(
        config,
        provider_config.clone(),
        service_config.clone(),
        custom_scenery_path.clone(),
        install_location.clone(),
        use_tui,
    );
    // X-World Pro (#213): preflight contributed the detection when
    // packages.xworld_pro is on; overlay consolidation leaves those continents out.
    // The session's decision, not the file's: `--no-prefetch` and
    // `--no-cache` turn prefetch off (before this, `--no-prefetch` reached
    // only the banner and prefetch ran anyway).
    orch_config.prefetch.enabled = prefetch_off.is_none();
    orch_config.xworld_pro = ctx
        .xworld_pro()
        .map(xearthlayer::xworld_pro::OverlayCoverage::from_detection);

    // Start ServiceOrchestrator - this initializes:
    // - Cache services (CacheLayer with internal GC daemon)
    // - ServiceBuilder with cache bridges
    // - MountManager with load monitor
    // - FUSE request analyzer (if prefetch enabled)
    // - APT aggregator (aircraft position provider)
    // Signals stop the run cleanly (#314): SIGINT as before, and SIGTERM (a
    // system shutdown, `kill`, a service manager) and SIGHUP (the terminal
    // closed), which used to kill the process with the mount still up. Each
    // raises the flag the dashboard and the headless loop poll. The watch
    // starts before anything is mounted: started after the scenery index, a
    // Ctrl+C in the seconds between the mount appearing and the front end
    // starting got the default action, which killed the process and left the
    // volume mounted with nothing serving it. A signal during startup is
    // remembered, and the front end leaves at once and unmounts.
    let shutdown = Arc::new(AtomicBool::new(false));
    let watch = SignalWatch::start_for_shutdown(Arc::clone(&shutdown))
        .map_err(|e| CliError::Config(format!("Failed to set signal handler: {}", e)))?;

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

    let front_end = if use_tui {
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
            xworld_pro: crate::ui::XworldProStatus::from_detection(
                config.packages.xworld_pro,
                ctx.xworld_pro(),
            ),
            startup_warnings: ctx.startup_warnings().len(),
        };
        run_tui(tui_config)
    } else {
        // Fallback to simple text output for non-TTY
        run_headless(&mut orchestrator, shutdown, config.general.update_check)
            .map(|()| orchestrator.cancellation())
    };

    // The mount comes down whatever the front end did. A dashboard error used
    // to return here and leave X-Plane reading a dead mount, and after SIGHUP
    // a dashboard error is only the terminal going away.
    let front_end_error = match front_end {
        Ok(cancellation) => {
            // Cancel all systems for cleanup
            cancellation.cancel();
            None
        }
        Err(error) => {
            orchestrator.cancellation().cancel();
            Some(error)
        }
    };

    // Get final telemetry before shutdown
    let final_snapshot = orchestrator.telemetry_snapshot();

    // Graceful shutdown (unmounts FUSE, stops cache GC)
    orchestrator.shutdown();

    // Print final session summary (to stdout after TUI exits), unless SIGHUP
    // took the terminal: a write to it would fail, and `println!` panics.
    if !terminal_gone() {
        ui::dashboard::print_session_summary(&final_snapshot);

        println!();
        println!("All packages unmounted. Goodbye!");
    }

    outcome(watch.received(), front_end_error)
}

/// How the run ended: a signal names the exit code (130, 143 or 129), a
/// front-end error with no signal behind it is reported, and `q` is success.
fn outcome(received: Option<Signal>, front_end_error: Option<CliError>) -> Result<(), CliError> {
    match (received, front_end_error) {
        (Some(signal), _) => Err(CliError::Stopped(signal)),
        (None, Some(error)) => Err(error),
        (None, None) => Ok(()),
    }
}

/// Why prefetch does not run this session, or `None` when it does.
///
/// `--no-cache` turns it off as well: prefetch fills the disk cache ahead of
/// the aircraft, and with the disk tiers off (#400) its tiles would reach only
/// the memory tier, to be evicted long before they are flown over, while its
/// regions were never confirmed.
fn prefetch_off_reason(
    config_enabled: bool,
    no_prefetch: bool,
    no_cache: bool,
) -> Option<&'static str> {
    if no_prefetch {
        Some("--no-prefetch")
    } else if no_cache {
        Some("--no-cache: prefetched tiles would reach only the memory cache")
    } else if !config_enabled {
        Some("prefetch.enabled = false")
    } else {
        None
    }
}

/// The banner's prefetch line when prefetch is off; nothing when it runs
/// (its details are printed once it starts).
fn prefetch_banner(off_reason: Option<&str>) -> Option<String> {
    off_reason.map(|reason| format!("Prefetch: Disabled ({reason})"))
}

/// The banner's cache line. `--no-cache` turns off the disk tiers only (the
/// memory tier always runs), so the line names what is still on and what is
/// off rather than claiming caching is disabled (#400).
fn cache_banner(memory_size: usize, disk_size: usize, no_cache: bool) -> String {
    if no_cache {
        format!(
            "Cache: {} memory, disk caching off (--no-cache)",
            format_size(memory_size)
        )
    } else {
        format!(
            "Cache: {} memory, {} disk",
            format_size(memory_size),
            format_size(disk_size)
        )
    }
}

/// Resolve the per-chunk HTTP timeout from the `--timeout` flag and config.
///
/// The flag wins when given, otherwise `executor.request_timeout_secs` applies.
///
/// Both inputs were inert before #249: the flag and the key each landed in a
/// `ServiceConfig::download()` that has no production reader, so the effective
/// per-chunk timeout was a compile-time constant that no configuration could
/// reach. Extracted so that resolution rule is assertable.
fn resolve_chunk_timeout_secs(flag: Option<u64>, configured: u64) -> u64 {
    flag.unwrap_or(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_line_names_both_tiers() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(
            cache_banner(2 * gb, 40 * gb, false),
            "Cache: 2 GB memory, 40 GB disk"
        );
    }

    #[test]
    fn prefetch_runs_only_when_the_config_enables_it_and_no_flag_stops_it() {
        assert_eq!(prefetch_off_reason(true, false, false), None);
        assert_eq!(
            prefetch_off_reason(false, false, false),
            Some("prefetch.enabled = false")
        );
        // --no-prefetch reached only the banner, so prefetch kept running.
        assert_eq!(
            prefetch_off_reason(true, true, false),
            Some("--no-prefetch")
        );
    }

    /// Prefetch fills the disk cache ahead of the aircraft. With the disk
    /// tiers off (#400) its tiles reach only the memory tier, where they are
    /// evicted long before they are flown over, and its regions are never
    /// confirmed.
    #[test]
    fn no_cache_turns_prefetch_off() {
        assert!(prefetch_off_reason(true, false, true)
            .is_some_and(|reason| reason.contains("--no-cache")));
    }

    #[test]
    fn the_prefetch_line_names_why_it_is_off() {
        assert_eq!(prefetch_banner(None), None);
        assert_eq!(
            prefetch_banner(Some("--no-prefetch")).as_deref(),
            Some("Prefetch: Disabled (--no-prefetch)")
        );
    }

    /// #400: `--no-cache` turns off the disk tiers only, and the line says so.
    #[test]
    fn the_cache_line_says_disk_caching_is_off() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(
            cache_banner(2 * gb, 40 * gb, true),
            "Cache: 2 GB memory, disk caching off (--no-cache)"
        );
    }

    #[test]
    fn chunk_timeout_falls_back_to_the_configured_value() {
        assert_eq!(resolve_chunk_timeout_secs(None, 45), 45);
    }

    #[test]
    fn chunk_timeout_flag_overrides_the_configured_value() {
        assert_eq!(resolve_chunk_timeout_secs(Some(90), 45), 90);
    }

    #[test]
    fn a_signal_decides_the_exit_code_even_when_the_front_end_failed() {
        // After SIGHUP the dashboard fails on the dead terminal; the signal
        // is the cause, so 129, not a dashboard error.
        let gone = Some(CliError::Config("Dashboard draw error".into()));
        assert!(matches!(
            outcome(Some(Signal::Hangup), gone),
            Err(CliError::Stopped(Signal::Hangup))
        ));
        assert_eq!(
            outcome(Some(Signal::Terminate), None).unwrap_err().report(),
            143
        );
    }

    #[test]
    fn a_front_end_error_with_no_signal_is_reported() {
        let error = Some(CliError::Config("Dashboard draw error".into()));
        assert!(matches!(
            outcome(None, error),
            Err(CliError::Config(message)) if message == "Dashboard draw error"
        ));
    }

    #[test]
    fn quitting_with_q_is_a_plain_success() {
        assert!(outcome(None, None).is_ok());
    }
}
