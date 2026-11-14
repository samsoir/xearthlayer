//! The CMDS walk: network chains turned into road segments, every other
//! command skipped byte-exactly.

use super::pool::Pool;
use super::{le_u32, Bytes, Chains, DsfError};

/// Chain indices a walk may visit beyond 4 per pool point. A real DSF
/// visits about one index per pool point.
pub(super) const WALK_SLACK: u64 = 65_536;

/// Turns network chains into segments by DSFLib's rule: a junction ends
/// the open segment (unless it is the chain's first point) and begins a
/// new one (unless it is the last); any other point is a shape point. The
/// segment buffer is reused, points of segments `keep` rejects are never
/// computed, and the indices visited are budgeted from the pool sizes, so
/// a small pool walked many times cannot grow the output or the time
/// without bound.
pub(super) struct Segmenter<'a, K> {
    pub(super) pools: &'a [Pool],
    pub(super) origin: (f64, f64),
    pub(super) keep: K,
    pub(super) visits_left: u64,
    pub(super) out: Chains,
    pub(super) cur: Vec<(f32, f32)>,
    pub(super) sub: u8,
    pub(super) open: bool,
    pub(super) keeping: bool,
}

impl<K: Fn(u8) -> bool> Segmenter<'_, K> {
    fn chain(
        &mut self,
        pool: usize,
        subtype: u8,
        idx: impl ExactSizeIterator<Item = u32>,
    ) -> Result<(), DsfError> {
        let p = self.pools.get(pool).ok_or(DsfError::BadPool)?;
        let count = idx.len() as u64;
        if count > self.visits_left {
            return Err(DsfError::TooLarge);
        }
        self.visits_left -= count;
        let last = idx.len().saturating_sub(1);
        for (counter, i) in idx.enumerate() {
            let i = i as usize;
            if i >= p.len() {
                return Err(DsfError::BadPool);
            }
            if p.is_junction(i) {
                if counter > 0 && self.open {
                    if self.keeping {
                        self.cur.push(p.point(i, self.origin));
                        self.out.push(self.sub, &self.cur);
                    }
                    self.open = false;
                }
                if counter < last {
                    self.cur.clear();
                    self.sub = subtype;
                    self.keeping = (self.keep)(subtype);
                    self.open = true;
                    if self.keeping {
                        self.cur.push(p.point(i, self.origin));
                    }
                }
            } else if self.open && self.keeping {
                self.cur.push(p.point(i, self.origin));
            }
        }
        Ok(())
    }
}

pub(super) fn walk_commands<K: Fn(u8) -> bool>(
    cmds: &[u8],
    seg: &mut Segmenter<'_, K>,
) -> Result<(), DsfError> {
    let mut r = Bytes::new(cmds);
    let (mut pool, mut offset, mut subtype) = (0usize, 0u32, 0u8);
    while !r.done() {
        match r.u8()? {
            1 => pool = usize::from(r.u16()?), // PoolSelect
            2 => offset = r.u32()?,            // JunctionOffsetSelect
            3 => {
                r.take(1)?; // SetDefinition8
            }
            4 => {
                r.take(2)?; // SetDefinition16
            }
            5 => {
                r.take(4)?; // SetDefinition32
            }
            6 => subtype = r.u8()?, // SetRoadSubtype8
            7 => {
                r.take(2)?; // Object
            }
            8 => {
                r.take(4)?; // ObjectRange
            }
            9 => {
                // NetworkChain: u8 count, u16 indices plus the junction offset.
                let n = usize::from(r.u8()?);
                let idx = r
                    .take(2 * n)?
                    .chunks_exact(2)
                    .map(|c| offset.wrapping_add(u32::from(u16::from_le_bytes([c[0], c[1]]))));
                seg.chain(pool, subtype, idx)?;
            }
            10 => {
                // NetworkChainRange: [first, last) plus the junction offset.
                let a = offset.wrapping_add(u32::from(r.u16()?));
                let b = offset.wrapping_add(u32::from(r.u16()?));
                seg.chain(pool, subtype, a..b.max(a))?;
            }
            11 => {
                // NetworkChain32: u8 count, u32 indices, no offset.
                let n = usize::from(r.u8()?);
                seg.chain(pool, subtype, r.take(4 * n)?.chunks_exact(4).map(le_u32))?;
            }
            12 => {
                // Polygon: u16 param, u8 count, u16 indices.
                r.take(2)?;
                let n = usize::from(r.u8()?);
                r.take(2 * n)?;
            }
            13 => {
                r.take(6)?; // PolygonRange
            }
            14 => {
                // NestedPolygon: u16 param, u8 windings, each u8 count + u16s.
                r.take(2)?;
                for _ in 0..r.u8()? {
                    let n = usize::from(r.u8()?);
                    r.take(2 * n)?;
                }
            }
            15 => {
                // NestedPolygonRange: u16 param, u8 count, count + 1 u16s.
                r.take(2)?;
                let n = usize::from(r.u8()?);
                r.take(2 + 2 * n)?;
            }
            16 => {} // TerrainPatch
            17 => {
                r.take(1)?; // TerrainPatchFlags
            }
            18 => {
                r.take(9)?; // TerrainPatchFlagsLOD
            }
            23 | 26 | 29 => {
                // Triangle, strip, fan: u8 count, u16 indices.
                let n = usize::from(r.u8()?);
                r.take(2 * n)?;
            }
            24 | 27 | 30 => {
                // Cross-pool variants: u8 count, (u16 pool, u16 index) pairs.
                let n = usize::from(r.u8()?);
                r.take(4 * n)?;
            }
            25 | 28 | 31 => {
                r.take(4)?; // Range variants
            }
            32 => {
                let n = usize::from(r.u8()?); // Comment8
                r.take(n)?;
            }
            33 => {
                let n = usize::from(r.u16()?); // Comment16
                r.take(n)?;
            }
            34 => {
                let n = r.u32()? as usize; // Comment32
                r.take(n)?;
            }
            other => return Err(DsfError::BadCommand(other)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::night::roads::dsf::test_dsf::{two_segment_dsf, Builder};
    use crate::night::roads::dsf::{read_dsf, DsfError};

    #[test]
    fn a_mid_chain_junction_splits_the_chain_into_two_segments() {
        for enc in 0..4u8 {
            let r = read_dsf(&two_segment_dsf(enc).bytes(), |_| true).unwrap();
            assert_eq!(r.chains.len(), 2, "encoding {enc}");
            assert_eq!(r.chains.points(0).len(), 3);
            assert_eq!(r.chains.points(1).len(), 3);
            let p = r.chains.points(1).start;
            assert!(
                (f64::from(r.chains.lon[p]) - 0.30).abs() < 1e-6,
                "encoding {enc}"
            );
            assert!((f64::from(r.chains.lat[p]) - 0.30).abs() < 1e-6);
            assert_eq!(r.chains.subtype, vec![50, 50]);
        }
    }

    #[test]
    fn keep_filters_subtypes_at_extraction() {
        let r = read_dsf(&two_segment_dsf(0).bytes(), |s| s != 50).unwrap();
        assert!(r.chains.is_empty());
    }

    #[test]
    fn range_and_32_bit_chains_and_junction_offset_are_read() {
        let mut b = Builder::new(46, 6);
        b.nets.push("lib/g10/roads.net".into());
        let pool = b.pool(
            46,
            6,
            &[
                (6.1, 46.1, 1),
                (6.2, 46.2, 0),
                (6.3, 46.3, 2),
                (6.4, 46.4, 0),
                (6.5, 46.5, 3),
            ],
            0,
        );
        b.cmds.push(1);
        b.cmds.extend(pool.to_le_bytes());
        b.cmds.push(6);
        b.cmds.push(10);
        b.cmds.push(2); // junction offset 2
        b.cmds.extend(2u32.to_le_bytes());
        b.cmds.push(10); // indices 2..5
        b.cmds.extend(0u16.to_le_bytes());
        b.cmds.extend(3u16.to_le_bytes());
        b.cmds.push(11); // 32-bit chain, no offset
        b.cmds.push(3);
        for i in [0u32, 1, 2] {
            b.cmds.extend(i.to_le_bytes());
        }
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
        assert_eq!(r.chains.points(0).len(), 3); // 2,3,4
        assert_eq!(r.chains.points(1).len(), 3); // 0,1,2
        let p = r.chains.points(0).start;
        assert!((f64::from(r.chains.lon[p]) - 0.3).abs() < 1e-6);
        let p = r.chains.points(1).start;
        assert!((f64::from(r.chains.lon[p]) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn non_network_commands_are_skipped_byte_exactly() {
        let mut b = two_segment_dsf(0);
        let mut pre = vec![];
        pre.extend([7u8, 0, 0]); // Object
        pre.extend([8u8, 0, 0, 1, 0]); // ObjectRange
        pre.extend([12u8, 0, 0, 2, 0, 0, 1, 0]); // Polygon param, 2 idx
        pre.extend([13u8, 0, 0, 0, 0, 1, 0]); // PolygonRange
        pre.extend([14u8, 0, 0, 1, 2, 0, 0, 1, 0]); // NestedPolygon 1 winding of 2
        pre.extend([15u8, 0, 0, 1, 0, 0, 2, 0]); // NestedPolygonRange count 1
        pre.extend([16u8]); // TerrainPatch
        pre.extend([17u8, 1]); // TerrainPatchFlags
        pre.push(18); // TerrainPatchFlagsLOD
        pre.push(1);
        pre.extend(0f32.to_le_bytes());
        pre.extend(1f32.to_le_bytes());
        pre.extend([23u8, 3, 0, 0, 1, 0, 2, 0]); // Triangle
        pre.extend([24u8, 1, 0, 0, 0, 0]); // TriangleCrossPool
        pre.extend([25u8, 0, 0, 3, 0]); // TriangleRange
        pre.extend([32u8, 6, 1, 0]); // filter comment
        pre.extend(5i32.to_le_bytes());
        pre.extend([33u8, 2, 0, 9, 9]); // Comment16 of 2 bytes
        pre.extend(b.cmds.iter());
        b.cmds = pre;
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
    }

    #[test]
    fn unknown_commands_and_truncation_are_errors_not_panics() {
        let mut b = two_segment_dsf(0);
        b.cmds.insert(0, 19);
        assert!(matches!(
            read_dsf(&b.bytes(), |_| true),
            Err(DsfError::BadCommand(19))
        ));
        let bytes = two_segment_dsf(0).bytes();
        for cut in [0, 5, 12, 20, bytes.len() / 2, bytes.len() - 17] {
            assert!(read_dsf(&bytes[..cut], |_| true).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn a_repeated_range_command_trips_the_walk_budget() {
        let mut b = Builder::new(46, 6);
        b.nets.push("lib/g10/roads.net".into());
        let pool = b.pool(46, 6, &[(6.1, 46.1, 1), (6.2, 46.2, 2)], 0);
        b.cmds.extend([1u8, pool as u8, 0]);
        // Budget 4 * 2 + 65,536 visits; each command visits 2.
        for _ in 0..40_000 {
            b.cmds.extend([10u8, 0, 0, 2, 0]);
        }
        assert_eq!(read_dsf(&b.bytes(), |_| true), Err(DsfError::TooLarge));
        assert_eq!(read_dsf(&b.bytes(), |_| false), Err(DsfError::TooLarge));
    }

    #[test]
    fn shape_points_after_one_junction_trip_the_walk_budget() {
        let mut b = Builder::new(46, 6);
        b.nets.push("lib/g10/roads.net".into());
        let pool = b.pool(46, 6, &[(6.1, 46.1, 1), (6.2, 46.2, 0), (6.3, 46.3, 0)], 0);
        b.cmds.extend([1u8, pool as u8, 0]);
        b.cmds.extend([9u8, 2, 0, 0, 1, 0]); // opens a segment
        for _ in 0..40_000 {
            b.cmds.extend([10u8, 1, 0, 3, 0]); // two shape points
        }
        assert_eq!(read_dsf(&b.bytes(), |_| true), Err(DsfError::TooLarge));
    }
}
