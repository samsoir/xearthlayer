//! Configuration management CLI commands.
//!
//! Provides `config get`, `config set`, `config list`, `config path`, and
//! `config upgrade` commands for viewing and modifying configuration settings
//! from the command line.

use clap::Subcommand;
use xearthlayer::config::{
    analyze_config, config_file_path, upgrade_config, ConfigFile, ConfigKey,
};

use crate::error::CliError;

/// Config subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Get a configuration value
    Get {
        /// Configuration key in format section.key (e.g., packages.library_url)
        key: String,
    },

    /// Set a configuration value
    Set {
        /// Configuration key in format section.key (e.g., packages.library_url)
        key: String,

        /// Value to set
        value: String,
    },

    /// List all configuration settings
    List,

    /// Show the configuration file path
    Path,

    /// Upgrade configuration file to current version
    ///
    /// Adds missing settings with their default values and removes deprecated
    /// settings. Creates a timestamped backup before modifying.
    Upgrade {
        /// Show what would be changed without modifying the file
        #[arg(long)]
        dry_run: bool,
    },
}

/// Run a config subcommand.
pub fn run(command: ConfigCommands) -> Result<(), CliError> {
    match command {
        ConfigCommands::Get { key } => run_get(&key),
        ConfigCommands::Set { key, value } => run_set(&key, &value),
        ConfigCommands::List => run_list(),
        ConfigCommands::Path => run_path(),
        ConfigCommands::Upgrade { dry_run } => run_upgrade(dry_run),
    }
}

/// Get a configuration value.
fn run_get(key: &str) -> Result<(), CliError> {
    let config_key: ConfigKey = key.parse().map_err(|_| {
        CliError::Config(format!(
            "Unknown configuration key '{}'. Use 'xearthlayer config list' to see available keys.",
            key
        ))
    })?;

    let config = ConfigFile::load().unwrap_or_default();
    let value = config_key.get(&config);

    if value.is_empty() {
        println!("(not set)");
    } else {
        println!("{}", value);
    }

    Ok(())
}

/// Set a configuration value.
fn run_set(key: &str, value: &str) -> Result<(), CliError> {
    let config_key: ConfigKey = key.parse().map_err(|_| {
        CliError::Config(format!(
            "Unknown configuration key '{}'. Use 'xearthlayer config list' to see available keys.",
            key
        ))
    })?;

    let mut config = ConfigFile::load().unwrap_or_default();
    config_key
        .set(&mut config, value)
        .map_err(|e| CliError::Config(e.to_string()))?;
    config.save()?;

    println!("Set {} = {}", config_key.name(), value);

    Ok(())
}

/// List all configuration settings.
fn run_list() -> Result<(), CliError> {
    let config = ConfigFile::load().unwrap_or_default();

    println!("Configuration Settings");
    println!("======================");
    println!();

    let mut current_section = "";

    for key in ConfigKey::all() {
        let section = key.section();

        // Print section header when section changes
        if section != current_section {
            if !current_section.is_empty() {
                println!();
            }
            println!("[{}]", section);
            current_section = section;
        }

        let value = key.get(&config);
        let key_name = key.key_name();

        if value.is_empty() {
            println!("  {} = (not set)", key_name);
        } else {
            println!("  {} = {}", key_name, value);
        }
    }

    Ok(())
}

/// Show the configuration file path.
fn run_path() -> Result<(), CliError> {
    println!("{}", config_file_path().display());
    Ok(())
}

/// Upgrade configuration file to current version.
fn run_upgrade(dry_run: bool) -> Result<(), CliError> {
    let path = config_file_path();

    if !path.exists() {
        println!("No configuration file found at {}", path.display());
        println!("Run 'xearthlayer init' to create one.");
        return Ok(());
    }

    // Analyze first to show what will change
    let analysis = analyze_config(&path).map_err(|e| CliError::Config(e.to_string()))?;

    if !analysis.needs_upgrade {
        println!("Configuration is up to date.");
        return Ok(());
    }

    // Display analysis
    println!("Configuration Upgrade Analysis");
    println!("==============================");
    println!();

    if !analysis.missing_keys.is_empty() {
        println!("Missing settings to add ({}):", analysis.missing_keys.len());
        let default_config = ConfigFile::default();
        for key_name in &analysis.missing_keys {
            // Get default value for display
            if let Ok(config_key) = key_name.parse::<ConfigKey>() {
                let default_value = config_key.get(&default_config);
                println!("  + {} = {}", key_name, default_value);
            } else {
                println!("  + {}", key_name);
            }
        }
        println!();
    }

    if !analysis.deprecated_keys.is_empty() {
        println!(
            "Deprecated settings to remove ({}):",
            analysis.deprecated_keys.len()
        );
        for key in &analysis.deprecated_keys {
            println!("  - {}", key);
        }
        println!();
    }

    if !analysis.unknown_keys.is_empty() {
        println!("Unknown settings (will be preserved):");
        for key in &analysis.unknown_keys {
            println!("  ? {}", key);
        }
        println!();
    }

    if dry_run {
        println!("[DRY RUN] No changes made.");
        return Ok(());
    }

    // Perform upgrade
    let result = upgrade_config(&path, false).map_err(|e| CliError::Config(e.to_string()))?;

    println!("Upgrade complete!");
    if let Some(backup) = result.backup_path {
        println!("Backup created: {}", backup.display());
    }
    println!(
        "Added {} setting(s), removed {} deprecated setting(s).",
        result.added_keys.len(),
        result.removed_keys.len()
    );

    Ok(())
}
