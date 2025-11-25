//! DDS (DirectX Surface) texture encoding for X-Plane compatibility.
//!
//! This module provides functionality to encode RGBA images into DDS format
//! with BC1/BC3 (DXT1/DXT5) compression and mipmap generation.
//!
//! # Features
//!
//! - **BC1/DXT1 Compression**: 4:1 compression for RGB(A) textures
//! - **BC3/DXT5 Compression**: 4:1 compression with full 8-bit alpha channel
//! - **Mipmap Generation**: Automatic mipmap chain creation with box filtering
//! - **Flexible API**: Support for custom mipmap chains and various configurations
//!
//! # Example
//!
//! ```no_run
//! use xearthlayer::dds::{DdsEncoder, DdsFormat};
//! use image::RgbaImage;
//!
//! // Create a test image
//! let image = RgbaImage::new(256, 256);
//!
//! // Encode to BC1 with mipmaps
//! let encoder = DdsEncoder::new(DdsFormat::BC1);
//! let dds_data = encoder.encode(&image).unwrap();
//!
//! // Save to file
//! std::fs::write("output.dds", dds_data).unwrap();
//! ```
//!
//! # Format Details
//!
//! ## BC1 (DXT1)
//!
//! - Block size: 8 bytes per 4×4 pixels (0.5 bytes per pixel)
//! - Color: Two RGB565 endpoints + 2-bit indices
//! - Alpha: 1-bit or none
//! - Best for: Opaque textures, simple transparency
//!
//! ## BC3 (DXT5)
//!
//! - Block size: 16 bytes per 4×4 pixels (1 byte per pixel)
//! - Color: Same as BC1 (8 bytes)
//! - Alpha: Two endpoints + 3-bit indices (8 bytes)
//! - Best for: Textures with smooth alpha gradients
//!
//! # Performance
//!
//! Encoding a 4096×4096 image with 5 mipmap levels typically takes ~1 second
//! on modern hardware. The implementation uses:
//!
//! - Simple bounding box color quantization (fast, good quality)
//! - Box filter for mipmap generation (fast, acceptable quality)
//! - Sequential block processing (parallel processing possible)
//!
//! # Compatibility
//!
//! Generated DDS files are compatible with:
//! - X-Plane flight simulator
//! - DirectX 9+ texture loaders
//! - OpenGL texture compression extensions
//! - Most DDS viewing/editing tools

mod bc1;
mod bc3;
mod conversion;
mod encoder;
mod header;
mod mipmap;
mod types;

// Public API
pub use encoder::DdsEncoder;
pub use types::{DdsError, DdsFormat, DdsHeader};

// Re-export for advanced usage
pub use mipmap::MipmapGenerator;
