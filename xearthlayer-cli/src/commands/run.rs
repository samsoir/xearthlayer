//! Run command - mount all installed ortho packages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use xearthlayer::config::{format_size, ConfigFile, DownloadConfig, TextureConfig};
use xearthlayer::log::TracingLogger;
use xearthlayer::manager::{LocalPackageStore, MountManager, ServiceBuilder};
use xearthlayer::package::PackageType;
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
}

/// Run the run command.
pub fn run(args: RunArgs) -> Result<(), CliError> {
    let runner = CliRunner::with_debug(args.debug)?;
    runner.log_startup("run");
    let config = runner.config();

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
    println!();

    // Create mount manager with the custom scenery path as target
    let mut mount_manager = MountManager::with_scenery_path(&custom_scenery_path);

    // Create a logger for all services
    let logger: Arc<dyn xearthlayer::log::Logger> = Arc::new(TracingLogger);

    // Create service builder with shared disk I/O limiter and configured profile
    // This ensures all packages share a single limiter tuned for the storage type
    let service_builder = ServiceBuilder::with_disk_io_profile(
        service_config.clone(),
        provider_config.clone(),
        logger.clone(),
        config.cache.disk_io_profile,
    );

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

    // Set up signal handler for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::SeqCst);
    })
    .map_err(|e| CliError::Config(format!("Failed to set signal handler: {}", e)))?;

    if use_tui {
        // Run with TUI dashboard
        run_with_dashboard(&mut mount_manager, shutdown, config)?;
    } else {
        // Fallback to simple text output for non-TTY
        run_with_simple_output(&mut mount_manager, shutdown)?;
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
) -> Result<(), CliError> {
    use ui::{Dashboard, DashboardConfig, DashboardEvent};

    let dashboard_config = DashboardConfig {
        memory_cache_max: config.cache.memory_size,
        disk_cache_max: config.cache.disk_size,
    };

    let mut dashboard = Dashboard::new(dashboard_config, shutdown.clone())
        .map_err(|e| CliError::Config(format!("Failed to create dashboard: {}", e)))?;

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
