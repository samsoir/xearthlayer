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
    // Removed in v0.2.11 - Circuit breaker now uses milliseconds instead of seconds
    "prefetch.circuit_breaker_open_secs",
    // Removed in v0.3.0 - Pipeline module replaced by executor daemon architecture
    // These settings were parsed but never wired through to the new executor.
    // The executor uses its own internal defaults via ResourcePoolConfig.
    "pipeline.max_http_concurrent",
    "pipeline.max_cpu_concurrent",
    "pipeline.max_prefetch_in_flight",
    "pipeline.request_timeout_secs",
    "pipeline.max_retries",
    "pipeline.retry_base_delay_ms",
    "pipeline.coalesce_channel_capacity",
    // Removed in v0.3.1 - Prewarm now uses tile-based (DSF grid) instead of radius
    // The new grid_size setting aligns prewarm with X-Plane's DSF tile boundaries.
    "prewarm.radius_nm",
    // Removed in v0.4.0 - Legacy prefetch strategies replaced by adaptive prefetch
    // RadialPrefetcher, HeadingAwarePrefetcher, and TileBasedPrefetcher are superseded
    // by the adaptive prefetch system with phase detection (ground/cruise).
    "prefetch.radial_radius",
    "prefetch.tile_based_rows_ahead",
    "prefetch.cone_angle",
    "prefetch.inner_radius_nm",
    "prefetch.outer_radius_nm",
    // Removed in #59 - circuit breaker now uses resource pool utilization, not FUSE rate
    "prefetch.circuit_breaker_threshold",
    // Removed in #58 - boundary-driven prefetch replaces band-based cruise strategy
    "prefetch.trigger_position",
    "prefetch.lead_distance",
    "prefetch.band_width",
    "prefetch.track_stability_threshold",
    "prefetch.turn_threshold",
    "prefetch.track_stability_duration",
    "prefetch.time_budget_margin",
    // Removed in #136 - only adaptive strategy exists, setting was redundant
    "prefetch.strategy",
    // Removed in #58 - per-axis load depth and dynamic window columns
    "prefetch.load_depth",
    "prefetch.default_window_cols",
    // Removed post-#172 - unified prefetch box replaces ring-based ground
    // strategy and removes the max_tiles_per_cycle cap entirely. Scenery
    // window assumptions (default_window_rows, window_lon_extent) are no
    // longer used — the box is specified directly via `box_extent`.
    "prefetch.max_tiles_per_cycle",
    "prefetch.default_window_rows",
    "prefetch.window_lon_extent",
    "prefetch.ground_ring_radius",
    // Removed in #58 - prewarm uses separate grid_rows/grid_cols
    "prewarm.grid_size",
    // Removed in v0.4.7 - never affected I/O concurrency. The executor's disk
    // pool took a flat DEFAULT_DISK_IO_CAPACITY, the profile-aware constructor
    // had no production callers, and the profile-sized StorageConcurrencyLimiter
    // built from this value sat behind #[allow(dead_code)]. Pool sizing is now
    // derived from the pool capacities themselves (#227).
    "cache.disk_io_profile",
    // Removed - online network module deleted from aircraft_position
    "online_network.enabled",
    "online_network.network_type",
    "online_network.pilot_id",
    "online_network.api_url",
    "online_network.poll_interval_secs",
    "online_network.max_stale_secs",
    // Removed - circuit breaker replaced by SimState from X-Plane Web API
    "prefetch.circuit_breaker_open_ms",
    "prefetch.circuit_breaker_half_open_secs",
    // Removed - UDP telemetry replaced by X-Plane Web API polling
    "prefetch.udp_port",
    // Removed - replaced by box_extent and box_max_bias (sliding prefetch box)
    "prefetch.forward_margin",
    "prefetch.behind_margin",
    // Removed in #94 — boundary monitors replaced by sliding prefetch box
    "prefetch.trigger_distance",
    "prefetch.load_depth_lat",
    "prefetch.load_depth_lon",
    // Removed in #160 — [download] section consolidated into [executor]
    // Use executor.request_timeout_secs instead
    "download.timeout",
    // Removed in #160 — [control_plane] section deprecated
    // max_concurrent_jobs moved to [executor]; stall/health settings are
    // hardcoded in the executor watchdog and not configurable
    "control_plane.max_concurrent_jobs",
    "control_plane.stall_threshold_secs",
    "control_plane.health_check_interval_secs",
    "control_plane.semaphore_timeout_secs",
    // Removed in #249 — resource pool sizing is derived from the tuned policy in
    // ResourcePoolConfig::default() and is not user-overridable. The values these
    // keys carried never reached the executor, so no value on disk represents a
    // considered choice; config list even reported a cpu_concurrent computed by a
    // second, contradictory formula that nothing consumed.
    "executor.network_concurrent",
    "executor.cpu_concurrent",
    "executor.disk_io_concurrent",
    // Removed in #160 — internal executor implementation details not useful as config
    "executor.max_concurrent_tasks",
    "executor.job_channel_capacity",
    "executor.request_channel_capacity",
    // Removed in #249 — neither key reached the executor.
    //
    // max_concurrent_jobs was only ever a number the dashboard divided by. It
    // named no limit: job admission is bounded by the executor's task dispatch
    // and the resource pools, neither of which consulted it.
    //
    // retry_base_delay_ms could not be honoured at all. The backoff between
    // chunk retries is `100ms * 2^attempt`, written as a literal in
    // tasks/download_chunks.rs, so there was nothing for the value to reach.
    // Retry count stays configurable through executor.max_retries.
    "executor.max_concurrent_jobs",
    "executor.retry_base_delay_ms",
    // Removed in #277 - they configured a throughput calibrator that nothing
    // fed and whose result nothing read, so no value here ever changed the mode.
    "prefetch.calibration_aggressive_threshold",
    "prefetch.calibration_opportunistic_threshold",
    "prefetch.calibration_sample_duration",
    // Removed in #340 - only on or off was left of the mode: `aggressive`
    // behaved as `opportunistic`, and `disabled` is `prefetch.enabled = false`
    // (see FOLDED_VALUES).
    "prefetch.mode",
];

/// A value a setting no longer accepts, and the value that replaces it.
///
/// `DEPRECATED_KEYS` retires a whole setting. This retires one value of a
/// setting that stays: the parser reads `value` as `replacement`, so an old
/// config keeps loading, and `config upgrade` rewrites it on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeprecatedValue {
    /// Setting in "section.key" format.
    pub key: &'static str,
    /// The retired value, matched case-insensitively.
    pub value: &'static str,
    /// What the retired value is read as.
    pub replacement: &'static str,
}

/// Retired values of current settings.
pub const DEPRECATED_VALUES: &[DeprecatedValue] = &[
    // #334: the rebuilt world is library generation v2. The old URL keeps the
    // old world, frozen, for pre-0.5 clients; the alpha staging channel is
    // replaced by the promoted v2 library.
    DeprecatedValue {
        key: "packages.library_url",
        value: super::defaults::LEGACY_LIBRARY_URL,
        replacement: super::defaults::DEFAULT_LIBRARY_URL,
    },
    DeprecatedValue {
        key: "packages.library_url",
        value: super::defaults::ALPHA_STAGING_LIBRARY_URL,
        replacement: super::defaults::DEFAULT_LIBRARY_URL,
    },
];

/// Look up the retired-value entry for `key = value`, if there is one.
pub fn deprecated_value(key: &str, value: &str) -> Option<DeprecatedValue> {
    let value = value.trim();
    DEPRECATED_VALUES
        .iter()
        .find(|d| d.key == key && d.value.eq_ignore_ascii_case(value))
        .copied()
}

/// A value of a retired setting that must survive the setting: the parser
/// reads `key = value` as `into = becomes`, and `config upgrade` writes it so.
///
/// `DEPRECATED_KEYS` drops a setting. A fold is for the setting whose removal
/// alone would change behaviour, because one of its values switched something
/// that a current setting now owns. A fold only ever applies `becomes`, so it
/// can narrow `into` but never undo an explicit value the other way: with
/// `becomes = "false"`, an `into` already false stays false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldedValue {
    /// The retired setting, in "section.key" format, listed in `DEPRECATED_KEYS`.
    pub key: &'static str,
    /// Its value that still matters, matched case-insensitively.
    pub value: &'static str,
    /// The current setting that carries it now.
    pub into: &'static str,
    /// What `into` is set to.
    pub becomes: &'static str,
}

/// Values of retired settings that move to a current setting.
pub const FOLDED_VALUES: &[FoldedValue] = &[
    // #340: `prefetch.mode = disabled` switched prefetch off by a second route.
    // The new switch is the old `enabled` AND the mode not being `disabled`.
    FoldedValue {
        key: "prefetch.mode",
        value: "disabled",
        into: "prefetch.enabled",
        becomes: "false",
    },
];

impl FoldedValue {
    /// One line for the user: what their file said and what it now says.
    pub fn describe(&self) -> String {
        format!(
            "{} = {} is now {} = {}",
            self.key, self.value, self.into, self.becomes
        )
    }
}

/// Look up the fold for the retired `key = value`, if there is one.
pub fn folded_value(key: &str, value: &str) -> Option<FoldedValue> {
    let value = value.trim();
    FOLDED_VALUES
        .iter()
        .find(|f| f.key == key && f.value.eq_ignore_ascii_case(value))
        .copied()
}

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

    /// Settings in user's config holding a retired value, to be rewritten.
    pub migrated_values: Vec<DeprecatedValue>,

    /// Retired settings whose value moves to a current setting. Each one's key
    /// is also in `deprecated_keys`.
    pub folded_values: Vec<FoldedValue>,

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
            migrated_values: Vec::new(),
            folded_values: Vec::new(),
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

        if !self.migrated_values.is_empty() {
            parts.push(format!("{} retired value(s)", self.migrated_values.len()));
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

    /// Retired values that were rewritten to their replacement.
    pub migrated_values: Vec<DeprecatedValue>,

    /// Retired settings whose value was moved to a current setting.
    pub folded_values: Vec<FoldedValue>,

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
            "\nAdded {} setting(s), removed {} deprecated setting(s), rewrote {} retired value(s).",
            self.added_keys.len(),
            self.removed_keys.len(),
            self.migrated_values.len()
        ));

        for fold in &self.folded_values {
            msg.push_str(&format!("\n{}", fold.describe()));
        }

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
    let mut migrated_values: Vec<DeprecatedValue> = Vec::new();
    let mut folded_values: Vec<FoldedValue> = Vec::new();
    for (section, props) in ini.iter() {
        let section_name = section.unwrap_or("");
        for (key, value) in props.iter() {
            let name = format!("{}.{}", section_name, key);
            migrated_values.extend(deprecated_value(&name, value));
            folded_values.extend(folded_value(&name, value));
            user_keys.insert(name);
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
    let needs_upgrade = !missing_keys.is_empty()
        || !deprecated_keys.is_empty()
        || !unknown_keys.is_empty()
        || !migrated_values.is_empty();

    Ok(ConfigUpgradeAnalysis {
        missing_keys,
        deprecated_keys,
        unknown_keys,
        migrated_values,
        folded_values,
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
            migrated_values: Vec::new(),
            folded_values: Vec::new(),
            dry_run,
        });
    }

    // For dry run, just return what would be done
    if dry_run {
        return Ok(UpgradeResult {
            backup_path: None,
            added_keys: analysis.missing_keys,
            removed_keys: analysis.deprecated_keys,
            migrated_values: analysis.migrated_values,
            folded_values: analysis.folded_values,
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

    // Load existing config (fills missing values with defaults, and the parser
    // reads each retired value as its replacement and applies each fold, so
    // a folded value is written before its retired key disappears)
    let config = ConfigFile::load_from(path)?;

    // Save back - this regenerates the complete INI with all settings
    config.save_to(path)?;

    Ok(UpgradeResult {
        backup_path,
        added_keys: analysis.missing_keys,
        removed_keys: analysis.deprecated_keys,
        migrated_values: analysis.migrated_values,
        folded_values: analysis.folded_values,
        dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::defaults::{
        ALPHA_STAGING_LIBRARY_URL, DEFAULT_LIBRARY_URL, LEGACY_LIBRARY_URL,
    };
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

        // Debug output (always print)
        eprintln!("Missing keys: {:?}", analysis.missing_keys);
        eprintln!("Unknown keys: {:?}", analysis.unknown_keys);
        eprintln!("Deprecated keys: {:?}", analysis.deprecated_keys);
        eprintln!("Needs upgrade: {}", analysis.needs_upgrade);

        assert!(
            !analysis.needs_upgrade,
            "Expected no upgrade needed but missing={:?} unknown={:?} deprecated={:?}",
            analysis.missing_keys, analysis.unknown_keys, analysis.deprecated_keys
        );
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
            migrated_values: vec![
                deprecated_value("packages.library_url", LEGACY_LIBRARY_URL).unwrap()
            ],
            folded_values: Vec::new(),
            needs_upgrade: true,
        };

        let summary = analysis.summary();
        assert!(summary.contains("2 new setting(s)"));
        assert!(summary.contains("1 deprecated setting(s)"));
        assert!(summary.contains("1 retired value(s)"));
    }

    #[test]
    fn test_deprecated_executor_pool_keys() {
        // Removed in #249: pool sizing is derived from the tuned policy in
        // ResourcePoolConfig::default() and is no longer user-overridable.
        assert!(DEPRECATED_KEYS.contains(&"executor.network_concurrent"));
        assert!(DEPRECATED_KEYS.contains(&"executor.cpu_concurrent"));
        assert!(DEPRECATED_KEYS.contains(&"executor.disk_io_concurrent"));
    }

    #[test]
    fn test_analyze_flags_executor_pool_keys_as_deprecated() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[executor]
network_concurrent = 256
cpu_concurrent = 16
disk_io_concurrent = 64
"#,
        )
        .unwrap();

        let analysis = analyze_config(&config_path).unwrap();

        for key in [
            "executor.network_concurrent",
            "executor.cpu_concurrent",
            "executor.disk_io_concurrent",
        ] {
            assert!(
                analysis.deprecated_keys.contains(&key.to_string()),
                "{key} should be reported as deprecated, not silently kept"
            );
            assert!(
                !analysis.unknown_keys.contains(&key.to_string()),
                "{key} is a known removal, not an unrecognised key"
            );
            assert!(
                !analysis.missing_keys.contains(&key.to_string()),
                "{key} must not also be reported as missing - that is the \
                 self-contradiction that made needs_upgrade permanently true \
                 during the disk_io_profile removal"
            );
        }
    }

    #[test]
    fn test_deprecated_boundary_prefetch_keys() {
        assert!(DEPRECATED_KEYS.contains(&"prefetch.trigger_position"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.lead_distance"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.band_width"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.track_stability_threshold"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.turn_threshold"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.track_stability_duration"));
        assert!(DEPRECATED_KEYS.contains(&"prefetch.time_budget_margin"));
    }

    /// `cache.disk_io_profile` never affected I/O concurrency: the pool that
    /// consumed it had no production callers and the limiter built from it sat
    /// behind `#[allow(dead_code)]`. Removing the key must take it out of an
    /// existing user config, not leave it as an unknown setting.
    #[test]
    fn upgrade_removes_the_inert_disk_io_profile_key() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            "[provider]\ntype = bing\n\n[cache]\ndisk_io_profile = nvme\n",
        )
        .unwrap();

        let result = upgrade_config(&config_path, false).unwrap();

        assert!(
            result
                .removed_keys
                .iter()
                .any(|k| k == "cache.disk_io_profile"),
            "upgrade must report the removal, got {:?}",
            result.removed_keys
        );

        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !contents.contains("disk_io_profile"),
            "the key must be gone from the upgraded file"
        );
    }

    #[test]
    fn test_upgrade_result_summary_dry_run() {
        let result = UpgradeResult {
            backup_path: None,
            added_keys: vec!["a".to_string()],
            removed_keys: Vec::new(),
            migrated_values: Vec::new(),
            folded_values: Vec::new(),
            dry_run: true,
        };

        assert!(result.summary().contains("DRY RUN"));
    }

    #[test]
    fn upgrade_summary_names_each_moved_value() {
        let result = UpgradeResult {
            backup_path: None,
            added_keys: Vec::new(),
            removed_keys: vec!["prefetch.mode".to_string()],
            migrated_values: Vec::new(),
            folded_values: FOLDED_VALUES.to_vec(),
            dry_run: false,
        };

        assert!(
            result
                .summary()
                .contains("prefetch.mode = disabled is now prefetch.enabled = false"),
            "{}",
            result.summary()
        );
    }

    #[test]
    fn test_deprecated_calibration_keys() {
        // Removed in #277: nothing fed the calibrator and nothing read its result.
        for key in [
            "prefetch.calibration_aggressive_threshold",
            "prefetch.calibration_opportunistic_threshold",
            "prefetch.calibration_sample_duration",
        ] {
            assert!(DEPRECATED_KEYS.contains(&key), "{key}");
        }
    }

    fn current_config_with_library_url(path: &Path, url: &str) {
        ConfigFile::default().save_to(path).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let line = format!("\nlibrary_url = {DEFAULT_LIBRARY_URL}\n");
        assert_eq!(contents.matches(&line).count(), 1, "one library_url line");
        std::fs::write(
            path,
            contents.replace(&line, &format!("\nlibrary_url = {url}\n")),
        )
        .unwrap();
    }

    #[test]
    fn upgrade_moves_a_retired_library_url_to_the_v2_library() {
        for retired in [LEGACY_LIBRARY_URL, ALPHA_STAGING_LIBRARY_URL] {
            let temp_dir = TempDir::new().unwrap();
            let config_path = temp_dir.path().join("config.ini");
            current_config_with_library_url(&config_path, retired);

            let analysis = analyze_config(&config_path).unwrap();
            assert!(analysis.needs_upgrade, "{retired}");
            assert_eq!(analysis.migrated_values.len(), 1);
            assert_eq!(analysis.migrated_values[0].key, "packages.library_url");

            upgrade_config(&config_path, false).unwrap();
            let contents = std::fs::read_to_string(&config_path).unwrap();
            assert!(contents.contains(&format!("\nlibrary_url = {DEFAULT_LIBRARY_URL}\n")));
            assert!(!contents.contains(retired));
            assert!(!analyze_config(&config_path).unwrap().needs_upgrade);
        }
    }

    /// A complete, current config with `extra` appended to its `[prefetch]`
    /// section and its `enabled` line set to `enabled`.
    fn current_config_with_prefetch(path: &Path, enabled: bool, extra: &str) {
        ConfigFile::default().save_to(path).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let line = "\nenabled = true\n; Web API port";
        assert_eq!(
            contents.matches(line).count(),
            1,
            "one prefetch enabled line"
        );
        let replaced = format!("\nenabled = {enabled}\n{extra}; Web API port");
        std::fs::write(path, contents.replace(line, &replaced)).unwrap();
    }

    /// #340: `prefetch.mode` is retired, and `disabled` was its only value
    /// that did anything `prefetch.enabled` does not.
    #[test]
    fn prefetch_mode_is_retired_and_disabled_folds_into_enabled() {
        assert!(DEPRECATED_KEYS.contains(&"prefetch.mode"));
        assert!(
            DEPRECATED_VALUES.iter().all(|d| d.key != "prefetch.mode"),
            "a retired key has no retired values"
        );
        assert_eq!(
            folded_value("prefetch.mode", " Disabled "),
            Some(FoldedValue {
                key: "prefetch.mode",
                value: "disabled",
                into: "prefetch.enabled",
                becomes: "false",
            })
        );
        for mode in ["opportunistic", "aggressive", "auto", "anything"] {
            assert_eq!(folded_value("prefetch.mode", mode), None, "{mode}");
        }
    }

    /// Every fold retires its key and lands on a current key with a value that
    /// key accepts, so the parser can always apply it.
    #[test]
    fn every_fold_moves_a_retired_key_to_a_valid_value_of_a_current_key() {
        for fold in FOLDED_VALUES {
            assert!(DEPRECATED_KEYS.contains(&fold.key), "{}", fold.key);
            let into: ConfigKey = fold.into.parse().unwrap();
            into.validate(fold.becomes).unwrap();
        }
    }

    #[test]
    fn upgrade_turns_mode_disabled_into_enabled_false() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        current_config_with_prefetch(&config_path, true, "mode = disabled\n");

        let analysis = analyze_config(&config_path).unwrap();
        assert!(analysis.needs_upgrade);
        assert_eq!(analysis.deprecated_keys, ["prefetch.mode"]);
        assert_eq!(analysis.folded_values, [FOLDED_VALUES[0]]);

        let dry = upgrade_config(&config_path, true).unwrap();
        assert_eq!(dry.folded_values, [FOLDED_VALUES[0]]);
        assert_eq!(dry.removed_keys, ["prefetch.mode"]);

        let result = upgrade_config(&config_path, false).unwrap();
        assert_eq!(result.folded_values, [FOLDED_VALUES[0]]);
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("\nenabled = false\n"), "{contents}");
        assert!(!contents.contains("\nmode ="), "{contents}");
        assert!(!analyze_config(&config_path).unwrap().needs_upgrade);
        assert!(
            !ConfigFile::load_from(&config_path)
                .unwrap()
                .prefetch
                .enabled
        );
    }

    #[test]
    fn upgrade_drops_every_other_mode_and_leaves_enabled_alone() {
        for (enabled, mode) in [
            (true, "opportunistic"),
            (true, "aggressive"),
            (true, "auto"),
            (true, "nonsense"),
            (false, "aggressive"),
            (false, "disabled"),
        ] {
            let temp_dir = TempDir::new().unwrap();
            let config_path = temp_dir.path().join("config.ini");
            current_config_with_prefetch(&config_path, enabled, &format!("mode = {mode}\n"));

            upgrade_config(&config_path, false).unwrap();

            let contents = std::fs::read_to_string(&config_path).unwrap();
            let expected = enabled && mode != "disabled";
            assert!(
                contents.contains(&format!("\nenabled = {expected}\n")),
                "{enabled} {mode}"
            );
            assert!(!contents.contains("\nmode ="), "{enabled} {mode}");
        }
    }

    #[test]
    fn a_config_without_mode_needs_no_upgrade_for_it() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        current_config_with_prefetch(&config_path, false, "");

        let analysis = analyze_config(&config_path).unwrap();

        assert!(!analysis.needs_upgrade, "{analysis:?}");
        assert!(analysis.folded_values.is_empty());
    }

    #[test]
    fn upgrade_removes_the_inert_calibration_keys() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        std::fs::write(
            &config_path,
            "[prefetch]\ncalibration_aggressive_threshold = 30\n\
             calibration_opportunistic_threshold = 10\ncalibration_sample_duration = 60\n",
        )
        .unwrap();

        let result = upgrade_config(&config_path, false).unwrap();

        assert_eq!(result.removed_keys.len(), 3, "{:?}", result.removed_keys);
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(!contents.contains("calibration_"));
    }
}
