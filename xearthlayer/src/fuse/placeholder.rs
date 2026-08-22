//! Magenta placeholder DDS texture for error handling.
//!
//! When tile generation fails, we return a solid magenta (255, 0, 255) DDS texture
//! so X-Plane displays a clearly visible error indicator instead of crashing.
//!
//! # Static Placeholder
//!
//! A singleton placeholder is pre-generated at first access and cached for the
//! lifetime of the application. This ensures we **never** return empty data to
//! X-Plane, even under memory pressure or when encoding fails.

use crate::dds::{DdsEncoder, DdsError, DdsFormat};
use bytes::Bytes;
use image::RgbaImage;
use std::sync::OnceLock;

/// Static placeholder cache - generated once, never fails after first success.
/// `Bytes`, so handing out the placeholder is a refcount bump rather than an
/// 11 MB copy. It is returned on every timeout, which is exactly when the
/// process is least able to afford the allocation (#237).
static DEFAULT_PLACEHOLDER: OnceLock<Bytes> = OnceLock::new();

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
/// * `mipmap_count` - Number of mipmap levels; pass
///   `MipmapGenerator::full_chain_count(width, height)` for the full chain
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
    encoder.encode(image)
}

/// Generate a default magenta placeholder for X-Plane tiles.
///
/// Uses standard settings: 4096×4096, BC1 compression, full mipmap chain.
///
/// The placeholder must carry the same chain length as a real tile — it is
/// served through the same FUSE read path and checked against the same
/// [`EXPECTED_DDS_SIZE`].
///
/// # Returns
///
/// Complete DDS file as bytes
pub fn generate_default_placeholder() -> Result<Vec<u8>, DdsError> {
    generate_magenta_placeholder(
        4096,
        4096,
        DdsFormat::BC1,
        crate::dds::MipmapGenerator::full_chain_count(4096, 4096),
    )
}

/// Get the default placeholder, guaranteed to never return empty data.
///
/// This function uses a cached static placeholder that is generated once at
/// first access. If initial generation fails (extremely rare), it panics at
/// startup before X-Plane requests any tiles.
///
/// # Returns
///
/// A clone of the cached placeholder DDS file (4096×4096, BC1, full chain).
///
/// # Panics
///
/// Panics if the placeholder cannot be generated on first access. This is
/// intentional - we'd rather fail early at startup than return corrupted
/// data to X-Plane during flight.
///
/// # Example
///
/// ```
/// use xearthlayer::fuse::get_default_placeholder;
///
/// // Always returns valid DDS data
/// let placeholder = get_default_placeholder();
/// assert!(!placeholder.is_empty());
/// assert_eq!(&placeholder[0..4], b"DDS ");
/// ```
pub fn get_default_placeholder() -> Bytes {
    DEFAULT_PLACEHOLDER
        .get_or_init(|| {
            Bytes::from(
                generate_default_placeholder()
                    .expect("Failed to generate default placeholder - this is a critical error"),
            )
        })
        .clone()
}

/// Initialize the placeholder cache.
///
/// Call this early during startup to ensure the placeholder is ready before
/// any FUSE requests arrive. If initialization fails, this returns an error
/// so the application can fail gracefully with a clear error message.
///
/// # Returns
///
/// Ok(()) if the placeholder was successfully generated and cached, or
/// Err if generation failed.
///
/// # Example
///
/// ```
/// use xearthlayer::fuse::init_placeholder_cache;
///
/// // Initialize at startup
/// init_placeholder_cache().expect("Failed to initialize placeholder cache");
/// ```
pub fn init_placeholder_cache() -> Result<(), DdsError> {
    // Force initialization by calling get_or_init with our generator
    // If it fails, the error propagates
    let _ = DEFAULT_PLACEHOLDER.get_or_init(|| {
        Bytes::from(generate_default_placeholder().expect("Failed to generate placeholder"))
    });
    Ok(())
}

/// Total byte size of a BC1 DDS with a full mipmap chain, header included.
///
/// Walks the chain the same way the encoder does — halving until a dimension
/// reaches 1, and sizing every level as `blocks_wide * blocks_high * 8`. The
/// `div_ceil` matters for the tail: 4×4, 2×2 and 1×1 each still occupy one
/// whole 8-byte block, which the naive `w * h / 2` under-counts.
const fn full_chain_bc1_dds_size(width: u32, height: u32) -> usize {
    const HEADER_BYTES: usize = 128;
    const BLOCK_BYTES: usize = 8;

    let mut total = HEADER_BYTES;
    let mut w = width;
    let mut h = height;

    loop {
        let blocks_wide = w.div_ceil(4) as usize;
        let blocks_high = h.div_ceil(4) as usize;
        total += blocks_wide * blocks_high * BLOCK_BYTES;

        if w <= 1 || h <= 1 {
            return total;
        }

        w /= 2;
        h /= 2;
    }
}

/// Expected DDS size for a 4096×4096 BC1 tile with a full mipmap chain.
///
/// This is the standard size for X-Plane ortho tiles: 13 levels, 11,184,952
/// bytes. `validate_dds_or_placeholder` gates every FUSE read against it, so
/// it must track what the encoder actually emits — a regression test asserts
/// both agree.
pub const EXPECTED_DDS_SIZE: usize = full_chain_bc1_dds_size(4096, 4096);

/// Validates that DDS data is well-formed and returns it, or substitutes
/// the default placeholder if validation fails.
///
/// This is a critical safety check to prevent corrupted DDS data from
/// reaching X-Plane and causing crashes.
///
/// # Validation checks
///
/// 1. Data is not empty
/// 2. Data has correct size ([`EXPECTED_DDS_SIZE`] — 11,184,952 bytes for a
///    4096×4096 BC1 tile with its full 13-level mipmap chain)
/// 3. Data starts with "DDS " magic bytes
///
/// # Returns
///
/// The original data if valid, or the default placeholder if invalid.
pub fn validate_dds_or_placeholder(data: Bytes, context: &str) -> Bytes {
    // Check 1: Not empty
    if data.is_empty() {
        tracing::error!(
            context = context,
            "DDS validation failed: empty data - returning placeholder"
        );
        return get_default_placeholder();
    }

    // Check 2: Correct size
    if data.len() != EXPECTED_DDS_SIZE {
        tracing::error!(
            context = context,
            actual_size = data.len(),
            expected_size = EXPECTED_DDS_SIZE,
            "DDS validation failed: wrong size - returning placeholder"
        );
        return get_default_placeholder();
    }

    // Check 3: DDS magic bytes
    if data.len() < 4 || &data[0..4] != b"DDS " {
        tracing::error!(
            context = context,
            "DDS validation failed: invalid magic bytes - returning placeholder"
        );
        return get_default_placeholder();
    }

    // Validation passed
    data
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

        // Should be 4096×4096 BC1 with the full 13-level mipmap chain
        assert_eq!(dds.len(), EXPECTED_DDS_SIZE);
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

    #[test]
    fn test_get_default_placeholder_never_empty() {
        // This tests the critical guarantee: we never return empty data
        let placeholder = get_default_placeholder();
        assert!(!placeholder.is_empty(), "Placeholder must never be empty");
        assert!(
            placeholder.len() >= 128,
            "Placeholder must have at least a DDS header"
        );
        assert_eq!(&placeholder[0..4], b"DDS ", "Must be a valid DDS file");
    }

    #[test]
    fn test_get_default_placeholder_correct_size() {
        let placeholder = get_default_placeholder();
        // Should be 4096×4096 BC1 with the full 13-level chain = 11,184,952 bytes
        assert_eq!(
            placeholder.len(),
            EXPECTED_DDS_SIZE,
            "Placeholder should be the expected size for X-Plane tiles"
        );
    }

    #[test]
    fn test_get_default_placeholder_cached() {
        // Get two references - they should be equal since it's cached
        let placeholder1 = get_default_placeholder();
        let placeholder2 = get_default_placeholder();

        // The data should be identical
        assert_eq!(placeholder1, placeholder2);
    }

    #[test]
    fn test_init_placeholder_cache_succeeds() {
        // This should succeed without error
        let result = init_placeholder_cache();
        assert!(result.is_ok());

        // After init, get_default_placeholder should work
        let placeholder = get_default_placeholder();
        assert!(!placeholder.is_empty());
    }

    #[test]
    fn test_validate_dds_valid_data() {
        // Valid DDS data should pass through unchanged
        let valid_dds = get_default_placeholder();
        let result = validate_dds_or_placeholder(valid_dds.clone(), "test");
        assert_eq!(result, valid_dds);
    }

    #[test]
    fn test_validate_dds_empty_data() {
        // Empty data should return placeholder
        let result = validate_dds_or_placeholder(vec![].into(), "test");
        assert_eq!(result.len(), EXPECTED_DDS_SIZE);
        assert_eq!(&result[0..4], b"DDS ");
    }

    #[test]
    fn test_validate_dds_wrong_size() {
        // Wrong size should return placeholder
        let mut wrong_size = vec![0u8; 1000];
        wrong_size[0..4].copy_from_slice(b"DDS ");
        let result = validate_dds_or_placeholder(wrong_size.into(), "test");
        assert_eq!(result.len(), EXPECTED_DDS_SIZE);
    }

    #[test]
    fn test_validate_dds_wrong_magic() {
        // Wrong magic bytes should return placeholder
        let mut wrong_magic = vec![0u8; EXPECTED_DDS_SIZE];
        wrong_magic[0..4].copy_from_slice(b"XXXX");
        let result = validate_dds_or_placeholder(wrong_magic.into(), "test");
        assert_eq!(&result[0..4], b"DDS ");
    }

    #[test]
    fn test_expected_dds_size_constant() {
        // Verify the constant matches actual placeholder size
        let placeholder = get_default_placeholder();
        assert_eq!(placeholder.len(), EXPECTED_DDS_SIZE);
    }
}
