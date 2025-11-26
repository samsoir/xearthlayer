//! Configuration types for XEarthLayer components.
//!
//! This module provides structured configuration objects that group related
//! parameters together, following SOLID principles by:
//!
//! - **SRP**: Each config struct handles one concern
//! - **OCP**: New config types can be added without modifying existing ones
//! - **DIP**: Components depend on config traits/structs, not raw parameters
//!
//! # Example
//!
//! ```
//! use xearthlayer::config::{TextureConfig, DownloadConfig};
//! use xearthlayer::dds::DdsFormat;
//!
//! // Create texture configuration
//! let texture_config = TextureConfig::new(DdsFormat::BC1)
//!     .with_mipmap_count(5);
//!
//! // Create download configuration
//! let download_config = DownloadConfig::default();
//! ```

mod download;
mod texture;
mod xplane;

pub use download::DownloadConfig;
pub use texture::TextureConfig;
pub use xplane::{
    derive_mountpoint, detect_custom_scenery, detect_xplane_install, XPlanePathError,
};
