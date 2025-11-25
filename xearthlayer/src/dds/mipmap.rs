//! Mipmap chain generation for DDS textures.

use image::RgbaImage;

/// Mipmap generator.
pub struct MipmapGenerator;

impl MipmapGenerator {
    /// Generate full mipmap chain from source image.
    ///
    /// Continues downsampling by 2× until reaching 1×1 or a minimum size.
    ///
    /// # Arguments
    ///
    /// * `source` - Source image (must have power-of-2 dimensions)
    ///
    /// # Returns
    ///
    /// Vector of images: [original, half-size, quarter-size, ...]
    pub fn generate_chain(source: &RgbaImage) -> Vec<RgbaImage> {
        let mut mipmaps = vec![source.clone()];

        let mut current = source.clone();
        while current.width() > 1 && current.height() > 1 {
            current = Self::downsample_box_2x(&current);
            mipmaps.push(current.clone());
        }

        mipmaps
    }

    /// Generate mipmap chain up to a specific count.
    ///
    /// # Arguments
    ///
    /// * `source` - Source image
    /// * `count` - Number of mipmap levels to generate (including original)
    ///
    /// # Returns
    ///
    /// Vector of `count` images
    pub fn generate_chain_with_count(source: &RgbaImage, count: usize) -> Vec<RgbaImage> {
        let mut mipmaps = vec![source.clone()];

        let mut current = source.clone();
        for _ in 1..count {
            if current.width() <= 1 || current.height() <= 1 {
                break;
            }
            current = Self::downsample_box_2x(&current);
            mipmaps.push(current.clone());
        }

        mipmaps
    }

    /// Downsample image by 2× using box filter (simple average).
    ///
    /// Each output pixel is the average of a 2×2 block of input pixels.
    fn downsample_box_2x(source: &RgbaImage) -> RgbaImage {
        let new_width = source.width() / 2;
        let new_height = source.height() / 2;

        let mut output = RgbaImage::new(new_width, new_height);

        for y in 0..new_height {
            for x in 0..new_width {
                // Average 2×2 block
                let p00 = source.get_pixel(x * 2, y * 2);
                let p10 = source.get_pixel(x * 2 + 1, y * 2);
                let p01 = source.get_pixel(x * 2, y * 2 + 1);
                let p11 = source.get_pixel(x * 2 + 1, y * 2 + 1);

                let avg = image::Rgba([
                    ((p00[0] as u16 + p10[0] as u16 + p01[0] as u16 + p11[0] as u16) / 4) as u8,
                    ((p00[1] as u16 + p10[1] as u16 + p01[1] as u16 + p11[1] as u16) / 4) as u8,
                    ((p00[2] as u16 + p10[2] as u16 + p01[2] as u16 + p11[2] as u16) / 4) as u8,
                    ((p00[3] as u16 + p10[3] as u16 + p01[3] as u16 + p11[3] as u16) / 4) as u8,
                ]);

                output.put_pixel(x, y, avg);
            }
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downsample_256_to_128() {
        let source = RgbaImage::new(256, 256);
        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 128);
        assert_eq!(downsampled.height(), 128);
    }

    #[test]
    fn test_downsample_4096_to_2048() {
        let source = RgbaImage::new(4096, 4096);
        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 2048);
        assert_eq!(downsampled.height(), 2048);
    }

    #[test]
    fn test_downsample_solid_color() {
        let mut source = RgbaImage::new(4, 4);
        // Fill with solid red
        for pixel in source.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]);
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // Should still be solid red
        for pixel in downsampled.pixels() {
            assert_eq!(pixel[0], 255);
            assert_eq!(pixel[1], 0);
            assert_eq!(pixel[2], 0);
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn test_downsample_checkerboard() {
        let mut source = RgbaImage::new(4, 4);

        // Create checkerboard: black and white
        for y in 0..4 {
            for x in 0..4 {
                let color = if (x + y) % 2 == 0 {
                    image::Rgba([0, 0, 0, 255])
                } else {
                    image::Rgba([255, 255, 255, 255])
                };
                source.put_pixel(x, y, color);
            }
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // Each pixel should be gray (average of black and white)
        // Some pixels have 2 black + 2 white = gray (127-128)
        // Others might have different ratios
        for pixel in downsampled.pixels() {
            // Should be some shade of gray
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 255); // Alpha unchanged
        }
    }

    #[test]
    fn test_generate_chain_256() {
        let source = RgbaImage::new(256, 256);
        let chain = MipmapGenerator::generate_chain(&source);

        // 256 → 128 → 64 → 32 → 16 → 8 → 4 → 2 → 1
        assert_eq!(chain.len(), 9);

        assert_eq!(chain[0].dimensions(), (256, 256));
        assert_eq!(chain[1].dimensions(), (128, 128));
        assert_eq!(chain[2].dimensions(), (64, 64));
        assert_eq!(chain[3].dimensions(), (32, 32));
        assert_eq!(chain[4].dimensions(), (16, 16));
        assert_eq!(chain[5].dimensions(), (8, 8));
        assert_eq!(chain[6].dimensions(), (4, 4));
        assert_eq!(chain[7].dimensions(), (2, 2));
        assert_eq!(chain[8].dimensions(), (1, 1));
    }

    #[test]
    fn test_generate_chain_4096() {
        let source = RgbaImage::new(4096, 4096);
        let chain = MipmapGenerator::generate_chain(&source);

        // 4096 → 2048 → 1024 → 512 → 256 → 128 → 64 → 32 → 16 → 8 → 4 → 2 → 1
        assert_eq!(chain.len(), 13);

        assert_eq!(chain[0].dimensions(), (4096, 4096));
        assert_eq!(chain[1].dimensions(), (2048, 2048));
        assert_eq!(chain[2].dimensions(), (1024, 1024));
        assert_eq!(chain[3].dimensions(), (512, 512));
        assert_eq!(chain[4].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_with_count() {
        let source = RgbaImage::new(4096, 4096);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 5);

        // Should generate exactly 5 levels
        assert_eq!(chain.len(), 5);

        assert_eq!(chain[0].dimensions(), (4096, 4096));
        assert_eq!(chain[1].dimensions(), (2048, 2048));
        assert_eq!(chain[2].dimensions(), (1024, 1024));
        assert_eq!(chain[3].dimensions(), (512, 512));
        assert_eq!(chain[4].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_with_count_exceeds_possible() {
        let source = RgbaImage::new(4, 4);
        // Request 10 levels but can only generate 3 (4→2→1)
        let chain = MipmapGenerator::generate_chain_with_count(&source, 10);

        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].dimensions(), (4, 4));
        assert_eq!(chain[1].dimensions(), (2, 2));
        assert_eq!(chain[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_generate_chain_single_level() {
        let source = RgbaImage::new(256, 256);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 1);

        // Should only have original
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_4x4() {
        let source = RgbaImage::new(4, 4);
        let chain = MipmapGenerator::generate_chain(&source);

        // 4 → 2 → 1
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].dimensions(), (4, 4));
        assert_eq!(chain[1].dimensions(), (2, 2));
        assert_eq!(chain[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_mipmap_preserves_average_color() {
        let mut source = RgbaImage::new(4, 4);

        // Fill with gradient
        for y in 0..4 {
            for x in 0..4 {
                let val = ((x + y) * 255 / 6) as u8;
                source.put_pixel(x, y, image::Rgba([val, val, val, 255]));
            }
        }

        let chain = MipmapGenerator::generate_chain(&source);

        // The 1×1 mipmap should approximate the average color
        let final_pixel = chain.last().unwrap().get_pixel(0, 0);

        // Average value should be somewhere in middle range
        assert!(
            final_pixel[0] > 50 && final_pixel[0] < 200,
            "Final mipmap should have mid-range color, got {}",
            final_pixel[0]
        );
    }

    #[test]
    fn test_downsample_preserves_alpha() {
        let mut source = RgbaImage::new(4, 4);

        // Fill with varying RGB but constant alpha
        for y in 0..4 {
            for x in 0..4 {
                source.put_pixel(x, y, image::Rgba([x as u8 * 60, y as u8 * 60, 128, 200]));
            }
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // All pixels should have alpha around 200
        for pixel in downsampled.pixels() {
            assert_eq!(pixel[3], 200, "Alpha should be preserved");
        }
    }

    #[test]
    fn test_downsample_averages_correctly() {
        let mut source = RgbaImage::new(2, 2);

        // Set specific values
        source.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        source.put_pixel(1, 0, image::Rgba([100, 0, 0, 255]));
        source.put_pixel(0, 1, image::Rgba([0, 100, 0, 255]));
        source.put_pixel(1, 1, image::Rgba([0, 0, 100, 255]));

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 1);
        assert_eq!(downsampled.height(), 1);

        let avg = downsampled.get_pixel(0, 0);

        // Average: R=(0+100+0+0)/4=25, G=(0+0+100+0)/4=25, B=(0+0+0+100)/4=25
        assert_eq!(avg[0], 25);
        assert_eq!(avg[1], 25);
        assert_eq!(avg[2], 25);
        assert_eq!(avg[3], 255);
    }
}
