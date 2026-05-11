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
//! - GPU enumeration: [`xearthlayer::system::gpu`]
//! - X-Plane detection: [`xearthlayer::config::detect_scenery_dir`]
//! - Configuration: [`xearthlayer::config::ConfigFile`]

use std::path::{Path, PathBuf};
use std::time::Duration;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use indicatif::{ProgressBar, ProgressStyle};

use xearthlayer::config::DiskIoProfile;
use xearthlayer::config::{
    config_file_path, detect_scenery_dir, format_size, ConfigFile, SceneryDetectionResult, GB, MB,
};
use xearthlayer::system::{
    enumerate_gpus, GpuAdapter, SystemInfo, MIN_DISK_CACHE_BYTES, MIN_MEMORY_CACHE_BYTES,
};

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
    /// Fraction of disk cache budget allocated to encoded DDS tiles
    pub dds_disk_ratio: f64,
    /// Texture compressor backend: "software", "ispc", or "gpu"
    pub texture_compressor: String,
    /// GPU device selector when compressor is "gpu". Ignored otherwise but
    /// always written to keep the config file shape consistent.
    pub texture_gpu_device: String,
}

/// Run the interactive setup wizard.
pub fn run_wizard() -> Result<(), CliError> {
    let theme = ColorfulTheme::default();

    print_banner();

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
                let backup_path = config_path.with_extension("ini.backup");
                std::fs::copy(&config_path, &backup_path)
                    .map_err(|e| CliError::Config(format!("Failed to backup config: {}", e)))?;
                println!("Backup created: {}", backup_path.display());
                println!();
            }
        }
    }

    // Step 1: X-Plane Custom Scenery
    print_step_header("Step 1: X-Plane Custom Scenery");
    let xplane_scenery_dir = step_xplane(&theme)?;

    // Step 2: Package Location
    print_step_header("Step 2: Package Location");
    let package_dir = step_package_location(&theme)?;

    // Step 3: Cache Configuration (directory + budgets + I/O profile)
    print_step_header("Step 3: Cache Configuration");
    let cache_settings = step_cache(&theme)?;

    // Step 4: DDS Encoding (GPU selection if multi-GPU)
    print_step_header("Step 4: DDS Encoding");
    let (texture_compressor, texture_gpu_device) = step_encoding(&theme)?;

    // Build setup config
    let setup_config = SetupConfig {
        xplane_scenery_dir,
        package_dir,
        cache_dir: cache_settings.cache_dir,
        disk_io_profile: cache_settings.disk_io_profile,
        memory_cache_size: cache_settings.memory_cache_size,
        disk_cache_size: cache_settings.disk_cache_size,
        dds_disk_ratio: cache_settings.dds_disk_ratio,
        texture_compressor,
        texture_gpu_device,
    };

    write_config(&setup_config)?;
    print_completion_message();
    Ok(())
}

fn print_banner() {
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
}

fn print_step_header(title: &str) {
    println!();
    println!("{}", style(title).bold().underlined());
    println!();
}

fn print_completion_message() {
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

/// Output of Step 3, the consolidated cache configuration.
struct CacheSettings {
    cache_dir: PathBuf,
    disk_io_profile: DiskIoProfile,
    memory_cache_size: usize,
    disk_cache_size: usize,
    dds_disk_ratio: f64,
}

/// Step 3: Cache directory + disk budget + memory budget + I/O profile.
///
/// This step absorbs what used to be split between "cache location" and
/// "system configuration" — the budgets are derived from system info, so
/// it makes more sense for them to live next to the cache directory choice.
fn step_cache(theme: &ColorfulTheme) -> Result<CacheSettings, CliError> {
    let default_cache_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
        .join("cache");

    // 3a. Cache directory selection
    let cache_dir = prompt_cache_directory(theme, &default_cache_dir)?;

    // 3b. Detect hardware for the chosen cache directory
    let system_info = SystemInfo::detect(&cache_dir);
    println!();
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
    if system_info.cache_path_available_bytes > 0 {
        println!(
            "  Available:      {}",
            style(format_size(system_info.cache_path_available_bytes as usize)).cyan()
        );
    }
    println!();

    // 3c. Disk cache size
    let disk_cache_size = prompt_disk_cache_size(theme, &system_info)?;

    // 3d. DDS disk ratio
    let dds_disk_ratio = prompt_dds_disk_ratio(theme)?;

    // 3e. Memory cache size
    let memory_cache_size = prompt_memory_cache_size(theme, &system_info)?;

    // 3f. I/O profile (default to detected)
    let disk_io_profile = prompt_disk_io_profile(theme, system_info.disk_io_profile)?;

    Ok(CacheSettings {
        cache_dir,
        disk_io_profile,
        memory_cache_size,
        disk_cache_size,
        dds_disk_ratio,
    })
}

fn prompt_cache_directory(theme: &ColorfulTheme, default_path: &Path) -> Result<PathBuf, CliError> {
    println!("Where should XEarthLayer store cached tiles?");
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
        Ok(default_path.to_path_buf())
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

fn prompt_disk_cache_size(
    theme: &ColorfulTheme,
    system_info: &SystemInfo,
) -> Result<usize, CliError> {
    let recommended = system_info.recommended_disk_cache();
    let recommended_gb = recommended / GB;
    let available_gb = (system_info.cache_path_available_bytes / GB as u64) as usize;

    println!("{}", style("Disk cache size:").bold());
    if available_gb > 0 {
        println!(
            "  ℹ  Available: {} GB — default is 25% floored to nearest 10 GB ({} GB)",
            available_gb, recommended_gb
        );
    } else {
        println!(
            "  ℹ  Default is 25% of free space; floor is {} GB",
            MIN_DISK_CACHE_BYTES / GB
        );
    }

    let chosen_gb: usize = Input::with_theme(theme)
        .with_prompt("Disk cache size (GB)")
        .default(recommended_gb)
        .interact_text()
        .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

    if available_gb > 0 && chosen_gb > available_gb {
        println!(
            "  {} {} GB exceeds available space ({} GB) — proceeding anyway",
            style("⚠").yellow(),
            chosen_gb,
            available_gb
        );
    }
    let final_gb = chosen_gb.max(MIN_DISK_CACHE_BYTES / GB);
    if final_gb != chosen_gb {
        println!("  {} Clamped to minimum {} GB", style("ℹ").cyan(), final_gb);
    }
    println!("  {} {} GB", style("✓").green(), final_gb);
    Ok(final_gb * GB)
}

fn prompt_dds_disk_ratio(theme: &ColorfulTheme) -> Result<f64, CliError> {
    const DEFAULT_RATIO: f64 = 0.6;
    println!();
    println!("{}", style("DDS disk ratio:").bold());
    println!("  ℹ  Proportion of disk cache for encoded DDS tiles vs raw image chunks.");
    println!("     Recommended to leave at default unless you know you need to change it.");

    let raw: String = Input::with_theme(theme)
        .with_prompt("DDS disk ratio")
        .default(format!("{}", DEFAULT_RATIO))
        .interact_text()
        .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

    let ratio: f64 = raw
        .parse()
        .map_err(|_| CliError::Config(format!("'{}' is not a number", raw)))?;
    let ratio = ratio.clamp(0.0, 1.0);
    println!("  {} {}", style("✓").green(), ratio);
    Ok(ratio)
}

fn prompt_memory_cache_size(
    theme: &ColorfulTheme,
    system_info: &SystemInfo,
) -> Result<usize, CliError> {
    let recommended = system_info.recommended_memory_cache();
    let recommended_mb = recommended / MB;
    let total_mb = system_info.total_memory / MB;
    let max_mb = (system_info.total_memory / 4) / MB;
    let min_mb = MIN_MEMORY_CACHE_BYTES / MB;

    println!();
    println!("{}", style("Memory cache size:").bold());
    println!(
        "  ℹ  System RAM: {} MB — default is RAM ÷ 12 ({} MB)",
        total_mb, recommended_mb
    );
    println!(
        "     Allowed range: {} MB – {} MB (clamped if outside)",
        min_mb, max_mb
    );

    let chosen_mb: usize = Input::with_theme(theme)
        .with_prompt("Memory cache size (MB)")
        .default(recommended_mb)
        .interact_text()
        .map_err(|e| CliError::Config(format!("Input error: {}", e)))?;

    let clamped_mb = chosen_mb.clamp(min_mb, max_mb.max(min_mb));
    if clamped_mb != chosen_mb {
        println!(
            "  {} Clamped to {} MB (was {} MB)",
            style("ℹ").cyan(),
            clamped_mb,
            chosen_mb
        );
    }
    println!("  {} {} MB", style("✓").green(), clamped_mb);
    Ok(clamped_mb * MB)
}

fn prompt_disk_io_profile(
    theme: &ColorfulTheme,
    detected: DiskIoProfile,
) -> Result<DiskIoProfile, CliError> {
    println!();
    println!("{}", style("Disk I/O profile:").bold());
    println!(
        "  ℹ  Detected: {} (default). Override only if you have a specific reason.",
        style(profile_label(detected)).cyan()
    );

    let profiles = ["auto", "nvme", "ssd", "hdd"];
    let default_idx = match detected {
        DiskIoProfile::Nvme => 1,
        DiskIoProfile::Ssd => 2,
        DiskIoProfile::Hdd => 3,
        DiskIoProfile::Auto => 0,
    };

    let idx = Select::with_theme(theme)
        .with_prompt("I/O profile")
        .items(&profiles)
        .default(default_idx)
        .interact()
        .map_err(|e| CliError::Config(format!("Selection error: {}", e)))?;

    let profile = match idx {
        1 => DiskIoProfile::Nvme,
        2 => DiskIoProfile::Ssd,
        3 => DiskIoProfile::Hdd,
        _ => DiskIoProfile::Auto,
    };
    println!("  {} {}", style("✓").green(), profile_label(profile));
    Ok(profile)
}

fn profile_label(profile: DiskIoProfile) -> &'static str {
    match profile {
        DiskIoProfile::Nvme => "NVMe",
        DiskIoProfile::Ssd => "SSD",
        DiskIoProfile::Hdd => "HDD",
        DiskIoProfile::Auto => "Auto",
    }
}

/// Step 4: DDS encoding backend (and GPU device selection if applicable).
///
/// Returns `(compressor, gpu_device)` ready to write to config. The
/// `gpu_device` is always populated even when ISPC is selected so that
/// switching to GPU later in `config set` doesn't require revisiting
/// this step.
fn step_encoding(theme: &ColorfulTheme) -> Result<(String, String), CliError> {
    let adapters = enumerate_with_spinner();

    if adapters.len() < 2 {
        // Single adapter (or none): GPU selection has no meaningful
        // choice. Stick with the safer ISPC default.
        if adapters.is_empty() {
            println!(
                "{}",
                style("No GPU adapters detected — using ISPC (CPU-based, recommended).").cyan()
            );
        } else {
            println!(
                "{}",
                style(format!(
                    "Single GPU detected ({}) — using ISPC (CPU-based, avoids competing with X-Plane).",
                    adapters[0]
                ))
                .cyan()
            );
        }
        return Ok(("ispc".to_string(), "integrated".to_string()));
    }

    println!("Multiple GPUs detected:");
    for (i, adapter) in adapters.iter().enumerate() {
        println!("  {}. {}", i + 1, adapter);
    }
    println!();
    println!(
        "{}",
        style(
            "⚠  Do NOT select the GPU that X-Plane uses for rendering — this will\n   cause frame drops. If unsure, keep the default (ISPC)."
        )
        .yellow()
    );
    println!();

    let mut items: Vec<String> = vec!["ISPC (CPU, recommended default)".to_string()];
    for adapter in &adapters {
        items.push(format!("GPU: {}", adapter));
    }

    let idx = Select::with_theme(theme)
        .with_prompt("Encoding backend")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| CliError::Config(format!("Selection error: {}", e)))?;

    if idx == 0 {
        println!("  {} ISPC (CPU)", style("✓").green());
        Ok(("ispc".to_string(), "integrated".to_string()))
    } else {
        let adapter = &adapters[idx - 1];
        let gpu_device = adapter.config_value(&adapters);
        println!(
            "  {} GPU: {} (config: {})",
            style("✓").green(),
            adapter,
            gpu_device
        );
        Ok(("gpu".to_string(), gpu_device))
    }
}

/// Enumerate adapters while showing a spinner — wgpu's first call can
/// take 30+ seconds on multi-adapter systems while it opens each
/// driver. Without feedback the wizard appears frozen.
fn enumerate_with_spinner() -> Vec<GpuAdapter> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message("Detecting GPUs (this may take a moment)…");
    spinner.enable_steady_tick(Duration::from_millis(120));

    let adapters = enumerate_gpus();

    spinner.finish_and_clear();
    adapters
}

/// Write the setup configuration to config.ini.
fn write_config(setup: &SetupConfig) -> Result<(), CliError> {
    let mut config = ConfigFile::load().unwrap_or_default();

    if let Some(ref scenery_dir) = setup.xplane_scenery_dir {
        config.xplane.scenery_dir = Some(scenery_dir.clone());
        config.packages.custom_scenery_path = Some(scenery_dir.clone());
    }

    config.packages.install_location = Some(setup.package_dir.clone());

    config.cache.directory = setup.cache_dir.clone();
    config.cache.memory_size = setup.memory_cache_size;
    config.cache.disk_size = setup.disk_cache_size;
    config.cache.dds_disk_ratio = setup.dds_disk_ratio;
    config.cache.disk_io_profile = setup.disk_io_profile;

    config.texture.compressor = setup.texture_compressor.clone();
    config.texture.gpu_device = setup.texture_gpu_device.clone();

    if let Some(parent) = setup.package_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = setup.cache_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    config
        .save()
        .map_err(|e| CliError::Config(format!("Failed to save config: {}", e)))?;

    Ok(())
}
