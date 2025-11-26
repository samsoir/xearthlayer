//! Configuration types for XEarthLayer components.
//!
//! This module provides structured configuration objects that group related
//! parameters together, following SOLID principles by:
//!
//! - **SRP**: Each config struct handles one concern
//! - **OCP**: New config types can be added without modifying existing ones
//! - **DIP**: Components depend on config traits/structs, not raw parameters
//!
//! # Configuration File
//!
//! XEarthLayer uses a configuration file at `~/.xearthlayer/config.ini`.
//! Use [`ConfigFile::load()`] to load settings or [`ConfigFile::ensure_exists()`]
//! to create a default config file.
//!
//! # Example
//!
//! ```
//! use xearthlayer::config::{TextureConfig, DownloadConfig, ConfigFile};
//! use xearthlayer::dds::DdsFormat;
//!
//! // Load configuration from file (or use defaults)
//! let config = ConfigFile::load().unwrap_or_default();
//!
//! // Create texture configuration
//! let texture_config = TextureConfig::new(DdsFormat::BC1)
//!     .with_mipmap_count(5);
//!
//! // Create download configuration
//! let download_config = DownloadConfig::default();
//! ```

mod download;
mod file;
mod size;
mod texture;
mod xplane;

pub use download::DownloadConfig;
pub use file::{
    config_directory, config_file_path, CacheSettings, ConfigFile, ConfigFileError,
    DownloadSettings, GenerationSettings, LoggingSettings, ProviderSettings, TextureSettings,
    XPlaneSettings,
};
pub use size::{format_size, parse_size, Size, SizeParseError};
pub use texture::TextureConfig;
pub use xplane::{
    derive_mountpoint, detect_custom_scenery, detect_xplane_install, XPlanePathError,
};
