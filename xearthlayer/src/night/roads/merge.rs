//! Merge one tile's DSF networks as X-Plane presents them: highest
//! priority first; a chain is dropped when any point lies in a zone of a
//! higher-priority DSF. Unlit subtypes are dropped too.
//!
//! The merged tile is what the road cache keeps in memory, so it is stored
//! compactly. Points are 16-bit offsets in the tile (1/65535 degree, about
//! 1.7 m, far below a lit texel), chains are stored back to back with one
//! byte each (class and point count), and the tile is a 256 x 256 grid of
//! cells (1/256 degree). In a dense tile the chains are sorted by the cell
//! of their box's south-west corner, with each cell's first chain and
//! first point in a prefix index; a chain whose box spans more than
//! [`MAX_SPAN`] cells carries its box instead, in a short list scanned per
//! query. A sparse tile, where the index would cost more than a box per
//! chain, has no index and every chain carries its box. Consumers visit
//! the chains near a box through [`MergedRoads::for_each_chain_near`] and
//! see degrees through [`ChainRef`], never the encoding. A
//! one-bit-per-cell occupancy map (8 KB per tile) answers
//! [`MergedRoads::crosses`] in time proportional to the query's cells, not
//! to the tile's chains: the lit probe asks it for every footprint X-Plane
//! stats.

use std::ops::Range;

use super::class::RoadClass;
use super::dsf::DsfRoads;
use super::exclusion::Exclusion;

/// Steps per degree of a point offset.
const STEPS: f64 = 65535.0;
/// A cell is `1 << CELL_SHIFT` steps: 1/256 degree.
const CELL_SHIFT: u32 = 8;
/// Cells along each side of a tile.
const CELLS: usize = 256;
/// Occupancy words per row of cells.
const ROW_WORDS: usize = CELLS / 64;
/// One bit per cell: row `r` (from the south) is words `r * ROW_WORDS..`,
/// cell column `c` bit `c % 64` of word `c / 64` of its row.
type Occupancy = Box<[u64; CELLS * ROW_WORDS]>;
/// Heap bytes of an [`Occupancy`].
const OCCUPANCY_BYTES: usize = CELLS * ROW_WORDS * 8;

/// The most cells a chain's box may span (north minus south, east minus
/// west) and still be found from the cell of its south-west corner. On a
/// full install's densest tiles, 1 leaves 0.1% (Tokyo) to 1% (Geneva) of
/// chains to the boxed list.
const MAX_SPAN: u8 = 1;
/// A chain byte holds the point count less one in its low bits.
const COUNT_BITS: u32 = 5;
/// The most points a stored chain holds; a longer chain is stored as
/// pieces that share their joint point.
const MAX_CHAIN_POINTS: usize = 1 << COUNT_BITS;
/// Entries of each prefix array: one per cell, and the end.
const INDEX_LEN: usize = CELLS * CELLS + 1;
/// Heap bytes of a [`CellIndex`].
const INDEX_BYTES: usize = 2 * INDEX_LEN * 4;

const _: () = assert!(RoadClass::COUNT <= 1 << (8 - COUNT_BITS));

/// A chain's byte: its class above its point count less one.
fn chain_byte(class: RoadClass, points: usize) -> u8 {
    debug_assert!((1..=MAX_CHAIN_POINTS).contains(&points));
    ((class.index() as u8) << COUNT_BITS) | (points - 1) as u8
}

fn class_of(byte: u8) -> RoadClass {
    RoadClass::ALL[usize::from(byte >> COUNT_BITS)]
}

fn points_of(byte: u8) -> usize {
    usize::from(byte & ((1 << COUNT_BITS) - 1)) + 1
}

/// A tile-relative offset (degrees, clamped to the tile) in steps.
fn quantise(offset: f32) -> u16 {
    (f64::from(offset).clamp(0.0, 1.0) * STEPS).round() as u16
}

fn cell_of(step: u16) -> u8 {
    (step >> CELL_SHIFT) as u8
}

/// The bits of occupancy word `k` of a row (cell columns `64k..64k + 63`)
/// that lie in `west..=east`.
fn span_mask(k: usize, west: u8, east: u8) -> u64 {
    let lo = k * 64;
    let (w, e) = (usize::from(west).max(lo), usize::from(east).min(lo + 63));
    if w > e {
        return 0;
    }
    let width = e - w + 1;
    let ones = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    ones << (w - lo)
}

/// The occupancy words and masks covering cells `south..=north` by
/// `west..=east`, row by row.
fn spans(south: u8, west: u8, north: u8, east: u8) -> impl Iterator<Item = (usize, u64)> {
    let words = usize::from(west) / 64..=usize::from(east) / 64;
    (usize::from(south)..=usize::from(north)).flat_map(move |row| {
        words
            .clone()
            .map(move |k| (row * ROW_WORDS + k, span_mask(k, west, east)))
    })
}

/// A box in cells of a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellBox {
    south: u8,
    west: u8,
    north: u8,
    east: u8,
}

impl CellBox {
    fn of([south, west, north, east]: [u8; 4]) -> Self {
        Self {
            south,
            west,
            north,
            east,
        }
    }

    fn meets(&self, q: &CellBox) -> bool {
        self.north >= q.south && self.south <= q.north && self.east >= q.west && self.west <= q.east
    }

    /// The larger of its height and width, in cells.
    fn span(&self) -> u8 {
        (self.north - self.south).max(self.east - self.west)
    }

    /// The cell id `row * CELLS + column` of its south-west corner.
    fn corner(&self) -> usize {
        usize::from(self.south) * CELLS + usize::from(self.west)
    }
}

/// Where each cell's chains start, for chains sorted by cell: cell `k`
/// holds chains `chain[k]..chain[k + 1]`, whose points start at
/// `point[k]`. Entry `CELLS * CELLS` is the end of the indexed chains.
#[derive(Debug, Clone, PartialEq)]
struct CellIndex {
    chain: Box<[u32]>,
    point: Box<[u32]>,
}

/// One chain of a [`MergedRoads`], as a visit hands it over.
#[derive(Debug, Clone, Copy)]
pub struct ChainRef<'a> {
    origin: (f64, f64),
    class: RoadClass,
    lon: &'a [u16],
    lat: &'a [u16],
}

impl<'a> ChainRef<'a> {
    pub fn class(&self) -> RoadClass {
        self.class
    }

    /// The chain's points as absolute `(lon, lat)` degrees.
    pub fn points(&self) -> impl Iterator<Item = (f64, f64)> + 'a {
        let (lat0, lon0) = self.origin;
        self.lon
            .iter()
            .zip(self.lat)
            .map(move |(&x, &y)| (lon0 + f64::from(x) / STEPS, lat0 + f64::from(y) / STEPS))
    }

    /// The points' quantised offsets east of the tile's west edge.
    pub fn lon_steps(&self) -> &'a [u16] {
        self.lon
    }

    /// The points' quantised offsets north of the tile's south edge.
    pub fn lat_steps(&self) -> &'a [u16] {
        self.lat
    }

    /// The latitude in degrees of a quantised offset north, exactly as
    /// [`ChainRef::points`] decodes it.
    pub fn lat_of(&self, step: u16) -> f64 {
        self.origin.0 + f64::from(step) / STEPS
    }

    /// A latitude as offset steps north of the tile's south edge,
    /// unrounded and unclamped.
    pub fn step_of_lat(&self, lat: f64) -> f64 {
        (lat - self.origin.0) * STEPS
    }
}

/// One tile's lit road chains after the priority merge.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedRoads {
    pub tile: (i32, i32),
    /// Point offsets east of the tile's west edge, in steps.
    lon: Vec<u16>,
    /// Point offsets north of the tile's south edge, in steps.
    lat: Vec<u16>,
    /// Per chain, [`chain_byte`]: indexed chains first, by cell, then the
    /// boxed chains.
    chains: Vec<u8>,
    /// Present in a dense tile.
    index: Option<CellIndex>,
    /// The cell boxes `[south, west, north, east]` of the last
    /// `boxes.len()` chains.
    boxes: Vec<[u8; 4]>,
    /// The largest span of an indexed chain's box, at most [`MAX_SPAN`].
    span: u8,
    /// The cells of every chain's box.
    occupied: Occupancy,
    /// Chains dropped by a higher-priority exclusion zone.
    pub removed: usize,
}

impl MergedRoads {
    pub fn empty(tile: (i32, i32)) -> Self {
        Self {
            tile,
            lon: vec![],
            lat: vec![],
            chains: vec![],
            index: None,
            boxes: vec![],
            span: 0,
            occupied: Box::new([0; CELLS * ROW_WORDS]),
            removed: 0,
        }
    }

    /// Number of stored chains (a chain longer than
    /// [`MAX_CHAIN_POINTS`] counts once per piece).
    pub fn len(&self) -> usize {
        self.chains.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chains.is_empty()
    }

    /// Heap bytes held, and the struct, for the byte-bounded tile cache.
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.lon.capacity() * 2
            + self.lat.capacity() * 2
            + self.chains.capacity()
            + self.index.as_ref().map_or(0, |_| INDEX_BYTES)
            + self.boxes.capacity() * 4
            + OCCUPANCY_BYTES
    }

    /// An absolute-degree box as cells of this tile, or `None` when it
    /// misses the tile. Conservative: every point whose exact or decoded
    /// position is in the box is in a cell of the result.
    fn cells(&self, south: f64, west: f64, north: f64, east: f64) -> Option<CellBox> {
        let (lat0, lon0) = (f64::from(self.tile.0), f64::from(self.tile.1));
        // Half a step either way covers the rounding of a point's offset.
        let range = |lo: f64, hi: f64, origin: f64| {
            let (lo, hi) = ((lo - origin) * STEPS - 0.5, (hi - origin) * STEPS + 0.5);
            (hi >= 0.0 && lo <= STEPS)
                .then(|| (cell_of(lo.max(0.0) as u16), cell_of(hi.min(STEPS) as u16)))
        };
        let (south, north) = range(south, north, lat0)?;
        let (west, east) = range(west, east, lon0)?;
        Some(CellBox {
            south,
            west,
            north,
            east,
        })
    }

    /// Hands `f` every chain whose cell box meets the box (absolute
    /// degrees), and perhaps a few more near it: never one twice, never
    /// one with a point in the box missed. An indexed tile visits the
    /// cells from `span` south and west of the box to its north-east
    /// corner, row by row, then the boxed chains that meet it.
    pub fn for_each_chain_near(
        &self,
        south: f64,
        west: f64,
        north: f64,
        east: f64,
        mut f: impl FnMut(ChainRef<'_>),
    ) {
        let Some(q) = self.cells(south, west, north, east) else {
            return;
        };
        let (mut chain, mut point) = (0, 0);
        if let Some(ix) = &self.index {
            let (s, w) = (
                q.south.saturating_sub(self.span),
                q.west.saturating_sub(self.span),
            );
            for row in usize::from(s)..=usize::from(q.north) {
                let (a, b) = (
                    row * CELLS + usize::from(w),
                    row * CELLS + usize::from(q.east) + 1,
                );
                let (from, to) = (ix.chain[a] as usize, ix.chain[b] as usize);
                self.visit_run(from..to, ix.point[a] as usize, |_| true, &mut f);
            }
            (chain, point) = (
                ix.chain[INDEX_LEN - 1] as usize,
                ix.point[INDEX_LEN - 1] as usize,
            );
        }
        let boxed = chain..self.chains.len();
        self.visit_run(
            boxed,
            point,
            |c| CellBox::of(self.boxes[c - chain]).meets(&q),
            &mut f,
        );
    }

    /// Hands `f` the chains of `run` that `keep`, the first of whose points
    /// is point `point`.
    fn visit_run(
        &self,
        run: Range<usize>,
        mut point: usize,
        keep: impl Fn(usize) -> bool,
        f: &mut impl FnMut(ChainRef<'_>),
    ) {
        let origin = (f64::from(self.tile.0), f64::from(self.tile.1));
        for c in run {
            let byte = self.chains[c];
            let end = point + points_of(byte);
            if keep(c) {
                f(ChainRef {
                    origin,
                    class: class_of(byte),
                    lon: &self.lon[point..end],
                    lat: &self.lat[point..end],
                });
            }
            point = end;
        }
    }

    /// Whether any chain's box meets the box (absolute degrees). Two cell
    /// boxes meet exactly when they share a cell, so the occupancy map
    /// gives the answer a scan of every chain's box would.
    pub fn crosses(&self, south: f64, west: f64, north: f64, east: f64) -> bool {
        self.cells(south, west, north, east).is_some_and(|q| {
            spans(q.south, q.west, q.north, q.east).any(|(i, mask)| self.occupied[i] & mask != 0)
        })
    }
}

/// Fed the DSFs of one tile from highest priority to lowest. Chains are
/// kept in arrival order with their boxes until [`Merger::finish`] lays
/// the tile out.
pub struct Merger {
    tile: (i32, i32),
    zones: Vec<Exclusion>,
    lon: Vec<u16>,
    lat: Vec<u16>,
    chains: Vec<u8>,
    boxes: Vec<[u8; 4]>,
    occupied: Occupancy,
    removed: usize,
}

impl Merger {
    pub fn new(tile: (i32, i32)) -> Self {
        Self {
            tile,
            zones: vec![],
            lon: vec![],
            lat: vec![],
            chains: vec![],
            boxes: vec![],
            occupied: Box::new([0; CELLS * ROW_WORDS]),
            removed: 0,
        }
    }

    /// Merges `layer` under every layer added before it, then adds its own
    /// zones for the layers below. Quantises straight from the layer's
    /// arrays into the output: no allocation per chain. The exclusion test
    /// uses the layer's exact points.
    pub fn add(&mut self, layer: &DsfRoads) {
        let (lat0, lon0) = (f64::from(self.tile.0), f64::from(self.tile.1));
        let c = &layer.chains;
        for i in 0..c.len() {
            let Some(class) = RoadClass::of(c.subtype[i]) else {
                continue;
            };
            // A chain of fewer than two points has no segment to draw, and
            // a stored chain needs at least one point: this also keeps a
            // corrupt extraction from underflowing the chain byte.
            let pts = c.points(i);
            if pts.len() < 2 {
                continue;
            }
            let (lon, lat) = (&c.lon[pts.clone()], &c.lat[pts]);
            if !self.zones.is_empty()
                && lon.iter().zip(lat).any(|(&x, &y)| {
                    let (x, y) = (f64::from(x) + lon0, f64::from(y) + lat0);
                    self.zones.iter().any(|z| z.contains(x, y))
                })
            {
                self.removed += 1;
                continue;
            }
            // Pieces of at most MAX_CHAIN_POINTS, each from the last point
            // of the one before.
            let mut from = 0;
            loop {
                let to = (from + MAX_CHAIN_POINTS).min(lon.len());
                self.push(class, &lon[from..to], &lat[from..to]);
                if to == lon.len() {
                    break;
                }
                from = to - 1;
            }
        }
        self.zones.extend(layer.exclusions.iter().cloned());
    }

    /// Stores one chain of at most [`MAX_CHAIN_POINTS`] points.
    fn push(&mut self, class: RoadClass, lon: &[f32], lat: &[f32]) {
        // Min and max step of `[south, west, north, east]`.
        let mut b = [u16::MAX, u16::MAX, 0, 0];
        for (&x, &y) in lon.iter().zip(lat) {
            let (qx, qy) = (quantise(x), quantise(y));
            self.lon.push(qx);
            self.lat.push(qy);
            b = [b[0].min(qy), b[1].min(qx), b[2].max(qy), b[3].max(qx)];
        }
        let [s, w, n, e] = b.map(cell_of);
        for (i, mask) in spans(s, w, n, e) {
            self.occupied[i] |= mask;
        }
        self.chains.push(chain_byte(class, lon.len()));
        self.boxes.push([s, w, n, e]);
    }

    /// Whether a zone covers the whole tile, so no lower DSF can add roads.
    pub fn covered(&self) -> bool {
        self.zones
            .iter()
            .any(|z| z.covers_tile(self.tile.0, self.tile.1))
    }

    /// The merged tile, indexed when the index costs less than a box for
    /// every chain it would cover.
    pub fn finish(self) -> MergedRoads {
        let short = self
            .boxes
            .iter()
            .filter(|&&b| CellBox::of(b).span() <= MAX_SPAN)
            .count();
        self.finish_with(short * 4 > INDEX_BYTES)
    }

    /// The merged tile, with or without the cell index. With it, the
    /// chains are reordered by cell (a counting sort) into exactly sized
    /// arrays; the arrival-order arrays (with their growth slack) are freed
    /// only once the copy is done, so the transient peak is two to three
    /// times the tile while the exact copies are allocated.
    pub(super) fn finish_with(self, index: bool) -> MergedRoads {
        let mut out = MergedRoads {
            tile: self.tile,
            lon: vec![],
            lat: vec![],
            chains: vec![],
            index: None,
            boxes: vec![],
            span: 0,
            occupied: self.occupied,
            removed: self.removed,
        };
        if !index {
            (out.lon, out.lat, out.chains, out.boxes) =
                (self.lon, self.lat, self.chains, self.boxes);
            out.lon.shrink_to_fit();
            out.lat.shrink_to_fit();
            out.chains.shrink_to_fit();
            out.boxes.shrink_to_fit();
            return out;
        }
        // Bucket per chain: its corner cell, or the last for a boxed chain.
        const BOXED: usize = INDEX_LEN - 1;
        let bucket = |b: [u8; 4]| {
            let b = CellBox::of(b);
            if b.span() <= MAX_SPAN {
                b.corner()
            } else {
                BOXED
            }
        };
        // Chains and points per bucket, then each bucket's first of each.
        let mut chain_at = vec![0u32; INDEX_LEN + 1];
        let mut point_at = vec![0u32; INDEX_LEN + 1];
        for (&byte, &b) in self.chains.iter().zip(&self.boxes) {
            let k = bucket(b);
            chain_at[k + 1] += 1;
            point_at[k + 1] += points_of(byte) as u32;
            if k != BOXED {
                out.span = out.span.max(CellBox::of(b).span());
            }
        }
        for k in 1..=INDEX_LEN {
            chain_at[k] += chain_at[k - 1];
            point_at[k] += point_at[k - 1];
        }
        let boxed = (chain_at[INDEX_LEN] - chain_at[BOXED]) as usize;
        out.lon = vec![0; self.lon.len()];
        out.lat = vec![0; self.lat.len()];
        out.chains = vec![0; self.chains.len()];
        out.boxes = Vec::with_capacity(boxed);
        // Scatter in arrival order through per-bucket cursors.
        let (mut next_chain, mut next_point) = (chain_at.clone(), point_at.clone());
        let mut src = 0;
        for (&byte, &b) in self.chains.iter().zip(&self.boxes) {
            let (k, n) = (bucket(b), points_of(byte));
            let (c, p) = (next_chain[k] as usize, next_point[k] as usize);
            out.chains[c] = byte;
            out.lon[p..p + n].copy_from_slice(&self.lon[src..src + n]);
            out.lat[p..p + n].copy_from_slice(&self.lat[src..src + n]);
            if k == BOXED {
                out.boxes.push(b);
            }
            next_chain[k] += 1;
            next_point[k] += n as u32;
            src += n;
        }
        chain_at.truncate(INDEX_LEN);
        point_at.truncate(INDEX_LEN);
        out.index = Some(CellIndex {
            chain: chain_at.into_boxed_slice(),
            point: point_at.into_boxed_slice(),
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::generator::noise::{hash2, unit};
    use crate::night::roads::dsf::Chains;
    use std::collections::HashMap;

    fn layer(chains: &[(u8, &[(f32, f32)])], zones: &[&str]) -> DsfRoads {
        let mut c = Chains::new();
        for (s, pts) in chains {
            c.push(*s, pts);
        }
        DsfRoads {
            tile: (46, 6),
            overlay: true,
            exclusions: zones.iter().map(|z| Exclusion::parse(z).unwrap()).collect(),
            chains: c,
        }
    }

    fn covered_after(z: &[&str]) -> bool {
        let mut m = Merger::new((46, 6));
        m.add(&layer(&[], z));
        m.covered()
    }

    fn merger_of(chains: &[(u8, &[(f32, f32)])]) -> Merger {
        let mut m = Merger::new((46, 6));
        m.add(&layer(chains, &[]));
        m
    }

    fn merged_of(chains: &[(u8, &[(f32, f32)])]) -> MergedRoads {
        merger_of(chains).finish()
    }

    /// A chain's class and quantised `(lon, lat)` steps.
    type Steps = (RoadClass, Vec<(u16, u16)>);

    fn steps(c: &ChainRef<'_>) -> Steps {
        let pts = c
            .lon_steps()
            .iter()
            .copied()
            .zip(c.lat_steps().iter().copied());
        (c.class(), pts.collect())
    }

    /// The chains `r` visits for a box (absolute degrees).
    fn visit(r: &MergedRoads, s: f64, w: f64, n: f64, e: f64) -> Vec<Steps> {
        let mut out = vec![];
        r.for_each_chain_near(s, w, n, e, |c| out.push(steps(&c)));
        out
    }

    /// Every chain of `r`: a visit of the whole tile.
    fn all(r: &MergedRoads) -> Vec<Steps> {
        let (lat0, lon0) = (f64::from(r.tile.0), f64::from(r.tile.1));
        visit(r, lat0, lon0, lat0 + 1.0, lon0 + 1.0)
    }

    /// Every chain of `r` decoded to tile-relative offsets.
    fn decoded(r: &MergedRoads) -> Vec<(RoadClass, Vec<(f64, f64)>)> {
        let (lat0, lon0) = (f64::from(r.tile.0), f64::from(r.tile.1));
        let mut out = vec![];
        r.for_each_chain_near(lat0, lon0, lat0 + 1.0, lon0 + 1.0, |c| {
            out.push((
                c.class(),
                c.points().map(|(x, y)| (x - lon0, y - lat0)).collect(),
            ))
        });
        out
    }

    /// `[south, west, north, east]` cells holding every point.
    fn cell_box(pts: &[(u16, u16)]) -> [u8; 4] {
        pts.iter().fold([u8::MAX, u8::MAX, 0, 0], |b, &(x, y)| {
            let (x, y) = (cell_of(x), cell_of(y));
            [b[0].min(y), b[1].min(x), b[2].max(y), b[3].max(x)]
        })
    }

    fn meets(b: [u8; 4], q: &CellBox) -> bool {
        b[2] >= q.south && b[0] <= q.north && b[3] >= q.west && b[1] <= q.east
    }

    /// Input points quantised as the merge stores them.
    fn quantised(pts: &[(f32, f32)]) -> Vec<(u16, u16)> {
        pts.iter()
            .map(|&(x, y)| (quantise(x), quantise(y)))
            .collect()
    }

    const STEP: f64 = 1.0 / 65535.0;

    #[test]
    fn a_higher_zone_removes_lower_chains_with_any_point_inside() {
        let mut m = Merger::new((46, 6));
        // High: a zone on the south-west quarter.
        m.add(&layer(
            &[(10, &[(0.1, 0.1), (0.2, 0.2)])],
            &["6/46/6.5/46.5"],
        ));
        // Low: one chain crossing into the zone, one wholly outside.
        m.add(&layer(
            &[
                (50, &[(0.4, 0.4), (0.9, 0.9)]),
                (50, &[(0.7, 0.7), (0.8, 0.8)]),
            ],
            &[],
        ));
        let r = m.finish();
        assert_eq!(r.len(), 2, "high chain + the low chain wholly outside");
        assert_eq!(r.removed, 1);
    }

    #[test]
    fn a_layer_never_excludes_its_own_or_higher_chains() {
        let mut m = Merger::new((46, 6));
        m.add(&layer(&[(10, &[(0.1, 0.1), (0.2, 0.2)])], &[]));
        m.add(&layer(&[(50, &[(0.3, 0.3), (0.4, 0.4)])], &["6/46/7/47"]));
        let r = m.finish();
        assert_eq!(r.len(), 2);
        assert_eq!(r.removed, 0);
    }

    #[test]
    fn only_a_whole_tile_rectangle_covers() {
        assert!(covered_after(&["6/46/7/47"]));
        assert!(!covered_after(&["6/46/6.9/47"]));
        assert!(!covered_after(&["6/46/7/47;6,46,7,46,7,47,6,47"]));
    }

    #[test]
    fn unlit_subtypes_are_not_merged_and_classes_and_boxes_follow_chains() {
        let r = merged_of(&[
            (74, &[(0.1, 0.1), (0.2, 0.2)]),
            (100, &[(0.3, 0.5), (0.6, 0.4)]),
        ]);
        assert_eq!(r.len(), 1);
        let c = all(&r);
        assert_eq!(c[0].0, RoadClass::Motorway);
        // Points are (lon, lat) offsets: lon 0.3..0.6, lat 0.4..0.5. The
        // cell box is [south, west, north, east] in 1/256 degree cells and
        // asymmetric on purpose, so a swapped axis fails.
        let cell = |v: f64| (((v * 65535.0).round() as u16) >> 8) as u8;
        assert_eq!(
            cell_box(&c[0].1),
            [cell(0.4), cell(0.3), cell(0.5), cell(0.6)]
        );
        assert!(r.crosses(46.45, 6.35, 46.46, 6.36));
        assert!(!r.crosses(46.8, 6.8, 46.9, 6.9));
    }

    #[test]
    fn merged_points_and_offsets_stay_aligned_across_layers() {
        let mut m = Merger::new((46, 6));
        m.add(&layer(
            &[(10, &[(0.1, 0.2), (0.3, 0.4)])],
            &["6/46/6.15/46.5"],
        ));
        m.add(&layer(
            &[
                (50, &[(0.12, 0.3), (0.13, 0.31)]),
                (50, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]),
            ],
            &[],
        ));
        let r = m.finish();
        assert_eq!(r.len(), 2);
        assert_eq!(r.removed, 1);
        let close = |got: &[(f64, f64)], want: &[(f64, f64)]| {
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(want) {
                assert!(
                    (g.0 - w.0).abs() < STEP && (g.1 - w.1).abs() < STEP,
                    "{g:?} {w:?}"
                );
            }
        };
        let chains = decoded(&r);
        let of = |class| &chains.iter().find(|c| c.0 == class).unwrap().1;
        close(of(RoadClass::Primary), &[(0.1, 0.2), (0.3, 0.4)]);
        close(of(RoadClass::Local), &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        assert!(r.bytes() > 0);
    }

    #[test]
    fn an_empty_merge_crosses_and_visits_nothing() {
        let r = MergedRoads::empty((46, 6));
        assert!(!r.crosses(46.0, 6.0, 47.0, 7.0));
        assert!(all(&r).is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        let r = Merger::new((46, 6)).finish_with(true);
        assert!(all(&r).is_empty() && r.is_empty());
    }

    #[test]
    fn a_road_class_is_one_byte() {
        assert_eq!(std::mem::size_of::<RoadClass>(), 1);
    }

    #[test]
    fn a_chain_byte_holds_every_class_and_length() {
        for subtype in 0..=255u8 {
            let Some(class) = RoadClass::of(subtype) else {
                continue;
            };
            for points in 1..=MAX_CHAIN_POINTS {
                let m = chain_byte(class, points);
                assert_eq!((class_of(m), points_of(m)), (class, points));
            }
        }
    }

    #[test]
    fn points_round_trip_within_half_a_step_and_clamp_at_the_edges() {
        let xs = [0.0f32, 1.0, -1e-6, 1.000_001, 0.123_456, 0.5, 0.999_99];
        let pts: Vec<(f32, f32)> = xs.iter().map(|&x| (x, 1.0 - x)).collect();
        let r = merged_of(&[(100, &pts)]);
        let got = decoded(&r).remove(0).1;
        for ((&x, g), &(ox, oy)) in xs.iter().zip(&got).zip(&pts) {
            let want_x = f64::from(ox).clamp(0.0, 1.0);
            let want_y = f64::from(oy).clamp(0.0, 1.0);
            assert!(
                (g.0 - want_x).abs() <= STEP / 2.0 + 1e-6,
                "lon {x}: {}",
                g.0
            );
            assert!(
                (g.1 - want_y).abs() <= STEP / 2.0 + 1e-6,
                "lat {x}: {}",
                g.1
            );
            assert!((0.0..=1.0).contains(&g.0) && (0.0..=1.0).contains(&g.1));
        }
        assert_eq!(got[0].0, 0.0);
        assert_eq!(got[1].0, 1.0);
        assert_eq!(got[2].0, 0.0, "clamped, not wrapped");
        assert_eq!(got[3].0, 1.0, "clamped, not wrapped");
    }

    #[test]
    fn long_chains_are_split_with_a_shared_point_and_lose_none() {
        for n in [2usize, 31, 32, 33, 63, 64, 100, 300] {
            let pts: Vec<(f32, f32)> = (0..n)
                .map(|k| (0.1 + k as f32 * 1e-4, 0.2 + (k % 7) as f32 * 1e-4))
                .collect();
            let r = merged_of(&[(50, &pts)]);
            // Pieces of up to MAX_CHAIN_POINTS, each starting on the last
            // point of the one before.
            let pieces = if n <= MAX_CHAIN_POINTS {
                1
            } else {
                (n - 2) / (MAX_CHAIN_POINTS - 1) + 1
            };
            assert_eq!(r.len(), pieces, "{n} points");
            let got = all(&r);
            assert!(got.iter().all(|c| c.1.len() <= MAX_CHAIN_POINTS));
            assert!(got.iter().all(|c| c.0 == RoadClass::Local));
            // In any order: rejoin by the shared points.
            let mut rest = got.into_iter().map(|c| c.1).collect::<Vec<_>>();
            let want = quantised(&pts);
            let start = rest.iter().position(|c| c[0] == want[0]).unwrap();
            let mut joined = rest.swap_remove(start);
            while !rest.is_empty() {
                let last = *joined.last().unwrap();
                let k = rest
                    .iter()
                    .position(|c| c[0] == last)
                    .expect("the next piece starts on the joint");
                joined.extend_from_slice(&rest.swap_remove(k)[1..]);
            }
            assert_eq!(joined, want, "{n} points");
        }
    }

    #[test]
    fn chains_of_fewer_than_two_points_are_skipped() {
        // Built by offsets, as a decoded cache entry could hold them.
        let chains = Chains {
            lon: vec![0.1, 0.2, 0.3],
            lat: vec![0.1, 0.2, 0.3],
            start: vec![0, 0, 1, 3],
            subtype: vec![100, 100, 100],
        };
        let mut m = Merger::new((46, 6));
        m.add(&DsfRoads {
            chains,
            ..layer(&[], &[])
        });
        let r = m.finish();
        assert_eq!(r.len(), 1, "only the two-point chain");
        assert_eq!(all(&r)[0].1.len(), 2);
        assert!(
            !r.crosses(46.09, 6.09, 46.11, 6.11),
            "nothing kept of the one-point chain"
        );
    }

    /// Chains of 2 to 5 points: corners, edges and pseudo-random.
    fn sample_chains() -> Vec<Vec<(f32, f32)>> {
        let mut out = vec![
            vec![(0.0, 0.0), (0.0, 0.0)],
            vec![(1.0, 1.0), (1.0, 1.0)],
            vec![(0.0, 1.0), (1.0, 0.0)],
            vec![(0.999_99, 0.5), (1.0, 0.5)],
            vec![(0.5, 0.003_906_25), (0.5, 0.003_906_3)],
        ];
        for k in 0..300i64 {
            let n = 2 + (hash2(k, 0, 1) % 4) as usize;
            let mut p = vec![];
            let (mut x, mut y) = (unit(hash2(k, 1, 1)), unit(hash2(k, 2, 1)));
            for m in 0..n as i64 {
                p.push((x.clamp(0.0, 1.0) as f32, y.clamp(0.0, 1.0) as f32));
                x += (unit(hash2(k, 3 + m, 1)) - 0.5) * 0.05;
                y += (unit(hash2(k, 40 + m, 1)) - 0.5) * 0.05;
            }
            out.push(p);
        }
        out
    }

    #[test]
    fn a_visit_finds_a_chain_from_any_box_around_its_points() {
        let chains = sample_chains();
        let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (100u8, &c[..])).collect();
        for index in [false, true] {
            let r = merger_of(&input).finish_with(index);
            assert_eq!(r.len(), chains.len());
            let (lat0, lon0) = (46.0, 6.0);
            for c in &chains {
                let want = quantised(c);
                // A box around each exact input point finds the chain, at any size.
                for &(x, y) in c {
                    let (x, y) = (lon0 + f64::from(x), lat0 + f64::from(y));
                    for grow in [0.0, 1e-6, 0.0004, 0.01] {
                        let got = visit(&r, y - grow, x - grow, y + grow, x + grow);
                        assert!(
                            got.iter().any(|g| g.1 == want),
                            "index {index}: {x},{y} grow {grow}"
                        );
                    }
                }
            }
            // Random boxes: whenever an exact point lies in the box, the chain is found.
            for k in 0..500i64 {
                let (x, y) = (unit(hash2(k, 7, 2)), unit(hash2(k, 8, 2)));
                let (w, h) = (unit(hash2(k, 9, 2)) * 0.1, unit(hash2(k, 10, 2)) * 0.1);
                let (s, wst, n, e) = (lat0 + y, lon0 + x, lat0 + y + h, lon0 + x + w);
                let got = visit(&r, s, wst, n, e);
                for c in &chains {
                    let meets = c.iter().any(|&(px, py)| {
                        let (px, py) = (lon0 + f64::from(px), lat0 + f64::from(py));
                        (wst..=e).contains(&px) && (s..=n).contains(&py)
                    });
                    if meets {
                        let want = quantised(c);
                        assert!(got.iter().any(|g| g.1 == want), "index {index}: box {k}");
                    }
                }
            }
        }
    }

    #[test]
    fn cells_and_crosses_handle_the_tile_edges() {
        let r = merged_of(&[(100, &[(0.9, 1.0), (1.0, 1.0)])]);
        // A chain on the north and east edge (offset 1.0) is found.
        assert!(r.crosses(46.999, 6.999, 47.0, 7.0));
        assert!(r.cells(46.999, 6.999, 47.0, 7.0).is_some());
        assert_eq!(visit(&r, 46.999, 6.999, 47.0, 7.0).len(), 1);
        // A box that only overlaps the tile edge, mostly outside, finds it too.
        assert!(r.crosses(46.9999, 6.95, 47.5, 7.5));
        assert_eq!(visit(&r, 46.9999, 6.95, 47.5, 7.5).len(), 1);
        // Boxes wholly outside the tile miss.
        assert!(!r.crosses(47.01, 6.0, 47.5, 7.5));
        for b in [
            (47.01, 6.0, 47.5, 7.5),
            (45.0, 6.0, 45.99, 7.0),
            (46.0, 7.01, 47.0, 7.5),
            (46.0, 4.0, 47.0, 5.99),
        ] {
            assert!(r.cells(b.0, b.1, b.2, b.3).is_none(), "{b:?}");
            assert!(visit(&r, b.0, b.1, b.2, b.3).is_empty(), "{b:?}");
        }
        // And a box in the tile's empty corner does not.
        assert!(!r.crosses(46.0, 6.0, 46.1, 6.1));
    }

    #[test]
    fn bytes_counts_every_vec_and_the_index() {
        // Chain A has 2 points, chain B has 3: 5 points, 2 chains, each
        // within a cell.
        let chains: [(u8, &[(f32, f32)]); 2] = [
            (100, &[(0.1, 0.1), (0.1002, 0.1001)]),
            (50, &[(0.3, 0.3), (0.3001, 0.3001), (0.3002, 0.3002)]),
        ];
        let fixed = std::mem::size_of::<MergedRoads>() + OCCUPANCY_BYTES;
        // Unindexed: both chains carry a box.
        let r = merged_of(&chains);
        assert!(r.index.is_none());
        assert_eq!(r.bytes(), 5 * 2 + 5 * 2 + 2 + 2 * 4 + fixed);
        // Indexed: neither chain spans more than a cell, so neither carries a box.
        let r = merger_of(&chains).finish_with(true);
        assert!(r.boxes.is_empty());
        assert_eq!(r.bytes(), 5 * 2 + 5 * 2 + 2 + INDEX_BYTES + fixed);
        assert_eq!(MergedRoads::empty((0, 0)).bytes(), fixed);
        assert_eq!(OCCUPANCY_BYTES, 8192, "256 x 256 cells, one bit each");
        assert_eq!(
            INDEX_BYTES,
            2 * 4 * (256 * 256 + 1),
            "two u32 per cell, and an end"
        );
    }

    #[test]
    fn the_index_is_kept_only_when_it_costs_less_than_boxes() {
        // Two-point chains on a grid, every one within a cell.
        let grid = |n: usize| -> Vec<Vec<(f32, f32)>> {
            (0..n)
                .map(|k| {
                    let (x, y) = ((k % 1000) as f32 / 1000.0, (k / 1000) as f32 / 1000.0);
                    vec![(x, y), (x + 1e-4, y)]
                })
                .collect()
        };
        let even = INDEX_BYTES / 4;
        for (n, indexed) in [(even, false), (even + 1, true)] {
            let chains = grid(n);
            let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (50u8, &c[..])).collect();
            let auto = merger_of(&input).finish();
            assert_eq!(auto.index.is_some(), indexed, "{n} chains");
            let with = merger_of(&input).finish_with(true).bytes();
            let without = merger_of(&input).finish_with(false).bytes();
            assert_eq!(auto.bytes(), with.min(without), "{n} chains");
        }
    }

    #[test]
    fn only_chains_spanning_more_than_the_limit_carry_a_box_in_an_indexed_tile() {
        let c = 1.0 / 256.0;
        let r = merger_of(&[
            // Within one cell, across one cell line, across six.
            (100, &[(0.5 * c, 0.5 * c), (0.6 * c, 0.5 * c)]),
            (100, &[(10.5 * c, 10.5 * c), (11.5 * c, 11.5 * c)]),
            (100, &[(20.5 * c, 20.5 * c), (26.5 * c, 20.5 * c)]),
        ])
        .finish_with(true);
        assert_eq!(r.boxes.len(), 1);
        assert_eq!(r.span, MAX_SPAN);
        let r = merger_of(&[(100, &[(0.5 * c, 0.5 * c), (0.6 * c, 0.5 * c)])]).finish_with(true);
        assert_eq!((r.boxes.len(), r.span), (0, 0));
    }

    /// The per-chain answer `crosses` must equal: any chain's cell box
    /// meets the query's.
    fn crosses_scan(r: &MergedRoads, south: f64, west: f64, north: f64, east: f64) -> bool {
        r.cells(south, west, north, east)
            .is_some_and(|q| all(r).iter().any(|c| meets(cell_box(&c.1), &q)))
    }

    /// `n` chains of 2 to 4 points; one in eight spans up to half the tile,
    /// the rest stay within a few cells, some on the tile edges.
    fn random_chains(seed: u64, n: i64) -> Vec<Vec<(f32, f32)>> {
        (0..n)
            .map(|k| {
                let reach = if hash2(k, 0, seed) % 8 == 0 {
                    0.5
                } else {
                    0.02
                };
                let edge = |v: f64, h: u64| match h % 16 {
                    0 => 0.0,
                    1 => 1.0,
                    _ => v,
                };
                let (x, y) = (
                    edge(unit(hash2(k, 1, seed)), hash2(k, 2, seed)),
                    edge(unit(hash2(k, 3, seed)), hash2(k, 4, seed)),
                );
                let points = 2 + (hash2(k, 5, seed) % 3) as i64;
                (0..points)
                    .map(|m| {
                        let dx = (unit(hash2(k, 10 + m, seed)) - 0.5) * reach;
                        let dy = (unit(hash2(k, 20 + m, seed)) - 0.5) * reach;
                        (
                            (x + dx).clamp(0.0, 1.0) as f32,
                            (y + dy).clamp(0.0, 1.0) as f32,
                        )
                    })
                    .collect()
            })
            .collect()
    }

    /// Box `k` of a random set (absolute degrees): from a point to a few
    /// cells, some reaching past the tile, some wholly outside it, some on
    /// its edges.
    fn random_box(k: i64, seed: u64) -> (f64, f64, f64, f64) {
        let (lat0, lon0) = (46.0, 6.0);
        let size = [0.0, 1e-5, 0.002, 0.02, 0.3][(hash2(k, 30, seed) % 5) as usize];
        let (x, y) = (
            unit(hash2(k, 31, seed)) * 1.2 - 0.1,
            unit(hash2(k, 32, seed)) * 1.2 - 0.1,
        );
        let (w, h) = (
            unit(hash2(k, 33, seed)) * size,
            unit(hash2(k, 34, seed)) * size,
        );
        match k % 50 {
            0 => (lat0, lon0, lat0 + 1.0, lon0 + 1.0),
            1 => (lat0 + 1.0, lon0 + 1.0, lat0 + 1.0, lon0 + 1.0),
            2 => (lat0, lon0, lat0, lon0),
            _ => (lat0 + y, lon0 + x, lat0 + y + h, lon0 + x + w),
        }
    }

    const RANDOM_TILES: [(u64, i64); 5] = [(1, 0), (2, 1), (3, 12), (4, 400), (5, 3000)];

    #[test]
    fn crosses_answers_exactly_as_the_chain_scan() {
        for (seed, n) in RANDOM_TILES {
            let chains = random_chains(seed, n);
            let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (100u8, &c[..])).collect();
            for index in [false, true] {
                let r = merger_of(&input).finish_with(index);
                let mut hits = 0;
                for k in 0..4000i64 {
                    let b = random_box(k, seed);
                    let want = crosses_scan(&r, b.0, b.1, b.2, b.3);
                    assert_eq!(
                        r.crosses(b.0, b.1, b.2, b.3),
                        want,
                        "index {index} seed {seed} box {k}"
                    );
                    hits += usize::from(want);
                }
                if n >= 400 {
                    assert!(hits > 100, "seed {seed}: the boxes must meet chains too");
                }
            }
        }
    }

    /// Chains by their steps, with how many times each occurs.
    fn counted(chains: impl IntoIterator<Item = Steps>) -> HashMap<Vec<(u16, u16)>, usize> {
        let mut m = HashMap::new();
        for c in chains {
            *m.entry(c.1).or_default() += 1;
        }
        m
    }

    #[test]
    fn a_visit_is_complete_and_conservative_against_every_chain() {
        for (seed, n) in RANDOM_TILES {
            let chains = random_chains(seed, n);
            let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (100u8, &c[..])).collect();
            for index in [false, true] {
                let r = merger_of(&input).finish_with(index);
                let every = all(&r);
                let every_count = counted(every.clone());
                let input_count =
                    counted(input.iter().map(|c| (RoadClass::Motorway, quantised(c.1))));
                assert_eq!(
                    every_count, input_count,
                    "a whole-tile visit is every chain"
                );
                let mut visited_some = 0;
                for k in 0..1000i64 {
                    let b = random_box(k, seed);
                    let got = visit(&r, b.0, b.1, b.2, b.3);
                    visited_some += usize::from(!got.is_empty());
                    let Some(q) = r.cells(b.0, b.1, b.2, b.3) else {
                        assert!(got.is_empty(), "seed {seed} box {k}: off the tile");
                        continue;
                    };
                    let got = counted(got);
                    // Complete: every chain whose cell box meets the query's.
                    let want = counted(every.iter().filter(|c| meets(cell_box(&c.1), &q)).cloned());
                    for (c, times) in &want {
                        assert_eq!(
                            got.get(c),
                            Some(times),
                            "index {index} seed {seed} box {k}: missed"
                        );
                    }
                    // Conservative, not everything: a visited chain's box is
                    // within the span limit of the query.
                    let grown = CellBox {
                        south: q.south.saturating_sub(MAX_SPAN),
                        west: q.west.saturating_sub(MAX_SPAN),
                        north: q.north.saturating_add(MAX_SPAN),
                        east: q.east.saturating_add(MAX_SPAN),
                    };
                    for (c, times) in &got {
                        assert!(
                            meets(cell_box(c), &grown),
                            "index {index} seed {seed} box {k}: far"
                        );
                        assert!(times <= &every_count[c], "visited twice");
                    }
                }
                if n >= 400 {
                    assert!(
                        visited_some > 100,
                        "seed {seed}: the boxes must meet chains too"
                    );
                }
            }
        }
    }
}
