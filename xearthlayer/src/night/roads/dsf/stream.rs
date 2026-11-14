//! Forward-only reading of a DSF from a file, a 7z entry or any reader:
//! atoms are walked by their headers, what roads need is kept, the rest is
//! read past, and reading stops as soon as the roads are known.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::pool::{Pool, MAX_POOL_POINTS};
use super::walk::{walk_commands, Segmenter, WALK_SLACK};
use super::{
    le_u32, Bytes, Chains, DsfError, DsfFileStats, DsfRoads, MAX_DSF_BYTES, MAX_RETAINED_ATOM_BYTES,
};
use crate::night::roads::exclusion::Exclusion;

/// Retained bodies are read in steps of this many bytes, so a declared
/// size never allocates far ahead of the bytes that arrive.
const RETAIN_STEP: u64 = 16 * 1024 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::roads::dsf::read_dsf;
    use crate::night::roads::dsf::test_dsf::{two_segment_dsf, Builder};

    #[test]
    fn reads_properties_net_and_exclusions() {
        let r = read_dsf(&two_segment_dsf(0).bytes(), |_| true).unwrap();
        assert_eq!(r.tile, (46, 6));
        assert!(r.overlay);
        assert_eq!(r.net_def.as_deref(), Some("lib/g10/roads_EU.net"));
        assert_eq!(r.exclusions.len(), 1);
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

    /// `cargo test --release -p xearthlayer --lib night::roads::dsf::stream::tests::golden -- --ignored --nocapture`
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
        let (lit, _) = read_dsf_file(
            std::path::Path::new(path),
            crate::night::roads::is_lit_subtype,
        )
        .unwrap();
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
            let (r, stats) = read_dsf_file(
                std::path::Path::new(&path),
                crate::night::roads::is_lit_subtype,
            )
            .unwrap();
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
