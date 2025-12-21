//! Configuration file handling for ~/.xearthlayer/config.ini.
//!
//! Loads and saves user configuration with sensible defaults.

use crate::config::size::{format_size, parse_size};
use crate::dds::DdsFormat;
use crate::pipeline::DiskIoProfile;
use ini::Ini;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Configuration file errors.
#[derive(Debug, Error)]
pub enum ConfigFileError {
    /// Failed to read config file
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] ini::Error),

    /// Failed to write config file
    #[error("Failed to write config file: {0}")]
    WriteError(String),

    /// Invalid configuration value
    #[error("Invalid configuration: {section}.{key} = '{value}' - {reason}")]
    InvalidValue {
        section: String,
        key: String,
        value: String,
        reason: String,
    },

    /// Failed to create config directory
    #[error("Failed to create config directory: {0}")]
    DirectoryError(std::io::Error),
}

/// Complete application configuration loaded from config.ini.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// Provider settings
    pub provider: ProviderSettings,
    /// Cache settings
    pub cache: CacheSettings,
    /// Texture settings
    pub texture: TextureSettings,
    /// Download settings
    pub download: DownloadSettings,
    /// Generation settings
    pub generation: GenerationSettings,
    /// X-Plane settings
    pub xplane: XPlaneSettings,
    /// Package manager settings
    pub packages: PackagesSettings,
    /// Logging settings
    pub logging: LoggingSettings,
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    /// Provider type: "apple", "arcgis", "bing", "go2", "google", "mapbox", or "usgs"
    pub provider_type: String,
    /// Google Maps API key (only required for "google" provider)
    pub google_api_key: Option<String>,
    /// MapBox access token (only required for "mapbox" provider)
    pub mapbox_access_token: Option<String>,
}

/// Cache configuration.
#[derive(Debug, Clone)]
pub struct CacheSettings {
    /// Cache directory path
    pub directory: PathBuf,
    /// Memory cache size in bytes
    pub memory_size: usize,
    /// Disk cache size in bytes
    pub disk_size: usize,
    /// Disk I/O profile for tuning concurrency based on storage type
    pub disk_io_profile: DiskIoProfile,
}

/// Texture configuration.
#[derive(Debug, Clone)]
pub struct TextureSettings {
    /// DDS format: BC1 or BC3
    pub format: DdsFormat,
}

/// Download configuration.
#[derive(Debug, Clone)]
pub struct DownloadSettings {
    /// Timeout in seconds for HTTP requests.
    pub timeout: u64,
}

/// Generation configuration.
#[derive(Debug, Clone)]
pub struct GenerationSettings {
    /// Number of threads for parallel tile generation.
    /// Default: number of CPU cores.
    pub threads: usize,
    /// Timeout in seconds for generating a single tile.
    /// If exceeded, returns a magenta placeholder.
    /// Default: 10 seconds.
    pub timeout: u64,
}

/// X-Plane configuration.
#[derive(Debug, Clone)]
pub struct XPlaneSettings {
    /// Custom Scenery directory (None = auto-detect)
    pub scenery_dir: Option<PathBuf>,
}

/// Package manager configuration.
#[derive(Debug, Clone)]
pub struct PackagesSettings {
    /// URL to the package library index.
    /// This is where the package manager fetches the list of available packages.
    pub library_url: Option<String>,
    /// Local directory for installed packages (default: ~/.xearthlayer/packages).
    pub install_location: Option<PathBuf>,
    /// X-Plane Custom Scenery directory for overlay symlinks.
    /// If None, auto-detects from xplane.scenery_dir or ~/.x-plane/x-plane_install_12.txt
    pub custom_scenery_path: Option<PathBuf>,
    /// Automatically install overlay packages when installing ortho for same region.
    pub auto_install_overlays: bool,
    /// Temporary directory for downloads (default: system temp dir).
    pub temp_dir: Option<PathBuf>,
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingSettings {
    /// Log file path
    pub file: PathBuf,
}

impl Default for ConfigFile {
    fn default() -> Self {
        let config_dir = config_directory();
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("xearthlayer");

        Self {
            provider: ProviderSettings {
                provider_type: "bing".to_string(),
                google_api_key: None,
                mapbox_access_token: None,
            },
            cache: CacheSettings {
                directory: cache_dir,
                memory_size: 2 * 1024 * 1024 * 1024, // 2GB
                disk_size: 20 * 1024 * 1024 * 1024,  // 20GB
                disk_io_profile: DiskIoProfile::Auto,
            },
            texture: TextureSettings {
                format: DdsFormat::BC1,
            },
            download: DownloadSettings { timeout: 30 },
            generation: GenerationSettings {
                threads: num_cpus(),
                timeout: 10,
            },
            xplane: XPlaneSettings { scenery_dir: None },
            packages: PackagesSettings {
                library_url: None,
                install_location: None,
                custom_scenery_path: None,
                auto_install_overlays: false,
                temp_dir: None,
            },
            logging: LoggingSettings {
                file: config_dir.join("xearthlayer.log"),
            },
        }
    }
}

/// Get the number of available CPU cores.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

impl ConfigFile {
    /// Load configuration from the default path (~/.xearthlayer/config.ini).
    ///
    /// If the file doesn't exist, creates it with defaults.
    pub fn load() -> Result<Self, ConfigFileError> {
        let path = config_file_path();
        Self::load_from(&path)
    }

    /// Load configuration from a specific path.
    ///
    /// If the file doesn't exist, returns defaults.
    pub fn load_from(path: &Path) -> Result<Self, ConfigFileError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let ini = Ini::load_from_file(path)?;
        Self::from_ini(&ini)
    }

    /// Save configuration to the default path (~/.xearthlayer/config.ini).
    pub fn save(&self) -> Result<(), ConfigFileError> {
        let path = config_file_path();
        self.save_to(&path)
    }

    /// Save configuration to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigFileError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigFileError::DirectoryError)?;
        }

        let content = self.to_config_string();
        std::fs::write(path, content).map_err(|e| ConfigFileError::WriteError(e.to_string()))
    }

    /// Create ConfigFile from parsed INI.
    fn from_ini(ini: &Ini) -> Result<Self, ConfigFileError> {
        let mut config = Self::default();

        // [provider] section
        if let Some(section) = ini.section(Some("provider")) {
            if let Some(v) = section.get("type") {
                let v = v.to_lowercase();
                let valid_providers =
                    ["apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs"];
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
                config.cache.disk_size =
                    parse_size(v).map_err(|_| ConfigFileError::InvalidValue {
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
                config.generation.threads =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
                        section: "generation".to_string(),
                        key: "threads".to_string(),
                        value: v.to_string(),
                        reason: "must be a positive integer".to_string(),
                    })?;
            }
            if let Some(v) = section.get("timeout") {
                config.generation.timeout =
                    v.parse().map_err(|_| ConfigFileError::InvalidValue {
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
                let v = v.trim().to_lowercase();
                config.packages.auto_install_overlays = v == "true" || v == "1" || v == "yes";
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

        Ok(config)
    }

    /// Convert to INI format with proper comments.
    fn to_config_string(&self) -> String {
        let google_api_key = self.provider.google_api_key.as_deref().unwrap_or("");
        let mapbox_access_token = self.provider.mapbox_access_token.as_deref().unwrap_or("");
        let scenery_dir = self
            .xplane
            .scenery_dir
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let library_url = self.packages.library_url.as_deref().unwrap_or("");
        let install_location = self
            .packages
            .install_location
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let custom_scenery_path = self
            .packages
            .custom_scenery_path
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();
        let auto_install_overlays = if self.packages.auto_install_overlays {
            "true"
        } else {
            "false"
        };
        let temp_dir = self
            .packages
            .temp_dir
            .as_ref()
            .map(|p| path_to_string(p))
            .unwrap_or_default();

        format!(
            r#"[provider]
; Imagery provider:
;   apple  - Apple Maps (free, tokens auto-acquired via DuckDuckGo)
;   arcgis - ArcGIS World Imagery (free, global coverage)
;   bing   - Bing Maps (free, no key required)
;   go2    - Google Maps via public tile servers (free, no key required, same as Ortho4XP)
;   google - Google Maps official API (paid, requires API key)
;   mapbox - MapBox satellite (free tier available, requires access token)
;   usgs   - USGS orthoimagery (free, US coverage only)
type = {}
; Google Maps API key (only required when type = google)
; Get one at: https://console.cloud.google.com (enable Map Tiles API)
google_api_key = {}
; MapBox access token (only required when type = mapbox)
; Get one at: https://www.mapbox.com/
mapbox_access_token = {}

[cache]
; Cache directory for storing downloaded tiles (default: ~/.cache/xearthlayer)
; Uncomment and modify to use a custom location:
; directory = /path/to/custom/cache
; Memory cache size (default: 2GB) - uses RAM for fastest access
; Supports: KB, MB, GB suffixes (e.g., 500MB, 2GB, 4GB)
memory_size = {}
; Disk cache size (default: 20GB) - persistent storage for tiles
; Supports: KB, MB, GB suffixes (e.g., 10GB, 20GB, 50GB)
disk_size = {}
; Disk I/O concurrency profile based on storage type (default: auto)
;   auto - Auto-detect storage type (recommended)
;   hdd  - Spinning disk (conservative: 1-4 concurrent ops)
;   ssd  - SATA/AHCI SSD (moderate: ~32-64 concurrent ops)
;   nvme - NVMe SSD (aggressive: ~128-256 concurrent ops)
disk_io_profile = {}

[texture]
; DDS compression format: bc1 (smaller, opaque) or bc3 (larger, with alpha)
; bc1 recommended for satellite imagery
format = {}

[download]
; Timeout in seconds for HTTP requests (default: 30)
timeout = {}

[generation]
; Number of threads for parallel tile generation (default: number of CPU cores)
; WARNING: Do not set this higher than your CPU core count
threads = {}
; Timeout in seconds for generating a single tile (default: 10)
; If exceeded, returns a magenta placeholder texture
timeout = {}

[xplane]
; X-Plane Custom Scenery directory for mounting scenery packs
; If empty, auto-detects from ~/.x-plane/x-plane_install_12.txt
; This is also where packages are installed by 'xearthlayer packages install'
scenery_dir = {}

[packages]
; URL to the XEarthLayer package library index
; This is where available packages are discovered for 'xearthlayer packages check/install'
library_url = {}
; Local directory for installed packages (default: ~/.xearthlayer/packages)
install_location = {}
; X-Plane Custom Scenery directory for overlay symlinks
; If empty, uses xplane.scenery_dir or auto-detects
custom_scenery_path = {}
; Automatically install overlay packages when installing ortho for same region
auto_install_overlays = {}
; Temporary directory for package downloads (default: system temp dir)
; Large packages are downloaded here before extraction
temp_dir = {}

[logging]
; Log file path (default: ~/.xearthlayer/xearthlayer.log)
file = {}
"#,
            self.provider.provider_type,
            google_api_key,
            mapbox_access_token,
            format_size(self.cache.memory_size),
            format_size(self.cache.disk_size),
            self.cache.disk_io_profile.as_str(),
            self.texture.format.to_string().to_lowercase(),
            self.download.timeout,
            self.generation.threads,
            self.generation.timeout,
            scenery_dir,
            library_url,
            install_location,
            custom_scenery_path,
            auto_install_overlays,
            temp_dir,
            path_to_string(&self.logging.file),
        )
    }

    /// Create the default config file if it doesn't exist.
    ///
    /// Returns the path to the config file.
    pub fn ensure_exists() -> Result<PathBuf, ConfigFileError> {
        let path = config_file_path();
        if !path.exists() {
            let config = Self::default();
            config.save_to(&path)?;
        }
        Ok(path)
    }
}

/// Get the path to the config directory (~/.xearthlayer).
pub fn config_directory() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
}

/// Get the path to the config file (~/.xearthlayer/config.ini).
pub fn config_file_path() -> PathBuf {
    config_directory().join("config.ini")
}

/// Expand ~ to home directory in paths.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Convert path to string, collapsing home dir to ~.
fn path_to_string(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = ConfigFile::default();

        assert_eq!(config.provider.provider_type, "bing");
        assert!(config.provider.google_api_key.is_none());
        assert_eq!(config.cache.memory_size, 2 * 1024 * 1024 * 1024);
        assert_eq!(config.cache.disk_size, 20 * 1024 * 1024 * 1024);
        assert_eq!(config.texture.format, DdsFormat::BC1);
        assert_eq!(config.download.timeout, 30);
        assert!(config.xplane.scenery_dir.is_none());
    }

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.ini");

        let mut config = ConfigFile::default();
        config.provider.provider_type = "google".to_string();
        config.provider.google_api_key = Some("test-api-key".to_string());
        config.cache.memory_size = 4 * 1024 * 1024 * 1024; // 4GB
        config.download.timeout = 60;

        config.save_to(&config_path).unwrap();

        let loaded = ConfigFile::load_from(&config_path).unwrap();

        assert_eq!(loaded.provider.provider_type, "google");
        assert_eq!(
            loaded.provider.google_api_key,
            Some("test-api-key".to_string())
        );
        assert_eq!(loaded.cache.memory_size, 4 * 1024 * 1024 * 1024);
        assert_eq!(loaded.download.timeout, 60);
    }

    #[test]
    fn test_load_nonexistent_returns_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("nonexistent.ini");

        let config = ConfigFile::load_from(&config_path).unwrap();
        let default = ConfigFile::default();

        assert_eq!(
            config.provider.provider_type,
            default.provider.provider_type
        );
        assert_eq!(config.download.timeout, default.download.timeout);
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
        assert_eq!(config.cache.memory_size, 2 * 1024 * 1024 * 1024);
        assert_eq!(config.texture.format, DdsFormat::BC1);
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
    fn test_packages_config_defaults() {
        let config = ConfigFile::default();
        assert!(config.packages.library_url.is_none());
        assert!(config.packages.temp_dir.is_none());
    }
}
