//! Magenta placeholder DDS texture for error handling.
//!
//! When tile generation fails, we return a solid magenta (255, 0, 255) DDS texture
//! so X-Plane displays a clearly visible error indicator instead of crashing.

use crate::dds::{DdsEncoder, DdsError, DdsFormat};
use image::RgbaImage;

/// Generate a magenta placeholder DDS texture.
///
/// Creates a solid magenta (255, 0, 255, 255) texture of the specified dimensions
/// and encodes it as a DDS file.
///
/// # Arguments
///
/// * `width` - Texture width in pixels (typically 4096)
/// * `height` - Texture height in pixels (typically 4096)
/// * `format` - DDS compression format (BC1 or BC3)
/// * `mipmap_count` - Number of mipmap levels (typically 5 for 4096→256)
///
/// # Returns
///
/// Complete DDS file as bytes
///
/// # Errors
///
/// Returns error if DDS encoding fails (should be rare for solid color)
///
/// # Examples
///
/// ```
/// use xearthlayer::fuse::generate_magenta_placeholder;
/// use xearthlayer::dds::DdsFormat;
///
/// let placeholder = generate_magenta_placeholder(4096, 4096, DdsFormat::BC1, 5).unwrap();
/// assert!(placeholder.len() > 0);
/// assert_eq!(&placeholder[0..4], b"DDS "); // Valid DDS header
/// ```
pub fn generate_magenta_placeholder(
    width: u32,
    height: u32,
    format: DdsFormat,
    mipmap_count: usize,
) -> Result<Vec<u8>, DdsError> {
    // Create solid magenta image
    let mut image = RgbaImage::new(width, height);
    for pixel in image.pixels_mut() {
        *pixel = image::Rgba([255, 0, 255, 255]); // Magenta
    }

    // Encode to DDS
    let encoder = DdsEncoder::new(format).with_mipmap_count(mipmap_count);
    encoder.encode(&image)
}

/// Generate a default magenta placeholder for X-Plane tiles.
///
/// Uses standard settings: 4096×4096, BC1 compression, 5 mipmap levels.
///
/// # Returns
///
/// Complete DDS file as bytes
pub fn generate_default_placeholder() -> Result<Vec<u8>, DdsError> {
    generate_magenta_placeholder(4096, 4096, DdsFormat::BC1, 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_magenta_placeholder_basic() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 3);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Check it's a valid DDS file
        assert!(dds.len() > 128); // At least header + some data
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn test_generate_magenta_placeholder_4096() {
        let result = generate_magenta_placeholder(4096, 4096, DdsFormat::BC1, 5);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Should have header (128) + data for 5 mipmap levels
        // 4096: 1024×1024 blocks * 8 = 8388608
        // 2048: 512×512 blocks * 8 = 2097152
        // 1024: 256×256 blocks * 8 = 524288
        // 512: 128×128 blocks * 8 = 131072
        // 256: 64×64 blocks * 8 = 32768
        // Total: 11173888 + 128 = 11174016
        assert_eq!(dds.len(), 11_174_016);
    }

    #[test]
    fn test_generate_magenta_placeholder_bc3() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC3, 3);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // BC3 is 16 bytes per block (vs 8 for BC1)
        assert!(dds.len() > 128);
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn test_generate_magenta_placeholder_different_sizes() {
        let sizes = vec![
            (256, 256),
            (512, 512),
            (1024, 1024),
            (2048, 2048),
            (4096, 4096),
        ];

        for (width, height) in sizes {
            let result = generate_magenta_placeholder(width, height, DdsFormat::BC1, 3);
            assert!(
                result.is_ok(),
                "Failed to generate placeholder for {}x{}",
                width,
                height
            );
            let dds = result.unwrap();
            assert_eq!(&dds[0..4], b"DDS ");
        }
    }

    #[test]
    fn test_generate_magenta_placeholder_various_mipmap_counts() {
        for mipmap_count in 1..=5 {
            let result = generate_magenta_placeholder(4096, 4096, DdsFormat::BC1, mipmap_count);
            assert!(result.is_ok(), "Failed with {} mipmap levels", mipmap_count);
        }
    }

    #[test]
    fn test_generate_default_placeholder() {
        let result = generate_default_placeholder();
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Should be 4096×4096 BC1 with 5 mipmaps
        assert_eq!(dds.len(), 11_174_016);
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn test_placeholder_not_empty() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 1);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Should have header + data
        // 256×256 = 64×64 blocks, each 8 bytes = 32768
        // Header: 128
        // Total: 32896
        assert_eq!(dds.len(), 32896);
    }

    #[test]
    fn test_placeholder_header_contains_dimensions() {
        let result = generate_magenta_placeholder(1024, 512, DdsFormat::BC1, 1);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Check dimensions in header (width at offset 16, height at offset 12)
        let height_bytes = &dds[12..16];
        let height = u32::from_le_bytes([
            height_bytes[0],
            height_bytes[1],
            height_bytes[2],
            height_bytes[3],
        ]);
        assert_eq!(height, 512);

        let width_bytes = &dds[16..20];
        let width = u32::from_le_bytes([
            width_bytes[0],
            width_bytes[1],
            width_bytes[2],
            width_bytes[3],
        ]);
        assert_eq!(width, 1024);
    }

    #[test]
    fn test_placeholder_header_mipmap_count() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 5);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // Mipmap count at offset 28
        let mipmap_bytes = &dds[28..32];
        let mipmap_count = u32::from_le_bytes([
            mipmap_bytes[0],
            mipmap_bytes[1],
            mipmap_bytes[2],
            mipmap_bytes[3],
        ]);
        assert_eq!(mipmap_count, 5);
    }

    #[test]
    fn test_placeholder_bc1_fourcc() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 1);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // FourCC at offset 84
        assert_eq!(&dds[84..88], b"DXT1");
    }

    #[test]
    fn test_placeholder_bc3_fourcc() {
        let result = generate_magenta_placeholder(256, 256, DdsFormat::BC3, 1);
        assert!(result.is_ok());
        let dds = result.unwrap();

        // FourCC at offset 84
        assert_eq!(&dds[84..88], b"DXT5");
    }

    #[test]
    fn test_multiple_placeholders_same_result() {
        let placeholder1 = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 3).unwrap();
        let placeholder2 = generate_magenta_placeholder(256, 256, DdsFormat::BC1, 3).unwrap();

        // Should be deterministic
        assert_eq!(placeholder1, placeholder2);
    }
}
