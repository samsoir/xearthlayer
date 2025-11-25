//! DDS header construction.

use crate::dds::types::*;

impl DdsHeader {
    /// Create a new DDS header for the given dimensions and format.
    ///
    /// # Arguments
    ///
    /// * `width` - Texture width in pixels
    /// * `height` - Texture height in pixels
    /// * `mipmap_count` - Number of mipmap levels (1 = no mipmaps)
    /// * `format` - Compression format (BC1 or BC3)
    pub fn new(width: u32, height: u32, mipmap_count: u32, format: DdsFormat) -> Self {
        let fourcc = match format {
            DdsFormat::BC1 => *b"DXT1",
            DdsFormat::BC3 => *b"DXT5",
        };

        // Calculate linear size for the main surface
        // BC1: 8 bytes per 4×4 block
        // BC3: 16 bytes per 4×4 block
        let block_size = match format {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        };
        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);
        let pitch_or_linear_size = blocks_wide * blocks_high * block_size;

        // Determine flags
        let mut flags = DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PIXELFORMAT | DDSD_LINEARSIZE;
        if mipmap_count > 1 {
            flags |= DDSD_MIPMAPCOUNT;
        }

        // Determine caps
        let mut caps = DDSCAPS_TEXTURE;
        if mipmap_count > 1 {
            caps |= DDSCAPS_COMPLEX | DDSCAPS_MIPMAP;
        }

        DdsHeader {
            magic: *b"DDS ",
            size: 124,
            flags,
            height,
            width,
            pitch_or_linear_size,
            depth: 0,
            mipmap_count,
            reserved1: [0; 11],
            pixel_format: DdsPixelFormat {
                size: 32,
                flags: DDPF_FOURCC,
                fourcc,
                rgb_bit_count: 0,
                r_bit_mask: 0,
                g_bit_mask: 0,
                b_bit_mask: 0,
                a_bit_mask: 0,
            },
            caps,
            caps2: 0,
            caps3: 0,
            caps4: 0,
            reserved2: 0,
        }
    }

    /// Convert header to byte array for writing to file.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(128);

        // Magic
        bytes.extend_from_slice(&self.magic);

        // Header fields
        bytes.extend_from_slice(&self.size.to_le_bytes());
        bytes.extend_from_slice(&self.flags.to_le_bytes());
        bytes.extend_from_slice(&self.height.to_le_bytes());
        bytes.extend_from_slice(&self.width.to_le_bytes());
        bytes.extend_from_slice(&self.pitch_or_linear_size.to_le_bytes());
        bytes.extend_from_slice(&self.depth.to_le_bytes());
        bytes.extend_from_slice(&self.mipmap_count.to_le_bytes());

        // Reserved1 (11 × u32)
        for &val in &self.reserved1 {
            bytes.extend_from_slice(&val.to_le_bytes());
        }

        // Pixel format (32 bytes)
        bytes.extend_from_slice(&self.pixel_format.size.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.flags.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.fourcc);
        bytes.extend_from_slice(&self.pixel_format.rgb_bit_count.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.r_bit_mask.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.g_bit_mask.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.b_bit_mask.to_le_bytes());
        bytes.extend_from_slice(&self.pixel_format.a_bit_mask.to_le_bytes());

        // Caps
        bytes.extend_from_slice(&self.caps.to_le_bytes());
        bytes.extend_from_slice(&self.caps2.to_le_bytes());
        bytes.extend_from_slice(&self.caps3.to_le_bytes());
        bytes.extend_from_slice(&self.caps4.to_le_bytes());
        bytes.extend_from_slice(&self.reserved2.to_le_bytes());

        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_magic() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        assert_eq!(&header.magic, b"DDS ");
    }

    #[test]
    fn test_header_size() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        assert_eq!(header.size, 124);
    }

    #[test]
    fn test_header_dimensions() {
        let header = DdsHeader::new(1024, 512, 1, DdsFormat::BC1);
        assert_eq!(header.width, 1024);
        assert_eq!(header.height, 512);
    }

    #[test]
    fn test_header_bc1_fourcc() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        assert_eq!(&header.pixel_format.fourcc, b"DXT1");
    }

    #[test]
    fn test_header_bc3_fourcc() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC3);
        assert_eq!(&header.pixel_format.fourcc, b"DXT5");
    }

    #[test]
    fn test_header_mipmap_count() {
        let header = DdsHeader::new(256, 256, 5, DdsFormat::BC1);
        assert_eq!(header.mipmap_count, 5);
    }

    #[test]
    fn test_header_no_mipmaps_flags() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);

        // Should have basic flags but not DDSD_MIPMAPCOUNT
        assert!(header.flags & DDSD_CAPS != 0);
        assert!(header.flags & DDSD_HEIGHT != 0);
        assert!(header.flags & DDSD_WIDTH != 0);
        assert!(header.flags & DDSD_PIXELFORMAT != 0);
        assert!(header.flags & DDSD_LINEARSIZE != 0);
        assert_eq!(header.flags & DDSD_MIPMAPCOUNT, 0);
    }

    #[test]
    fn test_header_with_mipmaps_flags() {
        let header = DdsHeader::new(256, 256, 5, DdsFormat::BC1);

        // Should have mipmap flag
        assert!(header.flags & DDSD_MIPMAPCOUNT != 0);
    }

    #[test]
    fn test_header_no_mipmaps_caps() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);

        // Should have TEXTURE but not COMPLEX or MIPMAP
        assert!(header.caps & DDSCAPS_TEXTURE != 0);
        assert_eq!(header.caps & DDSCAPS_COMPLEX, 0);
        assert_eq!(header.caps & DDSCAPS_MIPMAP, 0);
    }

    #[test]
    fn test_header_with_mipmaps_caps() {
        let header = DdsHeader::new(256, 256, 5, DdsFormat::BC1);

        // Should have TEXTURE, COMPLEX, and MIPMAP
        assert!(header.caps & DDSCAPS_TEXTURE != 0);
        assert!(header.caps & DDSCAPS_COMPLEX != 0);
        assert!(header.caps & DDSCAPS_MIPMAP != 0);
    }

    #[test]
    fn test_header_bc1_linear_size() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);

        // 256×256 = 64×64 blocks, each 8 bytes
        // 64 * 64 * 8 = 32768
        assert_eq!(header.pitch_or_linear_size, 32768);
    }

    #[test]
    fn test_header_bc3_linear_size() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC3);

        // 256×256 = 64×64 blocks, each 16 bytes
        // 64 * 64 * 16 = 65536
        assert_eq!(header.pitch_or_linear_size, 65536);
    }

    #[test]
    fn test_header_non_multiple_of_4() {
        // Test that non-multiple-of-4 dimensions are handled correctly
        let header = DdsHeader::new(100, 100, 1, DdsFormat::BC1);

        // 100×100 → 25×25 blocks, each 8 bytes
        // 25 * 25 * 8 = 5000
        assert_eq!(header.pitch_or_linear_size, 5000);
    }

    #[test]
    fn test_header_4096x4096_bc1() {
        let header = DdsHeader::new(4096, 4096, 5, DdsFormat::BC1);

        // 4096×4096 = 1024×1024 blocks, each 8 bytes
        // 1024 * 1024 * 8 = 8388608
        assert_eq!(header.pitch_or_linear_size, 8388608);
        assert_eq!(header.width, 4096);
        assert_eq!(header.height, 4096);
        assert_eq!(header.mipmap_count, 5);
    }

    #[test]
    fn test_header_to_bytes_size() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        let bytes = header.to_bytes();

        // Must be exactly 128 bytes (4 magic + 124 header)
        assert_eq!(bytes.len(), 128);
    }

    #[test]
    fn test_header_to_bytes_magic() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        let bytes = header.to_bytes();

        // First 4 bytes should be "DDS "
        assert_eq!(&bytes[0..4], b"DDS ");
    }

    #[test]
    fn test_header_to_bytes_size_field() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        let bytes = header.to_bytes();

        // Bytes 4-7 should be size (124) in little-endian
        let size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(size, 124);
    }

    #[test]
    fn test_header_to_bytes_dimensions() {
        let header = DdsHeader::new(1024, 512, 1, DdsFormat::BC1);
        let bytes = header.to_bytes();

        // Height at offset 12 (4 magic + 4 size + 4 flags)
        let height = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        assert_eq!(height, 512);

        // Width at offset 16
        let width = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        assert_eq!(width, 1024);
    }

    #[test]
    fn test_header_to_bytes_fourcc_bc1() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        let bytes = header.to_bytes();

        // Pixel format starts at offset 76 (4+124-32-20)
        // FourCC is at offset 84 (76 + 4 size + 4 flags)
        assert_eq!(&bytes[84..88], b"DXT1");
    }

    #[test]
    fn test_header_to_bytes_fourcc_bc3() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC3);
        let bytes = header.to_bytes();

        assert_eq!(&bytes[84..88], b"DXT5");
    }

    #[test]
    fn test_pixel_format_size_field() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        assert_eq!(header.pixel_format.size, 32);
    }

    #[test]
    fn test_pixel_format_fourcc_flag() {
        let header = DdsHeader::new(256, 256, 1, DdsFormat::BC1);
        assert!(header.pixel_format.flags & DDPF_FOURCC != 0);
    }
}
