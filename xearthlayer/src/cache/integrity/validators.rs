//! Validators for the opaque blob cache tiers (DDS tiles, raw chunks).

use super::{CacheEntryValidator, IntegrityError};

/// A validator expressed as magic bytes plus an optional exact length.
///
/// An empty `magic` slice makes [`IntegrityError::BadMagic`] unreachable —
/// every byte slice trivially starts with an empty prefix. This is how
/// [`raw_chunk_validator`] asserts non-emptiness only, without claiming a
/// magic sequence we have not verified.
pub struct MagicAndSize {
    name: &'static str,
    magic: &'static [u8],
    expected_len: Option<usize>,
}

impl CacheEntryValidator for MagicAndSize {
    fn name(&self) -> &'static str {
        self.name
    }

    fn validate(&self, bytes: &[u8]) -> Result<(), IntegrityError> {
        // Empty is checked first: an empty entry must report `Empty`, not
        // `BadMagic` or `WrongSize`, regardless of what magic/length this
        // validator otherwise requires.
        if bytes.is_empty() {
            return Err(IntegrityError::Empty);
        }

        if !bytes.starts_with(self.magic) {
            return Err(IntegrityError::BadMagic {
                expected: self.magic,
            });
        }

        if let Some(expected) = self.expected_len {
            if bytes.len() != expected {
                return Err(IntegrityError::WrongSize {
                    actual: bytes.len(),
                    expected,
                });
            }
        }

        Ok(())
    }
}

/// DDS tiles: `b"DDS "` magic and the exact full-mipmap-chain length.
pub fn dds_tile_validator() -> MagicAndSize {
    MagicAndSize {
        name: "dds_tile",
        magic: b"DDS ",
        expected_len: Some(EXPECTED_DDS_SIZE),
    }
}

/// Raw chunks: non-empty only.
///
/// Asserting a JPEG magic would require proving every provider (Bing, Go2,
/// Google, and any future addition) returns JPEG for every tile, which has
/// not been audited. Claiming an unverified magic would turn a correctness
/// fix into an outage by rejecting valid chunks, so this checks only that the
/// entry holds bytes at all.
pub fn raw_chunk_validator() -> MagicAndSize {
    MagicAndSize {
        name: "raw_chunk",
        magic: &[],
        expected_len: None,
    }
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
/// it must track what the encoder actually emits —
/// `fuse::placeholder::tests::test_generate_default_placeholder` asserts both
/// agree, through the real `DdsEncoder`.
///
// TODO(#253): BC1-only; texture.format also accepts bc3
pub const EXPECTED_DDS_SIZE: usize = full_chain_bc1_dds_size(4096, 4096);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dds_validator_accepts_a_well_formed_tile() {
        let mut bytes = vec![0u8; EXPECTED_DDS_SIZE];
        bytes[0..4].copy_from_slice(b"DDS ");

        assert_eq!(dds_tile_validator().validate(&bytes), Ok(()));
    }

    #[test]
    fn dds_validator_rejects_wrong_magic() {
        let mut bytes = vec![0u8; EXPECTED_DDS_SIZE];
        bytes[0..4].copy_from_slice(b"JPEG");

        assert!(matches!(
            dds_tile_validator().validate(&bytes),
            Err(IntegrityError::BadMagic { .. })
        ));
    }

    #[test]
    fn dds_validator_rejects_a_truncated_tile() {
        let mut bytes = vec![0u8; EXPECTED_DDS_SIZE / 2];
        bytes[0..4].copy_from_slice(b"DDS ");

        assert!(matches!(
            dds_tile_validator().validate(&bytes),
            Err(IntegrityError::WrongSize { .. })
        ));
    }

    #[test]
    fn validators_reject_empty_entries() {
        assert_eq!(
            dds_tile_validator().validate(&[]),
            Err(IntegrityError::Empty)
        );
        assert_eq!(
            raw_chunk_validator().validate(&[]),
            Err(IntegrityError::Empty)
        );
    }

    #[test]
    fn chunk_validator_accepts_any_non_empty_bytes() {
        // Deliberately not a JPEG: we have not audited every provider's
        // response format, so chunk validation asserts only non-emptiness.
        assert_eq!(raw_chunk_validator().validate(&[0x00, 0x01]), Ok(()));
    }
}
