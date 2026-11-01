//! Lit disk cache entries and the fingerprint of everything a lit texture
//! is generated from.
//!
//! An entry is the generated payload prefixed by the fingerprint of its
//! inputs, so a changed brightness, recipe, art file or generator version
//! makes the stored entry a miss instead of serving stale light.

use bytes::{BufMut, Bytes, BytesMut};
use sha2::{Digest, Sha256};

use super::generator::{LedRegion, LIT_GENERATOR_VERSION};
use super::store::FileIdentity;
use crate::cache::integrity::LIT_ENTRY_MAGIC;
use crate::fuse::DdsFilename;

/// Magic (4), fingerprint (32), payload length u64 LE (8).
const ENTRY_HEADER_LEN: usize = 44;

/// Digest of every input a lit texture's bytes depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LitFingerprint(pub [u8; 32]);

/// The inputs shared by every tile of a session: generator version,
/// brightness and the Laminar art fingerprint.
pub fn session_fingerprint(brightness: f64, art: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(LIT_GENERATOR_VERSION.to_le_bytes());
    hasher.update(brightness.to_bits().to_le_bytes());
    hasher.update(art);
    hasher.finalize().into()
}

/// The session inputs plus the identity and region of each recipe the
/// texture's footprint covers (`None` identity for a tile without one).
/// Order does not matter.
pub fn footprint_fingerprint(
    session: [u8; 32],
    recipes: &[((i32, i32), Option<FileIdentity>, LedRegion)],
) -> LitFingerprint {
    let mut sorted = recipes.to_vec();
    sorted.sort_by_key(|(tile, _, _)| *tile);
    let mut hasher = Sha256::new();
    hasher.update(session);
    for ((lat, lon), identity, region) in sorted {
        hasher.update(lat.to_le_bytes());
        hasher.update(lon.to_le_bytes());
        match identity {
            None => hasher.update([0u8]),
            Some(id) => {
                hasher.update([1u8]);
                hasher.update(id.len.to_le_bytes());
                hasher.update(id.mtime_nanos.to_le_bytes());
            }
        }
        hasher.update([region.tag()]);
    }
    LitFingerprint(hasher.finalize().into())
}

/// The disk cache key of a lit texture: `lit:{zoom}:{row}:{col}`.
pub fn lit_cache_key(name: &DdsFilename) -> String {
    format!("lit:{}:{}:{}", name.zoom, name.row, name.col)
}

/// The bytes stored for `payload`, stamped with `fp`.
pub fn encode_entry(fp: &LitFingerprint, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(ENTRY_HEADER_LEN + payload.len());
    out.put_slice(LIT_ENTRY_MAGIC);
    out.put_slice(&fp.0);
    out.put_u64_le(payload.len() as u64);
    out.put_slice(payload);
    out.freeze()
}

/// The payload of `entry` when it carries `fp` and is exactly
/// `expected_len` bytes long; `None` otherwise. Slices, no copy.
pub fn decode_entry(entry: Bytes, fp: &LitFingerprint, expected_len: u64) -> Option<Bytes> {
    if entry.len() < ENTRY_HEADER_LEN || &entry[..4] != LIT_ENTRY_MAGIC || entry[4..36] != fp.0 {
        return None;
    }
    let stored = u64::from_le_bytes(entry[36..44].try_into().ok()?);
    if stored != expected_len || (entry.len() - ENTRY_HEADER_LEN) as u64 != expected_len {
        return None;
    }
    Some(entry.slice(ENTRY_HEADER_LEN..))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;

    fn fp(b: u8) -> LitFingerprint {
        LitFingerprint([b; 32])
    }

    #[test]
    fn an_entry_round_trips_to_its_payload() {
        let entry = encode_entry(&fp(1), b"payload");
        assert_eq!(&entry[..4], b"XELL");
        assert_eq!(
            decode_entry(entry, &fp(1), 7).as_deref(),
            Some(&b"payload"[..])
        );
    }

    #[test]
    fn a_different_fingerprint_is_a_miss() {
        assert_eq!(
            decode_entry(encode_entry(&fp(1), b"payload"), &fp(2), 7),
            None
        );
    }

    #[test]
    fn a_wrong_or_truncated_length_is_a_miss() {
        let entry = encode_entry(&fp(1), b"payload");
        assert_eq!(
            decode_entry(entry.clone(), &fp(1), 8),
            None,
            "layout expects another size"
        );
        assert_eq!(
            decode_entry(entry.slice(..entry.len() - 1), &fp(1), 7),
            None,
            "torn write"
        );
        assert_eq!(decode_entry(Bytes::from_static(b"XEL"), &fp(1), 0), None);
    }

    #[test]
    fn every_input_moves_the_fingerprint() {
        let id = |len| {
            Some(FileIdentity {
                len,
                mtime_nanos: 5,
            })
        };
        let s = session_fingerprint(1.0, [0; 32]);
        let base = footprint_fingerprint(s, &[((37, -123), id(10), LedRegion::Unknown)]);
        assert_ne!(
            base,
            footprint_fingerprint(
                session_fingerprint(1.25, [0; 32]),
                &[((37, -123), id(10), LedRegion::Unknown)]
            ),
            "brightness"
        );
        assert_ne!(
            base,
            footprint_fingerprint(
                session_fingerprint(1.0, [1; 32]),
                &[((37, -123), id(10), LedRegion::Unknown)]
            ),
            "art"
        );
        assert_ne!(
            base,
            footprint_fingerprint(s, &[((37, -123), id(11), LedRegion::Unknown)]),
            "recipe size"
        );
        assert_ne!(
            base,
            footprint_fingerprint(
                s,
                &[(
                    (37, -123),
                    Some(FileIdentity {
                        len: 10,
                        mtime_nanos: 6
                    }),
                    LedRegion::Unknown
                )]
            ),
            "recipe mtime"
        );
        assert_ne!(
            base,
            footprint_fingerprint(s, &[((37, -123), None, LedRegion::Unknown)]),
            "recipe removed"
        );
        assert_ne!(
            base,
            footprint_fingerprint(
                s,
                &[
                    ((37, -123), id(10), LedRegion::Unknown),
                    ((37, -122), None, LedRegion::Unknown)
                ]
            ),
            "another tile"
        );
        assert_eq!(
            base,
            footprint_fingerprint(s, &[((37, -123), id(10), LedRegion::Unknown)]),
            "stable"
        );
    }

    #[test]
    fn the_region_moves_the_fingerprint() {
        let id = Some(FileIdentity {
            len: 10,
            mtime_nanos: 5,
        });
        let s = session_fingerprint(1.0, [0; 32]);
        assert_ne!(
            footprint_fingerprint(s, &[((37, -123), id, LedRegion::Na)]),
            footprint_fingerprint(s, &[((37, -123), id, LedRegion::Eu)]),
        );
    }

    #[test]
    fn the_tile_order_does_not_matter() {
        let s = session_fingerprint(1.0, [0; 32]);
        let a = ((37, -123), None, LedRegion::Unknown);
        let b = (
            (38, -123),
            Some(FileIdentity {
                len: 1,
                mtime_nanos: 2,
            }),
            LedRegion::Eu,
        );
        assert_eq!(
            footprint_fingerprint(s, &[a, b]),
            footprint_fingerprint(s, &[b, a])
        );
    }

    #[test]
    fn lit_keys_name_zoom_row_col() {
        let name = DdsFilename {
            row: 25312,
            col: 10528,
            zoom: 16,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        };
        assert_eq!(lit_cache_key(&name), "lit:16:25312:10528");
    }
}
