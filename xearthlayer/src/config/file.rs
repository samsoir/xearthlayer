//! Configuration file handling for ~/.xearthlayer/config.ini.
//!
//! Loads and saves user configuration with sensible defaults.
//! Settings structs live in [`super::settings`], constants in [`super::defaults`],
//! parsing in [`super::parser`], and serialization in [`super::writer`].

use ini::Ini;
use std::path::{Path, PathBuf};
use thiserror::Error;

// Re-export settings structs so that `super::file::ConfigFile` etc. still resolve
// for sibling modules (upgrade.rs, keys.rs) that use `use super::file::*`.
pub use super::settings::*;

// Re-export all defaults so that `super::file::DEFAULT_*` and helper fns still resolve
// for config/mod.rs re-exports.
pub use super::defaults::*;

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
        super::parser::parse_ini(&ini)
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

        let content = super::writer::to_config_string(self);
        std::fs::write(path, content).map_err(|e| ConfigFileError::WriteError(e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ConfigFile::default();

        assert_eq!(config.provider.provider_type, "bing");
        assert!(config.provider.google_api_key.is_none());
        assert_eq!(config.cache.memory_size, DEFAULT_MEMORY_CACHE_SIZE);
        assert_eq!(config.cache.disk_size, DEFAULT_DISK_CACHE_SIZE);
        assert_eq!(config.texture.format, crate::dds::DdsFormat::BC1);
        assert_eq!(config.download.timeout, DEFAULT_DOWNLOAD_TIMEOUT_SECS);
        assert!(config.xplane.scenery_dir.is_none());
    }

    #[test]
    fn test_load_nonexistent_returns_defaults() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config_path = temp_dir.path().join("nonexistent.ini");

        let config = ConfigFile::load_from(&config_path).unwrap();
        let default = ConfigFile::default();

        assert_eq!(
            config.provider.provider_type,
            default.provider.provider_type
        );
        assert_eq!(config.download.timeout, default.download.timeout);
    }
}
