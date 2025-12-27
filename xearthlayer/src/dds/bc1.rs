//! BC1/DXT1 block compression.
//!
//! BC1 compresses 4×4 blocks of RGB(A) pixels to 8 bytes:
//! - 2 bytes: color0 (RGB565)
//! - 2 bytes: color1 (RGB565)
//! - 4 bytes: 16 2-bit indices (one per pixel)
//!
//! The 2-bit indices select from a 4-color palette:
//! - 00: color0
//! - 01: color1
//! - 10: (2*color0 + color1) / 3
//! - 11: (color0 + 2*color1) / 3

use crate::dds::conversion::*;

/// BC1 block encoder.
pub struct Bc1Encoder;

impl Bc1Encoder {
    /// Compress a 4×4 RGBA block to 8 bytes.
    ///
    /// # Arguments
    ///
    /// * `pixels` - 16 RGBA pixels in row-major order (64 bytes total)
    ///
    /// # Returns
    ///
    /// 8-byte compressed block
    pub fn compress_block(pixels: &[[u8; 4]; 16]) -> [u8; 8] {
        // Find color bounding box
        let (c0, c1) = Self::find_endpoints(pixels);

        // Ensure c0 > c1 for 4-color mode
        let (c0, c1) = if c0 > c1 { (c0, c1) } else { (c1, c0) };

        // Generate indices
        let indices = Self::generate_indices(pixels, c0, c1);

        // Pack into 8 bytes
        let mut output = [0u8; 8];
        output[0..2].copy_from_slice(&c0.to_le_bytes());
        output[2..4].copy_from_slice(&c1.to_le_bytes());
        output[4..8].copy_from_slice(&indices.to_le_bytes());
        output
    }

    /// Find optimal color endpoints using bounding box method.
    ///
    /// Returns (max_color, min_color) as RGB565 values.
    fn find_endpoints(pixels: &[[u8; 4]; 16]) -> (u16, u16) {
        let mut min_r = 255u8;
        let mut min_g = 255u8;
        let mut min_b = 255u8;
        let mut max_r = 0u8;
        let mut max_g = 0u8;
        let mut max_b = 0u8;

        for pixel in pixels {
            min_r = min_r.min(pixel[0]);
            min_g = min_g.min(pixel[1]);
            min_b = min_b.min(pixel[2]);
            max_r = max_r.max(pixel[0]);
            max_g = max_g.max(pixel[1]);
            max_b = max_b.max(pixel[2]);
        }

        let c0 = rgb888_to_rgb565(max_r, max_g, max_b);
        let c1 = rgb888_to_rgb565(min_r, min_g, min_b);

        (c0, c1)
    }

    /// Generate 2-bit indices for each pixel.
    ///
    /// Finds the closest color in the 4-color palette for each pixel.
    fn generate_indices(pixels: &[[u8; 4]; 16], c0: u16, c1: u16) -> u32 {
        // Build 4-color palette
        let palette = [
            rgb565_to_rgb888(c0),
            rgb565_to_rgb888(c1),
            interpolate_rgb565(c0, c1, 1), // 2/3 c0, 1/3 c1
            interpolate_rgb565(c0, c1, 2), // 1/3 c0, 2/3 c1
        ];

        let mut indices: u32 = 0;

        for (i, pixel) in pixels.iter().enumerate() {
            // Find closest palette color
            let mut best_dist = u32::MAX;
            let mut best_index = 0u8;

            for (idx, pal_color) in palette.iter().enumerate() {
                let dist = color_distance_squared(pixel, pal_color);
                if dist < best_dist {
                    best_dist = dist;
                    best_index = idx as u8;
                }
            }

            // Pack 2-bit index
            indices |= (best_index as u32) << (i * 2);
        }

        indices
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_solid_black() {
        let pixels = [[0, 0, 0, 255]; 16];
        let compressed = Bc1Encoder::compress_block(&pixels);

        assert_eq!(compressed.len(), 8);

        // Both endpoints should be black (0x0000)
        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);
        assert_eq!(c0, 0);
        assert_eq!(c1, 0);

        // All indices should be 0 (both colors are the same)
        let indices =
            u32::from_le_bytes([compressed[4], compressed[5], compressed[6], compressed[7]]);
        assert_eq!(indices, 0);
    }

    #[test]
    fn test_compress_solid_white() {
        let pixels = [[255, 255, 255, 255]; 16];
        let compressed = Bc1Encoder::compress_block(&pixels);

        assert_eq!(compressed.len(), 8);

        // Both endpoints should be white (0xFFFF)
        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);
        assert_eq!(c0, 0xFFFF);
        assert_eq!(c1, 0xFFFF);

        // All indices should be 0
        let indices =
            u32::from_le_bytes([compressed[4], compressed[5], compressed[6], compressed[7]]);
        assert_eq!(indices, 0);
    }

    #[test]
    fn test_compress_solid_red() {
        let pixels = [[255, 0, 0, 255]; 16];
        let compressed = Bc1Encoder::compress_block(&pixels);

        // Endpoints should be red
        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);
        assert_eq!(c0, 0xF800); // Red in RGB565
        assert_eq!(c1, 0xF800);
    }

    #[test]
    fn test_compress_two_colors() {
        // Create block with 8 black pixels and 8 white pixels
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        for i in 8..16 {
            pixels[i] = [255, 255, 255, 255];
        }

        let compressed = Bc1Encoder::compress_block(&pixels);

        // Endpoints should be black and white
        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);

        // One should be white (0xFFFF), one black (0x0000)
        assert!(
            (c0 == 0xFFFF && c1 == 0x0000) || (c0 == 0x0000 && c1 == 0xFFFF),
            "Expected black and white endpoints, got c0={:04X} c1={:04X}",
            c0,
            c1
        );
    }

    #[test]
    fn test_compress_gradient() {
        // Create a gradient from black to white
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        for i in 0..16 {
            let val = (i * 255 / 15) as u8;
            pixels[i] = [val, val, val, 255];
        }

        let compressed = Bc1Encoder::compress_block(&pixels);

        // Endpoints should be near black and white
        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);

        // c0 should be > c1 for 4-color mode
        assert!(c0 > c1, "c0 ({:04X}) should be > c1 ({:04X})", c0, c1);

        // One endpoint should be near white, other near black
        let rgb0 = rgb565_to_rgb888(c0);
        let rgb1 = rgb565_to_rgb888(c1);

        assert!(
            rgb0[0] > 200 || rgb1[0] > 200,
            "Expected one endpoint near white"
        );
        assert!(
            rgb0[0] < 50 || rgb1[0] < 50,
            "Expected one endpoint near black"
        );
    }

    #[test]
    fn test_find_endpoints_black_white() {
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        pixels[0] = [255, 255, 255, 255];

        let (c0, c1) = Bc1Encoder::find_endpoints(&pixels);

        // Should find white and black
        assert_eq!(c0, 0xFFFF);
        assert_eq!(c1, 0x0000);
    }

    #[test]
    fn test_find_endpoints_rgb() {
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        pixels[0] = [255, 0, 0, 255]; // Red
        pixels[1] = [0, 255, 0, 255]; // Green
        pixels[2] = [0, 0, 255, 255]; // Blue

        let (c0, c1) = Bc1Encoder::find_endpoints(&pixels);

        let rgb0 = rgb565_to_rgb888(c0);
        let rgb1 = rgb565_to_rgb888(c1);

        // Max should have all channels at max
        assert_eq!(rgb0, [255, 255, 255]);
        // Min should have all channels at min
        assert_eq!(rgb1, [0, 0, 0]);
    }

    #[test]
    fn test_generate_indices_solid() {
        let pixels = [[128u8, 128, 128, 255]; 16];
        let c0 = rgb888_to_rgb565(128, 128, 128);
        let c1 = rgb888_to_rgb565(128, 128, 128);

        let indices = Bc1Encoder::generate_indices(&pixels, c0, c1);

        // All pixels should map to index 0 or 1 (both are the same color)
        // Check that no index is > 1
        for i in 0..16 {
            let idx = (indices >> (i * 2)) & 0x3;
            assert!(idx <= 1, "Index {} should be 0 or 1, got {}", i, idx);
        }
    }

    #[test]
    fn test_generate_indices_two_colors() {
        // First 8 pixels black, last 8 white
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        for i in 8..16 {
            pixels[i] = [255, 255, 255, 255];
        }

        let c0 = rgb888_to_rgb565(255, 255, 255);
        let c1 = rgb888_to_rgb565(0, 0, 0);

        let indices = Bc1Encoder::generate_indices(&pixels, c0, c1);

        // First 8 should be index 1 (black = c1)
        for i in 0..8 {
            let idx = (indices >> (i * 2)) & 0x3;
            assert_eq!(idx, 1, "Pixel {} should be index 1 (black)", i);
        }

        // Last 8 should be index 0 (white = c0)
        for i in 8..16 {
            let idx = (indices >> (i * 2)) & 0x3;
            assert_eq!(idx, 0, "Pixel {} should be index 0 (white)", i);
        }
    }

    #[test]
    fn test_compress_block_size() {
        let pixels = [[100u8, 150, 200, 255]; 16];
        let compressed = Bc1Encoder::compress_block(&pixels);
        assert_eq!(compressed.len(), 8);
    }

    #[test]
    fn test_compress_4color_mode() {
        // Create a block that requires 4-color mode
        let pixels = [[128u8, 128, 128, 255]; 16];
        let compressed = Bc1Encoder::compress_block(&pixels);

        let c0 = u16::from_le_bytes([compressed[0], compressed[1]]);
        let c1 = u16::from_le_bytes([compressed[2], compressed[3]]);

        // c0 should be >= c1 for 4-color mode (opaque)
        assert!(
            c0 >= c1,
            "Expected 4-color mode (c0 >= c1), got c0={:04X} c1={:04X}",
            c0,
            c1
        );
    }
}
