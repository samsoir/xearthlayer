//! Per-footprint road glow: for each texel the brightest class profile of
//! any nearby segment, in 0..=1 (class intensity included). Light comes
//! from geometry in metres, so it is world-anchored at every zoom.
//!
//! Cost: a tile is skipped whole when no chain nears the footprint, a chain
//! is culled by its box in tile-relative `f32`, and a segment visits only
//! the texels in its reach window, found from the even texel spacing. The
//! texel loop has no branch and no table (the fall-off is a few multiplies
//! in `f32`, the reach test a select), so the compiler vectorises it;
//! everything that varies only by row is computed once per row, with no
//! division. Lit texels are counted once, after drawing. The profile (one
//! `f32` per texel) is allocated when the first segment reaches the grid,
//! so a texture with no road nearby costs no heap at all.

use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

use super::class::RoadClass;
use super::merge::MergedRoads;
use super::model::{ClassParams, LampParams, RoadModel};
use crate::fuse::DdsFilename;
use crate::night::footprint::{Footprint, TILES_PER_TEXTURE};
use crate::night::generator::noise::{hash2, unit};
use crate::night::generator::tiling::{mercator, EARTH_RADIUS_M};

/// The cosine of the highest latitude [`reach_deg`] divides by.
const COS_REACH_FLOOR: f64 = 0.087_155_742_747_658_17; // cos 85 degrees

/// How far outside the footprint of `name` a road can still light a texel
/// of a `size` x `size` raster, in degrees, for either axis: the widest
/// reach of any class (half width + 3 sigma, with the sigma never below a
/// texel's ground size) over the least metres per degree (longitude, at the
/// footprint's highest latitude). Cheap and allocation free: only the
/// footprint's edges are needed. The raster culls with it and the has-light
/// probe must use it too, so the two cannot disagree.
pub fn reach_deg(name: &DdsFilename, size: u32, model: &RoadModel) -> f64 {
    let fp = Footprint::from_name(name);
    let metres_per_degree = EARTH_RADIUS_M.to_radians();
    // A texel's ground size is widest on the footprint row nearest the equator.
    let nearest = if fp.south <= 0.0 && fp.north >= 0.0 {
        0.0
    } else {
        fp.south.abs().min(fp.north.abs())
    };
    let texel_max = metres_per_degree * 360.0 * f64::from(TILES_PER_TEXTURE)
        / f64::from(1u32 << name.zoom)
        / f64::from(size.max(1))
        * nearest.to_radians().cos();
    let glow = model
        .classes
        .iter()
        .map(|p| f64::from(p.half_width_m) + 3.0 * f64::from(p.sigma_m).max(texel_max))
        .fold(0.0, f64::max);
    let lamps = model
        .rural_lamps
        .map_or(0.0, |l| 3.0 * f64::from(l.sigma_m).max(texel_max));
    let reach_m = glow.max(lamps);
    let highest = fp.south.abs().max(fp.north.abs()) + reach_m / metres_per_degree;
    let cos = highest.min(90.0).to_radians().cos().max(COS_REACH_FLOOR);
    reach_m / (metres_per_degree * cos)
}

const LAMP_SEED: u64 = 0x524F_4144_4C41_4D50;

/// Lamps closer than this would never finish walking a chain.
const MIN_LAMP_SPACING_M: f64 = 1.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct RoadFieldStats {
    pub segments: usize,
    pub raster_us: u64,
    pub lit_texels: usize,
}

/// A texture's road glow: one `0..=1` value per texel.
pub struct RoadField {
    size: u32,
    /// Empty when nothing is lit; otherwise `size * size`, north row first.
    profile: Vec<f32>,
    pub stats: RoadFieldStats,
}

impl RoadField {
    pub fn empty(size: u32) -> Self {
        Self {
            size,
            profile: Vec::new(),
            stats: RoadFieldStats::default(),
        }
    }

    /// Width and height in texels.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Whether no texel is lit.
    pub fn is_empty(&self) -> bool {
        self.stats.lit_texels == 0
    }

    /// The glow at texel (`row`, `col`), 0 when nothing reaches it.
    pub fn at(&self, row: u32, col: u32) -> f32 {
        debug_assert!(row < self.size && col < self.size);
        self.profile
            .get(row as usize * self.size as usize + col as usize)
            .copied()
            .unwrap_or(0.0)
    }

    /// The glow of texel row `row`, west first, `None` when nothing is lit
    /// (or past the last row): a reader of a whole texture pays one branch
    /// per row for an empty field instead of one call per texel.
    pub fn row(&self, row: u32) -> Option<&[f32]> {
        let size = self.size as usize;
        let start = row as usize * size;
        self.profile.get(start..start + size)
    }

    /// Whether the profile has been allocated.
    #[cfg(test)]
    fn allocated(&self) -> bool {
        self.profile.capacity() > 0
    }

    /// Rasters the roads of `tiles` over the `size` x `size` texels of the
    /// texture `name`, with the texel centres `CpuLitGenerator` samples.
    pub fn raster(
        name: &DdsFilename,
        size: u32,
        tiles: &[Arc<MergedRoads>],
        model: &RoadModel,
    ) -> Self {
        let started = Instant::now();
        if size == 0 {
            return Self::empty(size);
        }
        let fp = Footprint::from_name(name);
        let reach = reach_deg(name, size, model);
        // Built when the first tile with a chain near the footprint turns up.
        let mut canvas: Option<Canvas> = None;
        let mut pts: Vec<(f64, f64)> = Vec::new();
        let mut segments = 0usize;
        for tile in tiles {
            // The footprint and its reach, as cells of this tile.
            let Some(want) = tile.query(
                fp.south - reach,
                fp.west - reach,
                fp.north + reach,
                fp.east + reach,
            ) else {
                continue;
            };
            for ci in 0..tile.len() {
                if !tile.in_box(ci, &want) {
                    continue;
                }
                let canvas = canvas.get_or_insert_with(|| Canvas::new(Grid::new(name, size)));
                let class = tile.class(ci);
                pts.clear();
                pts.extend(tile.points(ci).map(|(lon, lat)| mercator(lat, lon)));
                segments += pts.len().saturating_sub(1);
                let lamps = model
                    .rural_lamps
                    .filter(|_| !matches!(class, RoadClass::Motorway | RoadClass::Ramp));
                match lamps {
                    None => canvas.draw_glow(&pts, model.classes[class.index()]),
                    Some(l) => {
                        let first = tile.points(ci).next().unwrap_or_default();
                        canvas.draw_lamps(&pts, chain_key(first), &l);
                    }
                }
            }
        }
        let Some(canvas) = canvas else {
            return Self::empty(size);
        };
        let lit_texels = canvas.lit();
        Self {
            size,
            // A window can reach the grid without lighting a texel.
            profile: if lit_texels == 0 {
                Vec::new()
            } else {
                canvas.profile
            },
            stats: RoadFieldStats {
                segments,
                raster_us: started.elapsed().as_micros() as u64,
                lit_texels,
            },
        }
    }
}

/// The lamp key of a chain: its first point `(lon, lat)` quantised to 1e-6 degrees.
fn chain_key((lon, lat): (f64, f64)) -> i64 {
    hash2(
        (lon * 1e6).round() as i64,
        (lat * 1e6).round() as i64,
        LAMP_SEED,
    ) as i64
}

/// cos(latitude) of a Web Mercator northing.
fn cos_lat_of(y: f64) -> f64 {
    1.0 / (y / EARTH_RADIUS_M).cosh()
}

/// The texel centres of a texture, in Web Mercator metres, exactly as
/// `CpuLitGenerator::generate` computes them.
struct Grid {
    size: usize,
    /// Per column, ascending.
    xs: Vec<f64>,
    /// Per row, descending (north row first).
    ys: Vec<f64>,
    /// Per row cos(latitude): Mercator metres to ground metres.
    cos: Vec<f64>,
    /// Per row, a texel's ground size: the least a glow sigma can be.
    texel: Vec<f64>,
    cos_min: f64,
    texel_max: f64,
    /// Per row, the inverse of `texel`.
    inv_texel: Vec<f64>,
    /// Columns per Mercator metre east, and rows per Mercator metre south
    /// (the centres are evenly spaced both ways).
    inv_dx: f64,
    inv_dy: f64,
}

impl Grid {
    fn new(name: &DdsFilename, size: u32) -> Self {
        let n = f64::from(1u32 << name.zoom);
        let side = f64::from(TILES_PER_TEXTURE);
        let px = f64::from(size);
        let lons = (0..size)
            .map(|j| (f64::from(name.col) + (f64::from(j) + 0.5) / px * side) / n * 360.0 - 180.0);
        let lats: Vec<f64> = (0..size)
            .map(|i| {
                let y = f64::from(name.row) + (f64::from(i) + 0.5) / px * side;
                (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees()
            })
            .collect();
        let xs: Vec<f64> = lons.map(|lon| mercator(0.0, lon).0).collect();
        let ys: Vec<f64> = lats.iter().map(|&lat| mercator(lat, 0.0).1).collect();
        let cos: Vec<f64> = lats.iter().map(|lat| lat.to_radians().cos()).collect();
        let last = size as usize - 1;
        // Mercator metres per texel along x.
        let dx = (xs[last] - xs[0]) / last.max(1) as f64;
        let texel: Vec<f64> = cos.iter().map(|c| dx * c).collect();
        Self {
            size: size as usize,
            cos_min: cos.iter().copied().fold(f64::MAX, f64::min).max(1e-6),
            texel_max: texel.iter().copied().fold(0.0, f64::max),
            inv_texel: texel.iter().map(|t| 1.0 / t).collect(),
            inv_dx: inverse(dx),
            inv_dy: inverse((ys[0] - ys[last]) / last.max(1) as f64),
            xs,
            ys,
            cos,
            texel,
        }
    }
}

/// `1 / x`, or 0 for 0 (a grid of one texel).
fn inverse(x: f64) -> f64 {
    if x > 0.0 {
        1.0 / x
    } else {
        0.0
    }
}

/// `v.partition_point(pred)` for a `pred` true on a prefix of `v`, walked
/// from `guess` (any index, clamped): exact whatever the guess, and a step
/// or two when the guess is close.
fn partition_from(v: &[f64], guess: f64, pred: impl Fn(f64) -> bool) -> usize {
    let mut k = (guess.max(0.0) as usize).min(v.len());
    while k > 0 && !pred(v[k - 1]) {
        k -= 1;
    }
    while k < v.len() && pred(v[k]) {
        k += 1;
    }
    k
}

/// A grid and the profile drawn on it (allocated by the first draw that
/// reaches the grid).
struct Canvas {
    grid: Grid,
    profile: Vec<f32>,
}

impl Canvas {
    fn new(grid: Grid) -> Self {
        Self {
            grid,
            profile: Vec::new(),
        }
    }

    /// Texels above zero, counted once when drawing is done rather than
    /// on every draw.
    fn lit(&self) -> usize {
        self.profile.iter().filter(|&&v| v > 0.0).count()
    }

    /// The class glow along each segment of a polyline.
    fn draw_glow(&mut self, pts: &[(f64, f64)], p: ClassParams) {
        let (hw, sigma) = (f64::from(p.half_width_m), f64::from(p.sigma_m));
        for w in pts.windows(2) {
            self.stamp(w[0], w[1], hw, sigma, p.intensity);
        }
    }

    /// Lamps at every `spacing_m` of ground arc length along a polyline,
    /// each lit by its chain's hash.
    fn draw_lamps(&mut self, pts: &[(f64, f64)], key: i64, l: &LampParams) {
        let (sigma, spacing) = (
            f64::from(l.sigma_m),
            f64::from(l.spacing_m).max(MIN_LAMP_SPACING_M),
        );
        let lit_share = f64::from(l.lit_share);
        let (mut along, mut next, mut k) = (0.0f64, 0.0f64, 0i64);
        for w in pts.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            let len = (x1 - x0).hypot(y1 - y0) * cos_lat_of(y0);
            while next <= along + len {
                if unit(hash2(key, k, LAMP_SEED)) < lit_share {
                    let t = if len > 0.0 { (next - along) / len } else { 0.0 };
                    let at = (x0 + t * (x1 - x0), y0 + t * (y1 - y0));
                    self.stamp(at, at, 0.0, sigma, l.intensity);
                }
                k += 1;
                next += spacing;
            }
            along += len;
        }
    }

    /// Max-combines the profile of the segment `a` to `b` (Mercator
    /// metres) into the texels within `half_width + 3 sigma` of it, with
    /// `sigma` at least a texel's ground size on each row.
    fn stamp(
        &mut self,
        a: (f64, f64),
        b: (f64, f64),
        half_width: f64,
        sigma_m: f64,
        intensity: f32,
    ) {
        let g = &self.grid;
        // The window uses the widest sigma and the widest Mercator reach
        // any row can have; each row then applies its own.
        let reach_m = (half_width + 3.0 * sigma_m.max(g.texel_max)) / g.cos_min;
        let (xmin, xmax) = (a.0.min(b.0) - reach_m, a.0.max(b.0) + reach_m);
        let (ymin, ymax) = (a.1.min(b.1) - reach_m, a.1.max(b.1) + reach_m);
        // The texel centres are evenly spaced, so each bound is guessed from
        // the spacing and settled by a step or two.
        let col = |x: f64| (x - g.xs[0]) * g.inv_dx;
        let row = |y: f64| (g.ys[0] - y) * g.inv_dy;
        let j0 = partition_from(&g.xs, col(xmin), |x| x < xmin);
        let j1 = partition_from(&g.xs, col(xmax) + 1.0, |x| x <= xmax);
        let i0 = partition_from(&g.ys, row(ymax), |y| y > ymax);
        let i1 = partition_from(&g.ys, row(ymin) + 1.0, |y| y >= ymin);
        if j0 >= j1 || i0 >= i1 {
            return;
        }
        if self.profile.is_empty() {
            self.profile = vec![0f32; g.size * g.size];
        }
        let seg = Segment::new(a, b);
        let sigma = (sigma_m, 1.0 / sigma_m);
        for i in i0..i1 {
            let k = RowKernel::new(&self.grid, i, half_width, sigma);
            self.stamp_row(i, j0..j1, &seg, &k, intensity);
        }
    }

    /// Max-combines the segment's profile into texels `cols` of row `i`.
    fn stamp_row(
        &mut self,
        i: usize,
        cols: std::ops::Range<usize>,
        seg: &Segment,
        k: &RowKernel,
        intensity: f32,
    ) {
        let g = &self.grid;
        let (a, (vx, vy)) = (seg.a, seg.v);
        let dy = g.ys[i] - a.1;
        let along_y = dy * vy;
        let row = &mut self.profile[i * g.size..(i + 1) * g.size];
        // Branch and table free, so it vectorises: every texel of the window
        // computes its value (0 out of reach) and keeps the larger. Inside
        // the core `u` is 0 and the fall-off exactly 1, so the core needs no
        // test either.
        for (&x, cell) in g.xs[cols.clone()].iter().zip(&mut row[cols]) {
            let dx = x - a.0;
            let t = ((dx * vx + along_y) * seg.inv_len2).clamp(0.0, 1.0);
            let (ex, ey) = (dx - t * vx, dy - t * vy);
            let d2 = (ex * ex + ey * ey) * k.c2;
            let u = ((d2 as f32).sqrt() - k.half_width) * k.inv_sigma;
            let v = if d2 <= k.reach2 {
                intensity * falloff(u)
            } else {
                0.0
            };
            *cell = cell.max(v);
        }
    }
}

/// A segment in Mercator metres: its start, its vector and the inverse of
/// its squared length (0 for a point).
struct Segment {
    a: (f64, f64),
    v: (f64, f64),
    inv_len2: f64,
}

impl Segment {
    fn new(a: (f64, f64), b: (f64, f64)) -> Self {
        let v = (b.0 - a.0, b.1 - a.1);
        let len2 = v.0 * v.0 + v.1 * v.1;
        Self {
            a,
            v,
            inv_len2: if len2 > 0.0 { 1.0 / len2 } else { 0.0 },
        }
    }
}

/// What a segment's glow needs on one texel row: everything that varies by
/// row and not by texel, with no division.
struct RowKernel {
    /// cos(latitude) squared: squared Mercator metres to ground metres.
    c2: f64,
    half_width: f32,
    reach2: f64,
    /// `1 / sigma`, the smaller of the two inverses (division rounds
    /// monotonically, so it is the inverse of the larger sigma exactly).
    inv_sigma: f32,
}

impl RowKernel {
    /// The sigma is `max(sigma_m, a texel)`; `inv_sigma_m` is `1 / sigma_m`.
    fn new(g: &Grid, i: usize, half_width: f64, (sigma_m, inv_sigma_m): (f64, f64)) -> Self {
        let inv_sigma = inv_sigma_m.min(g.inv_texel[i]);
        let reach = half_width + 3.0 * sigma_m.max(g.texel[i]);
        Self {
            c2: g.cos[i] * g.cos[i],
            half_width: half_width as f32,
            reach2: reach * reach,
            inv_sigma: inv_sigma as f32,
        }
    }
}

/// Squarings in [`falloff`].
const FALLOFF_SQUARINGS: u32 = 10;

/// The glow fall-off `exp(-u^2 / 2)` at `u = (distance - half width) /
/// sigma`, `u` clamped to `[0, 3]` (the glow's cut): `(1 - u^2 / 2^11)`
/// squared ten times, within 3e-4 of the Gaussian, exactly 1 at 0. No
/// branch and no table, so the texel loop vectorises. Every glow and lamp
/// texel is computed with it, so the same ground gets the same light in
/// any texture.
fn falloff(u: f32) -> f32 {
    let u = u.clamp(0.0, 3.0);
    let mut e = 1.0 - u * u * (0.5 / (1u32 << FALLOFF_SQUARINGS) as f32);
    for _ in 0..FALLOFF_SQUARINGS {
        e *= e;
    }
    e
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fuse::TextureKind;
    use crate::night::roads::dsf::{Chains, DsfRoads};
    use crate::night::roads::merge::Merger;
    use crate::night::roads::model::LampParams;
    use crate::night::Footprint;

    pub(crate) fn name_at_zoom(lat: f64, lon: f64, zoom: u8) -> DdsFilename {
        let n = f64::from(1u32 << zoom);
        let x = ((lon + 180.0) / 360.0 * n) as u32;
        let r = lat.to_radians();
        let y = ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        DdsFilename {
            row: y / 16 * 16,
            col: x / 16 * 16,
            zoom,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    /// One tile's merged roads from absolute (lon, lat) polylines.
    pub(crate) fn merged(tile: (i32, i32), roads: &[(u8, Vec<(f64, f64)>)]) -> Arc<MergedRoads> {
        let mut c = Chains::new();
        for (s, pts) in roads {
            let rel: Vec<(f32, f32)> = pts
                .iter()
                .map(|(x, y)| {
                    (
                        (x - f64::from(tile.1)) as f32,
                        (y - f64::from(tile.0)) as f32,
                    )
                })
                .collect();
            c.push(*s, &rel);
        }
        merge_tile(tile, c)
    }

    /// One tile's merged roads from tile-relative chains.
    fn merge_tile(tile: (i32, i32), chains: Chains) -> Arc<MergedRoads> {
        let mut m = Merger::new(tile);
        m.add(&DsfRoads {
            tile,
            overlay: true,
            net_def: Some("n".into()),
            exclusions: vec![],
            chains,
        });
        Arc::new(m.finish())
    }

    /// A street grid at about `pitch_m` over the footprint of `name` (which
    /// must lie in one 1 degree tile): a dense urban road field.
    pub(crate) fn street_grid_over(name: &DdsFilename, pitch_m: f64) -> Arc<MergedRoads> {
        let f = Footprint::from_name(name);
        let tile = (f.south.floor() as i32, f.west.floor() as i32);
        let rel = |v: f64, o: i32| (v - f64::from(o)) as f32;
        let mut c = Chains::new();
        street_grid(
            &mut c,
            (rel(f.south, tile.0), rel(f.north, tile.0)),
            (rel(f.west, tile.1), rel(f.east, tile.1)),
            pitch_m,
            0,
        );
        merge_tile(tile, c)
    }

    /// A road of `subtype` straight across a footprint, north to south
    /// through its centre and past both edges.
    fn across(name: &DdsFilename, subtype: u8) -> Arc<MergedRoads> {
        let f = Footprint::from_name(name);
        let lon = (f.west + f.east) / 2.0;
        merged(
            (46, 6),
            &[(subtype, vec![(lon, f.north + 0.01), (lon, f.south - 0.01)])],
        )
    }

    pub(crate) fn crossing(name: &DdsFilename) -> Arc<MergedRoads> {
        across(name, 100)
    }

    /// The `crossing` road rastered with the default model.
    pub(crate) fn crossing_field(name: &DdsFilename, size: u32) -> RoadField {
        RoadField::raster(name, size, &[crossing(name)], &RoadModel::DEFAULT)
    }

    #[test]
    fn partition_from_any_guess_is_the_partition_point() {
        use crate::night::generator::noise::{hash2, unit};
        let v: Vec<f64> = (0..300i32)
            .map(|k| f64::from(k) * 1.5 + unit(hash2(k.into(), 0, 3)))
            .collect();
        for k in 0..5_000i64 {
            let at = -20.0 + 500.0 * unit(hash2(k, 1, 3));
            let guess = -50.0 + 400.0 * unit(hash2(k, 2, 3));
            let want = v.partition_point(|&x| x < at);
            assert_eq!(
                partition_from(&v, guess, |x| x < at),
                want,
                "{at} from {guess}"
            );
        }
        assert_eq!(
            partition_from(&v, f64::NAN, |x| x < 10.0),
            v.partition_point(|&x| x < 10.0)
        );
        assert_eq!(partition_from(&[], 3.0, |x| x < 1.0), 0);
    }

    #[test]
    fn the_glow_falloff_is_within_a_1024th_of_the_gaussian() {
        let mut worst = 0.0f64;
        for k in 0..=300_000 {
            let u = 3.0 * f64::from(k) / 300_000.0;
            let err = (f64::from(falloff(u as f32)) - (-u * u / 2.0).exp()).abs();
            worst = worst.max(err);
        }
        // Measured: 2.7e-4.
        assert!(worst <= 1.0 / 1024.0, "largest error {worst:e}");
        assert!(worst < 3e-4, "largest error {worst:e}");
        assert_eq!(falloff(0.0), 1.0);
        // Rounding can put a texel at the cut a hair past 3 sigma.
        assert_eq!(falloff(3.0001), falloff(3.0));
        assert!(falloff(3.0) > 0.0);
    }

    /// `stamp` with every texel of the grid visited: no window, no span.
    fn stamp_everywhere(
        c: &mut Canvas,
        (a, b): ((f64, f64), (f64, f64)),
        half_width: f64,
        sigma_m: f64,
        intensity: f32,
    ) {
        let size = c.grid.size;
        if c.profile.is_empty() {
            c.profile = vec![0f32; size * size];
        }
        let seg = Segment::new(a, b);
        for i in 0..size {
            let k = RowKernel::new(&c.grid, i, half_width, (sigma_m, 1.0 / sigma_m));
            c.stamp_row(i, 0..size, &seg, &k, intensity);
        }
    }

    #[test]
    fn the_raster_equals_a_reference_that_visits_every_texel() {
        use crate::night::generator::noise::{hash2, unit};
        let n = name_at_zoom(46.5, 6.5, 16);
        for size in [64u32, 200] {
            let (mut fast, mut slow) = (
                Canvas::new(Grid::new(&n, size)),
                Canvas::new(Grid::new(&n, size)),
            );
            let g = &fast.grid;
            let (x0, x1) = (g.xs[0], g.xs[g.size - 1]);
            let (y0, y1) = (g.ys[g.size - 1], g.ys[0]);
            let r = |k: i64, s: u64| unit(hash2(k, size.into(), s));
            for k in 0..600 {
                // Anywhere over the grid and a little beyond it.
                let a = (
                    x0 + (x1 - x0) * (r(k, 1) * 1.2 - 0.1),
                    y0 + (y1 - y0) * (r(k, 2) * 1.2 - 0.1),
                );
                // Points, sub-texel, texel-sized and long segments, some
                // exactly horizontal or vertical.
                let len = [0.0, 3.0, 12.0, 80.0, 600.0][(k % 5) as usize] * r(k, 3);
                let angle = r(k, 4) * std::f64::consts::TAU;
                let mut b = (a.0 + len * angle.cos(), a.1 + len * angle.sin());
                match k % 7 {
                    0 => b.1 = a.1,
                    1 => b.0 = a.0,
                    _ => {}
                }
                let p = RoadModel::DEFAULT.classes[(k % 7) as usize];
                let (hw, sigma) = (f64::from(p.half_width_m), f64::from(p.sigma_m));
                fast.stamp(a, b, hw, sigma, p.intensity);
                stamp_everywhere(&mut slow, (a, b), hw, sigma, p.intensity);
            }
            assert!(fast.lit() > 0);
            assert_eq!(fast.lit(), slow.lit(), "{size} px");
            let differ = fast
                .profile
                .iter()
                .zip(&slow.profile)
                .filter(|(f, s)| f.to_bits() != s.to_bits())
                .count();
            assert_eq!(differ, 0, "{size} px");
        }
    }

    #[test]
    fn a_row_holds_the_texels_of_that_row() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = crossing_field(&n, 64);
        for i in 0..64 {
            let row = f.row(i).expect("a lit field has every row");
            assert_eq!(row.len(), 64);
            for j in 0..64 {
                assert_eq!(row[j as usize], f.at(i, j), "texel {i},{j}");
            }
        }
        assert!(RoadField::empty(64).row(0).is_none(), "nothing lit");
        assert!(f.row(64).is_none(), "past the last row");
        assert_eq!((f.size(), RoadField::empty(32).size()), (64, 32));
    }

    #[test]
    fn no_roads_is_empty() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = RoadField::raster(&n, 64, &[], &RoadModel::DEFAULT);
        assert!(f.is_empty());
        assert_eq!(f.stats.segments, 0);
        assert!(!f.allocated());
    }

    #[test]
    fn a_road_in_reach_only_allocates_when_it_can_light_something() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n);
        let lon = (f.west + f.east) / 2.0;
        // Beyond the footprint but inside the cull margin (about 170 m
        // out), and well outside any class reach.
        let y = f.north + 0.0015;
        let near = merged((46, 6), &[(100, vec![(lon - 0.01, y), (lon + 0.01, y)])]);
        let field = RoadField::raster(&n, 256, &[near], &RoadModel::DEFAULT);
        assert!(field.is_empty() && !field.allocated());
        // A road far away is culled with the whole tile.
        let far = merged((47, 6), &[(100, vec![(6.5, 47.5), (6.6, 47.5)])]);
        let field = RoadField::raster(&n, 256, &[far], &RoadModel::DEFAULT);
        assert!(field.is_empty() && !field.allocated());
        // And a road through it does allocate.
        assert!(crossing_field(&n, 64).allocated());
    }

    #[test]
    fn a_road_lights_a_line_and_fades_with_distance() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = crossing_field(&n, 256);
        let mid = 128;
        let on = f.at(mid, mid);
        assert!(on > 0.9, "core at full class intensity, got {on}");
        assert!(f.at(mid, mid + 4) < on);
        assert_eq!(f.at(mid, 0), 0.0, "far from the road");
        assert!(f.stats.lit_texels > 0);
        assert!(!f.is_empty());
    }

    #[test]
    fn parallel_duplicate_chains_are_no_brighter_than_one() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let one = crossing(&n);
        let f1 = RoadField::raster(&n, 128, &[one.clone()], &RoadModel::DEFAULT);
        let f2 = RoadField::raster(&n, 128, &[one.clone(), one], &RoadModel::DEFAULT);
        for i in 0..128 {
            for j in 0..128 {
                assert_eq!(f1.at(i, j), f2.at(i, j));
            }
        }
    }

    #[test]
    fn the_same_ground_gets_the_same_light_at_any_zoom() {
        // A ZL17 texture is a quarter of a ZL16 one; at the same metres per
        // texel (ZL16 at 512, ZL17 at 256) the overlapping texels agree.
        let n16 = name_at_zoom(46.5, 6.5, 16);
        let roads = crossing(&n16);
        let f16 = RoadField::raster(&n16, 512, &[roads.clone()], &RoadModel::DEFAULT);
        let n17 = DdsFilename {
            row: n16.row * 2,
            col: n16.col * 2,
            zoom: 17,
            ..n16.clone()
        };
        let f17 = RoadField::raster(&n17, 256, &[roads], &RoadModel::DEFAULT);
        for i in 0..256 {
            for j in 0..256 {
                assert!((f16.at(i, j) - f17.at(i, j)).abs() < 1e-4, "texel {i},{j}");
            }
        }
    }

    #[test]
    fn rural_lamps_draw_dots_not_a_line() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let roads = across(&n, 50);
        let glow = RoadField::raster(&n, 1024, &[roads.clone()], &RoadModel::DEFAULT);
        let lamp_model = RoadModel {
            rural_lamps: Some(LampParams::TEST),
            ..RoadModel::DEFAULT
        };
        let lamps = RoadField::raster(&n, 1024, &[roads], &lamp_model);
        assert!(lamps.stats.lit_texels > 0);
        assert!(lamps.stats.lit_texels < glow.stats.lit_texels);
    }

    #[test]
    fn a_road_just_outside_a_coarse_footprint_still_lights_its_border() {
        // At ZL12 and 1024 px a texel is about 100 m, so a glow reaches
        // over 300 m: a road 200 m west of the footprint lights its edge.
        let n = name_at_zoom(46.5, 6.5, 12);
        let f = Footprint::from_name(&n);
        let lat_mid = ((f.north + f.south) / 2.0).to_radians();
        let lon = f.west - (200.0 / (EARTH_RADIUS_M * lat_mid.cos())).to_degrees();
        let tile = (f.south.floor() as i32, f.west.floor() as i32);
        let road = merged(
            tile,
            &[(100, vec![(lon, f.north + 0.01), (lon, f.south - 0.01)])],
        );
        let field = RoadField::raster(&n, 1024, &[road], &RoadModel::DEFAULT);
        assert!(field.at(512, 0) > 0.0, "border texel lit");
        assert!(field.at(512, 1023) == 0.0);
    }

    #[test]
    fn the_reach_grows_with_the_texel_and_is_small_at_zl16() {
        let m = RoadModel::DEFAULT;
        let z16 = name_at_zoom(46.5, 6.5, 16);
        let z12 = name_at_zoom(46.5, 6.5, 12);
        let fine = reach_deg(&z16, 1024, &m);
        // 38 m of reach over 76 km of a degree of longitude at 46.5 N.
        assert!((0.0004..0.0007).contains(&fine), "{fine}");
        assert!(
            reach_deg(&z16, 128, &m) > fine,
            "a coarser texel reaches further"
        );
        assert!(
            reach_deg(&z12, 1024, &m) > fine,
            "a lower zoom reaches further"
        );
        let lamps = RoadModel {
            rural_lamps: Some(LampParams::TEST),
            ..m
        };
        assert!(reach_deg(&z16, 1024, &lamps) >= fine);
        // Never divides by zero near a pole.
        let polar = name_at_zoom(84.9, 6.5, 12);
        assert!(reach_deg(&polar, 64, &m).is_finite());
    }

    #[test]
    fn rural_lamps_land_on_the_same_ground_at_any_zoom() {
        let n16 = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n16);
        let w = f.east - f.west;
        // A slanted local road through the north-west quarter.
        let roads = merged(
            (46, 6),
            &[(
                50,
                vec![
                    (f.west + 0.2 * w, f.north + 0.01),
                    (f.west + 0.3 * w, f.south - 0.01),
                ],
            )],
        );
        let model = RoadModel {
            rural_lamps: Some(LampParams::TEST),
            ..RoadModel::DEFAULT
        };
        let f16 = RoadField::raster(&n16, 512, &[roads.clone()], &model);
        let n17 = DdsFilename {
            row: n16.row * 2,
            col: n16.col * 2,
            zoom: 17,
            ..n16.clone()
        };
        let f17 = RoadField::raster(&n17, 256, &[roads], &model);
        let mut lit = 0;
        for i in 0..256 {
            for j in 0..256 {
                assert!((f16.at(i, j) - f17.at(i, j)).abs() < 1e-4, "texel {i},{j}");
                lit += usize::from(f17.at(i, j) > 0.0);
            }
        }
        assert!(lit > 20, "the shared quarter holds lamps: {lit}");
    }

    #[test]
    fn a_window_that_reaches_the_grid_but_lights_nothing_leaves_no_profile() {
        // A slanted road passing north-west of the footprint: its box
        // overlaps the grid corner, the road itself is 2 km away.
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n);
        let road = merged(
            (46, 6),
            &[(
                100,
                vec![(f.west - 0.02, f.north), (f.west + 0.04, f.north + 0.06)],
            )],
        );
        let field = RoadField::raster(&n, 256, &[road], &RoadModel::DEFAULT);
        assert!(field.is_empty() && !field.allocated());
        assert_eq!(field.stats.segments, 1);
    }

    #[test]
    fn motorways_stay_glowing_under_rural_lamps() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let lamp_model = RoadModel {
            rural_lamps: Some(LampParams::TEST),
            ..RoadModel::DEFAULT
        };
        let f = RoadField::raster(&n, 256, &[crossing(&n)], &lamp_model);
        assert!(f.at(128, 128) > 0.9);
        assert!(f.at(10, 128) > 0.9, "lit along the whole line, not in dots");
    }

    #[test]
    fn a_chain_outside_the_footprint_leaves_it_dark() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n);
        let lon = (f.west + f.east) / 2.0;
        let y = f.north + 0.05;
        let r = merged((46, 6), &[(100, vec![(lon - 0.05, y), (lon + 0.05, y)])]);
        assert!(RoadField::raster(&n, 64, &[r], &RoadModel::DEFAULT).is_empty());
    }

    #[test]
    fn a_class_with_more_intensity_is_brighter() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let at = |subtype| {
            RoadField::raster(&n, 256, &[across(&n, subtype)], &RoadModel::DEFAULT).at(128, 128)
        };
        assert!(at(100) > at(50));
        assert!(at(50) > at(75));
        assert!(at(75) > 0.0);
    }

    #[test]
    fn segments_and_texels_are_counted() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n);
        let lon = (f.west + f.east) / 2.0;
        let mid = (f.north + f.south) / 2.0;
        let r = merged(
            (46, 6),
            &[(
                100,
                vec![(lon, f.north + 0.01), (lon, mid), (lon, f.south - 0.01)],
            )],
        );
        let field = RoadField::raster(&n, 64, &[r], &RoadModel::DEFAULT);
        assert_eq!(field.stats.segments, 2);
        assert!(field.stats.lit_texels >= 64);
    }

    /// A street grid over `[south, north] x [west, east]` (tile-relative
    /// degrees) with one east-west and one north-south chain of two to four
    /// points per `pitch_m` cell, classes mixed by a hash of the cell. A
    /// chain gets one more interior point with a chance of `bends_pct` per cent.
    fn street_grid(
        c: &mut Chains,
        (south, north): (f32, f32),
        (west, east): (f32, f32),
        pitch_m: f64,
        bends_pct: u64,
    ) {
        use crate::night::generator::noise::hash2;
        const SUBTYPES: [u8; 10] = [100, 120, 10, 30, 50, 50, 50, 70, 70, 75];
        let dlat = (pitch_m / 111_320.0) as f32;
        let dlon = (pitch_m / (111_320.0 * 46.5f64.to_radians().cos())) as f32;
        let (mut i, mut lat) = (0i64, south);
        while lat < north {
            let (mut j, mut lon) = (0i64, west);
            while lon < east {
                let h = hash2(i, j, 7);
                let subtype = SUBTYPES[(h % 10) as usize];
                // East-west, with a kink and a third point on some cells.
                let mut ew = vec![(lon, lat), (lon + dlon, lat)];
                if (h >> 8) % 3 == 0 {
                    ew.insert(1, (lon + dlon * 0.4, lat + dlat * 0.05));
                }
                if (h >> 16) % 5 == 0 {
                    ew.insert(1, (lon + dlon * 0.2, lat - dlat * 0.05));
                }
                let mut ns = vec![(lon, lat), (lon, lat + dlat)];
                if (h >> 24) % 100 < bends_pct {
                    ew.insert(1, (lon + dlon * 0.6, lat));
                }
                if (h >> 32) % 100 < bends_pct {
                    ns.insert(1, (lon + dlon * 0.02, lat + dlat * 0.5));
                }
                c.push(subtype, &ew);
                c.push(subtype, &ns);
                j += 1;
                lon += dlon;
            }
            i += 1;
            lat += dlat;
        }
    }

    /// Release-build timing of a dense urban texture; run with
    /// `cargo test --release -p xearthlayer --lib road_field_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn road_field_timing() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n);
        let mut c = Chains::new();
        // The whole tile at a 207 m pitch (about 400,000 chains) ...
        street_grid(&mut c, (0.0, 1.0), (0.0, 1.0), 207.0, 0);
        // ... and a dense core over the footprint at an 80 m pitch.
        let rel = |v: f64, o: i32| (v - f64::from(o)) as f32;
        street_grid(
            &mut c,
            (rel(f.south, 46), rel(f.north, 46)),
            (rel(f.west, 6), rel(f.east, 6)),
            80.0,
            0,
        );
        let chains = c.len();
        let tile = merge_tile((46, 6), c);
        println!("chains in the tile: {chains}");
        println!(
            "merged tile: {} bytes, {:.1} per chain",
            tile.bytes(),
            tile.bytes() as f64 / chains as f64
        );
        for (label, model) in [
            ("glow", RoadModel::DEFAULT),
            (
                "lamps",
                RoadModel {
                    rural_lamps: Some(LampParams::TEST),
                    ..RoadModel::DEFAULT
                },
            ),
        ] {
            for pass in ["cold", "warm"] {
                let started = std::time::Instant::now();
                let field = RoadField::raster(&n, 1024, &[tile.clone()], &model);
                let ms = started.elapsed().as_secs_f64() * 1e3;
                println!(
                    "ZL16 1024 px {label} {pass}: {ms:.1} ms, segments {}, lit texels {}",
                    field.stats.segments, field.stats.lit_texels
                );
                assert!(!field.is_empty());
            }
        }
    }

    /// A Tokyo-sized tile (about 1.93 million chains, about 5.9 million
    /// points): what the merged tile holds and how long a texture takes.
    #[test]
    #[ignore]
    fn merged_roads_of_a_tokyo_sized_tile() {
        let n = name_at_zoom(35.7, 139.7, 16);
        let mut c = Chains::new();
        street_grid(&mut c, (0.0, 1.0), (0.0, 1.0), 94.0, 80);
        let (chains, points) = (c.len(), c.lon.len());
        let before = points * 8 + (chains + 1) * 4 + chains * 18;
        let tile = merge_tile((35, 139), c);
        println!(
            "{chains} chains, {points} points: {} bytes now, {before} with f32 points and boxes",
            tile.bytes()
        );
        for pass in ["cold", "warm"] {
            let field = RoadField::raster(&n, 1024, &[tile.clone()], &RoadModel::DEFAULT);
            println!(
                "ZL16 1024 px {pass}: {:.1} ms, segments {}, lit texels {}",
                field.stats.raster_us as f64 / 1e3,
                field.stats.segments,
                field.stats.lit_texels
            );
        }
    }
}
