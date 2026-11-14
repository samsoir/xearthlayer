//! Streaming DSF reader for road networks. Reads the HEAD properties
//! (corner, overlay, `sim/exclude_net`), the NETW definition, the 32-bit
//! point pools and the command stream, keeping the network segments whose
//! subtype `keep` accepts. Every command is sized, so the others are
//! skipped byte-exactly.
//!
//! The reader is forward-only and keeps only what roads need: 16-bit
//! pools, rasters and any other atom are read past without buffering, a
//! DSF without a network stops after HEAD and DEFN, one with a network
//! stops once GEOD and CMDS are read, and a base mesh read for overlays
//! only ([`ReadMode::OverlaysOnly`]) stops after HEAD. [`read_dsf_file`] streams a raw file
//! or the single entry of a 7z, so decompression stops where reading does.
//!
//! Layout: `XPLNEDSF`, `u32 1`, atoms (`u32 id, u32 size` with the size
//! counting the 8 header bytes; ids are the letters packed big-endian and
//! stored little-endian), then a 16-byte MD5 footer. A PO32 atom is
//! `u32 count, u8 planes`, then per plane `u8 encoding` and its data; the
//! matching SC32 (the i-th SC32 pairs with the i-th PO32) holds
//! `(f32 scale, f32 offset)` per plane.

use super::exclusion::Exclusion;

mod pool;
mod stream;
mod walk;

#[cfg(test)]
pub(crate) mod test_dsf;

pub use stream::{read_dsf_file, read_dsf_stream};

pub const MAX_DSF_BYTES: usize = 512 * 1024 * 1024;
/// Ceiling on any one atom body the reader keeps (HEAD, DEFN, a PO32, an
/// SC32, CMDS), checked before anything is allocated for it.
pub const MAX_RETAINED_ATOM_BYTES: u64 = 256 * 1024 * 1024;

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

/// Which DSFs are read in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// A DSF that is not an overlay (a base mesh) stops right after HEAD:
    /// its `DsfRoads` has `overlay == false`, no network and no chains.
    OverlaysOnly,
    /// Every DSF is read until its roads are known.
    Any,
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
    /// Points in the DSF's PO32 pools.
    pub pool_points: u64,
    /// Chain indices the network walk visited.
    pub walk_visits: u64,
}

/// [`read_dsf_stream`] over a DSF already in memory, read in full.
pub fn read_dsf(bytes: &[u8], keep: impl Fn(u8) -> bool) -> Result<DsfRoads, DsfError> {
    read_dsf_stream(&mut &bytes[..], bytes.len() as u64, keep, ReadMode::Any)
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

#[cfg(test)]
mod tests {
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
    fn default_chains_equal_new() {
        assert_eq!(Chains::default(), Chains::new());
        assert_eq!(Chains::default().start, vec![0]);
    }
}
