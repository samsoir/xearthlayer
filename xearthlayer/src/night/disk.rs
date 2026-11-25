//! Lit disk cache entries and the fingerprint of everything a lit texture
//! is generated from.
//!
//! An entry is the generated payload prefixed by the fingerprint of its
//! inputs, so a changed brightness, recipe, art file, road model, road DSF
//! or generator version makes the stored entry a miss instead of serving
//! stale light.

use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use sha2::{Digest, Sha256};

use super::generator::{LedRegion, LIT_GENERATOR_VERSION};
use super::roads::service::DsfIdentity;
use super::store::FileIdentity;
use crate::cache::integrity::LIT_ENTRY_MAGIC;
use crate::fuse::DdsFilename;

/// Magic (4), fingerprint (32), payload length u64 LE (8).
const ENTRY_HEADER_LEN: usize = 44;

/// The road DSFs of one 1 degree tile, highest priority first.
pub type TileDsfs = ((i32, i32), Arc<[DsfIdentity]>);

/// Digest of every input a lit texture's bytes depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LitFingerprint(pub [u8; 32]);

/// The inputs shared by every tile of a session: generator version,
/// brightness, the Laminar art fingerprint and the road digest
/// (`roads::roads_digest`: `ROADS_VERSION` and the road model; all zero
/// without road lights).
pub fn session_fingerprint(brightness: f64, art: [u8; 32], roads: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(LIT_GENERATOR_VERSION.to_le_bytes());
    hasher.update(brightness.to_bits().to_le_bytes());
    hasher.update(art);
    hasher.update(roads);
    hasher.finalize().into()
}

/// The session inputs plus the identity and region of each recipe the
/// texture's footprint covers (`None` identity for a tile without one), and
/// the road DSFs of each tile, highest priority first (empty without road
/// lights). The order of tiles does not matter; the order of a tile's DSFs
/// does, because it decides which roads the merge keeps.
pub fn footprint_fingerprint(
    session: [u8; 32],
    recipes: &[((i32, i32), Option<FileIdentity>, LedRegion)],
    roads: &[TileDsfs],
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
    let mut tiles: Vec<_> = roads.iter().collect();
    tiles.sort_by_key(|(tile, _)| *tile);
    hasher.update((tiles.len() as u64).to_le_bytes());
    for ((lat, lon), dsfs) in tiles {
        hasher.update(lat.to_le_bytes());
        hasher.update(lon.to_le_bytes());
        hasher.update((dsfs.len() as u64).to_le_bytes());
        for id in dsfs.iter() {
            hasher.update((id.path.len() as u64).to_le_bytes());
            hasher.update(id.path.as_bytes());
            hasher.update(id.len.to_le_bytes());
            hasher.update(id.mtime_nanos.to_le_bytes());
        }
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

/// The stored payload length of `entry` when it has the magic and carries
/// `fp`; `None` for anything else, including a short header.
fn stamped_len(entry: &Bytes, fp: &LitFingerprint) -> Option<u64> {
    if entry.len() < ENTRY_HEADER_LEN || &entry[..4] != LIT_ENTRY_MAGIC || entry[4..36] != fp.0 {
        return None;
    }
    Some(u64::from_le_bytes(entry[36..44].try_into().ok()?))
}

/// Why a stored lit entry is not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryReject {
    /// Too short for its header, another magic, or other than as many
    /// payload bytes as its header says (a torn write).
    Corrupt,
    /// Stamped with another fingerprint: an input changed.
    Fingerprint,
    /// Stamped with another payload length than the layout's (another
    /// texture setting, or another layout of the levels).
    Length,
}

/// The payload of `entry` when it carries `fp` and is exactly
/// `expected_len` bytes long, or why not. Slices, no copy.
pub fn check_entry(
    entry: Bytes,
    fp: &LitFingerprint,
    expected_len: u64,
) -> Result<Bytes, EntryReject> {
    if entry.len() < ENTRY_HEADER_LEN || &entry[..4] != LIT_ENTRY_MAGIC {
        return Err(EntryReject::Corrupt);
    }
    if entry[4..36] != fp.0 {
        return Err(EntryReject::Fingerprint);
    }
    let stored = stamped_len(&entry, fp).ok_or(EntryReject::Corrupt)?;
    if stored != expected_len {
        return Err(EntryReject::Length);
    }
    if (entry.len() - ENTRY_HEADER_LEN) as u64 != expected_len {
        return Err(EntryReject::Corrupt);
    }
    Ok(entry.slice(ENTRY_HEADER_LEN..))
}

/// The payload of `entry` when it carries `fp` and is exactly
/// `expected_len` bytes long; `None` otherwise ([`check_entry`] says why).
/// Slices, no copy.
pub fn decode_entry(entry: Bytes, fp: &LitFingerprint, expected_len: u64) -> Option<Bytes> {
    check_entry(entry, fp, expected_len).ok()
}

/// Like [`decode_entry`] for entries whose length is not known in advance
/// (road cache entries): the stored length must match the bytes present.
pub fn decode_entry_unsized(entry: Bytes, fp: &LitFingerprint) -> Option<Bytes> {
    let stored = stamped_len(&entry, fp)?;
    ((entry.len() - ENTRY_HEADER_LEN) as u64 == stored).then(|| entry.slice(ENTRY_HEADER_LEN..))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;
    use std::sync::Arc;

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
    fn a_rejected_entry_says_why() {
        let entry = encode_entry(&fp(1), b"payload");
        assert_eq!(
            check_entry(entry.clone(), &fp(1), 7).as_deref(),
            Ok(&b"payload"[..])
        );
        assert_eq!(
            check_entry(entry.clone(), &fp(2), 7),
            Err(EntryReject::Fingerprint)
        );
        assert_eq!(
            check_entry(entry.clone(), &fp(1), 8),
            Err(EntryReject::Length)
        );
        assert_eq!(
            check_entry(entry.slice(..entry.len() - 1), &fp(1), 7),
            Err(EntryReject::Corrupt),
            "torn write"
        );
        let mut bad_magic = entry.to_vec();
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            check_entry(Bytes::from(bad_magic), &fp(1), 7),
            Err(EntryReject::Corrupt)
        );
        assert_eq!(
            check_entry(Bytes::from_static(b"XEL"), &fp(1), 0),
            Err(EntryReject::Corrupt),
            "short header"
        );
    }

    #[test]
    fn unsized_decode_checks_magic_fingerprint_and_length() {
        let e = encode_entry(&fp(1), b"abc");
        assert_eq!(
            decode_entry_unsized(e.clone(), &fp(1)).as_deref(),
            Some(&b"abc"[..])
        );
        assert!(decode_entry_unsized(e.clone(), &fp(2)).is_none());
        assert!(decode_entry_unsized(e.slice(..e.len() - 1), &fp(1)).is_none());
        let mut longer = e.to_vec();
        longer.push(0);
        assert!(decode_entry_unsized(Bytes::from(longer), &fp(1)).is_none());
        let mut bad_magic = e.to_vec();
        bad_magic[0] ^= 0xFF;
        assert!(decode_entry_unsized(Bytes::from(bad_magic), &fp(1)).is_none());
        assert!(decode_entry_unsized(Bytes::from_static(b"XEL"), &fp(1)).is_none());
    }

    #[test]
    fn every_input_moves_the_fingerprint() {
        let id = |len| {
            Some(FileIdentity {
                len,
                mtime_nanos: 5,
            })
        };
        let s = session_fingerprint(1.0, [0; 32], [0; 32]);
        let base = footprint_fingerprint(s, &[((37, -123), id(10), LedRegion::Unknown)], &[]);
        assert_ne!(
            base,
            footprint_fingerprint(
                session_fingerprint(1.25, [0; 32], [0; 32]),
                &[((37, -123), id(10), LedRegion::Unknown)],
                &[]
            ),
            "brightness"
        );
        assert_ne!(
            base,
            footprint_fingerprint(
                session_fingerprint(1.0, [1; 32], [0; 32]),
                &[((37, -123), id(10), LedRegion::Unknown)],
                &[]
            ),
            "art"
        );
        assert_ne!(
            base,
            footprint_fingerprint(s, &[((37, -123), id(11), LedRegion::Unknown)], &[]),
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
                )],
                &[]
            ),
            "recipe mtime"
        );
        assert_ne!(
            base,
            footprint_fingerprint(s, &[((37, -123), None, LedRegion::Unknown)], &[]),
            "recipe removed"
        );
        assert_ne!(
            base,
            footprint_fingerprint(
                s,
                &[
                    ((37, -123), id(10), LedRegion::Unknown),
                    ((37, -122), None, LedRegion::Unknown)
                ],
                &[]
            ),
            "another tile"
        );
        assert_eq!(
            base,
            footprint_fingerprint(s, &[((37, -123), id(10), LedRegion::Unknown)], &[]),
            "stable"
        );
    }

    #[test]
    fn the_region_moves_the_fingerprint() {
        let id = Some(FileIdentity {
            len: 10,
            mtime_nanos: 5,
        });
        let s = session_fingerprint(1.0, [0; 32], [0; 32]);
        assert_ne!(
            footprint_fingerprint(s, &[((37, -123), id, LedRegion::Na)], &[]),
            footprint_fingerprint(s, &[((37, -123), id, LedRegion::Eu)], &[]),
        );
    }

    #[test]
    fn the_tile_order_does_not_matter() {
        let s = session_fingerprint(1.0, [0; 32], [0; 32]);
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
            footprint_fingerprint(s, &[a, b], &[]),
            footprint_fingerprint(s, &[b, a], &[])
        );
    }

    #[test]
    fn road_identities_and_model_change_the_fingerprints() {
        let s1 = session_fingerprint(1.0, [0; 32], [1; 32]);
        let s2 = session_fingerprint(1.0, [0; 32], [2; 32]);
        assert_ne!(s1, s2, "road model");
        let id = |path: &str, len| DsfIdentity {
            path: path.into(),
            len,
            mtime_nanos: 1,
        };
        let tile = |ids: Vec<DsfIdentity>| ((46, 6), Arc::<[DsfIdentity]>::from(ids));
        let a = footprint_fingerprint(s1, &[], &[tile(vec![id("a.dsf", 1), id("b.dsf", 2)])]);
        let b = footprint_fingerprint(s1, &[], &[tile(vec![id("b.dsf", 2), id("a.dsf", 1)])]);
        let c = footprint_fingerprint(s1, &[], &[tile(vec![id("a.dsf", 1), id("b.dsf", 3)])]);
        let d = footprint_fingerprint(s1, &[], &[tile(vec![id("a.dsf", 1), id("c.dsf", 2)])]);
        let e = footprint_fingerprint(s1, &[], &[tile(vec![id("a.dsf", 1)])]);
        let none = footprint_fingerprint(s1, &[], &[tile(vec![])]);
        assert_ne!(a, b, "pack order matters");
        assert_ne!(a, c, "a changed DSF matters");
        assert_ne!(a, d, "another DSF path matters");
        assert_ne!(a, e, "a removed DSF matters");
        assert_ne!(
            none,
            footprint_fingerprint(s1, &[], &[]),
            "a tile without DSFs is hashed"
        );
        assert_eq!(
            a,
            footprint_fingerprint(s1, &[], &[tile(vec![id("a.dsf", 1), id("b.dsf", 2)])]),
            "stable"
        );
    }

    #[test]
    fn road_tiles_are_hashed_in_tile_order() {
        let s = session_fingerprint(1.0, [0; 32], [1; 32]);
        let ids = |path: &str| {
            Arc::<[DsfIdentity]>::from(vec![DsfIdentity {
                path: path.into(),
                len: 1,
                mtime_nanos: 1,
            }])
        };
        let (x, y) = (((46, 6), ids("x.dsf")), ((46, 7), ids("y.dsf")));
        assert_eq!(
            footprint_fingerprint(s, &[], &[x.clone(), y.clone()]),
            footprint_fingerprint(s, &[], &[y, x])
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
