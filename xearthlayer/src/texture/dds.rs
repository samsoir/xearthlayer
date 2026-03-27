//! DDS texture encoder implementation.
//!
//! Provides a `TextureEncoder` implementation that encodes RGBA images
//! to DirectDraw Surface (DDS) format with BC1/BC3 compression.

use crate::dds::{default_compressor, BlockCompressor, DdsEncoder, DdsFormat, DdsHeader};
use crate::texture::{TextureEncoder, TextureError};
use image::RgbaImage;
use std::sync::Arc;

/// DDS texture encoder.
///
/// Encodes RGBA images to DDS format with configurable compression
/// and mipmap generation. The compressor backend is shared via `Arc`
/// across clones, allowing efficient reuse in the concurrent pipeline.
///
/// # Example
///
/// ```
/// use xearthlayer::texture::{DdsTextureEncoder, TextureEncoder};
/// use xearthlayer::dds::DdsFormat;
///
/// let encoder = DdsTextureEncoder::new(DdsFormat::BC1)
///     .with_mipmap_count(5);
///
/// assert_eq!(encoder.extension(), "dds");
/// assert_eq!(encoder.name(), "DDS BC1");
/// ```
pub struct DdsTextureEncoder {
    format: DdsFormat,
    mipmap_count: usize,
    compressor: Arc<dyn BlockCompressor>,
}

impl Clone for DdsTextureEncoder {
    fn clone(&self) -> Self {
        Self {
            format: self.format,
            mipmap_count: self.mipmap_count,
            compressor: Arc::clone(&self.compressor),
        }
    }
}

impl std::fmt::Debug for DdsTextureEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DdsTextureEncoder")
            .field("format", &self.format)
            .field("mipmap_count", &self.mipmap_count)
            .field("compressor", &self.compressor.name())
            .finish()
    }
}

impl DdsTextureEncoder {
    /// Create a new DDS encoder with the specified compression format.
    ///
    /// By default, generates 5 mipmap levels (4096, 2048, 1024, 512, 256).
    ///
    /// # Arguments
    ///
    /// * `format` - DDS compression format (BC1 or BC3)
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: 5,
            compressor: default_compressor(),
        }
    }

    /// Set the number of mipmap levels to generate.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of mipmap levels (including base level)
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::texture::DdsTextureEncoder;
    /// use xearthlayer::dds::DdsFormat;
    ///
    /// // Generate 3 mipmap levels: 4096, 2048, 1024
    /// let encoder = DdsTextureEncoder::new(DdsFormat::BC1)
    ///     .with_mipmap_count(3);
    /// ```
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = count;
        self
    }

    /// Set the block compressor backend.
    ///
    /// # Arguments
    ///
    /// * `compressor` - A shared block compressor implementation
    pub fn with_compressor(mut self, compressor: Arc<dyn BlockCompressor>) -> Self {
        self.compressor = compressor;
        self
    }

    /// Get the compression format.
    pub fn format(&self) -> DdsFormat {
        self.format
    }

    /// Get the mipmap count.
    pub fn mipmap_count(&self) -> usize {
        self.mipmap_count
    }

    /// Calculate total data size for a DDS file with mipmaps.
    ///
    /// # Arguments
    ///
    /// * `width` - Base image width
    /// * `height` - Base image height
    /// * `format` - Compression format
    /// * `mipmap_count` - Number of mipmap levels
    fn calculate_data_size(
        width: u32,
        height: u32,
        format: DdsFormat,
        mipmap_count: usize,
    ) -> usize {
        let block_size: usize = match format {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        };

        let mut total_size = 0;
        let mut w = width;
        let mut h = height;

        for _ in 0..mipmap_count {
            // Calculate blocks (round up to multiple of 4)
            let blocks_wide = w.div_ceil(4) as usize;
            let blocks_high = h.div_ceil(4) as usize;
            total_size += blocks_wide * blocks_high * block_size;

            // Next mipmap level
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }

        total_size
    }
}

impl TextureEncoder for DdsTextureEncoder {
    fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
        let encoder = DdsEncoder::new(self.format)
            .with_mipmap_count(self.mipmap_count)
            .with_compressor(Arc::clone(&self.compressor));
        encoder.encode(image.clone()).map_err(TextureError::from)
    }

    fn expected_size(&self, width: u32, height: u32) -> usize {
        let header_size = std::mem::size_of::<DdsHeader>(); // 128 bytes
        let data_size = Self::calculate_data_size(width, height, self.format, self.mipmap_count);
        header_size + data_size
    }

    fn extension(&self) -> &str {
        "dds"
    }

    fn name(&self) -> &str {
        match self.format {
            DdsFormat::BC1 => "DDS BC1",
            DdsFormat::BC3 => "DDS BC3",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_new_default_mipmap_count() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.mipmap_count(), 5);
        assert_eq!(encoder.format(), DdsFormat::BC1);
    }

    #[test]
    fn test_with_mipmap_count() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3).with_mipmap_count(3);
        assert_eq!(encoder.mipmap_count(), 3);
        assert_eq!(encoder.format(), DdsFormat::BC3);
    }

    #[test]
    fn test_extension() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.extension(), "dds");
    }

    #[test]
    fn test_name_bc1() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.name(), "DDS BC1");
    }

    #[test]
    fn test_name_bc3() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3);
        assert_eq!(encoder.name(), "DDS BC3");
    }

    #[test]
    fn test_expected_size_4096_bc1_5_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        let size = encoder.expected_size(4096, 4096);

        // 5 levels: 4096, 2048, 1024, 512, 256
        // 4096: 1024×1024 blocks * 8 = 8388608
        // 2048: 512×512 blocks * 8 = 2097152
        // 1024: 256×256 blocks * 8 = 524288
        // 512: 128×128 blocks * 8 = 131072
        // 256: 64×64 blocks * 8 = 32768
        // Total data: 11173888
        // Header: 128
        // Total: 11174016
        assert_eq!(size, 11_174_016);
    }

    #[test]
    fn test_expected_size_4096_bc3_5_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3).with_mipmap_count(5);
        let size = encoder.expected_size(4096, 4096);

        // BC3 is double BC1 (16 bytes per block vs 8)
        // Data: 11173888 * 2 = 22347776
        // Header: 128
        // Total: 22347904
        assert_eq!(size, 22_347_904);
    }

    #[test]
    fn test_expected_size_256_bc1_no_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let size = encoder.expected_size(256, 256);

        // 256×256 = 64×64 blocks * 8 bytes = 32768 bytes data
        // Header: 128
        // Total: 32896
        assert_eq!(size, 32_896);
    }

    #[test]
    fn test_expected_size_small_image() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let size = encoder.expected_size(4, 4);

        // 4×4 = 1×1 blocks * 8 bytes = 8 bytes data
        // Header: 128
        // Total: 136
        assert_eq!(size, 136);
    }

    #[test]
    fn test_encode_small_image() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let image = RgbaImage::new(4, 4);

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Header (128) + 1 block (8) = 136 bytes
        assert_eq!(data.len(), 136);

        // Check magic
        assert_eq!(&data[0..4], b"DDS ");
    }

    #[test]
    fn test_encode_256x256_bc1() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let image = RgbaImage::new(256, 256);

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), encoder.expected_size(256, 256));
    }

    #[test]
    fn test_encode_with_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        let image = RgbaImage::new(256, 256);

        let result = encoder.encode(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should have 5 mipmap levels
        assert_eq!(data.len(), encoder.expected_size(256, 256));
    }

    #[test]
    fn test_encode_zero_dimensions() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        let image = RgbaImage::new(0, 0);

        let result = encoder.encode(&image);
        assert!(result.is_err());

        match result {
            Err(TextureError::InvalidDimensions { width, height, .. }) => {
                assert_eq!(width, 0);
                assert_eq!(height, 0);
            }
            _ => panic!("Expected InvalidDimensions error"),
        }
    }

    #[test]
    fn test_as_trait_object() {
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        assert_eq!(encoder.extension(), "dds");
        assert_eq!(encoder.name(), "DDS BC1");
        assert_eq!(encoder.expected_size(4096, 4096), 11_174_016);
    }

    #[test]
    fn test_clone() {
        let encoder1 = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(3);
        let encoder2 = encoder1.clone();

        assert_eq!(encoder1.format(), encoder2.format());
        assert_eq!(encoder1.mipmap_count(), encoder2.mipmap_count());
    }

    #[test]
    fn test_calculate_data_size_bc1() {
        // 4×4 image, 1 mipmap
        let size = DdsTextureEncoder::calculate_data_size(4, 4, DdsFormat::BC1, 1);
        assert_eq!(size, 8); // 1 block * 8 bytes

        // 256×256 image, 1 mipmap
        let size = DdsTextureEncoder::calculate_data_size(256, 256, DdsFormat::BC1, 1);
        assert_eq!(size, 32768); // 64*64 blocks * 8 bytes
    }

    #[test]
    fn test_calculate_data_size_bc3() {
        // 4×4 image, 1 mipmap
        let size = DdsTextureEncoder::calculate_data_size(4, 4, DdsFormat::BC3, 1);
        assert_eq!(size, 16); // 1 block * 16 bytes

        // 256×256 image, 1 mipmap
        let size = DdsTextureEncoder::calculate_data_size(256, 256, DdsFormat::BC3, 1);
        assert_eq!(size, 65536); // 64*64 blocks * 16 bytes
    }

    #[test]
    fn test_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DdsTextureEncoder>();
    }

    #[test]
    fn test_with_compressor_software() {
        use crate::dds::SoftwareCompressor;

        let encoder =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));

        // Should encode successfully with software compressor
        let image = RgbaImage::new(4, 4);
        let result = encoder.encode(&image);
        assert!(result.is_ok());
    }

    #[test]
    fn test_with_compressor_clone() {
        use crate::dds::SoftwareCompressor;

        let encoder1 =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));
        let encoder2 = encoder1.clone();

        // Both clones should encode successfully
        let image = RgbaImage::new(4, 4);
        assert!(encoder1.encode(&image).is_ok());
        assert!(encoder2.encode(&image).is_ok());
    }

    #[test]
    fn test_with_compressor_debug() {
        use crate::dds::SoftwareCompressor;

        let encoder =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));

        let debug_str = format!("{:?}", encoder);
        assert!(debug_str.contains("DdsTextureEncoder"));
        assert!(debug_str.contains("software"));
    }
}
