//! The road cache's bytes for one DSF's extraction (`DsfRoads`).
//!
//! Little-endian: `u32 version | i32 lat | i32 lon | u8 overlay |
//! u32 n_zones, per zone: 4 f64, u32 n_poly, n_poly * (f64, f64) |
//! u32 n_chains | u32 n_points |
//! (n_chains + 1) u32 start | n_chains u8 subtype | n_points f32 lon |
//! n_points f32 lat`. Decoding takes each run of bytes before it allocates
//! for it, so a corrupt count can never ask for more memory than the entry
//! holds.

use super::dsf::{Chains, DsfRoads};
use super::exclusion::Exclusion;

/// The byte layout of a road disk entry's payload, its first field: bump it
/// when the layout changes. An entry of another version does not decode
/// and is re-extracted. What an extraction keeps is
/// `service::ROAD_EXTRACT_VERSION`'s.
pub const ROAD_CODEC_VERSION: u32 = 2;

/// Bytes of one zone with no polygon: four f64 and a u32 vertex count.
const MIN_ZONE_BYTES: usize = 36;

pub fn encode_roads(r: &DsfRoads) -> Vec<u8> {
    let c = &r.chains;
    let mut o = Vec::with_capacity(64 + c.bytes());
    o.extend_from_slice(&ROAD_CODEC_VERSION.to_le_bytes());
    o.extend_from_slice(&r.tile.0.to_le_bytes());
    o.extend_from_slice(&r.tile.1.to_le_bytes());
    o.push(u8::from(r.overlay));
    o.extend_from_slice(&(r.exclusions.len() as u32).to_le_bytes());
    for z in &r.exclusions {
        for v in [z.west, z.south, z.east, z.north] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        let p = z.polygon.as_deref().unwrap_or(&[]);
        o.extend_from_slice(&(p.len() as u32).to_le_bytes());
        for (x, y) in p {
            o.extend_from_slice(&x.to_le_bytes());
            o.extend_from_slice(&y.to_le_bytes());
        }
    }
    o.extend_from_slice(&(c.len() as u32).to_le_bytes());
    o.extend_from_slice(&(c.lon.len() as u32).to_le_bytes());
    if c.start.is_empty() {
        o.extend_from_slice(&0u32.to_le_bytes());
    }
    for s in &c.start {
        o.extend_from_slice(&s.to_le_bytes());
    }
    o.extend_from_slice(&c.subtype);
    for v in c.lon.iter().chain(&c.lat) {
        o.extend_from_slice(&v.to_le_bytes());
    }
    o
}

/// A bounded little-endian reader over a byte slice.
struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }

    /// `n` elements of `size` bytes each, as one run.
    fn run(&mut self, n: usize, size: usize) -> Option<&'a [u8]> {
        self.take(n.checked_mul(size)?)
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.p
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn count(&mut self) -> Option<usize> {
        Some(self.u32()? as usize)
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn u32s(run: &[u8]) -> Vec<u32> {
    run.chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn f32s(run: &[u8]) -> Vec<f32> {
    run.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

pub fn decode_roads(b: &[u8]) -> Option<DsfRoads> {
    let mut r = Reader { b, p: 0 };
    if r.u32()? != ROAD_CODEC_VERSION {
        return None;
    }
    let tile = (r.i32()?, r.i32()?);
    let overlay = r.flag()?;

    let zones = r.count()?;
    if zones.checked_mul(MIN_ZONE_BYTES)? > r.remaining() {
        return None;
    }
    let mut exclusions = Vec::with_capacity(zones);
    for _ in 0..zones {
        let (west, south, east, north) = (r.f64()?, r.f64()?, r.f64()?, r.f64()?);
        let np = r.count()?;
        let poly: Vec<(f64, f64)> = r
            .run(np, 16)?
            .chunks_exact(16)
            .map(|c| {
                (
                    f64::from_le_bytes(c[..8].try_into().unwrap()),
                    f64::from_le_bytes(c[8..].try_into().unwrap()),
                )
            })
            .collect();
        exclusions.push(Exclusion {
            west,
            south,
            east,
            north,
            polygon: (np > 0).then_some(poly),
        });
    }

    let nc = r.count()?;
    let np = r.count()?;
    let start = u32s(r.run(nc.checked_add(1)?, 4)?);
    if start[0] != 0 || start.windows(2).any(|w| w[0] > w[1]) || start[nc] as usize != np {
        return None;
    }
    let subtype = r.take(nc)?.to_vec();
    let lon = f32s(r.run(np, 4)?);
    let lat = f32s(r.run(np, 4)?);
    if r.remaining() != 0 {
        return None;
    }
    Some(DsfRoads {
        tile,
        overlay,
        exclusions,
        chains: Chains {
            lon,
            lat,
            start,
            subtype,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chains() -> Chains {
        let mut chains = Chains::new();
        chains.push(10, &[(0.1, 0.2), (0.3, 0.4)]);
        chains.push(100, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        chains
    }

    fn roads(chains: Chains) -> DsfRoads {
        DsfRoads {
            tile: (46, 6),
            overlay: true,
            exclusions: vec![],
            chains,
        }
    }

    #[test]
    fn round_trips_every_field() {
        let r = DsfRoads {
            exclusions: vec![
                Exclusion::parse("6/46/7/47").unwrap(),
                Exclusion::parse("6/46/7/47;6,46,7,46,6,47").unwrap(),
            ],
            ..roads(chains())
        };
        assert_eq!(decode_roads(&encode_roads(&r)), Some(r));
    }

    #[test]
    fn an_entry_of_the_previous_layout_is_rejected() {
        // Version 1 carried the network definition name between the overlay
        // flag and the zones; its bytes must never be read as version 2.
        let b = encode_roads(&roads(chains()));
        assert_eq!(&b[..4], &ROAD_CODEC_VERSION.to_le_bytes());
        assert_eq!(ROAD_CODEC_VERSION, 2);
        let mut old = b.clone();
        old[..4].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode_roads(&old).is_none());
    }

    #[test]
    fn a_dsf_without_roads_or_zones_is_version_tile_overlay_and_three_counts() {
        // u32 version, i32 lat, i32 lon, u8 overlay, then zone, chain and
        // point counts and the one start offset an empty chain list holds.
        let b = encode_roads(&roads(Chains::new()));
        assert_eq!(b.len(), 4 + 4 + 4 + 1 + 4 + 4 + 4 + 4);
    }

    #[test]
    fn a_negative_tile_and_a_plain_dsf_round_trip() {
        let r = DsfRoads {
            tile: (-34, -71),
            overlay: false,
            ..roads(Chains::new())
        };
        assert_eq!(decode_roads(&encode_roads(&r)), Some(r));
    }

    #[test]
    fn a_dsf_without_roads_encodes_small() {
        let r = DsfRoads {
            overlay: false,
            ..roads(Chains::new())
        };
        assert!(encode_roads(&r).len() < 64);
        assert_eq!(decode_roads(&encode_roads(&r)), Some(r));
    }

    #[test]
    fn truncated_or_altered_entries_are_rejected() {
        let r = DsfRoads {
            exclusions: vec![Exclusion::parse("6/46/7/47;6,46,7,46,6,47").unwrap()],
            ..roads(chains())
        };
        let b = encode_roads(&r);
        for cut in 0..b.len() {
            assert!(decode_roads(&b[..cut]).is_none(), "cut {cut}");
        }
        let mut v = b.clone();
        v[0] ^= 0xFF; // version
        assert!(decode_roads(&v).is_none());
        let mut v = b.clone();
        v.push(0); // trailing byte
        assert!(decode_roads(&v).is_none());
    }

    #[test]
    fn inconsistent_chain_offsets_are_rejected() {
        let b = encode_roads(&roads(chains()));
        // Offsets start after the three counts that precede them:
        // header 13 + zones 4 + chains 4 + points 4.
        let start0 = 13 + 4 + 4 + 4;
        for (at, value) in [(0, 1u32), (1, 99), (1, 6), (2, 4)] {
            let mut v = b.clone();
            let o = start0 + at * 4;
            v[o..o + 4].copy_from_slice(&value.to_le_bytes());
            assert!(decode_roads(&v).is_none(), "offset {at} = {value}");
        }
        let mut v = b.clone();
        v[13 + 4..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_roads(&v).is_none(), "declared chain count");
    }

    #[test]
    fn an_empty_polygon_decodes_as_none() {
        // The parser never gives one (it needs three vertices), and the
        // even-odd test cannot take one: an entry that holds one decodes as
        // a box.
        let mut zone = Exclusion::parse("6/46/7/47").unwrap();
        zone.polygon = Some(vec![]);
        let r = DsfRoads {
            exclusions: vec![zone],
            ..roads(Chains::new())
        };
        let back = decode_roads(&encode_roads(&r)).unwrap();
        assert_eq!(back.exclusions[0].polygon, None);
    }

    #[test]
    fn declared_counts_beyond_the_bytes_are_rejected_without_allocating() {
        // zones = u32::MAX straight after the 13 header bytes.
        let mut v = encode_roads(&roads(Chains::new()));
        let zones_at = 13;
        v[zones_at..zones_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_roads(&v).is_none());
        // A zone's vertex count = u32::MAX: after the zone count (4) and the
        // zone's four f64 (32).
        let r = DsfRoads {
            exclusions: vec![Exclusion::parse("6/46/7/47;6,46,7,46,6,47").unwrap()],
            ..roads(Chains::new())
        };
        let mut v = encode_roads(&r);
        let n_poly = zones_at + 4 + 32;
        assert_eq!(v[n_poly..n_poly + 4], 3u32.to_le_bytes());
        v[n_poly..n_poly + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_roads(&v).is_none());
        // The point count = u32::MAX: after the zone count and the chain count.
        let mut v = encode_roads(&roads(chains()));
        let n_points = zones_at + 4 + 4;
        assert_eq!(v[n_points..n_points + 4], 5u32.to_le_bytes());
        v[n_points..n_points + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_roads(&v).is_none());
    }
}
