//! XEarthLayer desktop app (Tauri).
//!
//! The same functionality as the `xearthlayer` CLI and its dashboards, for
//! people who do not use a terminal. The window is a React frontend
//! (`xearthlayer-gui/src`); everything it does goes through the commands
//! registered here, which call the core library directly.
//!
//! # Modules
//!
//! - [`service`][]: start and stop the streaming service (`xearthlayer run`)
//!   and report its status and telemetry (the dashboard)
//! - [`packages`][]: installed packages, the library, install / update / remove
//!   jobs (`xearthlayer packages` and the package manager TUI)
//! - [`settings`][]: configuration (`xearthlayer config`, `init`)
//! - [`setup`][]: first-time setup (`xearthlayer setup`)
//! - [`maintenance`][]: cache, scenery index, patches, diagnostics
//! - [`environment`][]: the paths every command resolves from the configuration

// No console window behind the app on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod environment;
mod maintenance;
mod packages;
mod service;
mod settings;
mod setup;
mod views;

use std::sync::Arc;

use tauri::Manager;
use xearthlayer::logging::{init_logging_full, LogFileMode, LoggingGuard};

use crate::packages::PackageOps;
use crate::service::ServiceController;

/// Everything the commands share.
pub struct AppState {
    pub service: Arc<ServiceController>,
    pub packages: Arc<PackageOps>,
}

/// The GUI's own log, appended: `xearthlayer-gui.log` beside the package log
/// in the state directory. Only `xearthlayer run` writes the flight log (#320).
fn gui_log_file() -> std::path::PathBuf {
    xearthlayer::paths::packages_log_file().with_file_name("xearthlayer-gui.log")
}

fn init_logging() -> Option<LoggingGuard> {
    let path = gui_log_file();
    let dir = path.parent()?.to_string_lossy().into_owned();
    let file = path.file_name()?.to_string_lossy().into_owned();
    let debug = std::env::var_os("XEARTHLAYER_DEBUG").is_some();
    match init_logging_full(&dir, &file, LogFileMode::Append, false, debug, false) {
        Ok(guard) => {
            tracing::info!(version = xearthlayer::VERSION, "GUI session started");
            Some(guard)
        }
        Err(e) => {
            eprintln!("warning: failed to initialize logging ({e}); continuing without log file");
            None
        }
    }
}

/// SIGINT, SIGTERM and SIGHUP close the app the same way the window does, so
/// the mount comes down instead of being left behind with nothing serving it.
fn watch_signals(app: tauri::AppHandle) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    match signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) {
        Ok(mut signals) => {
            std::thread::spawn(move || {
                if let Some(signal) = signals.forever().next() {
                    tracing::info!(signal, "Signal received, closing");
                    app.exit(0);
                }
            });
        }
        Err(e) => tracing::warn!(error = %e, "Signal handlers not installed"),
    }
}

fn main() {
    // Before anything allocates in earnest and before any thread exists; see
    // `xearthlayer-cli/src/main.rs` for why (#227, #358).
    xearthlayer::metrics::configure_allocator();
    xearthlayer::system::disown_display();
    xearthlayer::panic::init();

    let _logging = init_logging();

    let state = AppState {
        service: Arc::new(ServiceController::default()),
        packages: Arc::new(PackageOps::default()),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(|app| {
            watch_signals(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // General
            settings::app_info,
            // Service (run + dashboard)
            service::service_start,
            service::service_stop,
            service::service_status,
            service::service_cancel_prewarm,
            // Packages
            packages::packages_installed,
            packages::packages_library,
            packages::packages_install,
            packages::packages_update_all,
            packages::packages_remove,
            packages::package_jobs,
            packages::package_jobs_clear,
            packages::package_cancel,
            packages::packages_rebuild_overlays,
            // Settings
            settings::config_list,
            settings::config_set,
            settings::config_upgrade,
            settings::config_init,
            // Setup
            setup::setup_detect,
            setup::setup_gpus,
            setup::setup_save,
            // Maintenance
            maintenance::cache_stats,
            maintenance::cache_clear,
            maintenance::scenery_index_status,
            maintenance::scenery_index_update,
            maintenance::scenery_index_clear,
            maintenance::patches_list,
            maintenance::diagnostics_report,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the XEarthLayer window");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Unmount before the process goes: the FUSE volume must never
            // outlive the app serving it.
            let state = handle.state::<AppState>();
            state.service.stop_and_wait();
            state.packages.cancel_all();
        }
    });
}
