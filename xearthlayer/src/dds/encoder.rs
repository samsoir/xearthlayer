//! DDS encoder - main API for encoding images to DDS format.

use crate::dds::bc1::Bc1Encoder;
use crate::dds::bc3::Bc3Encoder;
use crate::dds::mipmap::MipmapGenerator;
use crate::dds::types::{DdsError, DdsFormat, DdsHeader};
use image::RgbaImage;

/// DDS encoder configuration.
pub struct DdsEncoder {
    format: DdsFormat,
    generate_mipmaps: bool,
    mipmap_count: Option<usize>,
}

impl DdsEncoder {
    /// Create a new DDS encoder with the specified format.
    ///
    /// By default, generates full mipmap chain down to 1×1.
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            generate_mipmaps: true,
            mipmap_count: None,
        }
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

    /// Compress a single image to BC1/BC3 format.
    fn compress_image(&self, image: &RgbaImage) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        // Calculate block dimensions (round up to multiple of 4)
        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);

        let block_size = match self.format {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        };

        let mut output = Vec::with_capacity((blocks_wide * blocks_high * block_size) as usize);

        // Process each 4×4 block
        for block_y in 0..blocks_high {
            for block_x in 0..blocks_wide {
                let block = self.extract_block(image, block_x, block_y);
                let compressed = match self.format {
                    DdsFormat::BC1 => {
                        let bc1 = Bc1Encoder::compress_block(&block);
                        bc1.to_vec()
                    }
                    DdsFormat::BC3 => {
                        let bc3 = Bc3Encoder::compress_block(&block);
                        bc3.to_vec()
                    }
                };
                output.extend_from_slice(&compressed);
            }
        }

        Ok(output)
    }

    /// Extract a 4×4 pixel block from the image.
    ///
    /// Pads with black pixels if block extends beyond image bounds.
    fn extract_block(&self, image: &RgbaImage, block_x: u32, block_y: u32) -> [[u8; 4]; 16] {
        let mut block = [[0u8; 4]; 16];

        for y in 0..4 {
            for x in 0..4 {
                let pixel_x = block_x * 4 + x;
                let pixel_y = block_y * 4 + y;

                let pixel = if pixel_x < image.width() && pixel_y < image.height() {
                    let p = image.get_pixel(pixel_x, pixel_y);
                    [p[0], p[1], p[2], p[3]]
                } else {
                    [0, 0, 0, 0] // Pad with transparent black
                };

                block[(y * 4 + x) as usize] = pixel;
            }
        }

        block
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
    fn test_extract_block_full() {
        let mut image = RgbaImage::new(4, 4);
        // Fill with distinct colors
        for y in 0..4 {
            for x in 0..4 {
                image.put_pixel(
                    x,
                    y,
                    image::Rgba([(x * 64) as u8, (y * 64) as u8, 128, 255]),
                );
            }
        }

        let encoder = DdsEncoder::new(DdsFormat::BC1);
        let block = encoder.extract_block(&image, 0, 0);

        // Verify pixels match
        assert_eq!(block[0], [0, 0, 128, 255]); // (0,0)
        assert_eq!(block[1], [64, 0, 128, 255]); // (1,0)
        assert_eq!(block[4], [0, 64, 128, 255]); // (0,1)
        assert_eq!(block[15], [192, 192, 128, 255]); // (3,3)
    }

    #[test]
    fn test_extract_block_partial() {
        // Image smaller than 4×4
        let mut image = RgbaImage::new(3, 3);
        for y in 0..3 {
            for x in 0..3 {
                image.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }

        let encoder = DdsEncoder::new(DdsFormat::BC1);
        let block = encoder.extract_block(&image, 0, 0);

        // First 3×3 should be white
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(block[(y * 4 + x) as usize], [255, 255, 255, 255]);
            }
        }

        // Padding pixels should be black
        assert_eq!(block[3], [0, 0, 0, 0]); // (3,0) - out of bounds
        assert_eq!(block[12], [0, 0, 0, 0]); // (0,3) - out of bounds
        assert_eq!(block[15], [0, 0, 0, 0]); // (3,3) - out of bounds
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
