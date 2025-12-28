//! Configuration upgrade detection and migration.
//!
//! This module provides functionality to detect when a user's configuration file
//! is missing new settings added in newer versions of XEarthLayer, and to safely
//! upgrade the configuration while preserving user values.
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::config::{config_file_path, analyze_config, upgrade_config};
//!
//! let path = config_file_path();
//! let analysis = analyze_config(&path)?;
//!
//! if analysis.needs_upgrade {
//!     println!("Missing {} settings", analysis.missing_keys.len());
//!     upgrade_config(&path, false)?; // false = not a dry run
//! }
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ini::Ini;

use super::file::{ConfigFile, ConfigFileError};
use super::keys::ConfigKey;

/// List of deprecated configuration keys that should be removed during upgrade.
///
/// When deprecating a setting, add it here in "section.key" format.
/// These will be flagged for removal during config upgrade.
///
/// # Example
///
/// ```ignore
/// pub const DEPRECATED_KEYS: &[&str] = &[
///     "prefetch.old_timeout",
///     "cache.legacy_format",
/// ];
/// ```
pub const DEPRECATED_KEYS: &[&str] = &[
    // Removed in v0.2.9 - ZL12 prefetching simplified to use same radii as ZL14
    "prefetch.enable_zl12",
    "prefetch.zl12_inner_radius_nm",
    "prefetch.zl12_outer_radius_nm",
    // Removed in v0.2.9 - Legacy prefetch settings from old Predictor/Scheduler
    // These were never wired to the current PrefetcherBuilder
    "prefetch.cone_distance_nm",
    "prefetch.radial_radius_nm",
    "prefetch.batch_size",
    "prefetch.max_in_flight",
    // Removed in v0.2.9 - Separate radial/cone outer radii never used from config
    // Use outer_radius_nm instead (applies to both)
    "prefetch.radial_outer_radius_nm",
    "prefetch.cone_outer_radius_nm",
    // Removed in v0.2.9 - Duplicate of cone_angle (cone_angle is the one actually used)
    "prefetch.cone_half_angle",
];

/// Result of analyzing a configuration file for upgrade needs.
#[derive(Debug, Clone)]
pub struct ConfigUpgradeAnalysis {
    /// Keys that exist in current version but are missing from user's config.
    pub missing_keys: Vec<String>,

    /// Keys in user's config that are deprecated and should be removed.
    pub deprecated_keys: Vec<String>,

    /// Keys in user's config that are unrecognized (typos or from very old versions).
    /// These are preserved during upgrade but flagged for user review.
    pub unknown_keys: Vec<String>,

    /// Whether any upgrade action is needed.
    pub needs_upgrade: bool,
}

impl ConfigUpgradeAnalysis {
    /// Create an analysis indicating no upgrade is needed.
    pub fn up_to_date() -> Self {
        Self {
            missing_keys: Vec::new(),
            deprecated_keys: Vec::new(),
            unknown_keys: Vec::new(),
            needs_upgrade: false,
        }
    }

    /// Get a summary message suitable for display to users.
    pub fn summary(&self) -> String {
        if !self.needs_upgrade {
            return "Configuration is up to date.".to_string();
        }

        let mut parts = Vec::new();

        if !self.missing_keys.is_empty() {
            parts.push(format!("{} new setting(s)", self.missing_keys.len()));
        }

        if !self.deprecated_keys.is_empty() {
            parts.push(format!(
                "{} deprecated setting(s)",
                self.deprecated_keys.len()
            ));
        }

        if !self.unknown_keys.is_empty() {
            parts.push(format!("{} obsolete setting(s)", self.unknown_keys.len()));
        }

        if parts.is_empty() {
            "Configuration is up to date.".to_string()
        } else {
            format!("Configuration has {}", parts.join(" and "))
        }
    }
}

/// Result of an upgrade operation.
#[derive(Debug)]
pub struct UpgradeResult {
    /// Path to backup file created (if any).
    pub backup_path: Option<PathBuf>,

    /// Keys that were added with their default values.
    pub added_keys: Vec<String>,

    /// Keys that were removed (deprecated).
    pub removed_keys: Vec<String>,

    /// Whether this was a dry run (no changes made).
    pub dry_run: bool,
}

impl UpgradeResult {
    /// Get a summary message suitable for display to users.
    pub fn summary(&self) -> String {
        if self.dry_run {
            return "[DRY RUN] No changes made.".to_string();
        }

        let mut msg = String::from("Upgrade complete!");

        if let Some(ref backup) = self.backup_path {
            msg.push_str(&format!("\nBackup created: {}", backup.display()));
        }

        msg.push_str(&format!(
            "\nAdded {} setting(s), removed {} deprecated setting(s).",
            self.added_keys.len(),
            self.removed_keys.len()
        ));

        msg
    }
}

/// Analyze a configuration file to determine upgrade needs.
///
/// This function compares the keys present in the user's INI file against
/// the canonical list of keys defined in `ConfigKey::all()`, identifying:
///
/// - **Missing keys**: Settings in the current version but not in the user's config
/// - **Deprecated keys**: Settings that should be removed
/// - **Unknown keys**: Settings in user's config that aren't recognized
///
/// # Arguments
///
/// * `path` - Path to the configuration file to analyze
///
/// # Returns
///
/// A `ConfigUpgradeAnalysis` describing what changes are needed.
///
/// # Errors
///
/// Returns an error if the configuration file exists but cannot be parsed.
pub fn analyze_config(path: &Path) -> Result<ConfigUpgradeAnalysis, ConfigFileError> {
    // If no config file exists, no upgrade needed (defaults will be used)
    if !path.exists() {
        return Ok(ConfigUpgradeAnalysis::up_to_date());
    }

    // Parse raw INI to get actual keys present
    let ini = Ini::load_from_file(path)?;

    // Build set of keys present in user's INI file
    let mut user_keys: HashSet<String> = HashSet::new();
    for (section, props) in ini.iter() {
        let section_name = section.unwrap_or("");
        for (key, _) in props.iter() {
            user_keys.insert(format!("{}.{}", section_name, key));
        }
    }

    // Build set of all valid keys from ConfigKey::all()
    let valid_keys: HashSet<&str> = ConfigKey::all().iter().map(|k| k.name()).collect();

    // Build set of deprecated keys
    let deprecated: HashSet<&str> = DEPRECATED_KEYS.iter().copied().collect();

    // Find missing keys (in valid but not in user)
    let mut missing_keys: Vec<String> = valid_keys
        .iter()
        .filter(|k| !user_keys.contains(**k))
        .map(|k| k.to_string())
        .collect();
    missing_keys.sort();

    // Find deprecated keys (in user and in deprecated list)
    let mut deprecated_keys: Vec<String> = user_keys
        .iter()
        .filter(|k| deprecated.contains(k.as_str()))
        .cloned()
        .collect();
    deprecated_keys.sort();

    // Find unknown keys (in user but not valid and not deprecated)
    let mut unknown_keys: Vec<String> = user_keys
        .iter()
        .filter(|k| !valid_keys.contains(k.as_str()) && !deprecated.contains(k.as_str()))
        .cloned()
        .collect();
    unknown_keys.sort();

    // Upgrade needed if there are missing keys, deprecated keys, OR unknown keys
    // Unknown keys are stale settings from old versions that should be cleaned up
    let needs_upgrade =
        !missing_keys.is_empty() || !deprecated_keys.is_empty() || !unknown_keys.is_empty();

    Ok(ConfigUpgradeAnalysis {
        missing_keys,
        deprecated_keys,
        unknown_keys,
        needs_upgrade,
    })
}

/// Generate a timestamp string for backup filenames.
fn backup_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

/// Upgrade a configuration file in place.
///
/// This function performs the following steps:
///
/// 1. Analyzes the current config for upgrade needs
/// 2. Creates a timestamped backup (unless dry_run)
/// 3. Loads the config (existing values + defaults for missing)
/// 4. Saves the complete config back (regenerates with all settings)
///
/// User values are preserved - only missing settings are filled with defaults.
///
/// # Arguments
///
/// * `path` - Path to the configuration file to upgrade
/// * `dry_run` - If true, analyze only without making changes
///
/// # Returns
///
/// An `UpgradeResult` describing what was done.
///
/// # Errors
///
/// Returns an error if the file cannot be read, parsed, or written.
pub fn upgrade_config(path: &Path, dry_run: bool) -> Result<UpgradeResult, ConfigFileError> {
    let analysis = analyze_config(path)?;

    // If no upgrade needed, return early
    if !analysis.needs_upgrade {
        return Ok(UpgradeResult {
            backup_path: None,
            added_keys: Vec::new(),
            removed_keys: Vec::new(),
            dry_run,
        });
    }

    // For dry run, just return what would be done
    if dry_run {
        return Ok(UpgradeResult {
            backup_path: None,
            added_keys: analysis.missing_keys,
            removed_keys: analysis.deprecated_keys,
            dry_run: true,
        });
    }

    // Create backup with timestamp
    let backup_path = if path.exists() {
        let timestamp = backup_timestamp();
        let backup = path.with_extension(format!("ini.backup.{}", timestamp));
        std::fs::copy(path, &backup)
            .map_err(|e| ConfigFileError::WriteError(format!("Failed to create backup: {}", e)))?;
        Some(backup)
    } else {
        None
    };

    // Load existing config (fills missing values with defaults)
    let config = ConfigFile::load_from(path)?;

    // Save back - this regenerates the complete INI with all settings
    config.save_to(path)?;

    Ok(UpgradeResult {
        backup_path,
        added_keys: analysis.missing_keys,
        removed_keys: analysis.deprecated_keys,
        dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_analyze_nonexistent_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("nonexistent.ini");

        let analysis = analyze_config(&config_path).unwrap();

        assert!(!analysis.needs_upgrade);
        assert!(analysis.missing_keys.is_empty());
        assert!(analysis.deprecated_keys.is_empty());
        assert!(analysis.unknown_keys.is_empty());
    }

    #[test]
    fn test_analyze_empty_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Empty config file
        std::fs::write(&config_path, "").unwrap();

        let analysis = analyze_config(&config_path).unwrap();

        assert!(analysis.needs_upgrade);
        assert!(!analysis.missing_keys.is_empty());
        // Should be missing all keys
        assert_eq!(analysis.missing_keys.len(), ConfigKey::all().len());
    }

    #[test]
    fn test_analyze_partial_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[provider]
type = bing

[cache]
memory_size = 2GB
"#,
        )
        .unwrap();

        let analysis = analyze_config(&config_path).unwrap();

        assert!(analysis.needs_upgrade);
        // Should be missing most keys but not the ones we set
        assert!(!analysis.missing_keys.contains(&"provider.type".to_string()));
        assert!(!analysis
            .missing_keys
            .contains(&"cache.memory_size".to_string()));
        assert!(analysis
            .missing_keys
            .contains(&"texture.format".to_string()));
    }

    #[test]
    fn test_analyze_complete_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Save complete config using ConfigFile
        let config = ConfigFile::default();
        config.save_to(&config_path).unwrap();

        let analysis = analyze_config(&config_path).unwrap();

        assert!(!analysis.needs_upgrade);
        assert!(analysis.missing_keys.is_empty());
    }

    #[test]
    fn test_analyze_unknown_keys() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[provider]
type = bing
unknown_setting = value

[custom]
user_data = test
"#,
        )
        .unwrap();

        let analysis = analyze_config(&config_path).unwrap();

        assert!(analysis
            .unknown_keys
            .contains(&"provider.unknown_setting".to_string()));
        assert!(analysis
            .unknown_keys
            .contains(&"custom.user_data".to_string()));
    }

    #[test]
    fn test_upgrade_preserves_user_values() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[provider]
type = google
google_api_key = my-secret-key
"#,
        )
        .unwrap();

        let result = upgrade_config(&config_path, false).unwrap();

        assert!(!result.dry_run);
        assert!(result.backup_path.is_some());

        // Reload and verify user value preserved
        let upgraded = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(upgraded.provider.provider_type, "google");
        assert_eq!(
            upgraded.provider.google_api_key,
            Some("my-secret-key".to_string())
        );
    }

    #[test]
    fn test_upgrade_dry_run_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        let original = r#"
[provider]
type = bing
"#;
        std::fs::write(&config_path, original).unwrap();

        let result = upgrade_config(&config_path, true).unwrap();

        assert!(result.dry_run);
        assert!(result.backup_path.is_none());
        assert!(!result.added_keys.is_empty());

        // Verify file unchanged
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents, original);
    }

    #[test]
    fn test_upgrade_creates_backup() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(&config_path, "[provider]\ntype = bing\n").unwrap();

        let result = upgrade_config(&config_path, false).unwrap();

        assert!(result.backup_path.is_some());
        assert!(result.backup_path.as_ref().unwrap().exists());
    }

    #[test]
    fn test_upgrade_no_changes_needed() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Save complete config
        let config = ConfigFile::default();
        config.save_to(&config_path).unwrap();

        let result = upgrade_config(&config_path, false).unwrap();

        // No backup should be created when no upgrade needed
        assert!(result.backup_path.is_none());
        assert!(result.added_keys.is_empty());
        assert!(result.removed_keys.is_empty());
    }

    #[test]
    fn test_analysis_summary() {
        let analysis = ConfigUpgradeAnalysis {
            missing_keys: vec!["a".to_string(), "b".to_string()],
            deprecated_keys: vec!["c".to_string()],
            unknown_keys: Vec::new(),
            needs_upgrade: true,
        };

        let summary = analysis.summary();
        assert!(summary.contains("2 new setting(s)"));
        assert!(summary.contains("1 deprecated setting(s)"));
    }

    #[test]
    fn test_upgrade_result_summary_dry_run() {
        let result = UpgradeResult {
            backup_path: None,
            added_keys: vec!["a".to_string()],
            removed_keys: Vec::new(),
            dry_run: true,
        };

        assert!(result.summary().contains("DRY RUN"));
    }
}
