//! DDS format types and error definitions.

use std::fmt;

/// DDS compression format.
///
/// Every encoder interface takes a `DdsFormat` ([`ImageCompressor`],
/// [`MipmapCompressor`], [`DdsEncoder`]), and that parameter is the extension
/// point for formats XEarthLayer may support later. **Only BC1 is implemented
/// today** (#395): every other value is rejected with
/// [`DdsError::UnsupportedFormat`] when the service builds its encoder
/// (`service::builder::create_encoder`, so a wrong format fails at startup)
/// and at every encode entry point (`DdsEncoder::encode` and
/// `encode_with_mipmaps`, each compressor's `compress`, and the GPU channel
/// before it sends). Constructors such as `DdsEncoder::new` do not reject. [`DdsFormat::is_supported`] is the one statement of
/// what is supported.
///
/// The variants beyond BC1 name formats without implementing them; their
/// format facts stay because they describe the format and encode nothing:
/// the block size in [`DdsFormat::block_bytes`] (with the rest of the file
/// geometry in [`crate::dds::layout`]) and the FourCC in [`DdsHeader`].
/// Adding a format means a kernel in
/// every backend, a format-aware expected tile size for the FUSE read gate and
/// the DDS disk cache validator, a placeholder in that format, and a
/// user-facing setting. See `docs/dev/dds-implementation.md`.
///
/// [`ImageCompressor`]: crate::dds::ImageCompressor
/// [`MipmapCompressor`]: crate::dds::MipmapCompressor
/// [`DdsEncoder`]: crate::dds::DdsEncoder
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DdsFormat {
    /// BC1/DXT1 compression (4:1, no alpha or 1-bit alpha). The only
    /// supported format.
    BC1,
    /// BC3/DXT5 compression (4:1, 8-bit alpha). Not supported: rejected with
    /// [`DdsError::UnsupportedFormat`]. Retired in #395: twice the size of
    /// BC1 for an alpha channel opaque imagery does not use.
    BC3,
}

impl DdsFormat {
    /// Whether XEarthLayer can encode this format. Only BC1.
    pub const fn is_supported(self) -> bool {
        matches!(self, DdsFormat::BC1)
    }

    /// `Ok` for a supported format, else [`DdsError::UnsupportedFormat`]
    /// naming it. Every encode entry point calls this before doing any work.
    pub fn ensure_supported(self) -> Result<(), DdsError> {
        if self.is_supported() {
            Ok(())
        } else {
            Err(DdsError::UnsupportedFormat(format!(
                "{self}: XEarthLayer encodes BC1 only"
            )))
        }
    }
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
    fn only_bc1_is_supported() {
        assert!(DdsFormat::BC1.is_supported());
        assert!(!DdsFormat::BC3.is_supported());
    }

    #[test]
    fn ensure_supported_accepts_bc1() {
        assert_eq!(DdsFormat::BC1.ensure_supported(), Ok(()));
    }

    #[test]
    fn ensure_supported_rejects_every_other_format() {
        assert!(matches!(
            DdsFormat::BC3.ensure_supported(),
            Err(DdsError::UnsupportedFormat(msg)) if msg.contains("BC3")
        ));
    }

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
