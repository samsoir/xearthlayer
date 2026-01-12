//! Interactive setup wizard implementation.
//!
//! Guides users through initial XEarthLayer configuration using
//! dialoguer prompts for a friendly terminal experience.
//!
//! # Architecture
//!
//! This module handles only the **presentation layer** (TUI prompts and formatting).
//! All business logic lives in the core library:
//!
//! - System detection: [`xearthlayer::system::SystemInfo`]
//! - Recommendations: [`xearthlayer::system::RecommendedSettings`]
//! - X-Plane detection: [`xearthlayer::config::detect_scenery_dir`]
//! - Configuration: [`xearthlayer::config::ConfigFile`]

use std::path::PathBuf;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

use xearthlayer::config::DiskIoProfile;
use xearthlayer::config::{
    config_file_path, detect_scenery_dir, format_size, ConfigFile, SceneryDetectionResult,
};
use xearthlayer::system::SystemInfo;

use crate::error::CliError;

/// Configuration result from the setup wizard.
#[derive(Debug)]
pub struct SetupConfig {
    /// X-Plane Custom Scenery directory
    pub xplane_scenery_dir: Option<PathBuf>,
    /// Package installation directory
    pub package_dir: PathBuf,
    /// Cache directory
    pub cache_dir: PathBuf,
    /// Detected/selected storage profile
    pub disk_io_profile: DiskIoProfile,
    /// Memory cache size in bytes
    pub memory_cache_size: usize,
    /// Disk cache size in bytes
    pub disk_cache_size: usize,
}

/// Run the interactive setup wizard.
pub fn run_wizard() -> Result<(), CliError> {
    let theme = ColorfulTheme::default();

    // Print banner
    println!();
    println!(
        "{}",
        style("╔══════════════════════════════════════════════════╗").cyan()
    );
    println!(
        "{}",
        style("║         XEarthLayer Setup Wizard                 ║").cyan()
    );
    println!(
        "{}",
        style("╚══════════════════════════════════════════════════╝").cyan()
    );
    println!();

    // Check for existing config
    let config_path = config_file_path();
    if config_path.exists() {
        let action = handle_existing_config(&theme)?;
        match action {
            ExistingConfigAction::Cancel => {
                println!("Setup cancelled.");
                return Ok(());
            }
            ExistingConfigAction::Reconfigure => {
                println!("Reconfiguring existing installation...");
                println!();
            }
            ExistingConfigAction::BackupAndReplace => {
                // Create backup
                let backup_path = config_path.with_extension("ini.backup");
                std::fs::copy(&config_path, &backup_path)
                    .map_err(|e| CliError::Config(format!("Failed to backup config: {}", e)))?;
                println!("Backup created: {}", backup_path.display());
                println!();
            }
        }
    }

    // Step 1: X-Plane Custom Scenery
    println!(
        "{}",
        style("Step 1: X-Plane Custom Scenery").bold().underlined()
    );
    println!();
    let xplane_scenery_dir = step_xplane(&theme)?;

    // Step 2: Package Location
    println!();
    println!("{}", style("Step 2: Package Location").bold().underlined());
    println!();
    let package_dir = step_package_location(&theme)?;

    // Step 3: Cache Location
    println!();
    println!("{}", style("Step 3: Cache Location").bold().underlined());
    println!();
    let cache_dir = step_cache_location(&theme)?;

    // Step 4: System Configuration
    println!();
    println!(
        "{}",
        style("Step 4: System Configuration").bold().underlined()
    );
    println!();
    let system_info = SystemInfo::detect(&cache_dir);
    let (memory_cache_size, disk_cache_size, disk_io_profile) =
        step_system_config(&theme, &system_info)?;

    // Build setup config
    let setup_config = SetupConfig {
        xplane_scenery_dir,
        package_dir,
        cache_dir,
        disk_io_profile,
        memory_cache_size,
        disk_cache_size,
    };

    // Write configuration
    write_config(&setup_config)?;

    // Print completion message
    println!();
    println!(
        "{}",
        style("╔══════════════════════════════════════════════════╗").green()
    );
    println!(
        "{}",
        style("║         Configuration Complete!                  ║").green()
    );
    println!(
        "{}",
        style("╚══════════════════════════════════════════════════╝").green()
    );
    println!();
    println!(
        "Config written to: {}",
        style(config_file_path().display()).cyan()
    );
    println!();
    println!("{}:", style("Next Steps").bold());
    println!("  1. View your configuration:");
    println!("     {} config list", style("xearthlayer").cyan());
    println!();
    println!("  2. Install a scenery package:");
    println!("     {} packages list", style("xearthlayer").cyan());
    println!("     {} packages install na", style("xearthlayer").cyan());
    println!();
    println!("  3. Start XEarthLayer:");
    println!("     {}", style("xearthlayer").cyan());
    println!();

    Ok(())
}

/// Action to take when config already exists.
enum ExistingConfigAction {
    Reconfigure,
    BackupAndReplace,
    Cancel,
}

/// Handle existing configuration file.
fn handle_existing_config(theme: &ColorfulTheme) -> Result<ExistingConfigAction, CliError> {
    println!("{}", style("Existing configuration found!").yellow().bold());
    println!("Path: {}", config_file_path().display());
    println!();

    let choices = vec![
        "Reconfigure (update settings, preserve compatible values)",
        "Backup and replace (start fresh)",
        "Cancel (exit without changes)",
    ];

    let selection = Select::with_theme(theme)
        .with_prompt("What would you like to do?")
        .items(&choices)
        .default(0)
        .interact()
        .map_err(|e| CliError::Config(format!("Selection error: {}", e)))?;

    Ok(match selection {
        0 => ExistingConfigAction::Reconfigure,
        1 => ExistingConfigAction::BackupAndReplace,
        _ => ExistingConfigAction::Cancel,
    })
}

/// Step 1: Detect and select X-Plane installation.
fn step_xplane(theme: &ColorfulTheme) -> Result<Option<PathBuf>, CliError> {
    match detect_scenery_dir() {
        SceneryDetectionResult::NotFound => {
            println!(
                "{}",
                style("X-Plane 12 Custom Scenery folder not detected.").yellow()
            );
            println!("You can configure this later in ~/.xearthlayer/config.ini");
            println!();

            let manual = Confirm::with_theme(theme)
                .with_prompt("Would you like to enter the path manually?")
                .default(false)
                .interact()
                .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;

            if manual {
                let path: String = Input::with_theme(theme)
                    .with_prompt("X-Plane Custom Scenery path")
                    .interact_text()
                    .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

                let path = PathBuf::from(path);
                if path.exists() {
                    println!("  {} {}", style("✓").green(), path.display());
                    Ok(Some(path))
                } else {
                    println!("  {} Path does not exist, skipping", style("!").yellow());
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
        SceneryDetectionResult::Single(path) => {
            println!("{}", style("Detected X-Plane 12 Custom Scenery:").green());
            println!("  {}", path.display());
            println!();

            let use_detected = Confirm::with_theme(theme)
                .with_prompt("Use this installation?")
                .default(true)
                .interact()
                .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;

            if use_detected {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        }
        SceneryDetectionResult::Multiple(paths) => {
            println!(
                "{}",
                style("Multiple X-Plane 12 Custom Scenery folders detected:").green()
            );
            println!();

            let items: Vec<String> = paths
                .iter()
                .map(|p| p.display().to_string())
                .chain(std::iter::once("Skip (configure later)".to_string()))
                .collect();

            let selection = Select::with_theme(theme)
                .with_prompt("Select Custom Scenery folder")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|e| CliError::Config(format!("Selection error: {}", e)))?;

            if selection < paths.len() {
                let selected = paths[selection].clone();
                println!("  {} {}", style("✓").green(), selected.display());
                Ok(Some(selected))
            } else {
                Ok(None)
            }
        }
    }
}

/// Step 2: Configure package installation location.
fn step_package_location(theme: &ColorfulTheme) -> Result<PathBuf, CliError> {
    let default_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
        .join("packages");

    println!("Where should XEarthLayer store scenery packages?");
    println!();
    println!("Default: {}", style(default_path.display()).cyan());
    println!();

    let use_default = Confirm::with_theme(theme)
        .with_prompt("Use default location?")
        .default(true)
        .interact()
        .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;

    if use_default {
        println!("  {} {}", style("✓").green(), default_path.display());
        Ok(default_path)
    } else {
        let path: String = Input::with_theme(theme)
            .with_prompt("Package directory path")
            .default(default_path.display().to_string())
            .interact_text()
            .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

        let path = PathBuf::from(path);
        println!("  {} {}", style("✓").green(), path.display());
        Ok(path)
    }
}

/// Step 3: Configure cache location.
fn step_cache_location(theme: &ColorfulTheme) -> Result<PathBuf, CliError> {
    let default_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
        .join("cache");

    println!("Where should XEarthLayer store cached tiles?");
    println!();
    println!("Default: {}", style(default_path.display()).cyan());

    // Detect storage type for default location
    let storage_info = SystemInfo::detect(&default_path);
    println!(
        "Detected storage: {}",
        style(storage_info.storage_display()).cyan()
    );
    println!();

    let use_default = Confirm::with_theme(theme)
        .with_prompt("Use default location?")
        .default(true)
        .interact()
        .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;

    if use_default {
        println!("  {} {}", style("✓").green(), default_path.display());
        Ok(default_path)
    } else {
        let path: String = Input::with_theme(theme)
            .with_prompt("Cache directory path")
            .default(default_path.display().to_string())
            .interact_text()
            .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

        let path = PathBuf::from(path);
        println!("  {} {}", style("✓").green(), path.display());
        Ok(path)
    }
}

/// Step 4: Configure system settings based on detected hardware.
fn step_system_config(
    theme: &ColorfulTheme,
    system_info: &SystemInfo,
) -> Result<(usize, usize, DiskIoProfile), CliError> {
    println!("{}", style("Detected Hardware:").bold());
    println!("  CPU Cores:      {}", style(system_info.cpu_cores).cyan());
    println!(
        "  System Memory:  {}",
        style(system_info.memory_display()).cyan()
    );
    println!(
        "  Cache Storage:  {}",
        style(system_info.storage_display()).cyan()
    );
    println!();

    let recommended_memory = system_info.recommended_memory_cache();
    let recommended_disk = system_info.recommended_disk_cache();
    let recommended_profile = system_info.disk_io_profile;

    println!("{}", style("Recommended Settings:").bold());
    println!(
        "  Memory Cache:   {} (of {} available)",
        style(system_info.recommended_memory_cache_display()).cyan(),
        system_info.memory_display()
    );
    println!(
        "  Disk Cache:     {}",
        style(format_size(recommended_disk)).cyan()
    );
    println!(
        "  I/O Profile:    {} ({})",
        style(system_info.recommended_disk_io_profile()).cyan(),
        match recommended_profile {
            DiskIoProfile::Nvme => "high concurrency",
            DiskIoProfile::Ssd => "moderate concurrency",
            DiskIoProfile::Hdd => "low concurrency",
            DiskIoProfile::Auto => "auto-detect",
        }
    );
    println!();

    let accept_recommended = Confirm::with_theme(theme)
        .with_prompt("Accept recommended settings?")
        .default(true)
        .interact()
        .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;

    if accept_recommended {
        println!("  {} Using recommended settings", style("✓").green());
        Ok((recommended_memory, recommended_disk, recommended_profile))
    } else {
        const GB: usize = 1024 * 1024 * 1024;

        // Custom memory cache
        let memory_gb: usize = Input::with_theme(theme)
            .with_prompt("Memory cache size (GB)")
            .default(recommended_memory / GB)
            .interact_text()
            .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;
        let memory_cache = memory_gb * GB;

        // Custom disk cache
        let disk_gb: usize = Input::with_theme(theme)
            .with_prompt("Disk cache size (GB)")
            .default(recommended_disk / GB)
            .interact_text()
            .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;
        let disk_cache = disk_gb * GB;

        // I/O profile
        let profiles = vec!["auto", "nvme", "ssd", "hdd"];
        let default_idx = match recommended_profile {
            DiskIoProfile::Nvme => 1,
            DiskIoProfile::Ssd => 2,
            DiskIoProfile::Hdd => 3,
            DiskIoProfile::Auto => 0,
        };

        let profile_idx = Select::with_theme(theme)
            .with_prompt("Disk I/O profile")
            .items(&profiles)
            .default(default_idx)
            .interact()
            .map_err(|e| CliError::Config(format!("Selection error: {}", e)))?;

        let profile = match profile_idx {
            1 => DiskIoProfile::Nvme,
            2 => DiskIoProfile::Ssd,
            3 => DiskIoProfile::Hdd,
            _ => DiskIoProfile::Auto,
        };

        println!("  {} Custom settings configured", style("✓").green());
        Ok((memory_cache, disk_cache, profile))
    }
}

/// Write the setup configuration to config.ini.
fn write_config(setup: &SetupConfig) -> Result<(), CliError> {
    // Load existing config or create default
    let mut config = ConfigFile::load().unwrap_or_default();

    // Update X-Plane settings
    if let Some(ref scenery_dir) = setup.xplane_scenery_dir {
        config.xplane.scenery_dir = Some(scenery_dir.clone());
        config.packages.custom_scenery_path = Some(scenery_dir.clone());
    }

    // Update package settings
    config.packages.install_location = Some(setup.package_dir.clone());

    // Update cache settings
    config.cache.directory = setup.cache_dir.clone();
    config.cache.memory_size = setup.memory_cache_size;
    config.cache.disk_size = setup.disk_cache_size;
    config.cache.disk_io_profile = setup.disk_io_profile;

    // Ensure directories exist
    if let Some(parent) = setup.package_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = setup.cache_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Save config
    config
        .save()
        .map_err(|e| CliError::Config(format!("Failed to save config: {}", e)))?;

    Ok(())
}
