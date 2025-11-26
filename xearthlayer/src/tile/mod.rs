//! Tile generation abstraction layer.
//!
//! This module provides the `TileGenerator` trait and related types for
//! abstracting tile generation from the FUSE filesystem, following the
//! Dependency Inversion Principle.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    FUSE Filesystem                          │
//! │              (depends on Arc<dyn TileGenerator>)            │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   TileGenerator Trait                       │
//! │            generate(&TileRequest) -> Vec<u8>                │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!               ┌──────────────┴──────────────┐
//!               ▼                             ▼
//! ┌─────────────────────────┐   ┌─────────────────────────────┐
//! │  DefaultTileGenerator   │   │    MockTileGenerator        │
//! │  (orchestrator+encoder) │   │    (for testing)            │
//! └─────────────────────────┘   └─────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::tile::{DefaultTileGenerator, TileGenerator, TileRequest};
//! use xearthlayer::texture::{DdsTextureEncoder, TextureEncoder};
//! use xearthlayer::orchestrator::TileOrchestrator;
//! use xearthlayer::provider::{BingMapsProvider, ReqwestClient};
//! use xearthlayer::dds::DdsFormat;
//! use std::sync::Arc;
//!
//! // Create the tile generator (requires network access)
//! let http_client = ReqwestClient::new()?;
//! let provider = BingMapsProvider::new(http_client);
//! let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
//! let encoder: Arc<dyn TextureEncoder> = Arc::new(
//!     DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5)
//! );
//!
//! let generator: Arc<dyn TileGenerator> = Arc::new(
//!     DefaultTileGenerator::new(orchestrator, encoder)
//! );
//!
//! // Use through the trait interface
//! let request = TileRequest::new(37, -123, 16);
//! let data = generator.generate(&request)?;
//! ```

mod default;
mod error;
mod generator;
mod parallel;
mod request;

pub use default::DefaultTileGenerator;
pub use error::TileGeneratorError;
pub use generator::TileGenerator;
pub use parallel::{ParallelConfig, ParallelTileGenerator};
pub use request::TileRequest;
