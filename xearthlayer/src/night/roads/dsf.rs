//! Pure DSF reader for road networks. Reads the HEAD properties (corner,
//! overlay, `sim/exclude_net`), the NETW definition, the 32-bit point pools
//! and the command stream, keeping the network segments whose subtype
//! `keep` accepts. Every command is sized, so the others are skipped
//! byte-exactly.
//!
//! Layout: `XPLNEDSF`, `u32 1`, atoms (`u32 id, u32 size` with the size
//! counting the 8 header bytes; ids are the letters packed big-endian and
//! stored little-endian), then a 16-byte MD5 footer. A PO32 atom is
//! `u32 count, u8 planes`, then per plane `u8 encoding` and its data; the
//! matching SC32 holds `(f32 scale, f32 offset)` per plane.

use std::io::{Cursor, Read};

use super::exclusion::Exclusion;

pub const MAX_DSF_BYTES: usize = 512 * 1024 * 1024;
const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const COOKIE: &[u8; 8] = b"XPLNEDSF";
const FOOTER: usize = 16;

const HEAD: u32 = 0x4845_4144;
const PROP: u32 = 0x5052_4F50;
const DEFN: u32 = 0x4445_464E;
const NETW: u32 = 0x4E45_5457;
const GEOD: u32 = 0x4745_4F44;
const PO32: u32 = 0x504F_3332;
const SC32: u32 = 0x5343_3332;
const CMDS: u32 = 0x434D_4453;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DsfError {
    #[error("not a DSF")]
    NotDsf,
    #[error("truncated")]
    Truncated,
    #[error("bad atom")]
    BadAtom,
    #[error("bad command {0}")]
    BadCommand(u8),
    #[error("bad point pool")]
    BadPool,
    #[error("7z: {0}")]
    Archive(String),
    #[error("DSF larger than the ceiling")]
    TooLarge,
}

/// Road segments as flat point arrays. `lon`/`lat` are degrees east/north
/// of the tile's west/south corner; segment `i` owns points
/// `start[i]..start[i + 1]`, so `start` has one entry more than `subtype`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chains {
    pub lon: Vec<f32>,
    pub lat: Vec<f32>,
    pub start: Vec<u32>,
    pub subtype: Vec<u8>,
}

impl Chains {
    pub fn new() -> Self {
        Self {
            start: vec![0],
            ..Default::default()
        }
    }

    pub fn len(&self) -> usize {
        self.subtype.len()
    }

    pub fn is_empty(&self) -> bool {
        self.subtype.is_empty()
    }

    pub fn points(&self, i: usize) -> std::ops::Range<usize> {
        self.start[i] as usize..self.start[i + 1] as usize
    }

    pub fn push(&mut self, subtype: u8, pts: &[(f32, f32)]) {
        if self.start.is_empty() {
            self.start.push(0);
        }
        for &(x, y) in pts {
            self.lon.push(x);
            self.lat.push(y);
        }
        self.start.push(self.lon.len() as u32);
        self.subtype.push(subtype);
    }

    /// Heap bytes held, for the byte-bounded tile cache.
    pub fn bytes(&self) -> usize {
        self.lon.len() * 8 + self.start.len() * 4 + self.subtype.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DsfRoads {
    /// `(lat, lon)` of the south-west corner.
    pub tile: (i32, i32),
    pub overlay: bool,
    pub net_def: Option<String>,
    pub exclusions: Vec<Exclusion>,
    pub chains: Chains,
}

/// The DSF inside a 7z container, or `bytes` unchanged when not 7z.
pub fn unwrap_7z(bytes: Vec<u8>) -> Result<Vec<u8>, DsfError> {
    if !bytes.starts_with(&SEVENZ_MAGIC) {
        return Ok(bytes);
    }
    let err = |e: sevenz_rust2::Error| DsfError::Archive(e.to_string());
    let mut reader =
        sevenz_rust2::ArchiveReader::new(Cursor::new(bytes), sevenz_rust2::Password::empty())
            .map_err(err)?;
    let mut out: Option<Vec<u8>> = None;
    let mut too_large = false;
    reader
        .for_each_entries(|entry, r| {
            if entry.is_directory() {
                return Ok(true);
            }
            if entry.size() > MAX_DSF_BYTES as u64 {
                too_large = true;
                return Ok(false);
            }
            let mut buf = Vec::with_capacity(entry.size() as usize);
            r.take(MAX_DSF_BYTES as u64).read_to_end(&mut buf)?;
            out = Some(buf);
            Ok(false)
        })
        .map_err(err)?;
    if too_large {
        return Err(DsfError::TooLarge);
    }
    out.ok_or_else(|| DsfError::Archive("empty archive".into()))
}

/// A bounded little-endian reader over a byte slice.
struct Bytes<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Bytes<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DsfError> {
        let end = self.p.checked_add(n).ok_or(DsfError::Truncated)?;
        let s = self.b.get(self.p..end).ok_or(DsfError::Truncated)?;
        self.p = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, DsfError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DsfError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DsfError> {
        Ok(le_u32(self.take(4)?))
    }

    fn done(&self) -> bool {
        self.p >= self.b.len()
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}

/// Child atoms of `body` as (id, body).
fn atoms(body: &[u8]) -> Result<Vec<(u32, &[u8])>, DsfError> {
    let mut r = Bytes::new(body);
    let mut out = vec![];
    while !r.done() {
        let id = r.u32()?;
        let size = r.u32()? as usize;
        if size < 8 {
            return Err(DsfError::BadAtom);
        }
        out.push((id, r.take(size - 8)?));
    }
    Ok(out)
}

/// A string table: NUL-terminated strings, none for an empty body.
fn strings(body: &[u8]) -> Vec<String> {
    let body = body.strip_suffix(&[0]).unwrap_or(body);
    if body.is_empty() {
        return vec![];
    }
    body.split(|&c| c == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// One PO32 pool: raw planes 0 (lon), 1 (lat) and 3 (junction id, when
/// present) with their (scale, offset). Elevation and curve planes are
/// read past, never kept.
struct Pool {
    lon: Vec<u32>,
    lat: Vec<u32>,
    junction: Option<Vec<u32>>,
    lon_scale: (f32, f32),
    lat_scale: (f32, f32),
    junction_scale: (f32, f32),
}

impl Pool {
    fn len(&self) -> usize {
        self.lon.len()
    }

    fn is_junction(&self, i: usize) -> bool {
        self.junction
            .as_ref()
            .is_some_and(|j| scaled(j[i], self.junction_scale) != 0.0)
    }

    /// Point `i` in degrees from `(west, south)`.
    fn point(&self, i: usize, (west, south): (f64, f64)) -> (f32, f32) {
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

/// Reads one plane of `n` values: decoded into `out` (replacing its
/// contents) when given, else only read past. Encodings: 0 raw,
/// 1 differenced, 2 run-length, 3 run-length then differenced. A run byte
/// with the high bit set repeats the next value `b & 0x7F` times; clear,
/// it is followed by `b & 0x7F` literal values.
fn read_plane(r: &mut Bytes, n: usize, mut out: Option<&mut Vec<u32>>) -> Result<(), DsfError> {
    let enc = r.u8()?;
    if let Some(v) = out.as_deref_mut() {
        v.clear();
        v.reserve_exact(n);
    }
    match enc {
        0 | 1 => {
            let raw = r.take(n.checked_mul(4).ok_or(DsfError::BadPool)?)?;
            if let Some(v) = out.as_deref_mut() {
                v.extend(raw.chunks_exact(4).map(le_u32));
            }
        }
        2 | 3 => {
            let mut have = 0;
            while have < n {
                let b = r.u8()?;
                let c = usize::from(b & 0x7F);
                if c > n - have {
                    return Err(DsfError::BadPool);
                }
                if b & 0x80 != 0 {
                    let x = r.u32()?;
                    if let Some(v) = out.as_deref_mut() {
                        v.extend(std::iter::repeat_n(x, c));
                    }
                } else {
                    let raw = r.take(4 * c)?;
                    if let Some(v) = out.as_deref_mut() {
                        v.extend(raw.chunks_exact(4).map(le_u32));
                    }
                }
                have += c;
            }
        }
        _ => return Err(DsfError::BadPool),
    }
    if let (1 | 3, Some(v)) = (enc, out) {
        let mut acc = 0u32;
        for x in v.iter_mut() {
            acc = acc.wrapping_add(*x);
            *x = acc;
        }
    }
    Ok(())
}

fn read_pool(po: &[u8], sc: &[u8]) -> Result<Pool, DsfError> {
    let mut r = Bytes::new(po);
    let n = r.u32()? as usize;
    let planes = usize::from(r.u8()?);
    // An empty pool (Global Scenery ends GEOD with one of 0 points and 0
    // planes) is valid. Run-length planes expand at most about 101 times,
    // so a count beyond 128 values per atom byte is corrupt and must not
    // size an allocation.
    if (planes < 2 && n > 0)
        || n.checked_mul(4)
            .is_none_or(|b| b > po.len().saturating_mul(128))
    {
        return Err(DsfError::BadPool);
    }
    if !sc.len().is_multiple_of(8) || sc.len() / 8 < planes {
        return Err(DsfError::BadPool);
    }
    let scale = |k: usize| match sc.get(k * 8..k * 8 + 8) {
        Some(s) if k < planes => (
            f32::from_le_bytes(s[..4].try_into().unwrap()),
            f32::from_le_bytes(s[4..].try_into().unwrap()),
        ),
        _ => (0.0, 0.0),
    };
    let (mut lon, mut lat, mut junction) = (vec![], vec![], vec![]);
    for k in 0..planes {
        let out = match k {
            0 => Some(&mut lon),
            1 => Some(&mut lat),
            3 => Some(&mut junction),
            _ => None,
        };
        read_plane(&mut r, n, out)?;
    }
    Ok(Pool {
        lon,
        lat,
        junction: (planes > 3).then_some(junction),
        lon_scale: scale(0),
        lat_scale: scale(1),
        junction_scale: scale(3),
    })
}

pub fn read_dsf(bytes: &[u8], keep: impl Fn(u8) -> bool) -> Result<DsfRoads, DsfError> {
    if bytes.len() < 12 + FOOTER || &bytes[..8] != COOKIE || le_u32(&bytes[8..12]) != 1 {
        return Err(DsfError::NotDsf);
    }
    let top = atoms(&bytes[12..bytes.len() - FOOTER])?;
    let child = |id: u32| top.iter().find(|(a, _)| *a == id).map(|(_, b)| *b);
    let grandchild = |parent: u32, id: u32| -> Result<Option<&[u8]>, DsfError> {
        let Some(body) = child(parent) else {
            return Ok(None);
        };
        Ok(atoms(body)?
            .into_iter()
            .find(|(a, _)| *a == id)
            .map(|(_, b)| b))
    };

    let props = grandchild(HEAD, PROP)?.map(strings).unwrap_or_default();
    let props: Vec<(&str, &str)> = props
        .chunks_exact(2)
        .map(|p| (p[0].as_str(), p[1].as_str()))
        .collect();
    let prop = |k: &str| props.iter().find(|p| p.0 == k).map(|p| p.1);
    let corner = |k: &str| -> Result<f64, DsfError> {
        prop(k)
            .and_then(|v| v.parse().ok())
            .ok_or(DsfError::BadAtom)
    };
    let (west, south) = (corner("sim/west")?, corner("sim/south")?);
    let exclusions = props
        .iter()
        .filter(|p| p.0 == "sim/exclude_net")
        .filter_map(|p| Exclusion::parse(p.1))
        .collect();
    let net_def = grandchild(DEFN, NETW)?
        .and_then(|b| strings(b).into_iter().next())
        .filter(|s| !s.is_empty());
    let mut roads = DsfRoads {
        tile: (south.round() as i32, west.round() as i32),
        overlay: prop("sim/overlay") == Some("1"),
        net_def,
        exclusions,
        chains: Chains::new(),
    };
    if roads.net_def.is_none() {
        return Ok(roads); // no network: skip pools and commands
    }

    let geod = atoms(child(GEOD).ok_or(DsfError::BadAtom)?)?;
    let of = |id: u32| geod.iter().filter(move |(a, _)| *a == id).map(|(_, b)| *b);
    if of(PO32).count() != of(SC32).count() {
        return Err(DsfError::BadPool);
    }
    let pools = of(PO32)
        .zip(of(SC32))
        .map(|(p, s)| read_pool(p, s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seg = Segmenter {
        pools: &pools,
        origin: (west, south),
        keep,
        out: Chains::new(),
        cur: vec![],
        sub: 0,
        open: false,
        keeping: false,
    };
    walk_commands(child(CMDS).ok_or(DsfError::BadAtom)?, &mut seg)?;
    roads.chains = seg.out;
    // Grown by doubling; trimmed so `Chains::bytes` is what the cache holds.
    roads.chains.lon.shrink_to_fit();
    roads.chains.lat.shrink_to_fit();
    roads.chains.start.shrink_to_fit();
    roads.chains.subtype.shrink_to_fit();
    Ok(roads)
}

/// Turns network chains into segments by DSFLib's rule: a junction ends
/// the open segment (unless it is the chain's first point) and begins a
/// new one (unless it is the last); any other point is a shape point. The
/// segment buffer is reused, and points of segments `keep` rejects are
/// never computed.
struct Segmenter<'a, K> {
    pools: &'a [Pool],
    origin: (f64, f64),
    keep: K,
    out: Chains,
    cur: Vec<(f32, f32)>,
    sub: u8,
    open: bool,
    keeping: bool,
}

impl<K: Fn(u8) -> bool> Segmenter<'_, K> {
    fn chain(
        &mut self,
        pool: usize,
        subtype: u8,
        idx: impl ExactSizeIterator<Item = u32>,
    ) -> Result<(), DsfError> {
        let p = self.pools.get(pool).ok_or(DsfError::BadPool)?;
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

fn walk_commands<K: Fn(u8) -> bool>(
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
pub(crate) mod test_dsf {
    //! Builds minimal DSFs: HEAD/PROP, DEFN/NETW, GEOD/PO32+SC32 (raw
    //! encoding unless asked), CMDS. The MD5 footer is 16 zero bytes (the
    //! reader does not check it, as X-Plane's loader is the authority).
    pub struct Builder {
        pub props: Vec<(String, String)>,
        pub nets: Vec<String>,
        /// Pools: planes of raw u32 values, their (scale, offset) and the
        /// plane encoding.
        pub pools: Vec<(Vec<Vec<u32>>, Vec<(f32, f32)>, u8)>,
        pub cmds: Vec<u8>,
    }

    fn atom(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::from_be_bytes(*id).to_le_bytes().to_vec();
        out.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn strings(s: &[String]) -> Vec<u8> {
        s.iter().flat_map(|x| x.bytes().chain([0])).collect()
    }

    fn encode_plane(v: &[u32], enc: u8) -> Vec<u8> {
        let mut vals = v.to_vec();
        if enc == 1 || enc == 3 {
            let mut prev = 0u32;
            for x in vals.iter_mut() {
                let d = x.wrapping_sub(prev);
                prev = *x;
                *x = d;
            }
        }
        let mut out = vec![enc];
        if enc == 0 || enc == 1 {
            for x in vals {
                out.extend_from_slice(&x.to_le_bytes());
            }
        } else {
            // Literal runs only keep the builder simple; repeat runs are
            // tested on the plane reader directly.
            for chunk in vals.chunks(127) {
                out.push(chunk.len() as u8);
                for x in chunk {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        out
    }

    impl Builder {
        pub fn new(lat: i32, lon: i32) -> Self {
            Self {
                props: vec![
                    ("sim/west".into(), lon.to_string()),
                    ("sim/south".into(), lat.to_string()),
                    ("sim/east".into(), (lon + 1).to_string()),
                    ("sim/north".into(), (lat + 1).to_string()),
                ],
                nets: vec![],
                pools: vec![],
                cmds: vec![],
            }
        }

        /// A pool of points (lon, lat, junction) as 4 planes with exact
        /// degree scaling over the tile: scale 1, offset = corner.
        pub fn pool(&mut self, lat: i32, lon: i32, pts: &[(f64, f64, u32)], enc: u8) -> u16 {
            let q = |v: f64, o: i32| (((v - f64::from(o)) * 4294967295.0).round()) as u32;
            let planes = vec![
                pts.iter().map(|p| q(p.0, lon)).collect(),
                pts.iter().map(|p| q(p.1, lat)).collect(),
                pts.iter().map(|_| 0u32).collect(),
                pts.iter().map(|p| p.2).collect(),
            ];
            let scales = vec![(1.0, lon as f32), (1.0, lat as f32), (0.0, 0.0), (0.0, 0.0)];
            self.pools.push((planes, scales, enc));
            (self.pools.len() - 1) as u16
        }

        /// One road chain of `subtype` through absolute `(lon, lat)` points,
        /// junctions at both ends, in its own raw-encoded pool. Sets the
        /// network definition. Used by Tasks 9 and 10.
        pub fn road(&mut self, lat: i32, lon: i32, subtype: u8, pts: &[(f64, f64)]) -> &mut Self {
            let n = pts.len();
            let with_j: Vec<(f64, f64, u32)> = pts
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let j = if i == 0 || i + 1 == n {
                        (i + 1) as u32
                    } else {
                        0
                    };
                    (p.0, p.1, j)
                })
                .collect();
            let pool = self.pool(lat, lon, &with_j, 0);
            self.nets = vec!["lib/g10/roads.net".into()];
            self.cmds.push(1);
            self.cmds.extend(pool.to_le_bytes());
            self.cmds.push(6);
            self.cmds.push(subtype);
            self.cmds.push(9);
            self.cmds.push(n as u8);
            for i in 0..n as u16 {
                self.cmds.extend(i.to_le_bytes());
            }
            self
        }

        /// The 10 degree block folder of a tile: `+30-130` for (37, -123).
        pub fn block(lat: i32, lon: i32) -> String {
            format!(
                "{:+03}{:+04}",
                lat.div_euclid(10) * 10,
                lon.div_euclid(10) * 10
            )
        }

        pub fn bytes(&self) -> Vec<u8> {
            let mut out = b"XPLNEDSF".to_vec();
            out.extend_from_slice(&1u32.to_le_bytes());
            let props: Vec<String> = self
                .props
                .iter()
                .flat_map(|(k, v)| [k.clone(), v.clone()])
                .collect();
            out.extend(atom(b"HEAD", &atom(b"PROP", &strings(&props))));
            out.extend(atom(b"DEFN", &atom(b"NETW", &strings(&self.nets))));
            let mut geod = vec![];
            for (planes, scales, enc) in &self.pools {
                let count = planes.first().map_or(0, Vec::len) as u32;
                let mut po = count.to_le_bytes().to_vec();
                po.push(planes.len() as u8);
                for p in planes {
                    po.extend(encode_plane(p, *enc));
                }
                geod.extend(atom(b"PO32", &po));
                let sc: Vec<u8> = scales
                    .iter()
                    .flat_map(|(s, o)| s.to_le_bytes().into_iter().chain(o.to_le_bytes()))
                    .collect();
                geod.extend(atom(b"SC32", &sc));
            }
            out.extend(atom(b"GEOD", &geod));
            out.extend(atom(b"CMDS", &self.cmds));
            out.extend([0u8; 16]);
            out
        }
    }

    /// Writes an X-Plane install under `root`: `Custom Scenery/scenery_packs.ini`
    /// listing `packs` top to bottom (highest priority first), and each
    /// pack's DSF at `Earth nav data/<block>/<tile>.dsf`. `None` bytes
    /// writes `b"garbage"` (an unreadable DSF). Used by Tasks 9 and 10.
    pub fn install(root: &std::path::Path, tile: (i32, i32), packs: &[(&str, Option<Vec<u8>>)]) {
        let cs = root.join("Custom Scenery");
        std::fs::create_dir_all(&cs).unwrap();
        let mut ini = String::from("I\n1000 Version\nSCENERY\n\n");
        for (name, bytes) in packs {
            ini.push_str(&format!("SCENERY_PACK Custom Scenery/{name}/\n"));
            let dir = cs
                .join(name)
                .join("Earth nav data")
                .join(Builder::block(tile.0, tile.1));
            std::fs::create_dir_all(&dir).unwrap();
            let file = format!("{:+03}{:+04}.dsf", tile.0, tile.1);
            let body = bytes.clone().unwrap_or_else(|| b"garbage".to_vec());
            std::fs::write(dir.join(file), body).unwrap();
        }
        std::fs::write(cs.join("scenery_packs.ini"), ini).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_dsf::{self, Builder};
    use super::*;

    fn chain_cmd(b: &mut Builder, pool: u16, def: u8, subtype: u8, idx: &[u16]) {
        b.cmds.push(1); // PoolSelect
        b.cmds.extend(pool.to_le_bytes());
        b.cmds.push(3); // SetDefinition8
        b.cmds.push(def);
        b.cmds.push(6); // SetRoadSubtype8
        b.cmds.push(subtype);
        b.cmds.push(9); // NetworkChain
        b.cmds.push(idx.len() as u8);
        for i in idx {
            b.cmds.extend(i.to_le_bytes());
        }
    }

    fn two_segment_dsf(enc: u8) -> Builder {
        let mut b = Builder::new(46, 6);
        b.props.push(("sim/overlay".into(), "1".into()));
        b.props.push(("sim/exclude_net".into(), "6/46/7/47".into()));
        b.nets.push("lib/g10/roads_EU.net".into());
        // Junction, shape, junction (mid), shape, junction.
        let pool = b.pool(
            46,
            6,
            &[
                (6.10, 46.10, 1),
                (6.20, 46.20, 0),
                (6.30, 46.30, 2),
                (6.40, 46.40, 0),
                (6.50, 46.50, 3),
            ],
            enc,
        );
        chain_cmd(&mut b, pool, 0, 50, &[0, 1, 2, 3, 4]);
        b
    }

    #[test]
    fn builder_block_folders() {
        assert_eq!(Builder::block(37, -123), "+30-130");
        assert_eq!(Builder::block(46, 6), "+40+000");
    }

    #[test]
    fn builder_road_reads_back_as_one_segment() {
        let mut b = Builder::new(46, 6);
        b.road(46, 6, 30, &[(6.1, 46.1), (6.2, 46.15), (6.3, 46.2)]);
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.net_def.as_deref(), Some("lib/g10/roads.net"));
        assert_eq!(r.chains.subtype, vec![30]);
        assert_eq!(r.chains.points(0), 0..3);
    }

    #[test]
    fn builder_install_lays_out_packs_in_priority_order() {
        let dir = tempfile::tempdir().unwrap();
        let dsf = two_segment_dsf(0).bytes();
        test_dsf::install(
            dir.path(),
            (46, 6),
            &[("A", Some(dsf.clone())), ("B", None)],
        );
        let cs = dir.path().join("Custom Scenery");
        let ini = std::fs::read_to_string(cs.join("scenery_packs.ini")).unwrap();
        let a = ini.find("Custom Scenery/A/").unwrap();
        assert!(a < ini.find("Custom Scenery/B/").unwrap());
        let file = |p: &str| cs.join(p).join("Earth nav data/+40+000/+46+006.dsf");
        assert_eq!(std::fs::read(file("A")).unwrap(), dsf);
        assert_eq!(std::fs::read(file("B")).unwrap(), b"garbage");
    }

    #[test]
    fn reads_properties_net_and_exclusions() {
        let r = read_dsf(&two_segment_dsf(0).bytes(), |_| true).unwrap();
        assert_eq!(r.tile, (46, 6));
        assert!(r.overlay);
        assert_eq!(r.net_def.as_deref(), Some("lib/g10/roads_EU.net"));
        assert_eq!(r.exclusions.len(), 1);
    }

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
    fn an_empty_pool_is_accepted() {
        // Global Scenery ends GEOD with a PO32 of 0 points and 0 planes.
        let mut b = two_segment_dsf(2);
        b.pools.push((vec![], vec![], 0));
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
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
    fn a_dsf_without_networks_reads_quickly_to_nothing() {
        let mut b = Builder::new(46, 6);
        b.props.push(("sim/exclude_obj".into(), "6/46/7/47".into()));
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert!(r.net_def.is_none());
        assert!(r.chains.is_empty());
        assert!(r.exclusions.is_empty(), "only exclude_net is kept");
    }

    #[test]
    fn an_empty_network_name_is_no_network() {
        let mut b = two_segment_dsf(0);
        b.nets = vec![String::new()];
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert!(r.net_def.is_none());
        assert!(r.chains.is_empty());
    }

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
            let mut v = vec![];
            read_plane(&mut r, 4, Some(&mut v)).unwrap();
            assert_eq!(v, want, "encoding {enc}");
            assert_eq!(r.u8().unwrap(), 0xEE);
            let mut r = Bytes { b: &plane, p: 0 };
            read_plane(&mut r, 4, None).unwrap();
            assert_eq!(r.u8().unwrap(), 0xEE, "skipped encoding {enc}");
        }
    }

    #[test]
    fn a_run_past_the_plane_end_is_a_bad_pool() {
        let mut plane = vec![2u8, 0x85];
        plane.extend(7u32.to_le_bytes());
        let mut r = Bytes { b: &plane, p: 0 };
        assert_eq!(read_plane(&mut r, 4, None), Err(DsfError::BadPool));
    }

    #[test]
    fn chains_push_from_default() {
        let mut c = Chains::default();
        c.push(50, &[(0.1, 0.2), (0.3, 0.4)]);
        c.push(60, &[(0.5, 0.6), (0.7, 0.8), (0.9, 1.0)]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.points(1), 2..5);
        assert_eq!(c.lat[c.points(1).start], 0.6);
        assert_eq!(c.bytes(), 5 * 8 + 3 * 4 + 2);
    }

    #[test]
    fn unwrap_7z_returns_the_single_entry_and_passes_raw_through() {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
        use std::io::Write;
        let raw = two_segment_dsf(0).bytes();
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = ArchiveWriter::new(&mut buf).unwrap();
            w.push_archive_entry(
                ArchiveEntry::new_file("+46+006.dsf"),
                Some(std::io::Cursor::new(raw.clone())),
            )
            .unwrap();
            w.finish().unwrap().flush().unwrap();
        }
        assert_eq!(unwrap_7z(buf.into_inner()).unwrap(), raw);
        assert_eq!(unwrap_7z(raw.clone()).unwrap(), raw);
    }

    /// `cargo test --release -p xearthlayer --lib night::roads::dsf::tests::golden -- --ignored --nocapture`
    fn golden(path: &str) {
        let started = std::time::Instant::now();
        let file = std::fs::read(path).unwrap();
        let file_bytes = file.len();
        let bytes = unwrap_7z(file).unwrap();
        let unwrapped = started.elapsed();
        let ours = read_dsf(&bytes, |_| true).unwrap();
        println!(
            "{path}: parse {} ms ({} ms read and unwrap), {} segments",
            started.elapsed().as_millis(),
            unwrapped.as_millis(),
            ours.chains.len()
        );
        let t = std::time::Instant::now();
        let lit = read_dsf(&bytes, super::super::is_lit_subtype).unwrap();
        println!(
            "  file {file_bytes} B, DSF {} B; keep all: {} points, Chains {} B; \
             keep lit: {} ms, {} segments, {} points, Chains {} B",
            bytes.len(),
            ours.chains.lon.len(),
            ours.chains.bytes(),
            t.elapsed().as_millis(),
            lit.chains.len(),
            lit.chains.lon.len(),
            lit.chains.bytes()
        );
        let out = std::env::temp_dir().join(format!("golden-{}.txt", std::process::id()));
        let status = std::process::Command::new("DSFTool")
            .arg("--dsf2text")
            .arg(path)
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        let text = std::fs::read_to_string(&out).unwrap();
        let mut theirs: std::collections::BTreeMap<u8, usize> = Default::default();
        let mut first: Vec<(f64, f64)> = vec![];
        for l in text.lines().filter(|l| l.starts_with("BEGIN_SEGMENT ")) {
            let f: Vec<&str> = l.split_whitespace().collect();
            *theirs.entry(f[2].parse().unwrap()).or_default() += 1;
            first.push((f[4].parse().unwrap(), f[5].parse().unwrap()));
        }
        let mut mine: std::collections::BTreeMap<u8, usize> = Default::default();
        for s in &ours.chains.subtype {
            *mine.entry(*s).or_default() += 1;
        }
        assert_eq!(mine, theirs);
        let (lat0, lon0) = ours.tile;
        for (i, (lon, lat)) in first.iter().enumerate().step_by(997) {
            let p = ours.chains.points(i).start;
            assert!(
                (f64::from(ours.chains.lon[p]) + f64::from(lon0) - lon).abs() < 1e-6,
                "segment {i}"
            );
            assert!(
                (f64::from(ours.chains.lat[p]) + f64::from(lat0) - lat).abs() < 1e-6,
                "segment {i}"
            );
        }
        let _ = std::fs::remove_file(out);
    }

    #[test]
    #[ignore]
    fn golden_global_scenery() {
        golden("/media/FlightSim/X-Plane 12-beta/Global Scenery/X-Plane 12 Global Scenery/Earth nav data/+40-120/+40-111.dsf");
    }

    #[test]
    #[ignore]
    fn golden_xworld_pro_roads() {
        golden("/media/FlightSim/X-Plane 12-beta/Custom Scenery/simHeaven_X-WORLD-Pro_Europe-13-net3-roads/Earth nav data/+40+000/+46+006.dsf");
    }
}
