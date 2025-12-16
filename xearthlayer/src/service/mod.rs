//! High-level service facade for XEarthLayer operations.
//!
//! This module provides a simplified API that encapsulates all component
//! wiring and configuration, following the Facade pattern.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{XEarthLayerService, ServiceConfig};
//! use xearthlayer::provider::ProviderConfig;
//! use xearthlayer::config::{TextureConfig, DownloadConfig};
//! use xearthlayer::dds::DdsFormat;
//!
//! // Create service configuration
//! let config = ServiceConfig::builder()
//!     .texture(TextureConfig::new(DdsFormat::BC1).with_mipmap_count(5))
//!     .download(DownloadConfig::default())
//!     .build();
//!
//! // Create service
//! let service = XEarthLayerService::new(config, ProviderConfig::bing())?;
//!
//! // Download a tile
//! let data = service.download_tile(37.7749, -122.4194, 15)?;
//! ```

mod builder;
mod config;
mod dds_handler;
mod error;
mod facade;
mod fuse_mount;
mod network_logger;

pub use config::{ServiceConfig, ServiceConfigBuilder};
pub use dds_handler::DdsHandlerBuilder;
pub use error::ServiceError;
pub use facade::XEarthLayerService;
pub use fuse_mount::{FuseMountConfig, FuseMountService};
