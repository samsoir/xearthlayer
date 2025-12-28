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
//! let tile_data = service.download_tile(37.7749, -122.4194, 15)?;
//! ```

pub mod airport;
pub mod cache;
pub mod config;
pub mod coord;
pub mod dds;
pub mod diagnostics;
pub mod fuse;
pub mod log;
pub mod logging;
pub mod manager;
pub mod orchestrator;
pub mod package;
pub mod panic;
pub mod pipeline;
pub mod prefetch;
pub mod provider;
pub mod publisher;
pub mod service;
pub mod telemetry;
pub mod texture;
pub mod tile;
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
