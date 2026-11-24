//! Magenta placeholder DDS texture for error handling.
//!
//! When tile generation fails, we return a solid magenta (255, 0, 255) DDS texture
//! so X-Plane displays a clearly visible error indicator instead of crashing.
//!
//! # Static Placeholder
//!
//! The placeholder is a solid colour, so it needs no encoder: every 4×4 block
//! of every level is the same 8 bytes, one BC1 block of opaque magenta. The
//! file is the DDS header of a 4096×4096 BC1 tile with its full mip chain,
//! then that block once for every block of the chain: [`EXPECTED_DDS_SIZE`]
//! bytes, with its geometry from [`crate::dds::layout`]. Building it
//! allocates the file and nothing else (no image, no mip chain, no encode;
//! about a millisecond).
//!
//! [`init_placeholder_cache`] builds it when the service starts
//! (`XEarthLayerService::start`), so no FUSE handler ever builds it (#386).
//! It is never written to a cache.

use crate::dds::layout::{self, full_chain_levels};
use crate::dds::{DdsFormat, DdsHeader};
use bytes::Bytes;
use std::sync::OnceLock;

/// Static placeholder cache - built once, at service start.
/// `Bytes`, so handing out the placeholder is a refcount bump rather than an
/// 11 MB copy. It is returned on every timeout, which is exactly when the
/// process is least able to afford the allocation (#237).
static DEFAULT_PLACEHOLDER: OnceLock<Bytes> = OnceLock::new();

/// Width and height of the placeholder: a standard ortho tile.
const PLACEHOLDER_SIZE: u32 = 4096;

/// RGB565 magenta: red 31, green 0, blue 31, which decodes to exactly
/// (255, 0, 255).
const MAGENTA_565: u16 = 0xF81F;

/// One BC1 block of opaque magenta: both endpoints magenta, every index 0
/// (`color0`).
///
/// Equal endpoints select BC1's three-colour mode, whose index 3 is
/// transparent black; no texel here takes index 3, so every texel is opaque
/// magenta. It is the block the ISPC encoder wrote for solid magenta when the
/// placeholder was encoded from an image, so the placeholder's bytes did not
/// change when that encode was removed (#386).
const MAGENTA_BC1_BLOCK: [u8; 8] = {
    let c = MAGENTA_565.to_le_bytes();
    [c[0], c[1], c[0], c[1], 0, 0, 0, 0]
};

/// A `size`×`size` BC1 DDS with its full mip chain whose every block is
/// `block`: the header, then `block` once for every block of every level.
///
/// Its one large allocation is the file itself, sized by
/// [`crate::dds::layout`].
fn solid_bc1_full_chain(size: u32, block: [u8; 8]) -> Vec<u8> {
    let format = DdsFormat::BC1;
    let levels = full_chain_levels(size, size);
    let len = layout::file_len(size, size, levels, format);
    let mut bytes = Vec::with_capacity(len);
    bytes.extend_from_slice(&DdsHeader::new(size, size, levels, format).to_bytes());
    bytes.resize(len, 0);
    for chunk in bytes[layout::HEADER_LEN..].chunks_exact_mut(format.block_bytes()) {
        chunk.copy_from_slice(&block);
    }
    bytes
}

/// Get the default placeholder: a 4096×4096 BC1 DDS with its full mip chain,
/// solid opaque magenta, [`EXPECTED_DDS_SIZE`] bytes.
///
/// The placeholder is built once and cached for the life of the process;
/// [`init_placeholder_cache`] builds it at service start. Called before that,
/// it builds it here, which takes about a millisecond and cannot fail.
///
/// # Returns
///
/// The cached placeholder (a refcount bump, not a copy).
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
        .get_or_init(|| Bytes::from(solid_bc1_full_chain(PLACEHOLDER_SIZE, MAGENTA_BC1_BLOCK)))
        .clone()
}

/// Build the placeholder cache now.
///
/// `XEarthLayerService::start` calls this before any FUSE request can
/// arrive, so no FUSE handler ever builds the placeholder (#386). Building
/// it takes about a millisecond and cannot fail; calling this again does
/// nothing.
///
/// # Example
///
/// ```
/// use xearthlayer::fuse::init_placeholder_cache;
///
/// // Initialize at startup
/// init_placeholder_cache();
/// ```
pub fn init_placeholder_cache() {
    get_default_placeholder();
}

/// Expected DDS size for a 4096×4096 BC1 tile with a full mipmap chain.
///
/// This is the standard size for X-Plane ortho tiles: 13 levels, 11,184,952
/// bytes. `validate_dds_or_placeholder` gates every FUSE read against it, so
/// it must track what the encoder actually emits — a regression test asserts
/// both agree.
///
/// The arithmetic lives in [`crate::dds::layout`] and the constant in
/// `cache::integrity` (the canonical cache integrity model shared by all
/// cache tiers) so that DDS disk cache validation and this FUSE-boundary
/// check can never drift apart. This re-export exists so existing callers of
/// `fuse::EXPECTED_DDS_SIZE` keep working. The dependency direction is
/// `fuse -> cache::integrity`, never the reverse: `cache::integrity` must
/// not import anything from `fuse`.
pub use crate::cache::integrity::EXPECTED_DDS_SIZE;

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
    use crate::dds::DdsEncoder;
    use image::RgbaImage;

    /// The 16 texels of a BC1 block, decoded as RGBA.
    fn decode(block: &[u8]) -> Vec<[u8; 4]> {
        let mut rgba = [0u8; 4 * 4 * 4];
        bcdec_rs::bc1(block, &mut rgba, 16);
        rgba.chunks_exact(4)
            .map(|t| [t[0], t[1], t[2], t[3]])
            .collect()
    }

    /// The constant is what the encoder writes for solid magenta, so the
    /// placeholder built from it is byte for byte the one the old full
    /// encode produced (#386 review).
    #[test]
    fn the_magenta_block_is_what_ispc_encodes_for_solid_magenta() {
        use crate::dds::{DdsFormat, ImageCompressor, IspcCompressor};
        let magenta = RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 255, 255]));
        let encoded = IspcCompressor.compress(&magenta, DdsFormat::BC1).unwrap();
        assert_eq!(encoded, MAGENTA_BC1_BLOCK);
    }

    #[test]
    fn the_magenta_block_decodes_to_opaque_magenta() {
        assert_eq!(decode(&MAGENTA_BC1_BLOCK), vec![[255, 0, 255, 255]; 16]);
    }

    #[test]
    fn no_texel_of_the_magenta_block_takes_the_transparent_index() {
        // Equal endpoints select the three-colour mode, where index 3 is
        // transparent black: every texel must take another index.
        let c0 = u16::from_le_bytes([MAGENTA_BC1_BLOCK[0], MAGENTA_BC1_BLOCK[1]]);
        let c1 = u16::from_le_bytes([MAGENTA_BC1_BLOCK[2], MAGENTA_BC1_BLOCK[3]]);
        assert_eq!((c0, c1), (MAGENTA_565, MAGENTA_565));
        let indices = u32::from_le_bytes(MAGENTA_BC1_BLOCK[4..].try_into().unwrap());
        for texel in 0..16 {
            assert_ne!((indices >> (2 * texel)) & 3, 3, "texel {texel}");
        }
    }

    #[test]
    fn the_placeholder_is_the_size_of_a_tile() {
        assert_eq!(get_default_placeholder().len(), EXPECTED_DDS_SIZE);
    }

    #[test]
    fn the_placeholder_header_is_what_the_encoder_writes_for_a_real_tile() {
        let tile = DdsEncoder::new(DdsFormat::BC1)
            .encode(RgbaImage::new(4096, 4096))
            .unwrap();
        let placeholder = get_default_placeholder();
        assert_eq!(placeholder.len(), tile.len());
        assert_eq!(
            placeholder[..layout::HEADER_LEN],
            tile[..layout::HEADER_LEN],
            "DDS header"
        );
    }

    #[test]
    fn every_block_of_every_level_decodes_to_opaque_magenta() {
        let placeholder = get_default_placeholder();
        let payload = &placeholder[layout::HEADER_LEN..];
        // 4096 down to 1: 1,398,103 blocks.
        assert_eq!(payload.len(), 1_398_103 * 8);
        let blocks: std::collections::HashSet<&[u8]> = payload.chunks_exact(8).collect();
        assert_eq!(blocks.len(), 1, "one block, repeated");
        for block in blocks {
            assert_eq!(decode(block), vec![[255, 0, 255, 255]; 16]);
        }
    }

    #[test]
    fn a_solid_dds_is_the_header_then_its_block_with_no_image_behind_it() {
        // By construction: the payload is the block it is given, verbatim,
        // wherever it falls in the chain, so no image is downsampled or
        // encoded to make it, and the file is allocated once, at its size.
        let block = [1, 2, 3, 4, 5, 6, 7, 8];
        let dds = solid_bc1_full_chain(64, block);
        let levels = full_chain_levels(64, 64);
        assert_eq!(dds.len(), layout::file_len(64, 64, levels, DdsFormat::BC1));
        assert_eq!(dds.capacity(), dds.len(), "allocated once, at its size");
        assert_eq!(
            dds[..layout::HEADER_LEN],
            DdsHeader::new(64, 64, levels, DdsFormat::BC1).to_bytes()
        );
        assert!(dds[layout::HEADER_LEN..]
            .chunks_exact(8)
            .all(|b| b == block));
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

        // The data should be identical, and the same allocation
        assert_eq!(placeholder1, placeholder2);
        assert_eq!(placeholder1.as_ptr(), placeholder2.as_ptr());
    }

    #[test]
    fn init_placeholder_cache_builds_the_placeholder_get_returns() {
        init_placeholder_cache();
        init_placeholder_cache();

        let placeholder = DEFAULT_PLACEHOLDER.get().expect("built by init");
        assert_eq!(placeholder.as_ptr(), get_default_placeholder().as_ptr());
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
