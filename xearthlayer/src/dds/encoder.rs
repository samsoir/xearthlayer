//! DDS encoder - main API for encoding images to DDS format.
//!
//! The encoder delegates block compression to a [`BlockCompressor`] backend,
//! handling mipmap generation and DDS header assembly itself. This separation
//! allows swapping compression backends (ISPC SIMD, GPU compute, pure Rust)
//! without changing the encoding pipeline.

use crate::dds::compressor::{default_compressor, BlockCompressor};
use crate::dds::mipmap::MipmapGenerator;
use crate::dds::types::{DdsError, DdsFormat, DdsHeader};
use image::RgbaImage;
use std::sync::Arc;

/// DDS encoder configuration.
pub struct DdsEncoder {
    format: DdsFormat,
    generate_mipmaps: bool,
    mipmap_count: Option<usize>,
    compressor: Arc<dyn BlockCompressor>,
}

impl DdsEncoder {
    /// Create a new DDS encoder with the specified format.
    ///
    /// Uses the default ISPC SIMD compressor for block compression.
    /// By default, generates full mipmap chain down to 1×1.
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            generate_mipmaps: true,
            mipmap_count: None,
            compressor: default_compressor(),
        }
    }

    /// Use a specific block compressor backend.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::dds::{DdsEncoder, DdsFormat, SoftwareCompressor};
    /// use std::sync::Arc;
    ///
    /// let encoder = DdsEncoder::new(DdsFormat::BC1)
    ///     .with_compressor(Arc::new(SoftwareCompressor));
    /// ```
    pub fn with_compressor(mut self, compressor: Arc<dyn BlockCompressor>) -> Self {
        self.compressor = compressor;
        self
    }

    /// Disable mipmap generation.
    pub fn without_mipmaps(mut self) -> Self {
        self.generate_mipmaps = false;
        self
    }

    /// Set specific number of mipmap levels to generate.
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.generate_mipmaps = true;
        self.mipmap_count = Some(count);
        self
    }

    /// Encode RGBA image to DDS format.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image
    ///
    /// # Returns
    ///
    /// Complete DDS file as bytes (header + compressed data)
    ///
    /// # Errors
    ///
    /// Returns error if image dimensions are invalid or compression fails.
    pub fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, DdsError> {
        // Generate mipmaps
        let mipmaps = if self.generate_mipmaps {
            if let Some(count) = self.mipmap_count {
                MipmapGenerator::generate_chain_with_count(image, count)
            } else {
                MipmapGenerator::generate_chain(image)
            }
        } else {
            vec![image.clone()]
        };

        self.encode_with_mipmaps(&mipmaps)
    }

    /// Encode with pre-generated mipmap chain.
    ///
    /// # Arguments
    ///
    /// * `mipmaps` - Mipmap chain (level 0 = full resolution)
    ///
    /// # Returns
    ///
    /// Complete DDS file as bytes
    pub fn encode_with_mipmaps(&self, mipmaps: &[RgbaImage]) -> Result<Vec<u8>, DdsError> {
        if mipmaps.is_empty() {
            return Err(DdsError::InvalidMipmapChain(
                "Empty mipmap chain".to_string(),
            ));
        }

        let base_image = &mipmaps[0];
        let width = base_image.width();
        let height = base_image.height();

        // Validate dimensions
        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        // Create header
        let header = DdsHeader::new(width, height, mipmaps.len() as u32, self.format);
        let mut output = header.to_bytes();

        // Compress each mipmap level
        for mipmap in mipmaps {
            let compressed = self.compress_image(mipmap)?;
            output.extend_from_slice(&compressed);
        }

        Ok(output)
    }

    /// Compress a single image to BC1/BC3 format using the configured compressor.
    fn compress_image(&self, image: &RgbaImage) -> Result<Vec<u8>, DdsError> {
        self.compressor.compress(image, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_new() {
        let encoder = DdsEncoder::new(DdsFormat::BC1);
        assert!(encoder.generate_mipmaps);
        assert_eq!(encoder.mipmap_count, None);
    }

    #[test]
    fn test_encoder_without_mipmaps() {
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();
        assert!(!encoder.generate_mipmaps);
    }

    #[test]
    fn test_encoder_with_mipmap_count() {
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        assert!(encoder.generate_mipmaps);
        assert_eq!(encoder.mipmap_count, Some(5));
    }

    #[test]
    fn test_encode_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Header (128) + 1 block (8) = 136 bytes
        assert_eq!(dds.len(), 128 + 8);

        // Check magic
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn test_encode_4x4_bc3() {
        let image = RgbaImage::new(4, 4);
        let encoder = DdsEncoder::new(DdsFormat::BC3).without_mipmaps();

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Header (128) + 1 block (16) = 144 bytes
        assert_eq!(dds.len(), 128 + 16);
    }

    #[test]
    fn test_encode_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // 256×256 = 64×64 blocks, each 8 bytes
        // Header (128) + data (32768) = 32896
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_encode_with_mipmaps() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Should have 5 mipmap levels: 256, 128, 64, 32, 16
        // Sizes: 32768 + 8192 + 2048 + 512 + 128 = 43648
        // Total: 128 + 43648 = 43776
        assert_eq!(dds.len(), 43776);
    }

    #[test]
    fn test_encode_empty_mipmap_chain() {
        let encoder = DdsEncoder::new(DdsFormat::BC1);
        let mipmaps: Vec<RgbaImage> = vec![];

        let result = encoder.encode_with_mipmaps(&mipmaps);
        assert!(result.is_err());

        match result {
            Err(DdsError::InvalidMipmapChain(_)) => {}
            _ => panic!("Expected InvalidMipmapChain error"),
        }
    }

    #[test]
    fn test_encode_zero_dimensions() {
        let image = RgbaImage::new(0, 0);
        let encoder = DdsEncoder::new(DdsFormat::BC1);

        let result = encoder.encode(&image);
        assert!(result.is_err());

        match result {
            Err(DdsError::InvalidDimensions(0, 0)) => {}
            _ => panic!("Expected InvalidDimensions error"),
        }
    }

    #[test]
    fn test_compress_with_software_compressor() {
        use crate::dds::SoftwareCompressor;
        use std::sync::Arc;

        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .without_mipmaps()
            .with_compressor(Arc::new(SoftwareCompressor));

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();
        // Header (128) + 64×64 blocks × 8 bytes = 32896
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_compress_with_ispc_compressor() {
        use crate::dds::IspcCompressor;
        use std::sync::Arc;

        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .without_mipmaps()
            .with_compressor(Arc::new(IspcCompressor));

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_compress_image_4x4() {
        let image = RgbaImage::new(4, 4);
        let encoder = DdsEncoder::new(DdsFormat::BC1);

        let compressed = encoder.compress_image(&image).unwrap();

        // 4×4 = 1 block, BC1 = 8 bytes
        assert_eq!(compressed.len(), 8);
    }

    #[test]
    fn test_compress_image_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1);

        let compressed = encoder.compress_image(&image).unwrap();

        // 256×256 = 64×64 blocks, each 8 bytes = 32768
        assert_eq!(compressed.len(), 32768);
    }

    #[test]
    fn test_compress_image_256x256_bc3() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC3);

        let compressed = encoder.compress_image(&image).unwrap();

        // 256×256 = 64×64 blocks, each 16 bytes = 65536
        assert_eq!(compressed.len(), 65536);
    }

    #[test]
    fn test_encode_non_power_of_2() {
        // DDS can handle non-power-of-2 dimensions
        let image = RgbaImage::new(100, 100);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        // 100×100 requires 25×25 blocks (round up)
        // 25 * 25 * 8 = 5000 bytes data
        let dds = result.unwrap();
        assert_eq!(dds.len(), 128 + 5000);
    }

    #[test]
    fn test_full_encode_4096x4096_bc1() {
        let image = RgbaImage::new(4096, 4096);
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // 5 levels: 4096, 2048, 1024, 512, 256
        // 4096: 1024×1024 blocks * 8 = 8388608
        // 2048: 512×512 blocks * 8 = 2097152
        // 1024: 256×256 blocks * 8 = 524288
        // 512: 128×128 blocks * 8 = 131072
        // 256: 64×64 blocks * 8 = 32768
        // Total data: 11173888
        // Header: 128
        // Total: 11174016
        assert_eq!(dds.len(), 128 + 11_173_888);

        // Verify magic
        assert_eq!(&dds[0..4], b"DDS ");
    }
}
