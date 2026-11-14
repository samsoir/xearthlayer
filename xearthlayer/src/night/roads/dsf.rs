//! Streaming DSF reader for road networks. Reads the HEAD properties
//! (corner, overlay, `sim/exclude_net`), the NETW definition, the 32-bit
//! point pools and the command stream, keeping the network segments whose
//! subtype `keep` accepts. Every command is sized, so the others are
//! skipped byte-exactly.
//!
//! The reader is forward-only and keeps only what roads need: 16-bit
//! pools, rasters and any other atom are read past without buffering, a
//! DSF without a network stops after HEAD and DEFN, and one with a network
//! stops once GEOD and CMDS are read. [`read_dsf_file`] streams a raw file
//! or the single entry of a 7z, so decompression stops where reading does.
//!
//! Layout: `XPLNEDSF`, `u32 1`, atoms (`u32 id, u32 size` with the size
//! counting the 8 header bytes; ids are the letters packed big-endian and
//! stored little-endian), then a 16-byte MD5 footer. A PO32 atom is
//! `u32 count, u8 planes`, then per plane `u8 encoding` and its data; the
//! matching SC32 (the i-th SC32 pairs with the i-th PO32) holds
//! `(f32 scale, f32 offset)` per plane.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::exclusion::Exclusion;

pub const MAX_DSF_BYTES: usize = 512 * 1024 * 1024;
/// Ceiling on any one atom body the reader keeps (HEAD, DEFN, a PO32, an
/// SC32, CMDS), checked before anything is allocated for it.
pub const MAX_RETAINED_ATOM_BYTES: u64 = 256 * 1024 * 1024;
/// Ceiling on the points of all PO32 pools of one DSF: 24 times the 2.7
/// million of X-World Pro's `+46+006`. It also keeps the walk budget, and
/// so every point index in [`Chains`], below 2^32.
const MAX_POOL_POINTS: usize = 1 << 26;
/// Chain indices a walk may visit beyond 4 per pool point. A real DSF
/// visits about one index per pool point.
const WALK_SLACK: u64 = 65_536;
/// Retained bodies are read in steps of this many bytes, so a declared
/// size never allocates far ahead of the bytes that arrive.
const RETAIN_STEP: u64 = 16 * 1024 * 1024;
/// Most values one byte of a run-length plane can expand to: a 5-byte
/// repeat run holds 127.
const RLE_VALUES_PER_BYTE: usize = 26;

const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const COOKIE: &[u8; 8] = b"XPLNEDSF";
const HEADER: u64 = 12;
const FOOTER: u64 = 16;

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
    #[error("I/O: {0}")]
    Io(String),
}

/// Road segments as flat point arrays. `lon`/`lat` are degrees east/north
/// of the tile's west/south corner; segment `i` owns points
/// `start[i]..start[i + 1]`, so `start` has one entry more than `subtype`.
#[derive(Debug, Clone, PartialEq)]
pub struct Chains {
    pub lon: Vec<f32>,
    pub lat: Vec<f32>,
    pub start: Vec<u32>,
    pub subtype: Vec<u8>,
}

impl Default for Chains {
    fn default() -> Self {
        Self::new()
    }
}

impl Chains {
    pub fn new() -> Self {
        Self {
            lon: vec![],
            lat: vec![],
            start: vec![0],
            subtype: vec![],
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
        for &(x, y) in pts {
            self.lon.push(x);
            self.lat.push(y);
        }
        self.start.push(self.lon.len() as u32);
        self.subtype.push(subtype);
    }

    /// Appends chain `i` of `src` without building an intermediate point list.
    pub fn push_from(&mut self, src: &Chains, i: usize) {
        let r = src.points(i);
        self.lon.extend_from_slice(&src.lon[r.clone()]);
        self.lat.extend_from_slice(&src.lat[r]);
        self.start.push(self.lon.len() as u32);
        self.subtype.push(src.subtype[i]);
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

/// What reading one DSF file cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsfFileStats {
    /// Size of the file on disk.
    pub file_bytes: u64,
    /// Whether it is a 7z container.
    pub compressed: bool,
    /// DSF bytes (decompressed for a 7z) the reader consumed.
    pub read_bytes: u64,
}

/// Reads the roads of the DSF at `path`, raw or the first file of a 7z,
/// without holding the file or the decompressed DSF in memory.
pub fn read_dsf_file(
    path: &Path,
    keep: impl Fn(u8) -> bool,
) -> Result<(DsfRoads, DsfFileStats), DsfError> {
    let mut file = File::open(path).map_err(io_error)?;
    let file_bytes = file.metadata().map_err(io_error)?.len();
    let mut magic = Vec::with_capacity(SEVENZ_MAGIC.len());
    (&mut file)
        .take(SEVENZ_MAGIC.len() as u64)
        .read_to_end(&mut magic)
        .map_err(io_error)?;
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let compressed = magic == SEVENZ_MAGIC;
    let (roads, read_bytes) = if compressed {
        read_7z(file, keep)?
    } else {
        let mut r = Counting::new(BufReader::new(file));
        (read_dsf_stream(&mut r, file_bytes, keep)?, r.bytes)
    };
    let stats = DsfFileStats {
        file_bytes,
        compressed,
        read_bytes,
    };
    Ok((roads, stats))
}

/// The first file entry of a 7z, streamed into [`read_dsf_stream`]. The
/// iteration stops after it, so an early exit ends decompression.
fn read_7z(file: File, keep: impl Fn(u8) -> bool) -> Result<(DsfRoads, u64), DsfError> {
    let archive = |e: sevenz_rust2::Error| DsfError::Archive(e.to_string());
    let mut reader =
        sevenz_rust2::ArchiveReader::new(file, sevenz_rust2::Password::empty()).map_err(archive)?;
    let mut out = None;
    reader
        .for_each_entries(|entry, r| {
            // The crate keeps iterating later blocks after a `false`, so
            // every later entry is refused here too.
            if out.is_some() {
                return Ok(false);
            }
            if entry.is_directory() {
                return Ok(true);
            }
            out = Some(if entry.size() > MAX_DSF_BYTES as u64 {
                Err(DsfError::TooLarge)
            } else {
                let mut r = Counting::new(r);
                read_dsf_stream(&mut r, entry.size(), &keep).map(|roads| (roads, r.bytes))
            });
            Ok(false)
        })
        .map_err(archive)?;
    out.unwrap_or_else(|| Err(DsfError::Archive("no file entry".into())))
}

/// Reads the roads of a DSF of `len` bytes from `reader`, forward only.
pub fn read_dsf_stream(
    reader: &mut dyn Read,
    len: u64,
    keep: impl Fn(u8) -> bool,
) -> Result<DsfRoads, DsfError> {
    if len > MAX_DSF_BYTES as u64 {
        return Err(DsfError::TooLarge);
    }
    if len < HEADER + FOOTER {
        return Err(DsfError::NotDsf);
    }
    let mut s = Stream { r: reader, pos: 0 };
    let mut header = [0u8; HEADER as usize];
    s.fill(&mut header)?;
    if &header[..8] != COOKIE || le_u32(&header[8..]) != 1 {
        return Err(DsfError::NotDsf);
    }
    let end = len - FOOTER;
    let mut head = None;
    let mut net_def: Option<Option<String>> = None;
    let mut geod = None;
    let mut cmds = None;
    while s.pos < end {
        let (id, size) = s.atom(end)?;
        match id {
            HEAD => head = Some(Head::parse(&s.retain(size)?)?),
            DEFN => net_def = Some(network_definition(&s.retain(size)?)?),
            GEOD => geod = Some(read_geod(&mut s, size)?),
            CMDS => cmds = Some(s.retain(size)?),
            _ => s.skip(size)?,
        }
        // Network chains index only PO32 points: without any, CMDS can
        // hold no segment and is not read (mesh DSFs declare a network
        // yet carry only empty PO32 pools).
        let no_network = matches!(net_def, Some(None));
        let no_points = geod
            .as_deref()
            .is_some_and(|g: &[Pool]| g.iter().all(|p| p.len() == 0));
        let complete = geod.is_some() && cmds.is_some();
        if head.is_some() && net_def.is_some() && (no_network || no_points || complete) {
            break;
        }
    }
    let head = head.ok_or(DsfError::BadAtom)?;
    let mut roads = DsfRoads {
        tile: (head.south.round() as i32, head.west.round() as i32),
        overlay: head.overlay,
        net_def: net_def.flatten(),
        exclusions: head.exclusions,
        chains: Chains::new(),
    };
    if roads.net_def.is_none() {
        return Ok(roads);
    }
    let pools = geod.ok_or(DsfError::BadAtom)?;
    let points: usize = pools.iter().map(Pool::len).sum();
    if points == 0 {
        return Ok(roads);
    }
    let cmds = cmds.ok_or(DsfError::BadAtom)?;
    let mut seg = Segmenter {
        pools: &pools,
        origin: (head.west, head.south),
        keep,
        visits_left: 4 * points as u64 + WALK_SLACK,
        out: Chains::new(),
        cur: vec![],
        sub: 0,
        open: false,
        keeping: false,
    };
    walk_commands(&cmds, &mut seg)?;
    roads.chains = seg.out;
    // Grown by doubling; trimmed so `Chains::bytes` is what the cache holds.
    roads.chains.lon.shrink_to_fit();
    roads.chains.lat.shrink_to_fit();
    roads.chains.start.shrink_to_fit();
    roads.chains.subtype.shrink_to_fit();
    Ok(roads)
}

/// [`read_dsf_stream`] over a DSF already in memory.
pub fn read_dsf(bytes: &[u8], keep: impl Fn(u8) -> bool) -> Result<DsfRoads, DsfError> {
    read_dsf_stream(&mut &bytes[..], bytes.len() as u64, keep)
}

fn io_error(e: io::Error) -> DsfError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        DsfError::Truncated
    } else {
        DsfError::Io(e.to_string())
    }
}

/// Counts the bytes read through it.
struct Counting<R> {
    inner: R,
    bytes: u64,
}

impl<R> Counting<R> {
    fn new(inner: R) -> Self {
        Self { inner, bytes: 0 }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
}

/// A forward-only atom reader that tracks its position in the DSF.
struct Stream<'r> {
    r: &'r mut dyn Read,
    pos: u64,
}

impl Stream<'_> {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), DsfError> {
        self.r.read_exact(buf).map_err(io_error)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    /// The next atom header as (id, body length), its body inside `end`.
    fn atom(&mut self, end: u64) -> Result<(u32, u64), DsfError> {
        if end - self.pos < 8 {
            return Err(DsfError::BadAtom);
        }
        let mut h = [0u8; 8];
        self.fill(&mut h)?;
        let size = u64::from(le_u32(&h[4..]));
        if size < 8 || size - 8 > end - self.pos {
            return Err(DsfError::BadAtom);
        }
        Ok((le_u32(&h[..4]), size - 8))
    }

    /// The next `n` bytes, kept. Checked against the cap before any
    /// allocation, then read in steps as the bytes arrive.
    fn retain(&mut self, n: u64) -> Result<Vec<u8>, DsfError> {
        if n > MAX_RETAINED_ATOM_BYTES {
            return Err(DsfError::TooLarge);
        }
        let mut v = Vec::new();
        while (v.len() as u64) < n {
            let step = (n - v.len() as u64).min(RETAIN_STEP);
            v.reserve_exact(step as usize);
            let got = (&mut *self.r)
                .take(step)
                .read_to_end(&mut v)
                .map_err(io_error)?;
            if (got as u64) < step {
                return Err(DsfError::Truncated);
            }
        }
        self.pos += n;
        Ok(v)
    }

    /// Reads past the next `n` bytes without keeping them.
    fn skip(&mut self, n: u64) -> Result<(), DsfError> {
        let got = io::copy(&mut (&mut *self.r).take(n), &mut io::sink()).map_err(io_error)?;
        if got < n {
            return Err(DsfError::Truncated);
        }
        self.pos += n;
        Ok(())
    }
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

    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }

    fn done(&self) -> bool {
        self.p >= self.b.len()
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}

/// Child atoms of an in-memory `body` as (id, body).
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

fn child(body: &[u8], id: u32) -> Result<Option<&[u8]>, DsfError> {
    Ok(atoms(body)?
        .into_iter()
        .find(|(a, _)| *a == id)
        .map(|(_, b)| b))
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

/// What roads need from HEAD.
struct Head {
    west: f64,
    south: f64,
    overlay: bool,
    exclusions: Vec<Exclusion>,
}

impl Head {
    fn parse(body: &[u8]) -> Result<Self, DsfError> {
        let props = child(body, PROP)?.map(strings).unwrap_or_default();
        let props: Vec<(&str, &str)> = props
            .chunks_exact(2)
            .map(|p| (p[0].as_str(), p[1].as_str()))
            .collect();
        let prop = |k: &str| props.iter().find(|p| p.0 == k).map(|p| p.1);
        let corner = |k: &str, limit: f64| -> Result<f64, DsfError> {
            prop(k)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| (-limit..limit).contains(v))
                .ok_or(DsfError::BadAtom)
        };
        Ok(Self {
            west: corner("sim/west", 180.0)?,
            south: corner("sim/south", 90.0)?,
            overlay: prop("sim/overlay") == Some("1"),
            exclusions: props
                .iter()
                .filter(|p| p.0 == "sim/exclude_net")
                .filter_map(|p| Exclusion::parse(p.1))
                .collect(),
        })
    }
}

/// The first NETW string of DEFN; none when absent or empty.
fn network_definition(body: &[u8]) -> Result<Option<String>, DsfError> {
    Ok(child(body, NETW)?
        .and_then(|b| strings(b).into_iter().next())
        .filter(|s| !s.is_empty()))
}

/// GEOD's PO32 pools, each paired with its SC32. Every other child (the
/// 16-bit POOL and SCAL of a mesh) is read past without buffering, and
/// each PO32 body is dropped once decoded.
fn read_geod(s: &mut Stream, size: u64) -> Result<Vec<Pool>, DsfError> {
    let end = s.pos + size;
    let mut pools: Vec<Pool> = vec![];
    let mut scales: Vec<Vec<u8>> = vec![];
    let mut points = 0;
    while s.pos < end {
        let (id, n) = s.atom(end)?;
        match id {
            PO32 => {
                let pool = Pool::decode(&s.retain(n)?, MAX_POOL_POINTS - points)?;
                points += pool.len();
                pools.push(pool);
            }
            SC32 => scales.push(s.retain(n)?),
            _ => s.skip(n)?,
        }
    }
    if pools.len() != scales.len() {
        return Err(DsfError::BadPool);
    }
    for (pool, sc) in pools.iter_mut().zip(&scales) {
        pool.set_scales(sc)?;
    }
    Ok(pools)
}

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
struct Pool {
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
    fn decode(po: &[u8], max_points: usize) -> Result<Self, DsfError> {
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
    fn set_scales(&mut self, sc: &[u8]) -> Result<(), DsfError> {
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

    fn len(&self) -> usize {
        self.lon.len()
    }

    fn is_junction(&self, i: usize) -> bool {
        self.junction.as_ref().is_some_and(|j| j.get(i))
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

/// Turns network chains into segments by DSFLib's rule: a junction ends
/// the open segment (unless it is the chain's first point) and begins a
/// new one (unless it is the last); any other point is a shape point. The
/// segment buffer is reused, points of segments `keep` rejects are never
/// computed, and the indices visited are budgeted from the pool sizes, so
/// a small pool walked many times cannot grow the output or the time
/// without bound.
struct Segmenter<'a, K> {
    pools: &'a [Pool],
    origin: (f64, f64),
    keep: K,
    visits_left: u64,
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
        /// GEOD children written before the PO32/SC32 pairs (16-bit POOL
        /// and SCAL in real meshes).
        pub geod_extra: Vec<([u8; 4], Vec<u8>)>,
        /// Top-level atoms written after CMDS (DEMS in real meshes).
        pub trailing: Vec<([u8; 4], Vec<u8>)>,
    }

    pub fn atom(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
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
                geod_extra: vec![],
                trailing: vec![],
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
            for (id, body) in &self.geod_extra {
                geod.extend(atom(id, body));
            }
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
            for (id, body) in &self.trailing {
                out.extend(atom(id, body));
            }
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

    #[test]
    fn push_from_copies_one_chain_like_push() {
        let mut src = Chains::new();
        src.push(10, &[(0.1, 0.2), (0.3, 0.4)]);
        src.push(50, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        let mut a = Chains::new();
        a.push_from(&src, 1);
        let mut b = Chains::new();
        b.push(50, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        assert_eq!(a, b);
        let mut empty = Chains::default();
        empty.push_from(&src, 0);
        assert_eq!(empty.start, vec![0, 2]);
    }

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
    fn chains_push_from_default() {
        let mut c = Chains::default();
        c.push(50, &[(0.1, 0.2), (0.3, 0.4)]);
        c.push(60, &[(0.5, 0.6), (0.7, 0.8), (0.9, 1.0)]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.points(1), 2..5);
        assert_eq!(c.lat[c.points(1).start], 0.6);
        assert_eq!(c.bytes(), 5 * 8 + 3 * 4 + 2);
    }

    /// Writes `dsf` to `dir/name`, raw or as a single-entry 7z.
    fn write_dsf(
        dir: &std::path::Path,
        name: &str,
        dsf: &[u8],
        sevenz: bool,
    ) -> std::path::PathBuf {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
        let path = dir.join(name);
        if !sevenz {
            std::fs::write(&path, dsf).unwrap();
            return path;
        }
        let mut w = ArchiveWriter::create(&path).unwrap();
        w.push_archive_entry(
            ArchiveEntry::new_file(name),
            Some(std::io::Cursor::new(dsf.to_vec())),
        )
        .unwrap();
        w.finish().unwrap();
        path
    }

    /// The top-level atoms of `dsf`, each with its header.
    fn top_atoms(dsf: &[u8]) -> Vec<Vec<u8>> {
        let mut out = vec![];
        let mut p = 12;
        while p < dsf.len() - 16 {
            let size = le_u32(&dsf[p + 4..p + 8]) as usize;
            out.push(dsf[p..p + size].to_vec());
            p += size;
        }
        out
    }

    fn assemble(atoms: &[&Vec<u8>]) -> Vec<u8> {
        let mut out = b"XPLNEDSF".to_vec();
        out.extend(1u32.to_le_bytes());
        for a in atoms {
            out.extend(a.iter());
        }
        out.extend([0u8; 16]);
        out
    }

    /// Junk that does not compress away, so a 7z of it stays large.
    fn junk(n: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9u32;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn raw_and_7z_files_read_like_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let dsf = two_segment_dsf(3).bytes();
        let want = read_dsf(&dsf, |_| true).unwrap();
        for sevenz in [false, true] {
            let path = write_dsf(dir.path(), "+46+006.dsf", &dsf, sevenz);
            let (got, stats) = read_dsf_file(&path, |_| true).unwrap();
            assert_eq!(got, want, "7z {sevenz}");
            assert_eq!(stats.compressed, sevenz);
            assert_eq!(stats.file_bytes, std::fs::metadata(&path).unwrap().len());
        }
    }

    #[test]
    fn a_dsf_without_networks_stops_reading_after_head_and_defn() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Builder::new(46, 6);
        b.trailing.push((*b"DEMS", junk(2 << 20)));
        let dsf = b.bytes();
        for sevenz in [false, true] {
            let path = write_dsf(dir.path(), "+46+006.dsf", &dsf, sevenz);
            let (r, stats) = read_dsf_file(&path, |_| true).unwrap();
            assert!(r.net_def.is_none());
            assert_eq!(r.tile, (46, 6));
            assert!(stats.file_bytes > 2 << 20, "7z {sevenz}");
            assert!(stats.read_bytes < 4096, "7z {sevenz}: {stats:?}");
        }
    }

    #[test]
    fn reading_stops_once_the_network_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = two_segment_dsf(0);
        b.trailing.push((*b"DEMS", junk(1 << 20)));
        let dsf = b.bytes();
        let path = write_dsf(dir.path(), "+46+006.dsf", &dsf, false);
        let (r, stats) = read_dsf_file(&path, |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
        assert!(stats.read_bytes < 4096, "{stats:?}");
    }

    #[test]
    fn a_network_without_32_bit_points_stops_before_cmds() {
        // Mesh DSFs declare a network yet carry only empty PO32 pools.
        let dir = tempfile::tempdir().unwrap();
        let mut b = Builder::new(79, 12);
        b.nets.push("lib/g10/roads_EU.net".into());
        b.pools.push((vec![], vec![], 0));
        b.cmds = vec![16; 1 << 20]; // a megabyte of TerrainPatch
        let path = write_dsf(dir.path(), "+79+012.dsf", &b.bytes(), false);
        let (r, stats) = read_dsf_file(&path, |_| true).unwrap();
        assert_eq!(r.net_def.as_deref(), Some("lib/g10/roads_EU.net"));
        assert!(r.chains.is_empty());
        assert!(stats.read_bytes < 4096, "{stats:?}");
    }

    #[test]
    fn cmds_before_geod_reads_alike() {
        let dsf = two_segment_dsf(1).bytes();
        let a = top_atoms(&dsf);
        assert_eq!(a.len(), 4); // HEAD, DEFN, GEOD, CMDS
        let swapped = assemble(&[&a[0], &a[1], &a[3], &a[2]]);
        assert_eq!(
            read_dsf(&swapped, |_| true).unwrap(),
            read_dsf(&dsf, |_| true).unwrap()
        );
    }

    #[test]
    fn an_absent_defn_is_no_network() {
        let dsf = two_segment_dsf(0).bytes();
        let a = top_atoms(&dsf);
        let r = read_dsf(&assemble(&[&a[0], &a[2], &a[3]]), |_| true).unwrap();
        assert!(r.net_def.is_none());
        assert!(r.chains.is_empty());
    }

    #[test]
    fn sixteen_bit_pools_in_geod_are_skipped() {
        let mut b = two_segment_dsf(0);
        b.geod_extra.push((*b"POOL", junk(1000)));
        b.geod_extra.push((*b"SCAL", junk(24)));
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r, read_dsf(&two_segment_dsf(0).bytes(), |_| true).unwrap());
    }

    #[test]
    fn a_geod_child_beyond_the_retained_cap_is_skipped_not_retained() {
        // A 300 MiB POOL: retaining it would be TooLarge, skipping streams it.
        let dsf = two_segment_dsf(0).bytes();
        let a = top_atoms(&dsf);
        let pool_len = 300u64 << 20;
        let geod_body = &a[2][8..];
        let geod_len = 8 + 8 + pool_len + geod_body.len() as u64;
        let mut head = b"XPLNEDSF".to_vec();
        head.extend(1u32.to_le_bytes());
        head.extend(&a[0]);
        head.extend(&a[1]);
        head.extend(&a[2][..4]);
        head.extend((geod_len as u32).to_le_bytes());
        head.extend(&b"POOL"[..].iter().rev().copied().collect::<Vec<u8>>());
        head.extend(((pool_len + 8) as u32).to_le_bytes());
        let mut tail = geod_body.to_vec();
        tail.extend(&a[3]);
        tail.extend([0u8; 16]);
        let len = head.len() as u64 + pool_len + tail.len() as u64;
        let mut stream = std::io::Read::chain(
            std::io::Read::chain(&head[..], std::io::Read::take(std::io::repeat(0), pool_len)),
            &tail[..],
        );
        let r = read_dsf_stream(&mut stream, len, |_| true).unwrap();
        assert_eq!(r.chains.len(), 2);
    }

    struct PanicsOnRead;

    impl std::io::Read for PanicsOnRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("read after the length was known to be too large");
        }
    }

    #[test]
    fn a_length_above_the_ceiling_is_rejected_unread() {
        let len = MAX_DSF_BYTES as u64 + 1;
        assert_eq!(
            read_dsf_stream(&mut PanicsOnRead, len, |_| true),
            Err(DsfError::TooLarge)
        );
    }

    #[test]
    fn a_retained_atom_above_the_cap_is_rejected_before_allocating() {
        let mut bytes = b"XPLNEDSF".to_vec();
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(u32::from_be_bytes(*b"HEAD").to_le_bytes());
        bytes.extend((MAX_RETAINED_ATOM_BYTES as u32 + 9).to_le_bytes());
        let len = MAX_DSF_BYTES as u64;
        assert_eq!(
            read_dsf_stream(&mut &bytes[..], len, |_| true),
            Err(DsfError::TooLarge)
        );
    }

    #[test]
    fn a_stream_that_ends_early_is_truncated() {
        let dsf = two_segment_dsf(0).bytes();
        for cut in [6, 12, 30, dsf.len() / 2, dsf.len() - 20] {
            assert_eq!(
                read_dsf_stream(&mut &dsf[..cut], dsf.len() as u64, |_| true),
                Err(DsfError::Truncated),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_dsf_file(&dir.path().join("none.dsf"), |_| true),
            Err(DsfError::Io(_))
        ));
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

    #[test]
    fn corners_must_be_finite_and_on_the_globe() {
        for (k, v) in [
            ("sim/west", "NaN"),
            ("sim/west", "inf"),
            ("sim/west", "180"),
            ("sim/west", "-181"),
            ("sim/south", "90"),
            ("sim/south", "-91"),
        ] {
            let mut b = two_segment_dsf(0);
            b.props.iter_mut().find(|p| p.0 == k).unwrap().1 = v.into();
            assert_eq!(
                read_dsf(&b.bytes(), |_| true),
                Err(DsfError::BadAtom),
                "{k}={v}"
            );
        }
    }

    #[test]
    fn default_chains_equal_new() {
        assert_eq!(Chains::default(), Chains::new());
        assert_eq!(Chains::default().start, vec![0]);
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

    /// `cargo test --release -p xearthlayer --lib night::roads::dsf::tests::golden -- --ignored --nocapture`
    fn golden(path: &str) {
        let started = std::time::Instant::now();
        let (ours, stats) = read_dsf_file(std::path::Path::new(path), |_| true).unwrap();
        println!(
            "{path}: read_dsf_file {} ms, {} segments, {} points, Chains {} B, {stats:?}",
            started.elapsed().as_millis(),
            ours.chains.len(),
            ours.chains.lon.len(),
            ours.chains.bytes()
        );
        let t = std::time::Instant::now();
        let (lit, _) =
            read_dsf_file(std::path::Path::new(path), super::super::is_lit_subtype).unwrap();
        println!(
            "  keep lit: {} ms, {} segments, Chains {} B",
            t.elapsed().as_millis(),
            lit.chains.len(),
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

    /// Real DSFs without road segments: an airport overlay with no network
    /// (raw) and two 7z meshes that declare a network but hold no 32-bit
    /// points. `read_bytes` against the file shows where reading stopped.
    #[test]
    #[ignore]
    fn timing_dsfs_without_roads() {
        let xp = "/media/FlightSim/X-Plane 12-beta";
        let mesh = "Custom Scenery/Svalbard4XPlane - MESH/Earth nav data/+70+010";
        for path in [
            format!("{xp}/Global Scenery/Global Airports/Earth nav data/+50-010/+51-001.dsf"),
            format!("{xp}/{mesh}/+79+012.dsf"),
            format!("{xp}/{mesh}/+78+016.dsf"),
        ] {
            let started = std::time::Instant::now();
            let (r, stats) =
                read_dsf_file(std::path::Path::new(&path), super::super::is_lit_subtype).unwrap();
            println!(
                "{path}: {} ms, net {:?}, {} segments, {stats:?}",
                started.elapsed().as_millis(),
                r.net_def,
                r.chains.len()
            );
            assert!(r.chains.is_empty());
            if r.net_def.is_none() {
                assert!(stats.read_bytes < stats.file_bytes / 10, "{stats:?}");
            }
        }
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
