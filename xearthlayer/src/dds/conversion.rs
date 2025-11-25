//! Color conversion utilities for BC compression.

/// Convert RGB888 (8-bit per channel) to RGB565 (16-bit packed).
///
/// RGB565 format:
/// - Bits 15-11: Red (5 bits)
/// - Bits 10-5: Green (6 bits)
/// - Bits 4-0: Blue (5 bits)
pub fn rgb888_to_rgb565(r: u8, g: u8, b: u8) -> u16 {
    let r5 = (r >> 3) as u16; // 8-bit → 5-bit
    let g6 = (g >> 2) as u16; // 8-bit → 6-bit
    let b5 = (b >> 3) as u16; // 8-bit → 5-bit
    (r5 << 11) | (g6 << 5) | b5
}

/// Convert RGB565 (16-bit packed) to RGB888 (8-bit per channel).
///
/// Replicates lower bits to fill the 8-bit range for better accuracy.
pub fn rgb565_to_rgb888(color: u16) -> [u8; 3] {
    let r5 = (color >> 11) & 0x1F;
    let g6 = (color >> 5) & 0x3F;
    let b5 = color & 0x1F;

    // Replicate bits: r5 becomes rrrrrRRR (capital = replicated)
    // This gives better range coverage than simple shifting
    [
        ((r5 << 3) | (r5 >> 2)) as u8,
        ((g6 << 2) | (g6 >> 4)) as u8,
        ((b5 << 3) | (b5 >> 2)) as u8,
    ]
}

/// Calculate squared Euclidean distance between two RGB colors.
///
/// Uses perceptual weighting: human eye is more sensitive to green.
/// Weights: R=3, G=6, B=1 (approximates perceptual color space)
pub fn color_distance_squared(a: &[u8; 4], b: &[u8; 3]) -> u32 {
    let dr = (a[0] as i32 - b[0] as i32) * 3;
    let dg = (a[1] as i32 - b[1] as i32) * 6;
    let db = a[2] as i32 - b[2] as i32;
    (dr * dr + dg * dg + db * db) as u32
}

/// Interpolate between two RGB565 colors.
///
/// Returns color at position `t` in range [0, 3]:
/// - t=0: 100% c0
/// - t=1: 67% c0, 33% c1
/// - t=2: 33% c0, 67% c1
/// - t=3: 100% c1
pub fn interpolate_rgb565(c0: u16, c1: u16, t: u8) -> [u8; 3] {
    let rgb0 = rgb565_to_rgb888(c0);
    let rgb1 = rgb565_to_rgb888(c1);

    match t {
        0 => rgb0,
        1 => [
            ((2 * rgb0[0] as u16 + rgb1[0] as u16) / 3) as u8,
            ((2 * rgb0[1] as u16 + rgb1[1] as u16) / 3) as u8,
            ((2 * rgb0[2] as u16 + rgb1[2] as u16) / 3) as u8,
        ],
        2 => [
            ((rgb0[0] as u16 + 2 * rgb1[0] as u16) / 3) as u8,
            ((rgb0[1] as u16 + 2 * rgb1[1] as u16) / 3) as u8,
            ((rgb0[2] as u16 + 2 * rgb1[2] as u16) / 3) as u8,
        ],
        3 => rgb1,
        _ => panic!("Invalid interpolation parameter: {}", t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb565_black() {
        let rgb565 = rgb888_to_rgb565(0, 0, 0);
        assert_eq!(rgb565, 0);
        let rgb888 = rgb565_to_rgb888(rgb565);
        assert_eq!(rgb888, [0, 0, 0]);
    }

    #[test]
    fn test_rgb565_white() {
        let rgb565 = rgb888_to_rgb565(255, 255, 255);
        assert_eq!(rgb565, 0xFFFF);
        let rgb888 = rgb565_to_rgb888(rgb565);
        assert_eq!(rgb888, [255, 255, 255]);
    }

    #[test]
    fn test_rgb565_red() {
        let rgb565 = rgb888_to_rgb565(255, 0, 0);
        assert_eq!(rgb565, 0xF800); // 11111 000000 00000
        let rgb888 = rgb565_to_rgb888(rgb565);
        assert_eq!(rgb888, [255, 0, 0]);
    }

    #[test]
    fn test_rgb565_green() {
        let rgb565 = rgb888_to_rgb565(0, 255, 0);
        assert_eq!(rgb565, 0x07E0); // 00000 111111 00000
        let rgb888 = rgb565_to_rgb888(rgb565);
        assert_eq!(rgb888, [0, 255, 0]);
    }

    #[test]
    fn test_rgb565_blue() {
        let rgb565 = rgb888_to_rgb565(0, 0, 255);
        assert_eq!(rgb565, 0x001F); // 00000 000000 11111
        let rgb888 = rgb565_to_rgb888(rgb565);
        assert_eq!(rgb888, [0, 0, 255]);
    }

    #[test]
    fn test_rgb565_precision_loss() {
        // Test that conversion has acceptable precision loss
        let original = [123, 234, 56];
        let rgb565 = rgb888_to_rgb565(original[0], original[1], original[2]);
        let converted = rgb565_to_rgb888(rgb565);

        // Allow for precision loss in 5/6-bit conversion
        assert!((original[0] as i16 - converted[0] as i16).abs() <= 4); // Red: 8→5→8
        assert!((original[1] as i16 - converted[1] as i16).abs() <= 2); // Green: 8→6→8
        assert!((original[2] as i16 - converted[2] as i16).abs() <= 4); // Blue: 8→5→8
    }

    #[test]
    fn test_rgb565_roundtrip_edges() {
        // Test edge cases for round-trip conversion
        let test_cases = vec![
            [0, 0, 0],       // Black
            [255, 255, 255], // White
            [128, 128, 128], // Mid gray
            [255, 0, 0],     // Red
            [0, 255, 0],     // Green
            [0, 0, 255],     // Blue
            [255, 255, 0],   // Yellow
            [255, 0, 255],   // Magenta
            [0, 255, 255],   // Cyan
        ];

        for original in test_cases {
            let rgb565 = rgb888_to_rgb565(original[0], original[1], original[2]);
            let converted = rgb565_to_rgb888(rgb565);

            // Check each channel is within tolerance
            for i in 0..3 {
                let tolerance = if i == 1 { 2 } else { 4 }; // Green has 6 bits, others 5
                let diff = (original[i] as i16 - converted[i] as i16).abs();
                assert!(
                    diff <= tolerance,
                    "Channel {} failed: {} → {} (diff: {})",
                    i,
                    original[i],
                    converted[i],
                    diff
                );
            }
        }
    }

    #[test]
    fn test_color_distance_identical() {
        let color = [128, 64, 192, 255];
        let color_rgb = [128, 64, 192];
        assert_eq!(color_distance_squared(&color, &color_rgb), 0);
    }

    #[test]
    fn test_color_distance_different() {
        let a = [0, 0, 0, 255];
        let b = [255, 255, 255];
        let dist = color_distance_squared(&a, &b);
        // (255*3)^2 + (255*6)^2 + (255)^2 = 765^2 + 1530^2 + 255^2 = 585225 + 2340900 + 65025 = 2991150
        assert_eq!(dist, 2_991_150);
    }

    #[test]
    fn test_color_distance_green_sensitivity() {
        // Green should have more weight than blue
        let black = [0, 0, 0, 255];
        let green = [0, 100, 0];
        let blue = [0, 0, 100];

        let dist_green = color_distance_squared(&black, &green);
        let dist_blue = color_distance_squared(&black, &blue);

        // Green difference should be larger due to 6× weight
        assert!(dist_green > dist_blue);
    }

    #[test]
    fn test_interpolate_endpoints() {
        let c0 = rgb888_to_rgb565(255, 0, 0); // Red
        let c1 = rgb888_to_rgb565(0, 0, 255); // Blue

        // t=0 should be c0
        assert_eq!(interpolate_rgb565(c0, c1, 0), [255, 0, 0]);

        // t=3 should be c1
        assert_eq!(interpolate_rgb565(c0, c1, 3), [0, 0, 255]);
    }

    #[test]
    fn test_interpolate_midpoints() {
        let c0 = rgb888_to_rgb565(255, 255, 255); // White
        let c1 = rgb888_to_rgb565(0, 0, 0); // Black

        // t=1: 2/3 white, 1/3 black = ~170
        let mid1 = interpolate_rgb565(c0, c1, 1);
        assert!(mid1[0] >= 168 && mid1[0] <= 172);

        // t=2: 1/3 white, 2/3 black = ~85
        let mid2 = interpolate_rgb565(c0, c1, 2);
        assert!(mid2[0] >= 83 && mid2[0] <= 87);
    }

    #[test]
    #[should_panic(expected = "Invalid interpolation parameter: 4")]
    fn test_interpolate_invalid_t() {
        let c0 = rgb888_to_rgb565(255, 0, 0);
        let c1 = rgb888_to_rgb565(0, 0, 255);
        interpolate_rgb565(c0, c1, 4); // Should panic
    }
}
