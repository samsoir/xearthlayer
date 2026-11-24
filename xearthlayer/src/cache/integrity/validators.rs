//! Validators for the opaque blob cache tiers (DDS tiles, raw chunks).

use super::{CacheEntryValidator, IntegrityError};
use crate::dds::{layout, DdsFormat};

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

/// Magic that opens every lit disk cache entry.
pub const LIT_ENTRY_MAGIC: &[u8; 4] = b"XELL";

/// Lit payloads: the lit entry magic; length varies by zoom, and the night
/// source checks it (with the fingerprint) against the layout it expects.
pub fn lit_payload_validator() -> MagicAndSize {
    MagicAndSize {
        name: "lit_payload",
        magic: LIT_ENTRY_MAGIC,
        expected_len: None,
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

/// Expected DDS size for a 4096×4096 BC1 tile with a full mipmap chain.
///
/// This is the standard size for X-Plane ortho tiles: 13 levels, 11,184,952
/// bytes, computed by [`crate::dds::layout`], which owns the DDS size
/// arithmetic. `validate_dds_or_placeholder` gates every FUSE read against
/// it, so it must track what the encoder actually emits: the encoder's
/// `test_default_emits_full_chain_4096_bc1` pins the encoded length, and the
/// service's `expected_dds_size()` (FUSE `st_size`) is asserted equal to it
/// for the default configuration.
///
/// It is BC1-only by construction, which is correct because BC1 is the only
/// format the encoder accepts (#395). A second format would need a
/// format-aware size here, for FUSE `st_size` and for the DDS disk validator:
/// see `docs/dev/dds-implementation.md`, "Supported formats".
pub const EXPECTED_DDS_SIZE: usize = layout::file_len(
    4096,
    4096,
    layout::full_chain_levels(4096, 4096),
    DdsFormat::BC1,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lit_payloads_need_the_lit_magic_and_any_length() {
        let v = lit_payload_validator();
        assert!(v.validate(b"XELL").is_ok());
        assert!(v
            .validate(&[b"XELL".as_slice(), &[0u8; 1000]].concat())
            .is_ok());
        assert!(v.validate(b"DDS \0\0\0\0").is_err());
        assert!(v.validate(b"").is_err());
    }

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
