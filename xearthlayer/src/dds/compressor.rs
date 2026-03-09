//! Block compressor trait and implementations for DDS texture encoding.
//!
//! This module provides the [`BlockCompressor`] trait that abstracts BCn block
//! compression, allowing different backends to be swapped in without changing
//! the DDS encoding pipeline.
//!
//! # Available Implementations
//!
//! - [`IspcCompressor`] — SIMD-optimized via Intel ISPC (default, 5-10× faster)
//! - [`SoftwareCompressor`] — Pure-Rust fallback (no external dependencies)
//!
//! # Future Implementations
//!
//! - GPU compressor via `wgpu` compute shaders (planned)

use super::types::{DdsError, DdsFormat};
use image::RgbaImage;

// =============================================================================
// Trait
// =============================================================================

/// Trait for block-compressing an RGBA image to BCn format.
///
/// Implementations handle the full-surface compression in a single call.
/// The DDS encoder handles mipmap generation and header assembly; this trait
/// is only responsible for compressing one image level.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` for use in the concurrent tile
/// generation pipeline.
pub trait BlockCompressor: Send + Sync {
    /// Compress an RGBA image to BCn block format.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image (dimensions should be multiples of 4)
    /// * `format` - Target compression format (BC1 or BC3)
    ///
    /// # Returns
    ///
    /// Compressed block data (without DDS header).
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;

    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;
}

// =============================================================================
// ISPC compressor (intel_tex_2)
// =============================================================================

/// SIMD-optimized block compressor using Intel's ISPC texture compression.
///
/// This is the recommended compressor for production use. It processes
/// multiple 4×4 blocks simultaneously using CPU SIMD instructions (SSE4/AVX2),
/// providing 5-10× speedup over pure-Rust compression.
///
/// Prebuilt ISPC kernels are included for Linux (x86_64), macOS (x86_64,
/// aarch64), and Windows (x86_64) — no ISPC compiler needed at build time.
pub struct IspcCompressor;

impl BlockCompressor for IspcCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let rgba_data = image.as_raw();
        let stride = width * 4; // tightly packed RGBA

        let surface = intel_tex_2::RgbaSurface {
            data: rgba_data,
            width,
            height,
            stride,
        };

        let compressed = match format {
            DdsFormat::BC1 => intel_tex_2::bc1::compress_blocks(&surface),
            DdsFormat::BC3 => intel_tex_2::bc3::compress_blocks(&surface),
        };

        Ok(compressed)
    }

    fn name(&self) -> &str {
        "ispc"
    }
}

// =============================================================================
// Software compressor (pure Rust fallback)
// =============================================================================

/// Pure-Rust block compressor using hand-rolled BC1/BC3 encoding.
///
/// This compressor processes blocks one at a time without SIMD acceleration.
/// It exists as a fallback for platforms where ISPC prebuilt kernels are not
/// available, and for testing/comparison purposes.
pub struct SoftwareCompressor;

impl BlockCompressor for SoftwareCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);

        let block_size: u32 = match format {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        };

        let mut output = Vec::with_capacity((blocks_wide * blocks_high * block_size) as usize);

        for block_y in 0..blocks_high {
            for block_x in 0..blocks_wide {
                let block = extract_block(image, block_x, block_y);
                let compressed = match format {
                    DdsFormat::BC1 => super::bc1::Bc1Encoder::compress_block(&block).to_vec(),
                    DdsFormat::BC3 => super::bc3::Bc3Encoder::compress_block(&block).to_vec(),
                };
                output.extend_from_slice(&compressed);
            }
        }

        Ok(output)
    }

    fn name(&self) -> &str {
        "software"
    }
}

/// Extract a 4×4 pixel block from the image, padding with black if needed.
fn extract_block(image: &RgbaImage, block_x: u32, block_y: u32) -> [[u8; 4]; 16] {
    let mut block = [[0u8; 4]; 16];

    for y in 0..4 {
        for x in 0..4 {
            let pixel_x = block_x * 4 + x;
            let pixel_y = block_y * 4 + y;

            if pixel_x < image.width() && pixel_y < image.height() {
                let p = image.get_pixel(pixel_x, pixel_y);
                block[(y * 4 + x) as usize] = [p[0], p[1], p[2], p[3]];
            }
        }
    }

    block
}

// =============================================================================
// Default compressor selection
// =============================================================================

/// Create the default block compressor for this platform.
///
/// Returns the ISPC compressor, which uses SIMD-optimized encoding.
/// In the future, this may auto-detect GPU availability and return
/// a wgpu-based compressor when a suitable GPU is present.
pub fn default_compressor() -> Box<dyn BlockCompressor> {
    Box::new(IspcCompressor)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ispc_compress_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        // 1 block × 8 bytes
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_ispc_compress_4x4_bc3() {
        let image = RgbaImage::new(4, 4);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        // 1 block × 16 bytes
        assert_eq!(result.len(), 16);
    }

    #[test]
    fn test_ispc_compress_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        // 64×64 blocks × 8 bytes
        assert_eq!(result.len(), 32768);
    }

    #[test]
    fn test_ispc_compress_256x256_bc3() {
        let image = RgbaImage::new(256, 256);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        // 64×64 blocks × 16 bytes
        assert_eq!(result.len(), 65536);
    }

    #[test]
    fn test_software_compress_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let compressor = SoftwareCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_software_compress_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let compressor = SoftwareCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 32768);
    }

    #[test]
    fn test_ispc_and_software_same_output_size() {
        let image = RgbaImage::new(256, 256);
        let ispc = IspcCompressor;
        let software = SoftwareCompressor;

        let ispc_result = ispc.compress(&image, DdsFormat::BC1).unwrap();
        let sw_result = software.compress(&image, DdsFormat::BC1).unwrap();

        assert_eq!(ispc_result.len(), sw_result.len());
    }

    #[test]
    fn test_compressor_zero_dimensions() {
        let image = RgbaImage::new(0, 0);
        let compressor = IspcCompressor;
        assert!(compressor.compress(&image, DdsFormat::BC1).is_err());
    }

    #[test]
    fn test_default_compressor_is_ispc() {
        let compressor = default_compressor();
        assert_eq!(compressor.name(), "ispc");
    }

    #[test]
    fn test_compressor_names() {
        assert_eq!(IspcCompressor.name(), "ispc");
        assert_eq!(SoftwareCompressor.name(), "software");
    }
}
