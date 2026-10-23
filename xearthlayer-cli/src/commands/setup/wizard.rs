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

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use indicatif::{ProgressBar, ProgressStyle};

use xearthlayer::config::{
    config_file_path, detect_scenery_dir, format_size, ConfigFile, SceneryDetectionResult, GB, MB,
};
use xearthlayer::provider::catalog::{find as find_provider, PROVIDERS};
use xearthlayer::system::{
    enumerate_gpus, CompressorKind, DefaultEncoderPolicy, EncoderPolicy, GpuAdapter,
    HardwareProfile, SystemInfo, MIN_DISK_CACHE_BYTES, MIN_MEMORY_CACHE_BYTES,
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
    /// Imagery provider (`provider.type`).
    pub provider_type: String,
    /// Google Maps API key, when the chosen provider needs one.
    pub google_api_key: Option<String>,
    /// Mapbox access token, when the chosen provider needs one.
    pub mapbox_access_token: Option<String>,
    /// Give SimHeaven X-World Pro priority over XEarthLayer overlays.
    pub xworld_pro: bool,
}

/// Output of the provider step.
struct ProviderSelection {
    provider_type: String,
    google_api_key: Option<String>,
    mapbox_access_token: Option<String>,
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

    // Step 3: Imagery Provider. Before the cache step because which imagery a
    // user gets is a more fundamental choice than how much of it to keep.
    print_step_header("Step 3: Imagery Provider");
    let provider = step_provider(&theme)?;

    // Step 4: Cache Configuration (directory + budgets)
    print_step_header("Step 4: Cache Configuration");
    let cache_settings = step_cache(&theme)?;

    // Step 5: DDS Encoding (GPU selection if multi-GPU)
    print_step_header("Step 5: DDS Encoding");
    let (texture_compressor, texture_gpu_device) = step_encoding(&theme)?;

    // Step 6: Third-party Scenery. Last, so the other step numbers are unchanged,
    // and it needs Step 1's Custom Scenery folder.
    print_step_header("Step 6: Third-party Scenery");
    let xworld_pro = super::xworld::step_xworld(&theme, xplane_scenery_dir.as_deref())?;

    // Build setup config
    let setup_config = SetupConfig {
        xplane_scenery_dir,
        package_dir,
        cache_dir: cache_settings.cache_dir,
        memory_cache_size: cache_settings.memory_cache_size,
        disk_cache_size: cache_settings.disk_cache_size,
        dds_disk_ratio: cache_settings.dds_disk_ratio,
        texture_compressor,
        texture_gpu_device,
        provider_type: provider.provider_type,
        google_api_key: provider.google_api_key,
        mapbox_access_token: provider.mapbox_access_token,
        xworld_pro,
    };

    let written = write_config(&setup_config)?;
    print_completion_message(&written);

    // Setup leaves no scenery installed, so offer the way to get some. Without
    // a terminal there is nobody to ask and nothing to draw on.
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let answer = tty
        && Confirm::with_theme(&theme)
            .with_prompt("Open the package manager now to install scenery?")
            .default(true)
            .interact()
            .map_err(|e| CliError::Config(format!("Confirm error: {}", e)))?;
    offer_manager(answer, tty, crate::commands::packages::open_manager)
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

/// Show where XEarthLayer keeps its files.
///
/// New with the platform-native layout. Everything used to live in one
/// directory, so "where is my stuff" never came up. It now spans three or four,
/// and the end of the wizard is where that question gets answered rather than
/// left for the user to discover.
/// Where things live, from the configuration setup just wrote, so the summary
/// shows the user's choices rather than the role defaults. The package
/// location falls back to its default when the configuration leaves it unset.
fn layout_rows(config: &ConfigFile) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("Configuration", config_file_path()),
        ("Tile cache", config.cache.directory.clone()),
        (
            "Scenery packages",
            config
                .packages
                .install_location
                .clone()
                .unwrap_or_else(xearthlayer::paths::packages_dir),
        ),
        ("Log file", config.logging.file.clone()),
    ]
}

fn print_layout(config: &ConfigFile) {
    println!("{}:", style("Where things live").bold());
    for (label, path) in layout_rows(config) {
        println!("  {:<18}{}", label, style(path.display()).cyan());
    }
}

fn print_completion_message(config: &ConfigFile) {
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
    print_layout(config);
    println!();
    println!("{}:", style("Next Steps").bold());
    for step in next_steps() {
        for line in step.lines() {
            // Indented lines are commands to type; the rest is the step's text.
            if line.starts_with("     ") {
                println!("{}", style(line).cyan());
            } else {
                println!("{line}");
            }
        }
        println!();
    }
}

/// The numbered steps shown after setup, one string per step, plain text.
///
/// The first line of a step says what it is for; the indented lines under it
/// are the commands to type.
fn next_steps() -> Vec<String> {
    vec![
        "  1. Open the package manager to install scenery:\n     xearthlayer packages".to_string(),
        "  2. View your configuration:\n     xearthlayer config list".to_string(),
        "  3. Install a scenery package from the command line:\n     xearthlayer packages list\n     xearthlayer packages install <region code from the list>"
            .to_string(),
        "  4. Start XEarthLayer:\n     xearthlayer".to_string(),
    ]
}

/// Open the package manager when the user said yes and there is a terminal
/// to draw it in. The decision is separate from the prompt so it can be tested.
fn offer_manager(
    answer: bool,
    tty: bool,
    launch: impl FnOnce() -> Result<(), CliError>,
) -> Result<(), CliError> {
    if tty && answer {
        launch()
    } else {
        Ok(())
    }
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
            println!(
                "You can configure this later in {}",
                config_file_path().display()
            );
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
    let default_path = xearthlayer::paths::packages_dir();

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

/// Menu entries for the provider step, one per catalog entry, in catalog order.
///
/// Pure so the wording can be asserted: a user has to be able to see from the
/// menu alone whether a provider costs money or needs an account.
fn provider_menu_labels() -> Vec<String> {
    PROVIDERS
        .iter()
        .map(|entry| match entry.credential {
            Some(cred) => format!(
                "{} ({}, needs a {})",
                entry.name,
                entry.summary,
                cred.label()
            ),
            None => format!("{} ({})", entry.name, entry.summary),
        })
        .collect()
}

/// Index to preselect: the configured provider, else the first entry.
///
/// Re-running setup must not move a working installation to a different
/// provider, so whatever is configured comes up selected and Enter keeps it.
/// The catalog guarantees the first entry needs no credentials, which is the
/// right landing place for a value that is unset or no longer recognised.
fn default_provider_index(configured: &str) -> usize {
    find_provider(configured)
        .and_then(|entry| PROVIDERS.iter().position(|p| p.key == entry.key))
        .unwrap_or(0)
}

/// Resolve a credential prompt: empty entry keeps whatever is already set.
///
/// The prompt does not echo, so it cannot show the current value as a default.
/// Accepting an empty entry as "keep" is what lets a user re-run setup without
/// having to find their API key again.
fn resolve_credential(entered: String, existing: Option<&String>) -> Option<String> {
    let trimmed = entered.trim();
    if trimmed.is_empty() {
        return existing.cloned();
    }
    Some(trimmed.to_string())
}

/// Step 3: imagery provider, and its credential if it needs one.
fn step_provider(theme: &ColorfulTheme) -> Result<ProviderSelection, CliError> {
    let existing = ConfigFile::load().unwrap_or_default();

    println!("XEarthLayer streams satellite imagery from one of these sources.");
    println!(
        "{}",
        style("Most need no account. Only Google Maps and Mapbox require credentials.").cyan()
    );
    println!();

    let labels = provider_menu_labels();
    let selection = Select::with_theme(theme)
        .with_prompt("Imagery provider")
        .items(&labels)
        .default(default_provider_index(&existing.provider.provider_type))
        .interact()
        .map_err(|e| CliError::Config(format!("Provider selection failed: {}", e)))?;

    let entry = &PROVIDERS[selection];
    let mut google_api_key = None;
    let mut mapbox_access_token = None;

    if let Some(cred) = entry.credential {
        let already_set = match cred {
            xearthlayer::provider::ProviderCredential::GoogleApiKey => {
                existing.provider.google_api_key.as_ref()
            }
            xearthlayer::provider::ProviderCredential::MapboxAccessToken => {
                existing.provider.mapbox_access_token.as_ref()
            }
        };

        println!();
        if already_set.is_some() {
            println!(
                "{}",
                style(format!(
                    "A {} is already configured. Press Enter to keep it.",
                    cred.label()
                ))
                .cyan()
            );
        }

        // Password rather than Input: the value is a secret, and the terminal
        // may be shared, recorded, or scrolled back through later.
        let entered = Password::with_theme(theme)
            .with_prompt(cred.label())
            .allow_empty_password(true)
            .interact()
            .map_err(|e| CliError::Config(format!("Credential entry failed: {}", e)))?;

        let resolved = resolve_credential(entered, already_set);

        if resolved.is_none() {
            println!();
            println!(
                "{}",
                style(format!(
                    "No {} set. {} will not serve tiles until you run:\n  xearthlayer config set {} <value>",
                    cred.label(),
                    entry.name,
                    cred.config_key()
                ))
                .yellow()
            );
        }

        match cred {
            xearthlayer::provider::ProviderCredential::GoogleApiKey => google_api_key = resolved,
            xearthlayer::provider::ProviderCredential::MapboxAccessToken => {
                mapbox_access_token = resolved
            }
        }
    }

    println!();
    println!("{}", style(format!("Provider: {}", entry.name)).green());

    Ok(ProviderSelection {
        provider_type: entry.key.to_string(),
        google_api_key,
        mapbox_access_token,
    })
}

/// Output of Step 4, the consolidated cache configuration.
struct CacheSettings {
    cache_dir: PathBuf,
    memory_cache_size: usize,
    disk_cache_size: usize,
    dds_disk_ratio: f64,
}

/// Step 4: Cache directory + disk budget + memory budget.
///
/// This step absorbs what used to be split between "cache location" and
/// "system configuration" — the budgets are derived from system info, so
/// it makes more sense for them to live next to the cache directory choice.
fn step_cache(theme: &ColorfulTheme) -> Result<CacheSettings, CliError> {
    // The same default the program itself uses. These disagreed before the
    // resolver existed: the wizard proposed ~/.xearthlayer/cache while
    // ConfigFile::default() used the platform cache directory, so accepting the
    // wizard's suggestion silently produced a different layout than declining
    // it.
    let default_cache_dir = xearthlayer::paths::tile_cache_dir();

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

    Ok(CacheSettings {
        cache_dir,
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
        "  ℹ  System RAM: {} MB — default is RAM ÷ 12, rounded to the nearest GB ({} MB)",
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

/// Step 5: DDS encoding backend (and GPU device selection if applicable).
///
/// Returns `(compressor, gpu_device)` ready to write to config. The
/// `gpu_device` is always populated even when ISPC is selected so that
/// switching to GPU later in `config set` doesn't require revisiting
/// this step.
///
/// The choice goes through the [`EncoderPolicy`] (#203): with one GPU or
/// none there is nothing to offer, because that GPU is the one X-Plane
/// renders with, and the policy's default (ISPC) stands.
fn step_encoding(theme: &ColorfulTheme) -> Result<(String, String), CliError> {
    let hw = HardwareProfile::new(enumerate_with_spinner());
    let policy = DefaultEncoderPolicy;
    let candidates = hw.encoding_candidates();

    if candidates.is_empty() {
        match hw.adapters.first() {
            None => println!(
                "{}",
                style("No GPU adapters detected — using ISPC (CPU-based, recommended).").cyan()
            ),
            Some(only) => println!(
                "{}",
                style(format!(
                    "Single GPU detected ({only}) — using ISPC (CPU-based, avoids competing with X-Plane)."
                ))
                .cyan()
            ),
        }
        let decision = policy.select(&hw, None);
        return Ok((
            decision.backend.config_value().to_string(),
            "integrated".to_string(),
        ));
    }

    println!("Multiple GPUs detected:");
    for (i, adapter) in candidates.iter().enumerate() {
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
    for adapter in candidates {
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
        let decision = policy.select(&hw, Some(CompressorKind::Ispc));
        Ok((
            decision.backend.config_value().to_string(),
            "integrated".to_string(),
        ))
    } else {
        let adapter = &candidates[idx - 1];
        let gpu_device = adapter.config_value(&hw.adapters);
        let decision = policy.select(&hw, Some(CompressorKind::Gpu));
        if let Some(warning) = &decision.warning {
            println!("{}", style(format!("⚠  {warning}")).yellow());
        }
        println!(
            "  {} GPU: {} (config: {})",
            style("✓").green(),
            adapter,
            gpu_device
        );
        Ok((decision.backend.config_value().to_string(), gpu_device))
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
/// Write the setup choices into the configuration and return what was saved.
fn write_config(setup: &SetupConfig) -> Result<ConfigFile, CliError> {
    let mut config = ConfigFile::load().unwrap_or_default();

    if let Some(ref scenery_dir) = setup.xplane_scenery_dir {
        config.xplane.scenery_dir = Some(scenery_dir.clone());
        config.packages.custom_scenery_path = Some(scenery_dir.clone());
    }

    config.packages.install_location = Some(setup.package_dir.clone());
    config.packages.xworld_pro = setup.xworld_pro;

    config.cache.directory = setup.cache_dir.clone();
    config.cache.memory_size = setup.memory_cache_size;
    config.cache.disk_size = setup.disk_cache_size;
    config.cache.dds_disk_ratio = setup.dds_disk_ratio;

    config.texture.compressor = setup.texture_compressor.clone();
    config.texture.gpu_device = setup.texture_gpu_device.clone();

    config.provider.provider_type = setup.provider_type.clone();
    // Written only when present, so choosing a credential-free provider does
    // not discard a key the user may still want when switching back.
    if setup.google_api_key.is_some() {
        config.provider.google_api_key = setup.google_api_key.clone();
    }
    if setup.mapbox_access_token.is_some() {
        config.provider.mapbox_access_token = setup.mapbox_access_token.clone();
    }

    if let Some(parent) = setup.package_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = setup.cache_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    config
        .save()
        .map_err(|e| CliError::Config(format!("Failed to save config: {}", e)))?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_menu_offers_every_provider_in_catalog_order() {
        let labels = provider_menu_labels();

        assert_eq!(labels.len(), PROVIDERS.len());
        assert!(
            labels[0].starts_with("Bing Maps"),
            "the wizard must lead with a credential-free option, got {:?}",
            labels[0]
        );
        for (label, entry) in labels.iter().zip(PROVIDERS) {
            assert!(
                label.contains(entry.name),
                "{label} should name {}",
                entry.name
            );
            assert!(
                label.contains(entry.summary),
                "{label} should summarise the provider"
            );
        }
    }

    #[test]
    fn menu_labels_mark_the_providers_that_need_credentials() {
        // A first-time user has to be able to see the cost before choosing.
        for (label, entry) in provider_menu_labels().iter().zip(PROVIDERS) {
            if let Some(cred) = entry.credential {
                assert!(
                    label.contains(cred.label()),
                    "{label} should say it needs a {}",
                    cred.label()
                );
            }
        }
    }

    #[test]
    fn default_selection_is_the_configured_provider() {
        // Re-running setup must not silently move a working install to another
        // provider: the current value is preselected so Enter keeps it.
        let google = PROVIDERS.iter().position(|p| p.key == "google").unwrap();
        assert_eq!(default_provider_index("google"), google);
        assert_eq!(default_provider_index("GOOGLE"), google);
    }

    #[test]
    fn default_selection_falls_back_to_the_first_free_provider() {
        // An unset or unrecognised value lands on the first entry, which the
        // catalog guarantees needs no credentials.
        assert_eq!(default_provider_index(""), 0);
        assert_eq!(default_provider_index("nonsuch"), 0);
        assert!(PROVIDERS[0].is_free());
    }

    #[test]
    fn an_empty_credential_entry_keeps_the_existing_one() {
        let existing = Some("existing-key".to_string());

        assert_eq!(
            resolve_credential(String::new(), existing.as_ref()),
            Some("existing-key".to_string()),
            "pressing Enter must not wipe a working credential"
        );
    }

    #[test]
    fn a_new_credential_entry_replaces_and_is_trimmed() {
        let existing = Some("old".to_string());

        assert_eq!(
            resolve_credential("  new-key \n".to_string(), existing.as_ref()),
            Some("new-key".to_string()),
            "pasted credentials pick up surrounding whitespace"
        );
    }

    #[test]
    fn an_empty_entry_with_nothing_existing_stays_unset() {
        assert_eq!(resolve_credential("   ".to_string(), None), None);
    }

    fn row<'a>(rows: &'a [(&'static str, PathBuf)], label: &str) -> &'a PathBuf {
        &rows.iter().find(|(l, _)| *l == label).unwrap().1
    }

    #[test]
    fn the_layout_shows_the_configured_locations() {
        let mut config = ConfigFile::default();
        config.cache.directory = PathBuf::from("/media/Cache/xearthlayer-cache");
        config.packages.install_location =
            Some(PathBuf::from("/media/FlightSim/XEarthLayer Packages"));
        config.logging.file = PathBuf::from("/var/log/xel.log");
        let rows = layout_rows(&config);
        assert_eq!(
            row(&rows, "Tile cache"),
            &PathBuf::from("/media/Cache/xearthlayer-cache")
        );
        assert_eq!(
            row(&rows, "Scenery packages"),
            &PathBuf::from("/media/FlightSim/XEarthLayer Packages")
        );
        assert_eq!(row(&rows, "Log file"), &PathBuf::from("/var/log/xel.log"));
        assert_eq!(row(&rows, "Configuration"), &config_file_path());
    }

    #[test]
    fn an_unset_package_location_shows_the_default() {
        let mut config = ConfigFile::default();
        config.packages.install_location = None;
        assert_eq!(
            row(&layout_rows(&config), "Scenery packages"),
            &xearthlayer::paths::packages_dir()
        );
    }

    #[test]
    fn the_manager_opens_only_for_a_yes_at_a_terminal() {
        for (answer, tty, opens) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            let mut launched = false;
            offer_manager(answer, tty, || {
                launched = true;
                Ok(())
            })
            .unwrap();
            assert_eq!(launched, opens, "answer {answer}, tty {tty}");
        }
    }

    #[test]
    fn a_failing_manager_reports_its_error() {
        let result = offer_manager(true, true, || Err(CliError::Config("boom".to_string())));

        assert!(matches!(result, Err(CliError::Config(ref m)) if m == "boom"));
    }

    #[test]
    fn the_next_steps_lead_with_the_package_manager() {
        let steps = next_steps();

        assert!(
            steps[0].contains("xearthlayer packages"),
            "the first step should open the package manager, got {:?}",
            steps[0]
        );
        assert!(
            steps.iter().any(|s| s.contains("packages install")),
            "the install command should still be listed"
        );
    }
}
