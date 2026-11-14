//! Merge one tile's DSF networks as X-Plane presents them: highest
//! priority first; a chain is dropped when any point lies in a zone of a
//! higher-priority DSF. Unlit subtypes are dropped too.
//!
//! The merged tile is what the road cache keeps in memory, so it is stored
//! compactly: points as 16-bit offsets in the tile (1/65535 degree, about
//! 1.7 m, far below a lit texel), one class byte per chain and a 4-byte box
//! of 1/256 degree cells for culling. Consumers see degrees through
//! [`MergedRoads::points`] and cull through [`MergedRoads::query`] and
//! [`MergedRoads::in_box`], never the encoding. A one-bit-per-cell
//! occupancy map (8 KB per tile) answers [`MergedRoads::crosses`] in time
//! proportional to the query's cells, not to the tile's chains: the lit
//! probe asks it for every footprint X-Plane stats.

use super::class::RoadClass;
use super::dsf::DsfRoads;
use super::exclusion::Exclusion;

/// Steps per degree of a point offset.
const STEPS: f64 = 65535.0;
/// A cell is `1 << CELL_SHIFT` steps: 1/256 degree.
const CELL_SHIFT: u32 = 8;
/// Bytes held beyond the Vec contents (the struct itself, rounded).
const FIXED_BYTES: usize = 64;
/// Cells along each side of a tile.
const CELLS: usize = 256;
/// Occupancy words per row of cells.
const ROW_WORDS: usize = CELLS / 64;
/// One bit per cell: row `r` (from the south) is words `r * ROW_WORDS..`,
/// cell column `c` bit `c % 64` of word `c / 64` of its row.
type Occupancy = Box<[u64; CELLS * ROW_WORDS]>;
/// Heap bytes of an [`Occupancy`].
const OCCUPANCY_BYTES: usize = CELLS * ROW_WORDS * 8;

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

/// A query box in cells, as [`MergedRoads::query`] prepares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellBox {
    south: u8,
    west: u8,
    north: u8,
    east: u8,
}

/// One tile's lit road chains after the priority merge.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedRoads {
    pub tile: (i32, i32),
    /// Point offsets east of the tile's west edge, in steps.
    lon: Vec<u16>,
    /// Point offsets north of the tile's south edge, in steps.
    lat: Vec<u16>,
    /// Chain `i` owns points `start[i]..start[i + 1]`.
    start: Vec<u32>,
    class: Vec<RoadClass>,
    /// Per chain `[south, west, north, east]` cells containing every point.
    bbox: Vec<[u8; 4]>,
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
            start: vec![0],
            class: vec![],
            bbox: vec![],
            occupied: Box::new([0; CELLS * ROW_WORDS]),
            removed: 0,
        }
    }

    /// Number of chains.
    pub fn len(&self) -> usize {
        self.class.len()
    }

    pub fn is_empty(&self) -> bool {
        self.class.is_empty()
    }

    pub fn class(&self, i: usize) -> RoadClass {
        self.class[i]
    }

    /// Chain `i`'s points as absolute `(lon, lat)` degrees.
    pub fn points(&self, i: usize) -> impl Iterator<Item = (f64, f64)> + '_ {
        let r = self.start[i] as usize..self.start[i + 1] as usize;
        let (lat0, lon0) = (f64::from(self.tile.0), f64::from(self.tile.1));
        self.lon[r.clone()]
            .iter()
            .zip(&self.lat[r])
            .map(move |(&x, &y)| (lon0 + f64::from(x) / STEPS, lat0 + f64::from(y) / STEPS))
    }

    /// Heap bytes held, for the byte-bounded tile cache.
    pub fn bytes(&self) -> usize {
        self.lon.len() * 2
            + self.lat.len() * 2
            + self.start.len() * 4
            + self.class.len()
            + self.bbox.len() * 4
            + OCCUPANCY_BYTES
            + FIXED_BYTES
    }

    /// An absolute-degree box as cells of this tile, or `None` when it
    /// misses the tile. Conservative: every point whose exact or decoded
    /// position is in the box is in a cell of the result.
    pub fn query(&self, south: f64, west: f64, north: f64, east: f64) -> Option<CellBox> {
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

    /// Whether chain `i`'s cell box meets `q`.
    pub fn in_box(&self, i: usize, q: &CellBox) -> bool {
        let b = self.bbox[i];
        b[2] >= q.south && b[0] <= q.north && b[3] >= q.west && b[1] <= q.east
    }

    /// Whether any chain's box meets the box (absolute degrees). Two cell
    /// boxes meet exactly when they share a cell, so the occupancy map
    /// gives the answer a scan of every chain's box would.
    pub fn crosses(&self, south: f64, west: f64, north: f64, east: f64) -> bool {
        self.query(south, west, north, east).is_some_and(|q| {
            spans(q.south, q.west, q.north, q.east).any(|(i, mask)| self.occupied[i] & mask != 0)
        })
    }
}

/// Fed the DSFs of one tile from highest priority to lowest.
pub struct Merger {
    tile: (i32, i32),
    zones: Vec<Exclusion>,
    out: MergedRoads,
}

impl Merger {
    pub fn new(tile: (i32, i32)) -> Self {
        Self {
            tile,
            zones: vec![],
            out: MergedRoads::empty(tile),
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
            let pts = c.points(i);
            if pts.is_empty() {
                continue;
            }
            let (lon, lat) = (&c.lon[pts.clone()], &c.lat[pts]);
            if !self.zones.is_empty()
                && lon.iter().zip(lat).any(|(&x, &y)| {
                    let (x, y) = (f64::from(x) + lon0, f64::from(y) + lat0);
                    self.zones.iter().any(|z| z.contains(x, y))
                })
            {
                self.out.removed += 1;
                continue;
            }
            let out = &mut self.out;
            // Min and max step of `[south, west, north, east]`.
            let mut b = [u16::MAX, u16::MAX, 0, 0];
            for (&x, &y) in lon.iter().zip(lat) {
                let (qx, qy) = (quantise(x), quantise(y));
                out.lon.push(qx);
                out.lat.push(qy);
                b = [b[0].min(qy), b[1].min(qx), b[2].max(qy), b[3].max(qx)];
            }
            out.start.push(out.lon.len() as u32);
            out.class.push(class);
            let [s, w, n, e] = b.map(cell_of);
            for (i, mask) in spans(s, w, n, e) {
                out.occupied[i] |= mask;
            }
            out.bbox.push([s, w, n, e]);
        }
        self.zones.extend(layer.exclusions.iter().cloned());
    }

    /// Whether a zone covers the whole tile, so no lower DSF can add roads.
    pub fn covered(&self) -> bool {
        self.zones
            .iter()
            .any(|z| z.covers_tile(self.tile.0, self.tile.1))
    }

    pub fn finish(mut self) -> MergedRoads {
        let o = &mut self.out;
        o.lon.shrink_to_fit();
        o.lat.shrink_to_fit();
        o.start.shrink_to_fit();
        o.class.shrink_to_fit();
        o.bbox.shrink_to_fit();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::generator::noise::{hash2, unit};
    use crate::night::roads::dsf::Chains;

    fn layer(chains: &[(u8, &[(f32, f32)])], zones: &[&str]) -> DsfRoads {
        let mut c = Chains::new();
        for (s, pts) in chains {
            c.push(*s, pts);
        }
        DsfRoads {
            tile: (46, 6),
            overlay: true,
            net_def: Some("x.net".into()),
            exclusions: zones.iter().map(|z| Exclusion::parse(z).unwrap()).collect(),
            chains: c,
        }
    }

    fn covered_after(z: &[&str]) -> bool {
        let mut m = Merger::new((46, 6));
        m.add(&layer(&[], z));
        m.covered()
    }

    fn merged_of(chains: &[(u8, &[(f32, f32)])]) -> MergedRoads {
        let mut m = Merger::new((46, 6));
        m.add(&layer(chains, &[]));
        m.finish()
    }

    /// Chain `i` decoded to tile-relative offsets.
    fn offsets(r: &MergedRoads, i: usize) -> Vec<(f64, f64)> {
        r.points(i).map(|(x, y)| (x - 6.0, y - 46.0)).collect()
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
        assert_eq!(r.class(0), RoadClass::Motorway);
        // Points are (lon, lat) offsets: lon 0.3..0.6, lat 0.4..0.5. The
        // cell box is [south, west, north, east] in 1/256 degree cells and
        // asymmetric on purpose, so a swapped axis fails.
        let cell = |v: f64| (((v * 65535.0).round() as u16) >> 8) as u8;
        assert_eq!(r.bbox[0], [cell(0.4), cell(0.3), cell(0.5), cell(0.6)]);
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
        let close = |got: Vec<(f64, f64)>, want: &[(f64, f64)]| {
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(want) {
                assert!(
                    (g.0 - w.0).abs() < STEP && (g.1 - w.1).abs() < STEP,
                    "{g:?} {w:?}"
                );
            }
        };
        close(offsets(&r, 0), &[(0.1, 0.2), (0.3, 0.4)]);
        close(offsets(&r, 1), &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        assert_eq!(
            (r.class(0), r.class(1)),
            (RoadClass::Primary, RoadClass::Local)
        );
        assert!(r.bytes() > 0);
    }

    #[test]
    fn an_empty_merge_crosses_nothing() {
        let r = MergedRoads::empty((46, 6));
        assert!(!r.crosses(46.0, 6.0, 47.0, 7.0));
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn a_road_class_is_one_byte() {
        assert_eq!(std::mem::size_of::<RoadClass>(), 1);
    }

    #[test]
    fn points_round_trip_within_half_a_step_and_clamp_at_the_edges() {
        let xs = [0.0f32, 1.0, -1e-6, 1.000_001, 0.123_456, 0.5, 0.999_99];
        let pts: Vec<(f32, f32)> = xs.iter().map(|&x| (x, 1.0 - x)).collect();
        let r = merged_of(&[(100, &pts)]);
        let got = offsets(&r, 0);
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
    fn cell_boxes_hold_their_chain_and_never_miss_a_query_it_meets() {
        let chains = sample_chains();
        let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (100u8, &c[..])).collect();
        let r = merged_of(&input);
        assert_eq!(r.len(), chains.len());
        let (lat0, lon0) = (46.0, 6.0);
        for (i, c) in chains.iter().enumerate() {
            let b = r.bbox[i];
            for (x, y) in r.points(i) {
                // Each decoded point lies inside its cell box.
                let cx = ((x - lon0) * 65535.0).round() as u16 >> 8;
                let cy = ((y - lat0) * 65535.0).round() as u16 >> 8;
                assert!(b[1] as u16 <= cx && cx <= b[3] as u16, "chain {i} lon cell");
                assert!(b[0] as u16 <= cy && cy <= b[2] as u16, "chain {i} lat cell");
            }
            // A query around each exact input point finds the chain, at any size.
            for &(x, y) in c {
                let (x, y) = (lon0 + f64::from(x), lat0 + f64::from(y));
                for grow in [0.0, 1e-6, 0.0004, 0.01] {
                    let q = r.query(y - grow, x - grow, y + grow, x + grow).unwrap();
                    assert!(r.in_box(i, &q), "chain {i} at {x},{y} grow {grow}");
                }
            }
        }
        // Random boxes: whenever an exact point lies in the box, the chain is found.
        for k in 0..500i64 {
            let (x, y) = (unit(hash2(k, 7, 2)), unit(hash2(k, 8, 2)));
            let (w, h) = (unit(hash2(k, 9, 2)) * 0.1, unit(hash2(k, 10, 2)) * 0.1);
            let (s, wst, n, e) = (lat0 + y, lon0 + x, lat0 + y + h, lon0 + x + w);
            let q = r.query(s, wst, n, e);
            for (i, c) in chains.iter().enumerate() {
                let meets = c.iter().any(|&(px, py)| {
                    let (px, py) = (lon0 + f64::from(px), lat0 + f64::from(py));
                    (wst..=e).contains(&px) && (s..=n).contains(&py)
                });
                if meets {
                    assert!(q.is_some_and(|q| r.in_box(i, &q)), "chain {i} box {k}");
                }
            }
        }
    }

    #[test]
    fn query_and_crosses_handle_the_tile_edges() {
        let r = merged_of(&[(100, &[(0.9, 1.0), (1.0, 1.0)])]);
        // A chain on the north and east edge (offset 1.0) is found.
        assert!(r.crosses(46.999, 6.999, 47.0, 7.0));
        assert!(r.query(46.999, 6.999, 47.0, 7.0).is_some());
        // A box that only overlaps the tile edge, mostly outside, finds it too.
        assert!(r.crosses(46.9999, 6.95, 47.5, 7.5));
        // Boxes wholly outside the tile miss.
        assert!(!r.crosses(47.01, 6.0, 47.5, 7.5));
        assert!(r.query(47.01, 6.0, 47.5, 7.5).is_none());
        assert!(r.query(45.0, 6.0, 45.99, 7.0).is_none());
        assert!(r.query(46.0, 7.01, 47.0, 7.5).is_none());
        assert!(r.query(46.0, 4.0, 47.0, 5.99).is_none());
        // And a box in the tile's empty corner does not.
        assert!(!r.crosses(46.0, 6.0, 46.1, 6.1));
    }

    #[test]
    fn bytes_counts_every_vec() {
        // Chain A has 2 points, chain B has 3: 5 points, 2 chains.
        let mut m = Merger::new((46, 6));
        m.add(&layer(
            &[
                (100, &[(0.1, 0.1), (0.2, 0.2)]),
                (50, &[(0.3, 0.3), (0.4, 0.4), (0.5, 0.5)]),
            ],
            &[],
        ));
        let r = m.finish();
        let expect = 5 * 2 + 5 * 2 + 3 * 4 + 2 + 2 * 4 + OCCUPANCY_BYTES + FIXED_BYTES;
        assert_eq!(r.bytes(), expect);
        assert_eq!(
            MergedRoads::empty((0, 0)).bytes(),
            4 + OCCUPANCY_BYTES + FIXED_BYTES
        );
        assert_eq!(OCCUPANCY_BYTES, 8192, "256 x 256 cells, one bit each");
    }

    /// The per-chain answer `crosses` must equal: any chain's cell box
    /// meets the query's.
    fn crosses_scan(r: &MergedRoads, south: f64, west: f64, north: f64, east: f64) -> bool {
        r.query(south, west, north, east)
            .is_some_and(|q| (0..r.len()).any(|i| r.in_box(i, &q)))
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

    #[test]
    fn crosses_answers_exactly_as_the_chain_scan() {
        let (lat0, lon0) = (46.0, 6.0);
        for (seed, n) in [(1u64, 0i64), (2, 1), (3, 12), (4, 400), (5, 3000)] {
            let chains = random_chains(seed, n);
            let input: Vec<(u8, &[(f32, f32)])> = chains.iter().map(|c| (100u8, &c[..])).collect();
            let r = merged_of(&input);
            let mut hits = 0;
            for k in 0..4000i64 {
                // Boxes from a point to a few cells, some reaching past the
                // tile, some wholly outside it, some on its edges.
                let size = [0.0, 1e-5, 0.002, 0.02, 0.3][(hash2(k, 30, seed) % 5) as usize];
                let (x, y) = (
                    unit(hash2(k, 31, seed)) * 1.2 - 0.1,
                    unit(hash2(k, 32, seed)) * 1.2 - 0.1,
                );
                let (w, h) = (
                    unit(hash2(k, 33, seed)) * size,
                    unit(hash2(k, 34, seed)) * size,
                );
                let b = match k % 50 {
                    0 => (lat0, lon0, lat0 + 1.0, lon0 + 1.0),
                    1 => (lat0 + 1.0, lon0 + 1.0, lat0 + 1.0, lon0 + 1.0),
                    2 => (lat0, lon0, lat0, lon0),
                    _ => (lat0 + y, lon0 + x, lat0 + y + h, lon0 + x + w),
                };
                let want = crosses_scan(&r, b.0, b.1, b.2, b.3);
                assert_eq!(r.crosses(b.0, b.1, b.2, b.3), want, "seed {seed} box {k}");
                hits += usize::from(want);
            }
            if n >= 400 {
                assert!(hits > 100, "seed {seed}: the boxes must meet chains too");
            }
        }
    }
}
