//! XEarthLayer - Satellite imagery streaming for X-Plane
//!
//! This library provides the core functionality for streaming satellite imagery
//! to X-Plane flight simulator via a FUSE virtual filesystem.
//!
//! # High-Level API
//!
//! For most use cases, the [`service`] module provides a simplified facade:
//!
//! ```ignore
//! use xearthlayer::service::{XEarthLayerService, ServiceConfig};
//! use xearthlayer::provider::ProviderConfig;
//!
//! let config = ServiceConfig::default();
//! let service = XEarthLayerService::new(config, ProviderConfig::bing())?;
//!
//! // Mount FUSE filesystem for a scenery package
//! let handle = service.mount_package_async(package_path).await?;
//! ```

pub mod aircraft_position;
pub mod airport;
pub mod cache;
pub mod config;
pub mod coord;
pub mod dds;
pub mod diagnostics;
pub mod executor;
pub mod fuse;
pub mod geo_index;
pub mod jobs;
pub mod log;
pub mod logging;
pub mod manager;
pub mod metrics;
pub mod ortho_union;
pub mod package;
pub mod panic;
pub mod patches;
pub mod prefetch;
pub mod provider;
pub mod publisher;
pub mod runtime;
pub mod scene_tracker;
pub mod service;
pub mod system;
pub mod tasks;
pub mod texture;
pub mod time;
pub mod xplane;

/// Version of the XEarthLayer library and CLI.
///
/// This is synchronized across all components in the workspace.
/// The version is defined in `Cargo.toml` and injected at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns a greeting message from XEarthLayer.
///
/// This is a placeholder function demonstrating the library architecture.
/// The actual implementation will be replaced with real functionality.
pub fn greeting() -> String {
    String::from("Hello from XEarthLayer!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greeting_returns_hello_message() {
        let message = greeting();
        assert_eq!(message, "Hello from XEarthLayer!");
    }

    #[test]
    fn test_greeting_is_not_empty() {
        let message = greeting();
        assert!(!message.is_empty(), "Greeting should not be empty");
    }

    #[test]
    fn test_coord_module_exists() {
        // Verify coord module is accessible
        use crate::coord::to_tile_coords;
        let result = to_tile_coords(40.7128, -74.0060, 16);
        assert!(result.is_ok());
    }
}
