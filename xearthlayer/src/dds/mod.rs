//! DDS (DirectX Surface) texture encoding for X-Plane compatibility.
//!
//! This module provides functionality to encode RGBA images into DDS format
//! with BC1 (DXT1) compression and mipmap generation.
//!
//! # Features
//!
//! - **BC1/DXT1 Compression**: 4:1 compression for RGB(A) textures
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
//! let dds_data = encoder.encode(image).unwrap();
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
//! ## Other formats
//!
//! Every encoder interface takes a [`DdsFormat`], the extension point for
//! future formats, but only BC1 is implemented: any other format is rejected
//! with [`DdsError::UnsupportedFormat`] (see [`DdsFormat`]). BC3 (DXT5) was
//! retired in #395.
//!
//! # File geometry
//!
//! [`layout`] owns the arithmetic of a DDS file: the header length, the bytes
//! of a level, the levels of a full chain and the length of a file. Every
//! site that sizes a DDS takes its numbers from there.
//!
//! # Performance
//!
//! The default ISPC SIMD encoder compresses a 4096×4096 image with its full
//! 13-level mipmap chain significantly faster than the pure-Rust fallback, using AVX2/SSE4
//! SIMD instructions to process multiple blocks simultaneously.
//!
//! Two compressor traits serve different backend models:
//!
//! [`ImageCompressor`] — single-level compression (caller manages mipmap iteration):
//! - [`IspcCompressor`] — SIMD-optimized (default, recommended)
//! - [`SoftwareCompressor`] — Pure-Rust fallback
//!
//! [`compress_level_into`] pads a level of any size to whole blocks and
//! compresses it with a given [`ImageCompressor`] straight into a slice of
//! the level's exact size: the per-level entry point used by [`DdsEncoder`],
//! which writes a whole file into one buffer allocated at its size, and by
//! night lighting, which names [`IspcCompressor`]. [`compress_level`] is the
//! same into a buffer of its own.
//!
//! [`MipmapCompressor`] — full-pipeline compression:
//! - [`GpuEncoderChannel`] — Worker-side mipmap streaming via GPU compute
//!
//! A [`MipmapCompressor`] may hand an image back ([`ChainOutcome::Returned`])
//! instead of compressing it: the GPU channel does when its worker has
//! failed (#420, health in [`gpu_health`]). [`DdsEncoder`] then encodes it
//! with its fallback [`ImageCompressor`] through the same in-place loop as
//! an [`ImageCompressor`] backend, so the file is byte for byte that
//! compressor's.
//!
//! # Compatibility
//!
//! Generated DDS files are compatible with:
//! - X-Plane flight simulator
//! - DirectX 9+ texture loaders
//! - OpenGL texture compression extensions
//! - Most DDS viewing/editing tools

mod bc1;
mod compressor;
mod conversion;
mod encoder;
pub mod gpu_channel;
pub mod gpu_health;
pub mod gpu_stats;
mod header;
pub mod layout;
mod mipmap;
mod types;

// Public API
pub use compressor::{
    compress_level, compress_level_into, create_gpu_resources, default_compressor, ChainOutcome,
    ImageCompressor, IspcCompressor, MipmapCompressor, SoftwareCompressor,
};
pub use encoder::{AwaitedEncode, DdsEncoder};
pub use gpu_channel::create_gpu_encoder_channels;
pub use types::{DdsError, DdsFormat, DdsHeader};

// Re-export for advanced usage
pub use mipmap::{MipmapGenerator, MipmapStream};
