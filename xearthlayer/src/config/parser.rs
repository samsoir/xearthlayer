//! INI parsing logic for converting `Ini` → `ConfigFile`.
//!
//! This module contains the `parse_ini()` function and its helpers.
//! It is the single place where INI key names are mapped to struct fields.

use ini::Ini;
use std::path::PathBuf;

use super::defaults::TUI_SYMBOLS_VALUES;
use super::file::ConfigFileError;
use super::keys::{ConfigKey, ConfigKeyError};
use super::settings::ConfigFile;
use super::size::parse_size;
use super::upgrade::{deprecated_value, folded_value};

/// Parse an `Ini` object into a `ConfigFile`.
///
/// Starts from `ConfigFile::default()` and overlays any values found in the INI.
pub(super) fn parse_ini(ini: &Ini) -> Result<ConfigFile, ConfigFileError> {
    check_ranges(ini)?;
    let mut config = ConfigFile::default();

    // [general] section
    if let Some(section) = ini.section(Some("general")) {
        if let Some(v) = section.get("layout_version") {
            if let Ok(n) = v.trim().parse::<u32>() {
                config.general.layout_version = n;
            }
        }
        if let Some(v) = section.get("update_check") {
            let v = v.to_lowercase();
            config.general.update_check = v == "true" || v == "1" || v == "yes" || v == "on";
        }
    }

    // [provider] section
    if let Some(section) = ini.section(Some("provider")) {
        if let Some(v) = section.get("type") {
            let v = v.to_lowercase();
            if crate::provider::catalog::find(&v).is_none() {
                return Err(ConfigFileError::InvalidValue {
                    section: "provider".to_string(),
                    key: "type".to_string(),
                    value: v,
                    reason: format!(
                        "must be one of: {}",
                        crate::provider::catalog::keys_sorted_csv()
                    ),
                });
            }
            config.provider.provider_type = v;
        }
        if let Some(v) = section.get("google_api_key") {
            let v = v.trim();
            if !v.is_empty() {
                config.provider.google_api_key = Some(v.to_string());
            }
        }
        if let Some(v) = section.get("mapbox_access_token") {
            let v = v.trim();
            if !v.is_empty() {
                config.provider.mapbox_access_token = Some(v.to_string());
            }
        }
    }

    // [cache] section
    if let Some(section) = ini.section(Some("cache")) {
        if let Some(v) = section.get("directory") {
            let v = v.trim();
            if !v.is_empty() {
                config.cache.directory = expand_tilde(v);
            }
        }
        if let Some(v) = section.get("memory_size") {
            config.cache.memory_size =
                parse_size(v).map_err(|_| ConfigFileError::InvalidValue {
                    section: "cache".to_string(),
                    key: "memory_size".to_string(),
                    value: v.to_string(),
                    reason: "expected format like '2GB', '500MB', or '1024KB'".to_string(),
                })?;
        }
        if let Some(v) = section.get("disk_size") {
            config.cache.disk_size = parse_size(v).map_err(|_| ConfigFileError::InvalidValue {
                section: "cache".to_string(),
                key: "disk_size".to_string(),
                value: v.to_string(),
                reason: "expected format like '20GB', '500MB', or '1024KB'".to_string(),
            })?;
        }
        if let Some(v) = section.get("dds_disk_ratio") {
            // In 0.0..=1.0 already: `check_ranges` (#401).
            config.cache.dds_disk_ratio =
                v.parse::<f64>()
                    .map_err(|_| ConfigFileError::InvalidValue {
                        section: "cache".to_string(),
                        key: "dds_disk_ratio".to_string(),
                        value: v.to_string(),
                        reason: "expected a number between 0.0 and 1.0".to_string(),
                    })?;
        }
    }

    // [texture] section
    if let Some(section) = ini.section(Some("texture")) {
        // `format` was retired in #395 (BC1 only) and is not read: an old
        // config loads whatever the line says, and `config upgrade` drops it.
        if let Some(v) = section.get("compressor") {
            let v = v.to_lowercase();
            if crate::system::CompressorKind::parse(&v).is_none() {
                return Err(ConfigFileError::InvalidValue {
                    section: "texture".to_string(),
                    key: "compressor".to_string(),
                    value: v,
                    reason: format!(
                        "must be one of: {}",
                        crate::system::CompressorKind::VALUES.join(", ")
                    ),
                });
            }
            config.texture.compressor = v;
        }
        if let Some(v) = section.get("gpu_device") {
            config.texture.gpu_device = v.to_string();
        }
    }

    // [generation] section
    if let Some(section) = ini.section(Some("generation")) {
        if let Some(v) = section.get("threads") {
            config.generation.threads = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "generation".to_string(),
                key: "threads".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer".to_string(),
            })?;
        }
        if let Some(v) = section.get("timeout") {
            config.generation.timeout = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "generation".to_string(),
                key: "timeout".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (seconds)".to_string(),
            })?;
        }
    }

    // [xplane] section
    if let Some(section) = ini.section(Some("xplane")) {
        if let Some(v) = section.get("scenery_dir") {
            let v = v.trim();
            if !v.is_empty() {
                config.xplane.scenery_dir = Some(expand_tilde(v));
            }
        }
    }

    // [packages] section
    if let Some(section) = ini.section(Some("packages")) {
        if let Some(v) = section.get("library_url") {
            let v = match deprecated_value("packages.library_url", v) {
                Some(retired) => retired.replacement,
                None => v.trim(),
            };
            if !v.is_empty() {
                config.packages.library_url = Some(v.to_string());
            }
        }
        if let Some(v) = section.get("install_location") {
            let v = v.trim();
            if !v.is_empty() {
                config.packages.install_location = Some(expand_tilde(v));
            }
        }
        if let Some(v) = section.get("custom_scenery_path") {
            let v = v.trim();
            if !v.is_empty() {
                config.packages.custom_scenery_path = Some(expand_tilde(v));
            }
        }
        if let Some(v) = section.get("auto_install_overlays") {
            config.packages.auto_install_overlays = parse_bool(v);
        }
        if let Some(v) = section.get("disable_overlays") {
            config.packages.disable_overlays = parse_bool(v);
        }
        if let Some(v) = section.get("xworld_pro") {
            config.packages.xworld_pro = parse_bool(v);
        }
        if let Some(v) = section.get("temp_dir") {
            let v = v.trim();
            if !v.is_empty() {
                config.packages.temp_dir = Some(expand_tilde(v));
            }
        }
        if let Some(v) = section.get("concurrent_downloads") {
            if let Ok(n) = v.trim().parse::<usize>() {
                config.packages.concurrent_downloads = n.clamp(1, 10);
            }
        }
    }

    // [logging] section
    if let Some(section) = ini.section(Some("logging")) {
        if let Some(v) = section.get("file") {
            let v = v.trim();
            if !v.is_empty() {
                config.logging.file = expand_tilde(v);
            }
        }
    }

    // [prefetch] section
    if let Some(section) = ini.section(Some("prefetch")) {
        if let Some(v) = section.get("enabled") {
            config.prefetch.enabled = parse_bool(v);
        }
        if let Some(v) = section.get("web_api_port") {
            config.prefetch.web_api_port =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "web_api_port".to_string(),
                    value: v.to_string(),
                    reason: "must be a valid port number (1024-65535)".to_string(),
                })?;
        }
        // Legacy settings cone_angle, inner_radius_nm, outer_radius_nm are deprecated (v0.4.0)
        // `max_tiles_per_cycle` deprecated post-#172 (no longer used — the filter
        // pipeline + executor backpressure + pending queue handle rate limiting).
        // They are ignored if present in config file (use 'xearthlayer config upgrade')
        if let Some(v) = section.get("cycle_interval_ms") {
            config.prefetch.cycle_interval_ms =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "cycle_interval_ms".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (milliseconds)".to_string(),
                })?;
        }
        // Legacy settings radial_radius, tile_based_rows_ahead are deprecated (v0.4.0)
        // They are ignored if present in config file (use 'xearthlayer config upgrade')
        // Transition ramp settings
        if let Some(v) = section.get("takeoff_climb_ft") {
            config.prefetch.takeoff_climb_ft =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "takeoff_climb_ft".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive number (feet)".to_string(),
                })?;
        }
        if let Some(v) = section.get("takeoff_timeout_secs") {
            config.prefetch.takeoff_timeout_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "takeoff_timeout_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("landing_hysteresis_secs") {
            config.prefetch.landing_hysteresis_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "landing_hysteresis_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("ramp_duration_secs") {
            config.prefetch.ramp_duration_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "ramp_duration_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("ramp_start_fraction") {
            config.prefetch.ramp_start_fraction =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "ramp_start_fraction".to_string(),
                    value: v.to_string(),
                    reason: "must be a number between 0.1 and 0.5".to_string(),
                })?;
        }
        if let Some(v) = section.get("window_buffer") {
            config.prefetch.window_buffer =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "window_buffer".to_string(),
                    value: v.to_string(),
                    reason: "must be an integer between 0 and 3".to_string(),
                })?;
        }
        if let Some(v) = section.get("stale_region_timeout") {
            config.prefetch.stale_region_timeout =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "stale_region_timeout".to_string(),
                    value: v.to_string(),
                    reason: "must be an integer between 30 and 600".to_string(),
                })?;
        }
        // `default_window_rows` and `window_lon_extent` deprecated post-#172
        // (scenery window computation no longer used; unified prefetch box
        // replaces ground/cruise distinct logic). Ignored if present in
        // config file (use 'xearthlayer config upgrade').
        // Sliding prefetch box settings
        if let Some(v) = section.get("box_extent") {
            let v = current_value("prefetch.box_extent", v);
            config.prefetch.box_extent = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "prefetch".to_string(),
                key: "box_extent".to_string(),
                value: v.to_string(),
                reason: "must be a number between 7.0 and 15.0".to_string(),
            })?;
        }
        if let Some(v) = section.get("box_max_bias") {
            config.prefetch.box_max_bias =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "box_max_bias".to_string(),
                    value: v.to_string(),
                    reason: "must be a number between 0.5 and 0.9".to_string(),
                })?;
        }
        if let Some(v) = section.get("capacity_share") {
            config.prefetch.capacity_share =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "capacity_share".to_string(),
                    value: v.to_string(),
                    reason: "must be a number between 0.05 and 0.50".to_string(),
                })?;
        }
    }

    // [prewarm] section
    if let Some(section) = ini.section(Some("prewarm")) {
        if let Some(v) = section.get("grid_rows") {
            config.prewarm.grid_rows = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "prewarm".to_string(),
                key: "grid_rows".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (DSF tiles)".to_string(),
            })?;
        }
        if let Some(v) = section.get("grid_cols") {
            config.prewarm.grid_cols = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "prewarm".to_string(),
                key: "grid_cols".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (DSF tiles)".to_string(),
            })?;
        }
        // Legacy fallback: if old "grid_size" is present, use for both
        if let Some(v) = section.get("grid_size") {
            if section.get("grid_rows").is_none() && section.get("grid_cols").is_none() {
                if let Ok(size) = v.parse::<u32>() {
                    config.prewarm.grid_rows = size;
                    config.prewarm.grid_cols = size;
                }
            }
        }
    }

    // [patches] section
    if let Some(section) = ini.section(Some("patches")) {
        if let Some(v) = section.get("enabled") {
            config.patches.enabled = parse_bool(v);
        }
        if let Some(v) = section.get("directory") {
            let v = v.trim();
            if !v.is_empty() {
                config.patches.directory = Some(expand_tilde(v));
            } else {
                config.patches.directory = None;
            }
        }
    }

    // [executor] section
    if let Some(section) = ini.section(Some("executor")) {
        if let Some(v) = section.get("request_timeout_secs") {
            config.executor.request_timeout_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "request_timeout_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("max_retries") {
            config.executor.max_retries = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "executor".to_string(),
                key: "max_retries".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer".to_string(),
            })?;
        }
    }

    // [fuse] section
    if let Some(section) = ini.section(Some("fuse")) {
        if let Some(v) = section.get("max_background") {
            config.fuse.max_background = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "fuse".to_string(),
                key: "max_background".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (1-1024)".to_string(),
            })?;
        }
        if let Some(v) = section.get("congestion_threshold") {
            config.fuse.congestion_threshold =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "fuse".to_string(),
                    key: "congestion_threshold".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (1-1024)".to_string(),
                })?;
        }
    }

    // [night] section
    if let Some(section) = ini.section(Some("night")) {
        if let Some(v) = section.get("enabled") {
            config.night.enabled = parse_bool(v);
        }
        if let Some(v) = section.get("brightness") {
            let invalid = || ConfigFileError::InvalidValue {
                section: "night".to_string(),
                key: "brightness".to_string(),
                value: v.to_string(),
                reason: "must be a number from 0.5 to 1.5".to_string(),
            };
            let value: f64 = v.trim().parse().map_err(|_| invalid())?;
            if !(0.5..=1.5).contains(&value) {
                return Err(invalid());
            }
            config.night.brightness = value;
        }
    }

    // [tui] section
    if let Some(section) = ini.section(Some("tui")) {
        if let Some(v) = section.get("symbols") {
            let v = v.trim().to_lowercase();
            if !TUI_SYMBOLS_VALUES.contains(&v.as_str()) {
                return Err(ConfigFileError::InvalidValue {
                    section: "tui".to_string(),
                    key: "symbols".to_string(),
                    value: v,
                    reason: "must be 'auto', 'emoji', or 'plain'".to_string(),
                });
            }
            config.tui.symbols = v;
        }
    }

    // [network] section
    if let Some(section) = ini.section(Some("network")) {
        if let Some(v) = section.get("connections") {
            config.network.connections =
                v.parse().map_err(|reason| ConfigFileError::InvalidValue {
                    section: "network".to_string(),
                    key: "connections".to_string(),
                    value: v.trim().to_string(),
                    reason,
                })?;
        }
    }

    apply_folds(ini, &mut config)?;

    Ok(config)
}

/// Apply each retired setting's value that moved to a current setting
/// (`FOLDED_VALUES`). Run after every section is read, so a fold wins over
/// its target wherever the two lines are.
fn apply_folds(ini: &Ini, config: &mut ConfigFile) -> Result<(), ConfigFileError> {
    for (section, props) in ini.iter() {
        let section = section.unwrap_or("");
        for (key, value) in props.iter() {
            let Some(fold) = folded_value(&format!("{section}.{key}"), value) else {
                continue;
            };
            // Both always hold, asserted for every fold by the upgrade tests.
            let invalid = |reason: String| ConfigFileError::InvalidValue {
                section: section.to_string(),
                key: key.to_string(),
                value: value.to_string(),
                reason,
            };
            let into: ConfigKey = fold
                .into
                .parse()
                .map_err(|e: ConfigKeyError| invalid(e.to_string()))?;
            into.set(config, fold.becomes)
                .map_err(|e| invalid(e.to_string()))?;
        }
    }
    Ok(())
}

/// Parse a boolean value from a config string.
/// Accepts: true/false, yes/no, 1/0, on/off (case-insensitive)
pub(super) fn parse_bool(value: &str) -> bool {
    let v = value.trim().to_lowercase();
    v == "true" || v == "1" || v == "yes" || v == "on"
}

/// Expand ~ to home directory in paths.
pub(super) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// `value` as the program reads it: a retired value ([`deprecated_value`])
/// becomes its replacement.
fn current_value<'a>(key: &str, value: &'a str) -> &'a str {
    deprecated_value(key, value).map_or(value, |retired| retired.replacement)
}

/// Hold every range-checked key ([`ConfigKey::is_range_checked`]) present in
/// `ini` to the range `config set` enforces, before anything is parsed: a
/// hand-edited value outside it is refused with the setting named, never
/// passed to code that assumes the range (#401).
fn check_ranges(ini: &Ini) -> Result<(), ConfigFileError> {
    for key in ConfigKey::all().iter().filter(|k| k.is_range_checked()) {
        let (section, name) = key
            .name()
            .split_once('.')
            .expect("a config key is named section.key");
        let Some(value) = ini.section(Some(section)).and_then(|s| s.get(name)) else {
            continue;
        };
        // A retired value is read as its replacement, so it is the
        // replacement that must be in range.
        let value = current_value(key.name(), value);
        if let Err(ConfigKeyError::ValidationFailed { reason, .. }) = key.validate(value) {
            return Err(ConfigFileError::InvalidValue {
                section: section.to_string(),
                key: name.to_string(),
                value: value.to_string(),
                reason,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_config_file_without_layout_version_reads_as_predating_the_layout() {
        // The parser overlays a file onto ConfigFile::default(), so this is
        // only true because the default is 0. A current default here would make
        // every pre-0.5.0 configuration claim to be migrated already.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "[general]\nupdate_check = true\n").unwrap();
        let config = ConfigFile::load_from(&path).unwrap();
        assert_eq!(config.general.layout_version, 0);
    }

    #[test]
    fn a_stamped_layout_version_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "[general]\nlayout_version = 1\n").unwrap();
        assert_eq!(
            ConfigFile::load_from(&path).unwrap().general.layout_version,
            1
        );
    }
    use super::*;
    use crate::config::defaults::*;
    use crate::config::settings::ConfigFile;
    use tempfile::TempDir;

    /// Regression: a setting whose section moved without the parser following
    /// section to [executor] in #160, but the parser was not updated — a config
    /// written by XEarthLayer itself could not be read back.
    /// Guards the whole writer/parser pair: every value below is non-default and
    /// must survive a save/load cycle. Catches settings the writer emits into a
    /// section the parser does not read.
    #[test]
    fn test_non_default_values_survive_write_read_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        let mut config = ConfigFile::default();
        config.cache.dds_disk_ratio = 0.75;
        config.generation.timeout = 42;
        config.executor.request_timeout_secs = 33;
        config.executor.max_retries = 6;
        config.fuse.max_background = 512;
        config.fuse.congestion_threshold = 384;
        config.prefetch.cycle_interval_ms = 3000;
        config.packages.concurrent_downloads = 9;
        config.save_to(&config_path).unwrap();

        let loaded = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(loaded.cache.dds_disk_ratio, 0.75);
        assert_eq!(loaded.generation.timeout, 42);
        assert_eq!(loaded.executor.request_timeout_secs, 33);
        assert_eq!(loaded.executor.max_retries, 6);
        assert_eq!(loaded.fuse.max_background, 512);
        assert_eq!(loaded.fuse.congestion_threshold, 384);
        assert_eq!(loaded.prefetch.cycle_interval_ms, 3000);
        assert_eq!(loaded.packages.concurrent_downloads, 9);
    }

    #[test]
    fn test_night_enabled_round_trips() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        let mut config = ConfigFile::default();
        assert!(config.night.enabled);
        config.night.enabled = false;
        config.save_to(&config_path).unwrap();
        let loaded = ConfigFile::load_from(&config_path).unwrap();
        assert!(!loaded.night.enabled);
    }

    #[test]
    fn test_night_brightness_round_trips_and_defaults_to_one() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        let mut config = ConfigFile::default();
        assert_eq!(config.night.brightness, 1.0);
        config.night.brightness = 0.75;
        config.save_to(&config_path).unwrap();
        assert_eq!(
            ConfigFile::load_from(&config_path)
                .unwrap()
                .night
                .brightness,
            0.75
        );
    }

    #[test]
    fn test_night_brightness_out_of_range_is_rejected_on_load() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        for bad in ["3", "-1", "NaN", "inf", "abc"] {
            std::fs::write(&path, format!("[night]\nbrightness = {bad}\n")).unwrap();
            assert!(
                ConfigFile::load_from(&path).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    /// #401: a ratio outside 0.0 to 1.0 overflowed the disk budgets, so the
    /// file is held to the same rule as `config set`.
    #[test]
    fn dds_disk_ratio_outside_zero_to_one_is_rejected_on_load() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        for bad in ["1.5", "-0.1", "inf", "-inf", "NaN", "abc"] {
            std::fs::write(&path, format!("[cache]\ndds_disk_ratio = {bad}\n")).unwrap();
            match ConfigFile::load_from(&path) {
                Err(ConfigFileError::InvalidValue { key, reason, .. }) => {
                    assert_eq!(key, "dds_disk_ratio");
                    // The rule `config set` reports, word for word.
                    assert_eq!(reason, "must be a number between 0 and 1");
                }
                other => panic!("{bad} must be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn dds_disk_ratio_from_zero_to_one_is_accepted_on_load() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        for (good, expected) in [
            ("0", 0.0),
            ("0.0", 0.0),
            ("0.6", 0.6),
            ("1", 1.0),
            ("1.0", 1.0),
        ] {
            std::fs::write(&path, format!("[cache]\ndds_disk_ratio = {good}\n")).unwrap();
            assert_eq!(
                ConfigFile::load_from(&path).unwrap().cache.dds_disk_ratio,
                expected,
                "{good} must be accepted"
            );
        }
    }

    #[test]
    fn test_tui_symbols_round_trips_and_defaults_to_auto() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        let mut config = ConfigFile::default();
        assert_eq!(config.tui.symbols, "auto");
        config.tui.symbols = "plain".to_string();
        config.save_to(&config_path).unwrap();
        assert_eq!(
            ConfigFile::load_from(&config_path).unwrap().tui.symbols,
            "plain"
        );
    }

    #[test]
    fn network_connections_round_trips_and_defaults_to_high() {
        use crate::config::NetworkLevel;
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        let mut config = ConfigFile::default();
        assert_eq!(config.network.connections, NetworkLevel::High);
        for level in NetworkLevel::ALL {
            config.network.connections = level;
            config.save_to(&config_path).unwrap();
            assert_eq!(
                ConfigFile::load_from(&config_path)
                    .unwrap()
                    .network
                    .connections,
                level
            );
        }
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(written.contains("[network]"), "{written}");
        assert!(
            written.contains("; How many network connections"),
            "the key carries its help: {written}"
        );
    }

    #[test]
    fn network_connections_is_case_insensitive_and_rejects_unknown_values() {
        use crate::config::NetworkLevel;
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        std::fs::write(&path, "[network]\nconnections = Low\n").unwrap();
        assert_eq!(
            ConfigFile::load_from(&path).unwrap().network.connections,
            NetworkLevel::Low
        );
        std::fs::write(&path, "[network]\nconnections = medium\n").unwrap();
        match ConfigFile::load_from(&path) {
            Err(ConfigFileError::InvalidValue {
                section,
                key,
                reason,
                ..
            }) => {
                assert_eq!((section.as_str(), key.as_str()), ("network", "connections"));
                assert_eq!(reason, "must be one of: max, high, low");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn test_tui_symbols_is_case_insensitive_and_rejects_unknown_values() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        std::fs::write(&path, "[tui]\nsymbols = Emoji\n").unwrap();
        assert_eq!(ConfigFile::load_from(&path).unwrap().tui.symbols, "emoji");
        std::fs::write(&path, "[tui]\nsymbols = ascii\n").unwrap();
        assert!(ConfigFile::load_from(&path).is_err());
    }

    #[test]
    fn test_night_enabled_defaults_true_without_section() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        std::fs::write(&path, "[general]\nupdate_check = true\n").unwrap();
        let loaded = ConfigFile::load_from(&path).unwrap();
        assert!(loaded.night.enabled);
    }

    #[test]
    fn test_invalid_provider_type() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[provider]
type = invalid
"#,
        )
        .unwrap();

        let result = ConfigFile::load_from(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Check for the updated error message that includes all providers
        assert!(err.to_string().contains("must be one of:"));
        assert!(err.to_string().contains("bing"));
    }

    #[test]
    fn test_invalid_cache_size() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[cache]
memory_size = 2TB
"#,
        )
        .unwrap();

        let result = ConfigFile::load_from(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("memory_size"));
    }

    #[test]
    fn test_human_readable_sizes() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[cache]
memory_size = 4GB
disk_size = 50GB
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.cache.memory_size, 4 * 1024 * 1024 * 1024);
        assert_eq!(config.cache.disk_size, 50 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_expand_tilde() {
        let path = expand_tilde("~/test/path");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(path, home.join("test/path"));
        }

        // Non-tilde paths should be unchanged
        let path = expand_tilde("/absolute/path");
        assert_eq!(path, PathBuf::from("/absolute/path"));
    }

    /// #395: `texture.format` is no longer read, so a config written by an
    /// older version loads whatever the line says, and the rest of
    /// `[texture]` is still honoured.
    #[test]
    fn test_retired_texture_format_line_is_ignored() {
        for value in ["bc1", "bc3", "anything"] {
            let temp_dir = TempDir::new().unwrap();
            let config_path = temp_dir.path().join("config.ini");
            std::fs::write(
                &config_path,
                format!("[texture]\nformat = {value}\ncompressor = software\n"),
            )
            .unwrap();

            let config = ConfigFile::load_from(&config_path)
                .unwrap_or_else(|e| panic!("format = {value} must not fail the load: {e}"));
            assert_eq!(config.texture.compressor, "software");
        }
    }

    /// Every key whose `config set` rule is a range is held to it when the
    /// file is read too, so a hand-edited value cannot reach code that
    /// assumes the range (#401 for `cache.dds_disk_ratio`, then every such
    /// key).
    #[test]
    fn every_range_checked_key_refuses_an_out_of_range_value_on_load() {
        let keys: Vec<_> = ConfigKey::all()
            .iter()
            .filter(|k| k.is_range_checked())
            .collect();
        assert!(!keys.is_empty());
        for key in keys {
            let (section, name) = key.name().split_once('.').unwrap();
            let temp_dir = TempDir::new().unwrap();
            let path = temp_dir.path().join("config.ini");
            // Below the minimum of every range in use (all are at least 0).
            std::fs::write(&path, format!("[{section}]\n{name} = -1000000\n")).unwrap();

            match ConfigFile::load_from(&path) {
                Err(ConfigFileError::InvalidValue {
                    section: s, key: k, ..
                }) => assert_eq!((s.as_str(), k.as_str()), (section, name)),
                other => panic!("{}: expected InvalidValue, got {other:?}", key.name()),
            }
        }
    }

    #[test]
    fn every_range_checked_key_accepts_its_default_on_load() {
        let defaults = ConfigFile::default();
        for key in ConfigKey::all().iter().filter(|k| k.is_range_checked()) {
            let (section, name) = key.name().split_once('.').unwrap();
            let value = key.get(&defaults);
            let temp_dir = TempDir::new().unwrap();
            let path = temp_dir.path().join("config.ini");
            std::fs::write(&path, format!("[{section}]\n{name} = {value}\n")).unwrap();
            ConfigFile::load_from(&path)
                .unwrap_or_else(|e| panic!("{} = {value} must load: {e}", key.name()));
        }
    }

    /// 6.5 was the default from #98 until #125 raised it to 7.0, below the
    /// range `config set` always enforced, so configuration files written in
    /// between carry it. It is read as 7.0, not refused.
    #[test]
    fn the_former_default_box_extent_loads_as_the_current_one() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        std::fs::write(&path, "[prefetch]\nbox_extent = 6.5\n").unwrap();

        let config = ConfigFile::load_from(&path).expect("a former default must load");
        assert_eq!(config.prefetch.box_extent, 7.0);
    }

    #[test]
    fn a_box_extent_below_the_range_that_was_never_a_default_is_refused() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.ini");
        std::fs::write(&path, "[prefetch]\nbox_extent = 6.0\n").unwrap();
        assert!(matches!(
            ConfigFile::load_from(&path),
            Err(ConfigFileError::InvalidValue { .. })
        ));
    }

    #[test]
    fn the_settings_that_were_only_checked_by_config_set_are_range_checked() {
        for key in [
            ConfigKey::CacheDdsDiskRatio,
            ConfigKey::PrefetchTakeoffClimbFt,
            ConfigKey::PrefetchRampStartFraction,
            ConfigKey::PrefetchBoxExtent,
            ConfigKey::PrefetchBoxMaxBias,
            ConfigKey::PrefetchCapacityShare,
            ConfigKey::PrefetchTakeoffTimeoutSecs,
            ConfigKey::PrefetchLandingHysteresisSecs,
            ConfigKey::PrefetchRampDurationSecs,
            ConfigKey::PrefetchWindowBuffer,
            ConfigKey::PrefetchStaleRegionTimeout,
            ConfigKey::FuseMaxBackground,
            ConfigKey::FuseCongestionThreshold,
        ] {
            assert!(key.is_range_checked(), "{}", key.name());
        }
    }

    #[test]
    fn test_partial_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Only specify some settings, rest should use defaults
        std::fs::write(
            &config_path,
            r#"
[provider]
type = google
google_api_key = my-key

[download]
timeout = 45
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();

        // Specified values
        assert_eq!(config.provider.provider_type, "google");
        assert_eq!(config.provider.google_api_key, Some("my-key".to_string()));

        // Default values
        assert_eq!(config.cache.memory_size, DEFAULT_MEMORY_CACHE_SIZE);
    }

    #[test]
    fn test_parse_bool_true_values() {
        // Test all accepted "true" values
        assert!(parse_bool("true"));
        assert!(parse_bool("TRUE"));
        assert!(parse_bool("True"));
        assert!(parse_bool("yes"));
        assert!(parse_bool("YES"));
        assert!(parse_bool("Yes"));
        assert!(parse_bool("1"));
        assert!(parse_bool("on"));
        assert!(parse_bool("ON"));
        assert!(parse_bool("On"));
    }

    #[test]
    fn test_parse_bool_false_values() {
        // Test values that should be false
        assert!(!parse_bool("false"));
        assert!(!parse_bool("FALSE"));
        assert!(!parse_bool("no"));
        assert!(!parse_bool("NO"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("off"));
        assert!(!parse_bool("OFF"));
        // Invalid values also return false
        assert!(!parse_bool("invalid"));
        assert!(!parse_bool(""));
        assert!(!parse_bool("maybe"));
    }

    #[test]
    fn test_parse_bool_with_whitespace() {
        // Test that whitespace is trimmed
        assert!(parse_bool("  true  "));
        assert!(parse_bool("\ttrue\n"));
        assert!(parse_bool("  yes "));
        assert!(!parse_bool("  false  "));
        assert!(!parse_bool("  no  "));
    }

    #[test]
    fn test_patches_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[patches]
enabled = true
directory = /custom/patches/dir
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert!(config.patches.enabled);
        assert_eq!(
            config.patches.directory,
            Some(PathBuf::from("/custom/patches/dir"))
        );
    }

    #[test]
    fn test_patches_config_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[patches]
enabled = false
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert!(!config.patches.enabled);
    }

    #[test]
    fn test_packages_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[packages]
library_url = https://example.com/library.txt
temp_dir = /tmp/xearthlayer
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(
            config.packages.library_url,
            Some("https://example.com/library.txt".to_string())
        );
        assert_eq!(
            config.packages.temp_dir,
            Some(PathBuf::from("/tmp/xearthlayer"))
        );
    }

    #[test]
    fn config_with_removed_executor_pool_keys_still_loads() {
        // Every user upgrading to this release has these three keys on disk.
        // They must be ignored, never rejected -- a hard error here would stop
        // XEarthLayer starting for the entire existing install base.
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        std::fs::write(
            &config_path,
            r#"
[executor]
network_concurrent = 256
cpu_concurrent = 16
disk_io_concurrent = 64
max_retries = 5
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path)
            .expect("a config carrying the removed keys must still load");
        assert_eq!(
            config.executor.max_retries, 5,
            "surrounding keys still parse"
        );
    }

    #[test]
    fn config_with_the_removed_macos_backend_key_still_loads() {
        // A config written by a 0.5.1 development build may carry
        // `fuse.macos_backend`, with any of its old values. macOS now mounts
        // through FSKit only, so the line is ignored whatever it says, never
        // rejected.
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");
        for value in ["auto", "kernel", "macfuse-fskit", "kext"] {
            std::fs::write(
                &config_path,
                format!("[fuse]\nmax_background = 300\nmacos_backend = {value}\n"),
            )
            .unwrap();

            let config = ConfigFile::load_from(&config_path)
                .expect("a config carrying fuse.macos_backend must still load");
            assert_eq!(config.fuse.max_background, 300, "{value}");
        }
    }

    /// #340: `prefetch.mode` is retired. `disabled` reads as
    /// `prefetch.enabled = false` wherever the two lines are, any other value
    /// (including ones the old parser refused) is ignored, and an explicit
    /// `enabled = false` is never turned back on.
    #[test]
    fn retired_prefetch_mode_folds_into_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        for (body, expected) in [
            ("[prefetch]\nmode = disabled\n", false),
            ("[prefetch]\nmode = Disabled\nenabled = true\n", false),
            ("[prefetch]\nenabled = true\nmode = disabled\n", false),
            ("[prefetch]\nenabled = true\nmode = aggressive\n", true),
            ("[prefetch]\nmode = opportunistic\n", true),
            ("[prefetch]\nmode = auto\n", true),
            ("[prefetch]\nmode = nonsense\n", true),
            ("[prefetch]\nenabled = false\nmode = aggressive\n", false),
            ("[prefetch]\nenabled = false\n", false),
            ("[prefetch]\n", true),
        ] {
            std::fs::write(&path, body).unwrap();
            assert_eq!(
                ConfigFile::load_from(&path).unwrap().prefetch.enabled,
                expected,
                "{body:?}"
            );
        }
    }

    /// #334: the rebuilt world is library generation v2. The root library at
    /// the old URL stays frozen for pre-0.5 clients, so a config naming it, or
    /// the alpha testers' staging URL, must read the v2 library.
    #[test]
    fn retired_library_urls_load_as_the_v2_library() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        for retired in [
            LEGACY_LIBRARY_URL,
            ALPHA_STAGING_LIBRARY_URL,
            "  https://xearthlayer.app/packages/xearthlayer_package_library.txt  ",
        ] {
            std::fs::write(&path, format!("[packages]\nlibrary_url = {retired}\n")).unwrap();
            assert_eq!(
                ConfigFile::load_from(&path)
                    .unwrap()
                    .packages
                    .library_url
                    .as_deref(),
                Some(DEFAULT_LIBRARY_URL),
                "{retired}"
            );
        }
        let own = "https://example.com/my/xearthlayer_package_library.txt";
        std::fs::write(&path, format!("[packages]\nlibrary_url = {own}\n")).unwrap();
        assert_eq!(
            ConfigFile::load_from(&path)
                .unwrap()
                .packages
                .library_url
                .as_deref(),
            Some(own),
            "a library of one's own is left alone"
        );
    }

    #[test]
    fn the_default_library_is_the_v2_generation() {
        assert_eq!(
            DEFAULT_LIBRARY_URL,
            "https://xearthlayer.app/packages/v2/xearthlayer_package_library.txt"
        );
    }
}
