//! INI parsing logic for converting `Ini` → `ConfigFile`.
//!
//! This module contains the `parse_ini()` function and its helpers.
//! It is the single place where INI key names are mapped to struct fields.

use ini::Ini;
use std::path::PathBuf;

use super::defaults::clamp_http_concurrent;
use super::file::ConfigFileError;
use super::settings::ConfigFile;
use super::size::parse_size;
use crate::dds::DdsFormat;

/// Parse an `Ini` object into a `ConfigFile`.
///
/// Starts from `ConfigFile::default()` and overlays any values found in the INI.
pub(super) fn parse_ini(ini: &Ini) -> Result<ConfigFile, ConfigFileError> {
    let mut config = ConfigFile::default();

    // [provider] section
    if let Some(section) = ini.section(Some("provider")) {
        if let Some(v) = section.get("type") {
            let v = v.to_lowercase();
            let valid_providers = ["apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs"];
            if !valid_providers.contains(&v.as_str()) {
                return Err(ConfigFileError::InvalidValue {
                    section: "provider".to_string(),
                    key: "type".to_string(),
                    value: v,
                    reason: "must be one of: apple, arcgis, bing, go2, google, mapbox, usgs"
                        .to_string(),
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
        if let Some(v) = section.get("disk_io_profile") {
            config.cache.disk_io_profile =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "cache".to_string(),
                    key: "disk_io_profile".to_string(),
                    value: v.to_string(),
                    reason: "must be one of: auto, hdd, ssd, nvme".to_string(),
                })?;
        }
    }

    // [texture] section
    if let Some(section) = ini.section(Some("texture")) {
        if let Some(v) = section.get("format") {
            let v = v.to_lowercase();
            config.texture.format = match v.as_str() {
                "bc1" => DdsFormat::BC1,
                "bc3" => DdsFormat::BC3,
                _ => {
                    return Err(ConfigFileError::InvalidValue {
                        section: "texture".to_string(),
                        key: "format".to_string(),
                        value: v,
                        reason: "must be 'bc1' or 'bc3'".to_string(),
                    });
                }
            };
        }
    }

    // [download] section
    if let Some(section) = ini.section(Some("download")) {
        if let Some(v) = section.get("timeout") {
            config.download.timeout = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "download".to_string(),
                key: "timeout".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (seconds)".to_string(),
            })?;
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

    // [pipeline] section
    if let Some(section) = ini.section(Some("pipeline")) {
        if let Some(v) = section.get("max_http_concurrent") {
            let parsed: usize = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "pipeline".to_string(),
                key: "max_http_concurrent".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer".to_string(),
            })?;
            // Enforce hard limits to prevent provider rate limiting
            config.pipeline.max_http_concurrent = clamp_http_concurrent(parsed);
        }
        if let Some(v) = section.get("max_cpu_concurrent") {
            config.pipeline.max_cpu_concurrent =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "max_cpu_concurrent".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("max_prefetch_in_flight") {
            config.pipeline.max_prefetch_in_flight =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "max_prefetch_in_flight".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("request_timeout_secs") {
            config.pipeline.request_timeout_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "request_timeout_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("max_retries") {
            config.pipeline.max_retries = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "pipeline".to_string(),
                key: "max_retries".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer".to_string(),
            })?;
        }
        if let Some(v) = section.get("retry_base_delay_ms") {
            config.pipeline.retry_base_delay_ms =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "retry_base_delay_ms".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (milliseconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("coalesce_channel_capacity") {
            config.pipeline.coalesce_channel_capacity =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "pipeline".to_string(),
                    key: "coalesce_channel_capacity".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
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
            let v = v.trim();
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
        if let Some(v) = section.get("temp_dir") {
            let v = v.trim();
            if !v.is_empty() {
                config.packages.temp_dir = Some(expand_tilde(v));
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
        if let Some(v) = section.get("strategy") {
            let v = v.trim().to_lowercase();
            match v.as_str() {
                "auto" | "adaptive" => {
                    config.prefetch.strategy = "adaptive".to_string();
                }
                // Legacy strategies - map to adaptive with warning (deprecated in v0.4.0)
                "heading-aware" | "radial" | "tile-based" => {
                    tracing::warn!(
                        "Prefetch strategy '{}' is deprecated and will be removed in a future version. \
                         Using 'adaptive' instead. Please update your config file.",
                        v
                    );
                    config.prefetch.strategy = "adaptive".to_string();
                }
                _ => {
                    return Err(ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "strategy".to_string(),
                        value: v.to_string(),
                        reason: "must be 'auto' or 'adaptive'".to_string(),
                    });
                }
            }
        }
        if let Some(v) = section.get("mode") {
            let v = v.trim().to_lowercase();
            match v.as_str() {
                "auto" | "aggressive" | "opportunistic" | "disabled" => {
                    config.prefetch.mode = v;
                }
                _ => {
                    return Err(ConfigFileError::InvalidValue {
                        section: "prefetch".to_string(),
                        key: "mode".to_string(),
                        value: v.to_string(),
                        reason: "must be 'auto', 'aggressive', 'opportunistic', or 'disabled'"
                            .to_string(),
                    });
                }
            }
        }
        if let Some(v) = section.get("udp_port") {
            config.prefetch.udp_port = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "prefetch".to_string(),
                key: "udp_port".to_string(),
                value: v.to_string(),
                reason: "must be a valid port number (1-65535)".to_string(),
            })?;
        }
        // Legacy settings cone_angle, inner_radius_nm, outer_radius_nm are deprecated (v0.4.0)
        // They are ignored if present in config file (use 'xearthlayer config upgrade')
        if let Some(v) = section.get("max_tiles_per_cycle") {
            config.prefetch.max_tiles_per_cycle =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "max_tiles_per_cycle".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("cycle_interval_ms") {
            config.prefetch.cycle_interval_ms =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "cycle_interval_ms".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (milliseconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("circuit_breaker_threshold") {
            config.prefetch.circuit_breaker_threshold =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "circuit_breaker_threshold".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive number (FUSE jobs/second)".to_string(),
                })?;
        }
        if let Some(v) = section.get("circuit_breaker_open_ms") {
            config.prefetch.circuit_breaker_open_ms =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "circuit_breaker_open_ms".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (milliseconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("circuit_breaker_half_open_secs") {
            config.prefetch.circuit_breaker_half_open_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "circuit_breaker_half_open_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        // Legacy settings radial_radius, tile_based_rows_ahead are deprecated (v0.4.0)
        // They are ignored if present in config file (use 'xearthlayer config upgrade')
        // Adaptive prefetch calibration settings
        if let Some(v) = section.get("calibration_aggressive_threshold") {
            config.prefetch.calibration_aggressive_threshold =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "calibration_aggressive_threshold".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive number (tiles/sec)".to_string(),
                })?;
        }
        if let Some(v) = section.get("calibration_opportunistic_threshold") {
            config.prefetch.calibration_opportunistic_threshold =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "calibration_opportunistic_threshold".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive number (tiles/sec)".to_string(),
                })?;
        }
        if let Some(v) = section.get("calibration_sample_duration") {
            config.prefetch.calibration_sample_duration =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "calibration_sample_duration".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
    }

    // [control_plane] section
    if let Some(section) = ini.section(Some("control_plane")) {
        if let Some(v) = section.get("max_concurrent_jobs") {
            config.control_plane.max_concurrent_jobs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "control_plane".to_string(),
                    key: "max_concurrent_jobs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("stall_threshold_secs") {
            config.control_plane.stall_threshold_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "control_plane".to_string(),
                    key: "stall_threshold_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("health_check_interval_secs") {
            config.control_plane.health_check_interval_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "control_plane".to_string(),
                    key: "health_check_interval_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("semaphore_timeout_secs") {
            config.control_plane.semaphore_timeout_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "control_plane".to_string(),
                    key: "semaphore_timeout_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
    }

    // [prewarm] section
    if let Some(section) = ini.section(Some("prewarm")) {
        if let Some(v) = section.get("grid_size") {
            config.prewarm.grid_size = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "prewarm".to_string(),
                key: "grid_size".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer (DSF tiles per side)".to_string(),
            })?;
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
        if let Some(v) = section.get("network_concurrent") {
            let parsed: usize = v.parse().map_err(|_| ConfigFileError::InvalidValue {
                section: "executor".to_string(),
                key: "network_concurrent".to_string(),
                value: v.to_string(),
                reason: "must be a positive integer".to_string(),
            })?;
            // Clamp to valid range (same as HTTP concurrent)
            config.executor.network_concurrent = clamp_http_concurrent(parsed);
        }
        if let Some(v) = section.get("cpu_concurrent") {
            config.executor.cpu_concurrent =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "cpu_concurrent".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("disk_io_concurrent") {
            config.executor.disk_io_concurrent =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "disk_io_concurrent".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("max_concurrent_tasks") {
            config.executor.max_concurrent_tasks =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "max_concurrent_tasks".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("job_channel_capacity") {
            config.executor.job_channel_capacity =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "job_channel_capacity".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
        if let Some(v) = section.get("request_channel_capacity") {
            config.executor.request_channel_capacity =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "request_channel_capacity".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer".to_string(),
                })?;
        }
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
        if let Some(v) = section.get("retry_base_delay_ms") {
            config.executor.retry_base_delay_ms =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "executor".to_string(),
                    key: "retry_base_delay_ms".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (milliseconds)".to_string(),
                })?;
        }
    }

    // [online_network] section
    if let Some(section) = ini.section(Some("online_network")) {
        if let Some(v) = section.get("enabled") {
            config.online_network.enabled = parse_bool(v);
        }
        if let Some(v) = section.get("network_type") {
            let v = v.trim().to_lowercase();
            match v.as_str() {
                "vatsim" | "ivao" | "pilotedge" => {
                    config.online_network.network_type = v;
                }
                _ => {
                    return Err(ConfigFileError::InvalidValue {
                        section: "online_network".to_string(),
                        key: "network_type".to_string(),
                        value: v.to_string(),
                        reason: "must be 'vatsim', 'ivao', or 'pilotedge'".to_string(),
                    });
                }
            }
        }
        if let Some(v) = section.get("pilot_id") {
            config.online_network.pilot_id =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "online_network".to_string(),
                    key: "pilot_id".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (VATSIM CID)".to_string(),
                })?;
        }
        if let Some(v) = section.get("api_url") {
            let v = v.trim();
            if !v.is_empty() {
                config.online_network.api_url = v.to_string();
            }
        }
        if let Some(v) = section.get("poll_interval_secs") {
            config.online_network.poll_interval_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "online_network".to_string(),
                    key: "poll_interval_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("max_stale_secs") {
            config.online_network.max_stale_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "online_network".to_string(),
                    key: "max_stale_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be a positive integer (seconds)".to_string(),
                })?;
        }
    }

    Ok(config)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::defaults::*;
    use crate::config::settings::ConfigFile;
    use tempfile::TempDir;

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
        assert_eq!(config.download.timeout, 45);

        // Default values
        assert_eq!(config.cache.memory_size, DEFAULT_MEMORY_CACHE_SIZE);
        assert_eq!(config.texture.format, DdsFormat::BC1);
    }

    #[test]
    fn test_http_concurrent_clamped_to_ceiling() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value above maximum gets clamped to 256
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 500
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, MAX_HTTP_CONCURRENT);
    }

    #[test]
    fn test_http_concurrent_clamped_to_floor() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value below minimum gets clamped to 64
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 10
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, MIN_HTTP_CONCURRENT);
    }

    #[test]
    fn test_http_concurrent_in_range_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        // Test value within range is unchanged
        std::fs::write(
            &config_path,
            r#"
[pipeline]
max_http_concurrent = 128
"#,
        )
        .unwrap();

        let config = ConfigFile::load_from(&config_path).unwrap();
        assert_eq!(config.pipeline.max_http_concurrent, 128);
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
}
