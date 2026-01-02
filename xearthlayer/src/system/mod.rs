//! System hardware detection and recommendation utilities.
//!
//! This module provides platform-independent detection of system hardware
//! (CPU, memory, storage) and recommends optimal XEarthLayer configuration
//! based on the detected capabilities.
//!
//! # Example
//!
//! ```
//! use std::path::Path;
//! use xearthlayer::system::SystemInfo;
//!
//! let cache_path = Path::new("/home/user/.xearthlayer/cache");
//! let info = SystemInfo::detect(cache_path);
//!
//! println!("CPU cores: {}", info.cpu_cores);
//! println!("Memory: {}", info.memory_display());
//! println!("Storage: {}", info.storage_display());
//! println!("Recommended cache: {}", info.recommended_memory_cache_display());
//! ```
//!
//! # Design Notes
//!
//! This module is designed to be reusable across different frontends:
//! - CLI setup wizard (`xearthlayer setup`)
//! - GTK4 desktop application (future)
//! - Any other UI that needs hardware detection

mod hardware;
mod recommendations;

pub use hardware::{detect_cpu_cores, detect_total_memory, StorageType, SystemInfo};
pub use recommendations::{
    recommended_disk_cache, recommended_disk_io_profile, recommended_memory_cache,
    RecommendedSettings,
};
