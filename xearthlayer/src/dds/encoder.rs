//! DDS encoder - main API for encoding images to DDS format.
//!
//! The encoder delegates compression to a backend via [`CompressorBackend`]:
//! - [`ImageCompressor`] backends (ISPC, Software) compress one level at a time;
//!   the encoder manages mipmap iteration via [`MipmapStream`].
//! - [`MipmapCompressor`] backends (GPU channel) own the full mipmap pipeline;
//!   the encoder only handles DDS header assembly.

use crate::dds::compressor::{default_compressor, ImageCompressor, MipmapCompressor};
use crate::dds::mipmap::{MipmapGenerator, MipmapStream};
use crate::dds::types::{DdsError, DdsFormat, DdsHeader};
use image::RgbaImage;
use std::sync::Arc;
use tracing::debug;

/// Backend for compressing image data.
enum CompressorBackend {
    /// Single-level compressor — encoder manages mipmap iteration.
    Image(Arc<dyn ImageCompressor>),
    /// Full-pipeline compressor — backend owns mipmap iteration.
    Mipmap(Arc<dyn MipmapCompressor>),
}

/// DDS encoder configuration.
pub struct DdsEncoder {
    format: DdsFormat,
    generate_mipmaps: bool,
    mipmap_count: Option<usize>,
    backend: CompressorBackend,
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
            backend: CompressorBackend::Image(default_compressor()),
        }
    }

    /// Use a specific image compressor backend (ISPC, Software).
    ///
    /// The encoder manages mipmap iteration internally via [`MipmapStream`].
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
    pub fn with_compressor(mut self, compressor: Arc<dyn ImageCompressor>) -> Self {
        self.backend = CompressorBackend::Image(compressor);
        self
    }

    /// Use a mipmap compressor backend that owns the full pipeline.
    ///
    /// The backend receives the source image and returns compressed data
    /// for all mipmap levels. This avoids per-level channel round-trips
    /// for GPU-based compression.
    pub fn with_mipmap_compressor(mut self, compressor: Arc<dyn MipmapCompressor>) -> Self {
        self.backend = CompressorBackend::Mipmap(compressor);
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
    /// Takes ownership of the source image to avoid cloning. Mipmap levels
    /// are generated and compressed incrementally — only one uncompressed
    /// level is held in memory at a time.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image (ownership transferred)
    ///
    /// # Returns
    ///
    /// Complete DDS file as bytes (header + compressed data)
    ///
    /// # Errors
    ///
    /// Returns error if image dimensions are invalid or compression fails.
    pub fn encode(&self, image: RgbaImage) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        match &self.backend {
            CompressorBackend::Mipmap(compressor) => {
                let count = if self.generate_mipmaps {
                    self.resolve_mipmap_count(width, height)
                } else {
                    1
                };
                self.encode_with_mipmap_compressor(compressor, image, width, height, count)
            }
            CompressorBackend::Image(_) => {
                if self.generate_mipmaps {
                    let count = self.resolve_mipmap_count(width, height);
                    self.encode_mipmap_stream(image, width, height, count)
                } else {
                    self.encode_single_level(image, width, height)
                }
            }
        }
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

    /// Compress a single image (no mipmaps) into a DDS file.
    fn encode_single_level(
        &self,
        image: RgbaImage,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, DdsError> {
        let header = DdsHeader::new(width, height, 1, self.format);
        let mut output = header.to_bytes();

        let uncompressed_bytes = (width * height * 4) as usize;
        let compressed = self.compress_image(&image)?;

        debug!(
            width,
            height,
            format = ?self.format,
            uncompressed_bytes,
            compressed_bytes = compressed.len(),
            total_dds_bytes = output.len() + compressed.len(),
            "Single-level encode complete (no mipmaps)"
        );

        output.extend_from_slice(&compressed);
        Ok(output)
    }

    /// Compress an image with its mipmap chain using a streaming pipeline.
    ///
    /// Each mipmap level is generated, compressed, and dropped before the
    /// next is produced — only one uncompressed level lives in memory at a time.
    fn encode_mipmap_stream(
        &self,
        image: RgbaImage,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<u8>, DdsError> {
        let header = DdsHeader::new(width, height, count as u32, self.format);
        let mut output = header.to_bytes();

        debug!(
            width,
            height,
            mipmap_count = count,
            format = ?self.format,
            "Starting fused mipmap encode pipeline"
        );

        for (level_idx, level) in MipmapStream::new(image, count).enumerate() {
            let level_bytes = (level.width() * level.height() * 4) as usize;
            let compressed = self.compress_image(&level)?;

            debug!(
                level = level_idx,
                level_width = level.width(),
                level_height = level.height(),
                uncompressed_bytes = level_bytes,
                compressed_bytes = compressed.len(),
                "Compressed mipmap level"
            );

            output.extend_from_slice(&compressed);
            // `level` dropped here — memory reclaimed before next iteration
        }

        debug!(
            total_dds_bytes = output.len(),
            "Fused mipmap encode complete"
        );

        Ok(output)
    }

    /// Resolve the number of mipmap levels to generate for an image of the
    /// given dimensions, respecting any explicit `mipmap_count` override.
    ///
    /// When no explicit count is set, counts levels from full size down to 1×1.
    fn resolve_mipmap_count(&self, width: u32, height: u32) -> usize {
        self.mipmap_count
            .unwrap_or_else(|| MipmapGenerator::full_chain_count(width, height))
    }

    /// Delegate compression to a MipmapCompressor backend.
    ///
    /// The backend owns mipmap generation and compression. This method
    /// only handles DDS header assembly.
    fn encode_with_mipmap_compressor(
        &self,
        compressor: &Arc<dyn MipmapCompressor>,
        image: RgbaImage,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<u8>, DdsError> {
        debug!(
            width,
            height,
            mipmap_count = count,
            format = ?self.format,
            backend = compressor.name(),
            "Delegating mipmap chain to MipmapCompressor"
        );

        let compressed = compressor.compress_mipmap_chain(image, self.format, count)?;

        let header = DdsHeader::new(width, height, count as u32, self.format);
        let mut output = header.to_bytes();
        output.extend_from_slice(&compressed);

        debug!(
            total_dds_bytes = output.len(),
            compressed_bytes = compressed.len(),
            "MipmapCompressor encode complete"
        );

        Ok(output)
    }

    /// Compress a single image to BC1/BC3 format using the configured compressor.
    fn compress_image(&self, image: &RgbaImage) -> Result<Vec<u8>, DdsError> {
        match &self.backend {
            CompressorBackend::Image(compressor) => compressor.compress(image, self.format),
            CompressorBackend::Mipmap(_) => {
                unreachable!("compress_image called with MipmapCompressor backend")
            }
        }
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Header (128) + 1 block (16) = 144 bytes
        assert_eq!(dds.len(), 128 + 16);
    }

    #[test]
    fn test_encode_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

        let result = encoder.encode(image);
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

    #[test]
    fn test_fused_encode_matches_pregenerated() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);

        // Generate expected output via the pre-generated path
        let mipmaps = MipmapGenerator::generate_chain_with_count(&image, 5);
        let expected = encoder.encode_with_mipmaps(&mipmaps).unwrap();

        // Fused pipeline should produce identical bytes
        let result = encoder.encode(image).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_encode_dispatches_to_mipmap_compressor() {
        use crate::dds::compressor::MipmapCompressor;

        struct CapturingMipmapCompressor {
            expected_bc1_data: Vec<u8>,
        }

        impl MipmapCompressor for CapturingMipmapCompressor {
            fn compress_mipmap_chain(
                &self,
                image: RgbaImage,
                format: DdsFormat,
                mipmap_count: usize,
            ) -> Result<Vec<u8>, DdsError> {
                assert_eq!(image.dimensions(), (256, 256));
                assert_eq!(format, DdsFormat::BC1);
                assert_eq!(mipmap_count, 5);
                Ok(self.expected_bc1_data.clone())
            }

            fn name(&self) -> &str {
                "capturing-mock"
            }
        }

        // Compute expected compressed size: 5 levels of BC1 from 256×256
        // 256: 64×64×8=32768, 128: 32×32×8=8192, 64: 16×16×8=2048,
        // 32: 8×8×8=512, 16: 4×4×8=128
        let data_size = 32768 + 8192 + 2048 + 512 + 128;
        let mock_data = vec![0xAA; data_size];

        let compressor = CapturingMipmapCompressor {
            expected_bc1_data: mock_data.clone(),
        };

        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_mipmap_compressor(Arc::new(compressor));

        let image = RgbaImage::new(256, 256);
        let result = encoder.encode(image).unwrap();

        // Should be DDS header (128) + mock data
        assert_eq!(result.len(), 128 + data_size);
        // Verify header magic
        assert_eq!(&result[0..4], b"DDS ");
        // Verify our mock data follows the header
        assert_eq!(&result[128..], &mock_data[..]);
    }

    #[test]
    fn test_mipmap_compressor_output_matches_image_compressor() {
        use crate::dds::compressor::{ImageCompressor, MipmapCompressor, SoftwareCompressor};
        use crate::dds::mipmap::MipmapStream;

        /// MipmapCompressor that uses SoftwareCompressor internally,
        /// matching the ImageCompressor path's compression output.
        struct SoftwareMipmapCompressor;

        impl MipmapCompressor for SoftwareMipmapCompressor {
            fn compress_mipmap_chain(
                &self,
                image: RgbaImage,
                format: DdsFormat,
                mipmap_count: usize,
            ) -> Result<Vec<u8>, DdsError> {
                let sw = SoftwareCompressor;
                let mut output = Vec::new();
                for level in MipmapStream::new(image, mipmap_count) {
                    let compressed = sw.compress(&level, format)?;
                    output.extend_from_slice(&compressed);
                }
                Ok(output)
            }

            fn name(&self) -> &str {
                "software-mipmap"
            }
        }

        let image = RgbaImage::new(256, 256);

        // ImageCompressor path (fused MipmapStream loop)
        let image_encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_compressor(Arc::new(SoftwareCompressor));
        let image_result = image_encoder.encode(image.clone()).unwrap();

        // MipmapCompressor path (backend-owned pipeline)
        let mipmap_encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_mipmap_compressor(Arc::new(SoftwareMipmapCompressor));
        let mipmap_result = mipmap_encoder.encode(image).unwrap();

        // Byte-for-byte parity
        assert_eq!(image_result, mipmap_result);
    }
}
