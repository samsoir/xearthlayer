//! PO32 point pools: planes decoded into what roads keep (lon, lat and a
//! junction bit per point), the rest read past.

use super::{le_u32, Bytes, DsfError};

/// Ceiling on the points of all PO32 pools of one DSF (about 33.5 million,
/// 256 MiB of lon and lat planes): about 3.8 times the largest real file
/// seen, X-World Pro's Tokyo `+35+139`. Run-length planes can claim this
/// many points in a few megabytes, so the cap is absolute.
pub(super) const MAX_POOL_POINTS: usize = 1 << 25;
/// Most values one byte of a run-length plane can expand to: a 5-byte
/// repeat run holds 127.
const RLE_VALUES_PER_BYTE: usize = 26;

/// Where a decoded plane goes.
trait PlaneSink {
    /// False when the plane is only read past.
    const WANTS: bool = true;
    fn reserve(&mut self, n: usize);
    fn put(&mut self, v: u32);
}

/// Reads a plane past.
struct Skip;

impl PlaneSink for Skip {
    const WANTS: bool = false;
    fn reserve(&mut self, _: usize) {}
    fn put(&mut self, _: u32) {}
}

impl PlaneSink for Vec<u32> {
    fn reserve(&mut self, n: usize) {
        self.reserve_exact(n);
    }
    fn put(&mut self, v: u32) {
        self.push(v);
    }
}

/// One bit per value: whether it is nonzero. Junction ids are only ever
/// tested for zero, so this is all a pool keeps of them.
#[derive(Default)]
struct Nonzero {
    words: Vec<u64>,
    len: usize,
}

impl Nonzero {
    fn get(&self, i: usize) -> bool {
        self.words[i / 64] >> (i % 64) & 1 != 0
    }
}

impl PlaneSink for Nonzero {
    fn reserve(&mut self, n: usize) {
        self.words.reserve_exact(n.div_ceil(64));
    }
    fn put(&mut self, v: u32) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        if v != 0 {
            *self.words.last_mut().unwrap() |= 1 << (self.len % 64);
        }
        self.len += 1;
    }
}

/// Reads one plane of `n` values into `sink`. Encodings: 0 raw,
/// 1 differenced, 2 run-length, 3 run-length then differenced (a wrapping
/// running sum from 0). A run byte with the high bit set repeats the next
/// value `b & 0x7F` times; clear, it is followed by `b & 0x7F` literal
/// values. Room is reserved only for values the remaining bytes can hold.
fn read_plane<S: PlaneSink>(r: &mut Bytes, n: usize, sink: &mut S) -> Result<(), DsfError> {
    let enc = r.u8()?;
    let differenced = matches!(enc, 1 | 3);
    let mut acc = 0u32;
    let mut put = |sink: &mut S, x: u32| {
        if differenced {
            acc = acc.wrapping_add(x);
            sink.put(acc);
        } else {
            sink.put(x);
        }
    };
    match enc {
        0 | 1 => {
            let raw = r.take(n.checked_mul(4).ok_or(DsfError::BadPool)?)?;
            if S::WANTS {
                sink.reserve(n);
                for c in raw.chunks_exact(4) {
                    put(sink, le_u32(c));
                }
            }
        }
        2 | 3 => {
            if S::WANTS {
                sink.reserve(n.min(r.remaining().saturating_mul(RLE_VALUES_PER_BYTE)));
            }
            let mut have = 0;
            while have < n {
                let b = r.u8()?;
                let c = usize::from(b & 0x7F);
                if c > n - have {
                    return Err(DsfError::BadPool);
                }
                if b & 0x80 != 0 {
                    let x = r.u32()?;
                    if S::WANTS {
                        for _ in 0..c {
                            put(sink, x);
                        }
                    }
                } else {
                    let raw = r.take(4 * c)?;
                    if S::WANTS {
                        for v in raw.chunks_exact(4) {
                            put(sink, le_u32(v));
                        }
                    }
                }
                have += c;
            }
        }
        _ => return Err(DsfError::BadPool),
    }
    Ok(())
}

/// One PO32 pool: raw planes 0 (lon) and 1 (lat), plane 3 (junction id,
/// when present) as a bit per point, and the (scale, offset) of each.
/// Elevation and curve planes are read past, never kept.
pub(super) struct Pool {
    lon: Vec<u32>,
    lat: Vec<u32>,
    junction: Option<Nonzero>,
    planes: usize,
    lon_scale: (f32, f32),
    lat_scale: (f32, f32),
}

impl Pool {
    /// Decodes a PO32 body of at most `max_points` points; its scales
    /// follow in [`Pool::set_scales`].
    pub(super) fn decode(po: &[u8], max_points: usize) -> Result<Self, DsfError> {
        let mut r = Bytes::new(po);
        let n = r.u32()? as usize;
        let planes = usize::from(r.u8()?);
        // An empty pool (Global Scenery ends GEOD with one of 0 points and
        // 0 planes) is valid. Run-length planes expand at most about 101
        // times, so a count beyond 128 values per atom byte is corrupt.
        if (planes < 2 && n > 0)
            || n.checked_mul(4)
                .is_none_or(|b| b > po.len().saturating_mul(128))
        {
            return Err(DsfError::BadPool);
        }
        if n > max_points {
            return Err(DsfError::TooLarge);
        }
        let mut pool = Self {
            lon: vec![],
            lat: vec![],
            junction: (planes > 3).then(Nonzero::default),
            planes,
            lon_scale: (0.0, 0.0),
            lat_scale: (0.0, 0.0),
        };
        for k in 0..planes {
            match (k, pool.junction.as_mut()) {
                (0, _) => read_plane(&mut r, n, &mut pool.lon)?,
                (1, _) => read_plane(&mut r, n, &mut pool.lat)?,
                (3, Some(j)) => read_plane(&mut r, n, j)?,
                _ => read_plane(&mut r, n, &mut Skip)?,
            }
        }
        Ok(pool)
    }

    /// Applies the matching SC32. The junction plane is kept as a bit per
    /// point, which expresses the test `scaled value != 0` only when the
    /// scale is 0 (the raw id, as every DSF seen so far) or finite with no
    /// offset; any other junction scaling is refused, not guessed.
    pub(super) fn set_scales(&mut self, sc: &[u8]) -> Result<(), DsfError> {
        if !sc.len().is_multiple_of(8) || sc.len() / 8 < self.planes {
            return Err(DsfError::BadPool);
        }
        let scale = |k: usize| {
            let s = &sc[k * 8..k * 8 + 8];
            (
                f32::from_le_bytes(s[..4].try_into().unwrap()),
                f32::from_le_bytes(s[4..].try_into().unwrap()),
            )
        };
        if self.planes >= 2 {
            self.lon_scale = scale(0);
            self.lat_scale = scale(1);
        }
        if self.junction.is_some() {
            let (s, o) = scale(3);
            if !(s == 0.0 || (s.is_finite() && o == 0.0)) {
                return Err(DsfError::BadPool);
            }
        }
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.lon.len()
    }

    pub(super) fn is_junction(&self, i: usize) -> bool {
        self.junction.as_ref().is_some_and(|j| j.get(i))
    }

    /// Point `i` in degrees from `(west, south)`.
    pub(super) fn point(&self, i: usize, (west, south): (f64, f64)) -> (f32, f32) {
        (
            (scaled(self.lon[i], self.lon_scale) - west) as f32,
            (scaled(self.lat[i], self.lat_scale) - south) as f32,
        )
    }
}

fn scaled(raw: u32, (scale, offset): (f32, f32)) -> f64 {
    if scale == 0.0 {
        f64::from(raw)
    } else {
        f64::from(raw) * f64::from(scale) / 4_294_967_295.0 + f64::from(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::roads::dsf::read_dsf;
    use crate::night::roads::dsf::test_dsf::two_segment_dsf;

    #[test]
    fn rle_repeat_runs_decode_and_skip_alike() {
        // Repeat 3 x 7, then a literal run of one 5.
        let mut body = vec![0x83];
        body.extend(7u32.to_le_bytes());
        body.push(0x01);
        body.extend(5u32.to_le_bytes());
        for (enc, want) in [(2u8, vec![7, 7, 7, 5]), (3, vec![7, 14, 21, 26])] {
            let mut plane = vec![enc];
            plane.extend(&body);
            plane.push(0xEE); // the next field
            let mut r = Bytes { b: &plane, p: 0 };
            let mut v: Vec<u32> = vec![];
            read_plane(&mut r, 4, &mut v).unwrap();
            assert_eq!(v, want, "encoding {enc}");
            assert_eq!(r.u8().unwrap(), 0xEE);
            let mut r = Bytes { b: &plane, p: 0 };
            read_plane(&mut r, 4, &mut Skip).unwrap();
            assert_eq!(r.u8().unwrap(), 0xEE, "skipped encoding {enc}");
        }
    }

    #[test]
    fn nonzero_bits_cross_word_boundaries() {
        let mut bits = Nonzero::default();
        bits.reserve(130);
        for i in 0..130u32 {
            bits.put(if i % 3 == 0 { i + 1 } else { 0 });
        }
        assert_eq!(bits.words.len(), 3);
        assert!((0..130).all(|i| bits.get(i) == (i % 3 == 0)));
    }

    #[test]
    fn a_run_past_the_plane_end_is_a_bad_pool() {
        let mut plane = vec![2u8, 0x85];
        plane.extend(7u32.to_le_bytes());
        let mut r = Bytes { b: &plane, p: 0 };
        assert_eq!(read_plane(&mut r, 4, &mut Skip), Err(DsfError::BadPool));
    }

    #[test]
    fn an_empty_pool_is_accepted() {
        // Global Scenery ends GEOD with a PO32 of 0 points and 0 planes.
        let mut b = two_segment_dsf(2);
        b.pools.push((vec![], vec![], 0));
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
    }

    #[test]
    fn a_scaled_junction_plane_tests_the_scaled_value() {
        let mut b = two_segment_dsf(0);
        b.pools[0].1[3] = (1.0, 0.0);
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
        // With an offset every raw 0 would be a junction: refused rather
        // than guessed, as a bit per point cannot express it.
        b.pools[0].1[3] = (1.0, 5.0);
        assert_eq!(read_dsf(&b.bytes(), |_| true), Err(DsfError::BadPool));
    }

    #[test]
    fn a_huge_pool_count_in_a_tiny_atom_is_rejected() {
        let mut b = two_segment_dsf(0);
        let mut po = 1_000_000u32.to_le_bytes().to_vec();
        po.extend([4u8, 0, 0, 0, 0]);
        b.geod_extra.push((*b"PO32", po));
        b.geod_extra.push((*b"SC32", vec![0; 32]));
        assert_eq!(read_dsf(&b.bytes(), |_| true), Err(DsfError::BadPool));
    }

    #[test]
    fn pools_beyond_the_point_cap_are_too_large() {
        let mut b = two_segment_dsf(0);
        let n = MAX_POOL_POINTS as u32 + 1;
        let mut po = n.to_le_bytes().to_vec();
        po.push(2);
        po.resize(n as usize * 4 / 128 + 64, 0);
        b.geod_extra.push((*b"PO32", po));
        b.geod_extra.push((*b"SC32", vec![0; 16]));
        assert_eq!(read_dsf(&b.bytes(), |_| true), Err(DsfError::TooLarge));
    }
}
