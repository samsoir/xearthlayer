//! BC3/DXT5 block compression.
//!
//! BC3 compresses 4×4 blocks of RGBA pixels to 16 bytes:
//! - 8 bytes: Alpha channel compression
//!   - 1 byte: alpha0
//!   - 1 byte: alpha1
//!   - 6 bytes: 16 3-bit indices (one per pixel)
//! - 8 bytes: RGB compression (same as BC1)

use crate::dds::bc1::Bc1Encoder;

/// BC3 block encoder.
pub struct Bc3Encoder;

impl Bc3Encoder {
    /// Compress a 4×4 RGBA block to 16 bytes.
    ///
    /// # Arguments
    ///
    /// * `pixels` - 16 RGBA pixels in row-major order (64 bytes total)
    ///
    /// # Returns
    ///
    /// 16-byte compressed block (8 bytes alpha + 8 bytes RGB)
    pub fn compress_block(pixels: &[[u8; 4]; 16]) -> [u8; 16] {
        let mut output = [0u8; 16];

        // Compress alpha channel (first 8 bytes)
        let alpha_block = Self::compress_alpha(pixels);
        output[0..8].copy_from_slice(&alpha_block);

        // Compress RGB channels using BC1 (last 8 bytes)
        let rgb_block = Bc1Encoder::compress_block(pixels);
        output[8..16].copy_from_slice(&rgb_block);

        output
    }

    /// Compress alpha channel to 8 bytes.
    ///
    /// Uses 8-alpha interpolation mode:
    /// - alpha0, alpha1 (2 bytes)
    /// - 6 interpolated values
    /// - 16 3-bit indices (6 bytes)
    fn compress_alpha(pixels: &[[u8; 4]; 16]) -> [u8; 8] {
        // Find min/max alpha values
        let mut min_alpha = 255u8;
        let mut max_alpha = 0u8;

        for pixel in pixels {
            min_alpha = min_alpha.min(pixel[3]);
            max_alpha = max_alpha.max(pixel[3]);
        }

        // Use max as alpha0, min as alpha1 for 8-alpha mode
        let (alpha0, alpha1) = (max_alpha, min_alpha);

        // Generate alpha palette
        let palette = Self::build_alpha_palette(alpha0, alpha1);

        // Generate 3-bit indices
        let indices = Self::generate_alpha_indices(pixels, &palette);

        // Pack into 8 bytes
        let mut output = [0u8; 8];
        output[0] = alpha0;
        output[1] = alpha1;
        output[2..8].copy_from_slice(&indices[0..6]);

        output
    }

    /// Build 8-value alpha palette from two endpoints.
    fn build_alpha_palette(alpha0: u8, alpha1: u8) -> [u8; 8] {
        let a0 = alpha0 as u16;
        let a1 = alpha1 as u16;

        [
            alpha0,
            alpha1,
            ((6 * a0 + a1) / 7) as u8,     // 6/7 a0, 1/7 a1
            ((5 * a0 + 2 * a1) / 7) as u8, // 5/7 a0, 2/7 a1
            ((4 * a0 + 3 * a1) / 7) as u8, // 4/7 a0, 3/7 a1
            ((3 * a0 + 4 * a1) / 7) as u8, // 3/7 a0, 4/7 a1
            ((2 * a0 + 5 * a1) / 7) as u8, // 2/7 a0, 5/7 a1
            ((a0 + 6 * a1) / 7) as u8,     // 1/7 a0, 6/7 a1
        ]
    }

    /// Generate 3-bit indices for alpha values.
    fn generate_alpha_indices(pixels: &[[u8; 4]; 16], palette: &[u8; 8]) -> [u8; 8] {
        let mut indices = 0u64;

        for (i, pixel) in pixels.iter().enumerate() {
            let alpha = pixel[3];

            // Find closest palette value
            let mut best_dist = u32::MAX;
            let mut best_index = 0u8;

            for (idx, &pal_alpha) in palette.iter().enumerate() {
                let dist = (alpha as i32 - pal_alpha as i32).unsigned_abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_index = idx as u8;
                }
            }

            // Pack 3-bit index
            indices |= (best_index as u64) << (i * 3);
        }

        // Convert to byte array (only use first 6 bytes = 48 bits)
        indices.to_le_bytes()
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_solid_opaque() {
        let pixels = [[128u8, 64, 192, 255]; 16];
        let compressed = Bc3Encoder::compress_block(&pixels);

        assert_eq!(compressed.len(), 16);

        // Alpha should be 255 for both endpoints
        assert_eq!(compressed[0], 255);
        assert_eq!(compressed[1], 255);
    }

    #[test]
    fn test_compress_solid_transparent() {
        let pixels = [[128u8, 64, 192, 0]; 16];
        let compressed = Bc3Encoder::compress_block(&pixels);

        // Alpha should be 0 for both endpoints
        assert_eq!(compressed[0], 0);
        assert_eq!(compressed[1], 0);
    }

    #[test]
    fn test_compress_alpha_gradient() {
        let mut pixels = [[0u8, 0, 0, 255]; 16];
        for i in 0..16 {
            pixels[i][3] = (i * 255 / 15) as u8;
        }

        let compressed = Bc3Encoder::compress_block(&pixels);

        // Alpha0 should be max (near 255)
        // Alpha1 should be min (near 0)
        assert!(compressed[0] >= 250, "Alpha0 should be near 255");
        assert!(compressed[1] <= 5, "Alpha1 should be near 0");
    }

    #[test]
    fn test_compress_two_alpha_values() {
        // Half transparent, half opaque
        let mut pixels = [[0u8, 0, 0, 0]; 16];
        for i in 8..16 {
            pixels[i][3] = 255;
        }

        let compressed = Bc3Encoder::compress_block(&pixels);

        // Should have 0 and 255 as endpoints
        let alpha0 = compressed[0];
        let alpha1 = compressed[1];

        assert!(
            (alpha0 == 255 && alpha1 == 0) || (alpha0 == 0 && alpha1 == 255),
            "Expected 0 and 255 as alpha endpoints, got {} and {}",
            alpha0,
            alpha1
        );
    }

    #[test]
    fn test_build_alpha_palette() {
        let palette = Bc3Encoder::build_alpha_palette(255, 0);

        assert_eq!(palette[0], 255); // alpha0
        assert_eq!(palette[1], 0); // alpha1

        // Check interpolated values (palette[2..8]) are in descending order
        for i in 3..8 {
            assert!(
                palette[i - 1] >= palette[i],
                "Interpolated palette values should be descending, but palette[{}]={} < palette[{}]={}",
                i - 1, palette[i - 1], i, palette[i]
            );
        }

        // Check approximate values
        assert!(palette[2] >= 217 && palette[2] <= 219); // 6/7 * 255 ≈ 218
        assert!(palette[7] >= 36 && palette[7] <= 38); // 1/7 * 255 ≈ 36

        // Verify palette[2] is between alpha0 and alpha1
        assert!(palette[2] > palette[1] && palette[2] < palette[0]);
    }

    #[test]
    fn test_build_alpha_palette_partial_range() {
        let palette = Bc3Encoder::build_alpha_palette(200, 100);

        assert_eq!(palette[0], 200);
        assert_eq!(palette[1], 100);

        // All values should be in range [100, 200]
        for &val in &palette {
            assert!(
                (100..=200).contains(&val),
                "Palette value {} out of range",
                val
            );
        }
    }

    #[test]
    fn test_generate_alpha_indices_solid() {
        let pixels = [[0u8, 0, 0, 128]; 16];
        let palette = Bc3Encoder::build_alpha_palette(128, 128);

        let indices = Bc3Encoder::generate_alpha_indices(&pixels, &palette);

        // All indices should be 0 or 1 (both same value)
        let indices_u64 = u64::from_le_bytes(indices);
        for i in 0..16 {
            let idx = (indices_u64 >> (i * 3)) & 0x7;
            assert!(idx <= 1, "Index {} should be 0 or 1, got {}", i, idx);
        }
    }

    #[test]
    fn test_generate_alpha_indices_two_values() {
        // First 8 pixels: alpha=0, last 8: alpha=255
        let mut pixels = [[0u8, 0, 0, 0]; 16];
        for i in 8..16 {
            pixels[i][3] = 255;
        }

        let palette = Bc3Encoder::build_alpha_palette(255, 0);
        let indices = Bc3Encoder::generate_alpha_indices(&pixels, &palette);

        let indices_u64 = u64::from_le_bytes(indices);

        // First 8 should map to index 1 (alpha=0)
        for i in 0..8 {
            let idx = (indices_u64 >> (i * 3)) & 0x7;
            assert_eq!(idx, 1, "Pixel {} should be index 1 (alpha=0)", i);
        }

        // Last 8 should map to index 0 (alpha=255)
        for i in 8..16 {
            let idx = (indices_u64 >> (i * 3)) & 0x7;
            assert_eq!(idx, 0, "Pixel {} should be index 0 (alpha=255)", i);
        }
    }

    #[test]
    fn test_compress_alpha_solid() {
        let pixels = [[0u8, 0, 0, 200]; 16];
        let alpha_block = Bc3Encoder::compress_alpha(&pixels);

        assert_eq!(alpha_block.len(), 8);
        assert_eq!(alpha_block[0], 200);
        assert_eq!(alpha_block[1], 200);
    }

    #[test]
    fn test_compress_alpha_range() {
        let mut pixels = [[0u8, 0, 0, 128]; 16]; // Initialize with mid-range alpha
        pixels[0][3] = 50; // Min alpha
        pixels[15][3] = 200; // Max alpha

        let alpha_block = Bc3Encoder::compress_alpha(&pixels);

        // Should have 200 and 50 as endpoints (or vice versa)
        assert!(
            (alpha_block[0] == 200 && alpha_block[1] == 50)
                || (alpha_block[0] == 50 && alpha_block[1] == 200),
            "Expected endpoints 50 and 200, got {} and {}",
            alpha_block[0],
            alpha_block[1]
        );
    }

    #[test]
    fn test_compress_block_size() {
        let pixels = [[100u8, 150, 200, 128]; 16];
        let compressed = Bc3Encoder::compress_block(&pixels);
        assert_eq!(compressed.len(), 16);
    }

    #[test]
    fn test_compress_rgb_matches_bc1() {
        // RGB portion should match BC1 compression
        let pixels = [[100u8, 150, 200, 255]; 16];

        let bc3_compressed = Bc3Encoder::compress_block(&pixels);
        let bc1_compressed = Bc1Encoder::compress_block(&pixels);

        // Last 8 bytes of BC3 should match BC1
        assert_eq!(&bc3_compressed[8..16], &bc1_compressed[0..8]);
    }

    #[test]
    fn test_alpha_indices_use_48_bits() {
        // 16 pixels × 3 bits = 48 bits = 6 bytes
        let pixels = [[0u8, 0, 0, 128]; 16];
        let palette = Bc3Encoder::build_alpha_palette(255, 0);

        let indices = Bc3Encoder::generate_alpha_indices(&pixels, &palette);

        // Should have 8 bytes total
        assert_eq!(indices.len(), 8);

        // But only first 6 bytes are used (last 2 should be 0 or unused)
        // This is because 16 * 3 bits = 48 bits = 6 bytes
    }

    #[test]
    fn test_alpha0_greater_than_alpha1() {
        let mut pixels = [[0u8, 0, 0, 100]; 16];
        pixels[0][3] = 200;

        let alpha_block = Bc3Encoder::compress_alpha(&pixels);

        // alpha0 should be >= alpha1 for 8-alpha mode
        assert!(
            alpha_block[0] >= alpha_block[1],
            "alpha0 ({}) should be >= alpha1 ({})",
            alpha_block[0],
            alpha_block[1]
        );
    }
}
