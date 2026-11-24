//! Texture encoding abstractions for XEarthLayer.
//!
//! This module provides a trait-based abstraction for texture encoding,
//! following the Liskov Substitution Principle (LSP) to allow different
//! encoding strategies to be swapped without modifying consumers.
//!
//! # Architecture
//!
//! The [`TextureEncoder`] trait defines the interface for encoding RGBA
//! images into texture formats. This allows the FUSE filesystem to work
//! with any encoder implementation without direct coupling.
//!
//! ```text
//! ┌─────────────────────┐
//! │   FUSE Filesystem   │
//! │                     │
//! │ Arc<dyn TextureEncoder>
//! └──────────┬──────────┘
//!            │
//!            ▼
//! ┌─────────────────────┐
//! │  TextureEncoder     │ (trait)
//! └──────────┬──────────┘
//!            │
//!       ┌────┴────┐
//!       ▼         ▼
//! ┌──────────┐ ┌──────────┐
//! │DdsTexture│ │  Future  │
//! │ Encoder  │ │ Encoders │
//! └──────────┘ └──────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use xearthlayer::texture::{TextureEncoder, DdsTextureEncoder};
//! use xearthlayer::dds::DdsFormat;
//! use std::sync::Arc;
//!
//! // Create encoder as trait object
//! let encoder: Arc<dyn TextureEncoder> = Arc::new(
//!     DdsTextureEncoder::new(DdsFormat::BC1)
//!         .with_mipmap_count(5)
//! );
//!
//! // Use through trait interface
//! println!("Encoder: {}", encoder.name());
//! println!("Extension: {}", encoder.extension());
//! println!("Expected size for 4096×4096: {} bytes", encoder.expected_size(4096, 4096));
//! ```
//!
//! # Available Encoders
//!
//! - [`DdsTextureEncoder`] - Encodes to DDS format with BC1 compression
//!
//! # Future Encoders
//!
//! The trait design allows for future encoder implementations:
//! - KTX2 encoder (if X-Plane adds support)
//! - Mock encoder for testing

mod dds;
mod encoder;
mod error;

pub use dds::DdsTextureEncoder;
pub use encoder::TextureEncoder;
pub use error::TextureError;
