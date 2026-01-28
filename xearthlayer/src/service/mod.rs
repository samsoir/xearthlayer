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
//! // Mount FUSE filesystem
//! let handle = service.mount_package_async(package_path).await?;
//! ```

mod builder;
mod cache_layer;
mod config;
mod error;
mod facade;
mod fuse_mount;
mod orchestrator;
mod orchestrator_config;
mod prewarm;
mod runtime_builder;

pub use cache_layer::CacheLayer;
pub use config::{ServiceConfig, ServiceConfigBuilder};
pub use error::ServiceError;
pub use facade::XEarthLayerService;
pub use fuse_mount::{FuseMountConfig, FuseMountService};
pub use orchestrator::{
    MountResult, PrefetchHandle, ServiceOrchestrator, StartupProgress, StartupResult,
};
pub use orchestrator_config::{OrchestratorConfig, PrefetchConfig, PrewarmConfig};
pub use prewarm::{PrewarmOrchestrator, PrewarmStartError, PrewarmStartResult};
// PrewarmHandle and PrewarmStatus are exported from prefetch module
pub use runtime_builder::RuntimeBuilder;
