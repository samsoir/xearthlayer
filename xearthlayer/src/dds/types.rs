//! DDS format types and error definitions.

use std::fmt;

/// DDS compression format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DdsFormat {
    /// BC1/DXT1 compression (4:1, no alpha or 1-bit alpha)
    BC1,
    /// BC3/DXT5 compression (4:1, 8-bit alpha)
    BC3,
}

impl fmt::Display for DdsFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdsFormat::BC1 => write!(f, "BC1"),
            DdsFormat::BC3 => write!(f, "BC3"),
        }
    }
}

/// Errors that can occur during DDS encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdsError {
    /// Image dimensions are invalid (must be power of 2, at least 4×4)
    InvalidDimensions(u32, u32),
    /// Unsupported format or feature
    UnsupportedFormat(String),
    /// Compression failed
    CompressionFailed(String),
    /// Invalid mipmap chain
    InvalidMipmapChain(String),
}

impl fmt::Display for DdsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdsError::InvalidDimensions(w, h) => {
                write!(f, "Invalid dimensions: {}×{}", w, h)
            }
            DdsError::UnsupportedFormat(msg) => write!(f, "Unsupported format: {}", msg),
            DdsError::CompressionFailed(msg) => write!(f, "Compression failed: {}", msg),
            DdsError::InvalidMipmapChain(msg) => write!(f, "Invalid mipmap chain: {}", msg),
        }
    }
}

impl std::error::Error for DdsError {}

/// DDS file header (124 bytes total).
///
/// Based on Microsoft DDS specification:
/// https://docs.microsoft.com/en-us/windows/win32/direct3ddds/dds-header
#[repr(C)]
#[derive(Debug, Clone)]
pub struct DdsHeader {
    /// Magic number: "DDS " (0x20534444)
    pub magic: [u8; 4],
    /// Size of structure (124 bytes)
    pub size: u32,
    /// Flags indicating which fields are valid
    pub flags: u32,
    /// Surface height in pixels
    pub height: u32,
    /// Surface width in pixels
    pub width: u32,
    /// Pitch or linear size
    pub pitch_or_linear_size: u32,
    /// Depth for volume textures
    pub depth: u32,
    /// Number of mipmap levels
    pub mipmap_count: u32,
    /// Reserved
    pub reserved1: [u32; 11],
    /// Pixel format structure (32 bytes)
    pub pixel_format: DdsPixelFormat,
    /// Surface complexity capabilities
    pub caps: u32,
    /// Additional capabilities
    pub caps2: u32,
    /// Unused
    pub caps3: u32,
    /// Unused
    pub caps4: u32,
    /// Unused
    pub reserved2: u32,
}

/// DDS pixel format structure (32 bytes).
#[repr(C)]
#[derive(Debug, Clone)]
pub struct DdsPixelFormat {
    /// Size of structure (32 bytes)
    pub size: u32,
    /// Pixel format flags
    pub flags: u32,
    /// FourCC code (e.g., "DXT1", "DXT5")
    pub fourcc: [u8; 4],
    /// RGB bit count
    pub rgb_bit_count: u32,
    /// Red bit mask
    pub r_bit_mask: u32,
    /// Green bit mask
    pub g_bit_mask: u32,
    /// Blue bit mask
    pub b_bit_mask: u32,
    /// Alpha bit mask
    pub a_bit_mask: u32,
}

// =============================================================================
// DDS Format Constants
// =============================================================================
//
// These constants are defined per the Microsoft DDS specification:
// https://docs.microsoft.com/en-us/windows/win32/direct3ddds/dds-header
//
// Not all constants are currently used, but they are retained for:
// 1. Documentation of the complete DDS specification
// 2. Future feature support (volume textures, cubemaps, uncompressed formats)

// DDS header flags (DDSD_*)
pub const DDSD_CAPS: u32 = 0x1;
pub const DDSD_HEIGHT: u32 = 0x2;
pub const DDSD_WIDTH: u32 = 0x4;
pub const DDSD_PIXELFORMAT: u32 = 0x1000;
pub const DDSD_MIPMAPCOUNT: u32 = 0x20000;
pub const DDSD_LINEARSIZE: u32 = 0x80000;

// DDS pixel format flags (DDPF_*)
pub const DDPF_FOURCC: u32 = 0x4;

// DDS caps flags (DDSCAPS_*)
pub const DDSCAPS_COMPLEX: u32 = 0x8;
pub const DDSCAPS_MIPMAP: u32 = 0x400000;
pub const DDSCAPS_TEXTURE: u32 = 0x1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dds_format_equality() {
        assert_eq!(DdsFormat::BC1, DdsFormat::BC1);
        assert_eq!(DdsFormat::BC3, DdsFormat::BC3);
        assert_ne!(DdsFormat::BC1, DdsFormat::BC3);
    }

    #[test]
    fn test_dds_error_display() {
        let err = DdsError::InvalidDimensions(100, 200);
        assert_eq!(err.to_string(), "Invalid dimensions: 100×200");

        let err = DdsError::CompressionFailed("test".to_string());
        assert_eq!(err.to_string(), "Compression failed: test");
    }

    #[test]
    fn test_header_size() {
        // DDS header must be exactly 124 bytes (excluding magic)
        assert_eq!(std::mem::size_of::<DdsHeader>(), 128); // 124 + 4 magic
    }

    #[test]
    fn test_pixel_format_size() {
        // Pixel format must be exactly 32 bytes
        assert_eq!(std::mem::size_of::<DdsPixelFormat>(), 32);
    }
}
