//! The streaming service: what `xearthlayer run` and its dashboard do.
//!
//! The [`ServiceOrchestrator`] is created, run and shut down on one dedicated
//! thread, which owns it for its whole life; the commands only raise flags
//! and read the [`ServiceStatus`] that thread publishes. Nothing else touches
//! the orchestrator, so it never has to cross a thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::State;
use xearthlayer::aircraft_position::AircraftPositionProvider;
use xearthlayer::config::{ConfigFile, ControlPlaneSettings, TextureConfig};
use xearthlayer::manager::LocalPackageStore;
use xearthlayer::prefetch::{resolve_state, PrewarmHandle};
use xearthlayer::provider::ProviderConfig;
use xearthlayer::service::{
    OrchestratorConfig, PrewarmOrchestrator, ServiceConfig, ServiceOrchestrator, StartupProgress,
};
use xearthlayer::system::CompressorKind;
use xearthlayer::update::{RemoteUpdateChecker, UpdateChecker};
use xearthlayer::xplane::XPlaneEnvironment;
use xearthlayer::xworld_pro::OverlayCoverage;
use xearthlayer_package::PackageType;

use crate::environment::Environment;
use crate::views::TelemetryView;
use crate::AppState;

/// How often the running service publishes telemetry.
const TELEMETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Where the service is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Running,
    Stopping,
    Failed,
}

/// Options the user can give for one run, the GUI's `run` flags.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOptions {
    /// ICAO code to pre-warm around (`run --airport`).
    pub airport: Option<String>,
    /// Disable prefetch for this run (`run --no-prefetch`).
    #[serde(default)]
    pub no_prefetch: bool,
    /// Disable caching for this run (`run --no-cache`).
    #[serde(default)]
    pub no_cache: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupView {
    pub step: String,
    pub detail: Option<String>,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountView {
    pub mountpoint: String,
    pub source_count: usize,
    pub patches: Vec<String>,
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturesView {
    pub provider: String,
    pub encoder: String,
    pub encoder_warning: Option<String>,
    pub night_lighting: String,
    pub prefetch_enabled: bool,
    pub memory_cache_max: u64,
    pub dds_disk_cache_max: u64,
    pub chunk_disk_cache_max: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveView {
    pub prefetch_state: String,
    pub telemetry_connected: bool,
    pub position: Option<(f64, f64)>,
    pub position_source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmView {
    pub icao: String,
    pub airport: String,
    pub completed: usize,
    pub total: usize,
    pub cache_hits: usize,
    pub complete: bool,
    pub cancelled: bool,
}

/// Everything the dashboard shows.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub phase: Phase,
    pub error: Option<String>,
    pub startup: Option<StartupView>,
    pub mount: Option<MountView>,
    pub features: Option<FeaturesView>,
    pub telemetry: Option<TelemetryView>,
    pub live: Option<LiveView>,
    pub prewarm: Option<PrewarmView>,
    pub update_available: Option<String>,
    /// The final figures of the last run, kept after it stops.
    pub last_session: Option<TelemetryView>,
}

/// What the service thread and the commands share.
#[derive(Default)]
struct Shared {
    status: Mutex<ServiceStatus>,
    stop: AtomicBool,
    cancel_prewarm: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut ServiceStatus)) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut status);
    }
}

/// Starts and stops the service thread.
#[derive(Default)]
pub struct ServiceController {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceController {
    pub fn status(&self) -> ServiceStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the service is starting, running or stopping, so the files it
    /// serves must not change underneath it.
    pub fn is_active(&self) -> bool {
        matches!(
            self.status().phase,
            Phase::Starting | Phase::Running | Phase::Stopping
        )
    }

    pub fn start(&self, options: RunOptions) -> Result<(), String> {
        let mut thread = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_active() {
            return Err("XEarthLayer is already running.".into());
        }
        if let Some(previous) = thread.take() {
            let _ = previous.join();
        }
        self.shared.stop.store(false, Ordering::SeqCst);
        self.shared.cancel_prewarm.store(false, Ordering::SeqCst);
        self.shared.update(|s| {
            let last_session = s.last_session.take();
            *s = ServiceStatus {
                phase: Phase::Starting,
                startup: Some(StartupView {
                    step: "Preparing".into(),
                    ..StartupView::default()
                }),
                last_session,
                ..ServiceStatus::default()
            };
        });
        let shared = Arc::clone(&self.shared);
        let handle = std::thread::Builder::new()
            .name("xel-service".into())
            .spawn(move || {
                let result = run_service(&options, &shared);
                shared.update(|s| {
                    s.startup = None;
                    s.live = None;
                    s.prewarm = None;
                    match result {
                        Ok(()) => {
                            s.phase = Phase::Idle;
                            s.error = None;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Service failed");
                            s.phase = Phase::Failed;
                            s.error = Some(e);
                        }
                    }
                });
            })
            .map_err(|e| format!("Failed to start the service thread: {e}"))?;
        *thread = Some(handle);
        Ok(())
    }

    /// Ask the service to stop; it unmounts on its own thread.
    pub fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.update(|s| {
            if matches!(s.phase, Phase::Running | Phase::Starting) {
                s.phase = Phase::Stopping;
            }
        });
    }

    /// Stop and wait until the volume is unmounted. Used when the app exits.
    pub fn stop_and_wait(&self) {
        self.request_stop();
        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    pub fn cancel_prewarm(&self) {
        self.shared.cancel_prewarm.store(true, Ordering::SeqCst);
    }
}

// ============================================================================
// Commands
// ============================================================================

#[tauri::command]
pub fn service_start(
    state: State<'_, AppState>,
    options: Option<RunOptions>,
) -> Result<(), String> {
    if state.packages.is_busy() {
        return Err("A package operation is running. Wait for it to finish first.".into());
    }
    state.service.start(options.unwrap_or_default())
}

#[tauri::command]
pub fn service_stop(state: State<'_, AppState>) {
    state.service.request_stop();
}

#[tauri::command]
pub fn service_status(state: State<'_, AppState>) -> ServiceStatus {
    state.service.status()
}

#[tauri::command]
pub fn service_cancel_prewarm(state: State<'_, AppState>) {
    state.service.cancel_prewarm();
}

// ============================================================================
// The service thread
// ============================================================================

/// The imagery provider the configuration names, as `run` resolves it.
pub fn resolve_provider(config: &ConfigFile) -> Result<ProviderConfig, String> {
    let google_key = config.provider.google_api_key.clone();
    let mapbox_token = config.provider.mapbox_access_token.clone();
    match config.provider.provider_type.to_lowercase().as_str() {
        "apple" => Ok(ProviderConfig::apple()),
        "arcgis" => Ok(ProviderConfig::arcgis()),
        "go2" => Ok(ProviderConfig::go2()),
        "usgs" => Ok(ProviderConfig::usgs()),
        "google" => google_key.map(ProviderConfig::google).ok_or_else(|| {
            "The Google Maps provider needs an API key (provider.google_api_key).".to_string()
        }),
        "mapbox" => mapbox_token.map(ProviderConfig::mapbox).ok_or_else(|| {
            "The MapBox provider needs an access token (provider.mapbox_access_token).".to_string()
        }),
        _ => Ok(ProviderConfig::bing()),
    }
}

fn startup_view(progress: &StartupProgress) -> Option<StartupView> {
    let view = match progress {
        StartupProgress::ScanningDiskCache => StartupView {
            step: "Scanning disk cache".into(),
            ..StartupView::default()
        },
        StartupProgress::Mounting {
            phase,
            current_source,
            sources_complete,
            sources_total,
            files_scanned,
            using_cache,
        } => StartupView {
            step: format!(
                "Mounting scenery ({:?}{})",
                phase,
                if *using_cache { ", cached index" } else { "" }
            ),
            detail: current_source
                .clone()
                .map(|s| format!("{s} — {files_scanned} files")),
            done: *sources_complete,
            total: *sources_total,
        },
        StartupProgress::CreatingOverlay => StartupView {
            step: "Creating overlay links".into(),
            ..StartupView::default()
        },
        StartupProgress::StartingTelemetry => StartupView {
            step: "Connecting to X-Plane".into(),
            ..StartupView::default()
        },
        StartupProgress::BuildingSceneryIndex {
            package_name,
            package_index,
            total_packages,
            tiles_indexed,
            ..
        } => StartupView {
            step: "Building scenery index".into(),
            detail: Some(format!("{package_name} — {tiles_indexed} tiles")),
            done: *package_index,
            total: *total_packages,
        },
        StartupProgress::SceneryIndexComplete { total_tiles, .. } => StartupView {
            step: format!("Scenery index ready ({total_tiles} tiles)"),
            ..StartupView::default()
        },
        StartupProgress::StartingPrefetch => StartupView {
            step: "Starting prefetch".into(),
            ..StartupView::default()
        },
        StartupProgress::Complete => return None,
    };
    Some(view)
}

fn run_service(options: &RunOptions, shared: &Arc<Shared>) -> Result<(), String> {
    let env = Environment::load()?;
    let config = &env.config;
    let install_location = env.install_location.clone();
    let custom_scenery = env.require_custom_scenery()?;
    if !install_location.exists() {
        return Err(format!(
            "No packages are installed yet ({} does not exist). Install a package first.",
            install_location.display()
        ));
    }
    if !custom_scenery.exists() {
        return Err(format!(
            "The Custom Scenery folder {} does not exist.",
            custom_scenery.display()
        ));
    }

    let store = LocalPackageStore::new(&install_location);
    let packages = store.list().map_err(|e| e.to_string())?;
    let ortho_packages: Vec<_> = packages
        .iter()
        .filter(|p| p.package_type() == PackageType::Ortho)
        .collect();
    if ortho_packages.is_empty() {
        return Err(format!(
            "No ortho packages are installed in {}. Install one from the Packages page.",
            install_location.display()
        ));
    }

    let provider_config = resolve_provider(config)?;
    let texture_config = TextureConfig::new(config.texture.format)
        .with_compressor(config.texture.compressor.clone())
        .with_gpu_device(config.texture.gpu_device.clone());
    let service_config = ServiceConfig::builder()
        .texture(texture_config)
        .chunk_request_timeout_secs(config.executor.request_timeout_secs)
        .chunk_max_retries(config.executor.max_retries)
        .prefetch_capacity_share(config.prefetch.capacity_share)
        .network_level(config.network.connections)
        .cache_enabled(!options.no_cache)
        .cache_directory(config.cache.directory.clone())
        .cache_memory_size(config.cache.memory_size)
        .cache_disk_size(config.cache.disk_size)
        .cache_dds_disk_ratio(config.cache.dds_disk_ratio)
        .night_enabled(config.night.enabled)
        .generation_threads(config.generation.threads)
        .generation_timeout(config.generation.timeout)
        .control_plane(ControlPlaneSettings {
            stall_threshold_secs: config.control_plane.stall_threshold_secs,
            health_check_interval_secs: config.control_plane.health_check_interval_secs,
            semaphore_timeout_secs: config.control_plane.semaphore_timeout_secs,
        })
        .quiet_mode(true)
        .build();

    // `run --no-prefetch` is the same as prefetch.enabled = false.
    let mut run_config = config.clone();
    if options.no_prefetch {
        run_config.prefetch.enabled = false;
    }
    let mut orch_config = OrchestratorConfig::from_config_file(
        &run_config,
        provider_config.clone(),
        service_config,
        custom_scenery.clone(),
        install_location.clone(),
        true,
    );
    if config.packages.xworld_pro {
        let detection = xearthlayer::xworld_pro::detect(&custom_scenery);
        orch_config.xworld_pro = Some(OverlayCoverage::from_detection(&detection));
    }

    if shared.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut orchestrator = ServiceOrchestrator::start(orch_config)
        .map_err(|e| format!("Failed to start the service: {e}"))?;

    let progress_shared = Arc::clone(shared);
    let init = orchestrator.initialize_services(
        &store,
        &ortho_packages,
        Some(move |progress: StartupProgress| {
            let view = startup_view(&progress);
            progress_shared.update(|s| {
                if let Some(view) = view {
                    s.startup = Some(view);
                }
            });
        }),
    );
    let init = match init {
        Ok(result) => result,
        Err(e) => {
            orchestrator.shutdown();
            return Err(format!("Failed to start the service: {e}"));
        }
    };

    let dds_disk_max = (config.cache.disk_size as f64 * config.cache.dds_disk_ratio) as u64;
    let features = FeaturesView {
        provider: provider_config.name().to_string(),
        encoder: match orchestrator.gpu_adapter() {
            Some(adapter) => format!("GPU ({adapter})"),
            None => match CompressorKind::parse(&config.texture.compressor) {
                Some(CompressorKind::Software) => "Software".into(),
                _ => "ISPC (CPU)".into(),
            },
        },
        encoder_warning: orchestrator.gpu_warning().map(str::to_string),
        night_lighting: format!("{:?}", orchestrator.night_lighting()),
        prefetch_enabled: run_config.prefetch.enabled,
        memory_cache_max: config.cache.memory_size as u64,
        dds_disk_cache_max: dds_disk_max,
        chunk_disk_cache_max: (config.cache.disk_size as u64).saturating_sub(dds_disk_max),
    };
    let mount = MountView {
        mountpoint: init.mount.mountpoint.display().to_string(),
        source_count: init.mount.source_count,
        patches: init.mount.patch_names.clone(),
        packages: init.mount.package_regions.clone(),
    };
    shared.update(|s| {
        if s.phase == Phase::Starting {
            s.phase = Phase::Running;
        }
        s.startup = None;
        s.mount = Some(mount);
        s.features = Some(features);
    });

    let runtime = orchestrator.runtime_handle();
    if config.general.update_check {
        let update_shared = Arc::clone(shared);
        std::thread::spawn(move || {
            if let Ok(Some(info)) = RemoteUpdateChecker::new().check(xearthlayer::VERSION) {
                update_shared.update(|s| s.update_available = Some(info.to_string()));
            }
        });
    }

    let mut prewarm: Option<PrewarmHandle> = None;
    if let (Some(icao), Some(runtime)) = (
        options
            .airport
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        runtime.as_ref(),
    ) {
        let xplane_env = XPlaneEnvironment::from_custom_scenery_path(&custom_scenery).ok();
        let prewarm_config = orchestrator.config().prewarm.clone();
        let aircraft_position = orchestrator.aircraft_position();
        match PrewarmOrchestrator::start(
            &mut orchestrator,
            icao,
            xplane_env.as_ref(),
            &aircraft_position,
            &prewarm_config,
            runtime,
        ) {
            Ok(result) => {
                shared.update(|s| {
                    s.prewarm = Some(PrewarmView {
                        icao: icao.to_uppercase(),
                        airport: result.airport_name.clone(),
                        total: result.tile_count,
                        ..PrewarmView::default()
                    })
                });
                prewarm = Some(result.handle);
            }
            Err(e) => {
                tracing::warn!("Prewarm skipped: {e}");
                shared.update(|s| s.error = Some(format!("Pre-warm skipped: {e}")));
            }
        }
    }

    let prefetch_status = orchestrator.prefetch_status();
    let aircraft = orchestrator.aircraft_position();
    let mut last = Instant::now() - TELEMETRY_INTERVAL;
    while !shared.stop.load(Ordering::SeqCst) {
        if let Some(handle) = &prewarm {
            if shared.cancel_prewarm.swap(false, Ordering::SeqCst) {
                handle.cancel();
            }
            let status = handle.status();
            shared.update(|s| {
                if let Some(view) = s.prewarm.as_mut() {
                    view.completed = status.completed;
                    view.total = status.to_generate;
                    view.cache_hits = status.cache_hits;
                    view.complete = status.is_complete;
                    view.cancelled = status.was_cancelled;
                }
            });
            if status.is_complete {
                prewarm = None;
            }
        }

        if last.elapsed() >= TELEMETRY_INTERVAL {
            let telemetry = TelemetryView::from(&orchestrator.telemetry_snapshot());
            let prefetch = prefetch_status.snapshot();
            let live = LiveView {
                prefetch_state: resolve_state(prefetch.state_inputs).label(),
                telemetry_connected: aircraft.telemetry_status().to_string() == "Connected",
                position: aircraft.position(),
                position_source: aircraft.position_source().map(|p| format!("{p:?}")),
            };
            shared.update(|s| {
                s.telemetry = Some(telemetry);
                s.live = Some(live);
            });
            last = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    orchestrator.cancellation().cancel();
    let final_snapshot = TelemetryView::from(&orchestrator.telemetry_snapshot());
    orchestrator.shutdown();
    shared.update(|s| {
        s.last_session = Some(final_snapshot);
        s.telemetry = None;
        s.mount = None;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_providers_need_their_credential() {
        let mut config = ConfigFile::default();
        config.provider.provider_type = "google".into();
        config.provider.google_api_key = None;
        assert!(resolve_provider(&config).is_err());

        config.provider.google_api_key = Some("key".into());
        assert!(resolve_provider(&config).is_ok());

        config.provider.provider_type = "mapbox".into();
        config.provider.mapbox_access_token = None;
        assert!(resolve_provider(&config).is_err());
    }

    #[test]
    fn an_unknown_provider_falls_back_to_bing_as_run_does() {
        let mut config = ConfigFile::default();
        config.provider.provider_type = "nonsense".into();
        let provider = resolve_provider(&config).unwrap();
        assert_eq!(provider.name(), ProviderConfig::bing().name());
    }

    #[test]
    fn a_new_controller_is_idle() {
        let controller = ServiceController::default();
        assert_eq!(controller.status().phase, Phase::Idle);
        assert!(!controller.is_active());
    }

    #[test]
    fn startup_completion_clears_the_progress() {
        assert!(startup_view(&StartupProgress::Complete).is_none());
        assert!(startup_view(&StartupProgress::ScanningDiskCache).is_some());
    }
}
