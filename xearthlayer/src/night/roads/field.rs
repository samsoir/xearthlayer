//! Per-footprint road light: for each texel the brightest profile of any
//! nearby lamp (or, without lamps, segment glow), in 0..=1 (class
//! intensity included). Light comes from geometry in metres, so it is
//! world-anchored at every zoom.
//!
//! Cost: a merged tile hands over only the chains filed in cells near the
//! footprint (no test per chain of the tile), a point's northing is a
//! table lookup, and a segment visits only the texels in its reach window,
//! found from the even texel spacing. The
//! texel loop has no branch and no table (the fall-off is a few multiplies
//! in `f32`, the reach test a select), so the compiler vectorises it;
//! everything that varies only by row is computed once per row, with no
//! division. Lit texels are counted once, after drawing. The profile (one
//! `f32` per texel) is allocated when the first segment reaches the grid,
//! so a texture with no road nearby costs no heap at all.

use std::sync::Arc;
use std::time::Instant;

use super::merge::{ChainRef, MergedRoads};
use super::model::{ClassParams, LampParams, RoadModel};
use crate::fuse::DdsFilename;
use crate::night::footprint::{Footprint, TexelCentres, TILES_PER_TEXTURE};
use crate::night::generator::noise::{hash2, unit};
use crate::night::generator::tiling::{mercator, EARTH_RADIUS_M};

/// The cosine of the highest latitude [`reach_deg`] divides by.
const COS_REACH_FLOOR: f64 = 0.087_155_742_747_658_17; // cos 85 degrees

/// How far outside the footprint of `name` a road can still light a texel
/// of a `size` x `size` raster, in degrees, for either axis: the widest
/// reach of any class (half width + 3 sigma, with the sigma never below a
/// texel's ground size) over the least metres per degree (longitude, at the
/// footprint's highest latitude). A lamp is a class glow without its core,
/// so lamps reach further only by a sigma floor wider than every class's.
/// Cheap and allocation free: only the footprint's edges are needed. The
/// raster culls with it and the has-light probe must use it too, so the two
/// cannot disagree.
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
        .lamps
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
            // The chains near the footprint and its reach, in the tile's
            // own longitudes; a tile across the antimeridian is drawn
            // `shift` degrees over, beside the footprint.
            let RoadWindow {
                shift,
                south,
                west,
                north,
                east,
            } = RoadWindow::new(&fp, reach, tile.tile.1);
            let mut northings: Option<Northings> = None;
            tile.for_each_chain_near(south, west, north, east, |chain| {
                let canvas = canvas.get_or_insert_with(|| Canvas::new(Grid::new(name, size)));
                let ys = northings.get_or_insert_with(|| Northings::new(&chain, south, north));
                let p = model.classes[chain.class().index()];
                pts.clear();
                pts.extend(
                    chain
                        .points()
                        .zip(chain.lat_steps())
                        .map(|((lon, lat), &step)| {
                            (mercator(0.0, lon + shift).0, ys.at(step, lat))
                        }),
                );
                segments += pts.len().saturating_sub(1);
                match model.lamps {
                    None => canvas.draw_glow(&pts, p),
                    Some(l) => {
                        let first = chain.points().next().unwrap_or_default();
                        canvas.draw_lamps(&pts, chain_key(first), &l, p);
                    }
                }
            });
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

/// The Web Mercator northings of a tile's quantised latitudes over a
/// texture's footprint and a margin, so a point's northing is a lookup
/// instead of a logarithm. A latitude outside the table is computed; both
/// give `mercator(lat, _).1` of the same decoded latitude, so the table
/// never changes a result.
struct Northings {
    lo: usize,
    ys: Vec<f64>,
}

/// Steps either side of the footprint the table also covers: two cells,
/// so a chain found from a neighbouring cell is still a lookup.
const NORTHING_MARGIN: f64 = 512.0;

impl Northings {
    /// The table for latitudes `south..=north` of `chain`'s tile.
    fn new(chain: &ChainRef<'_>, south: f64, north: f64) -> Self {
        let step = |lat: f64| chain.step_of_lat(lat);
        let last = f64::from(u16::MAX);
        let lo = (step(south) - NORTHING_MARGIN).clamp(0.0, last) as usize;
        let hi = (step(north) + NORTHING_MARGIN).clamp(0.0, last) as usize;
        let ys = (lo..=hi)
            .map(|s| mercator(chain.lat_of(s as u16), 0.0).1)
            .collect();
        Self { lo, ys }
    }

    /// The northing of quantised latitude `step`, which decodes to `lat`.
    fn at(&self, step: u16, lat: f64) -> f64 {
        match self.ys.get(usize::from(step).wrapping_sub(self.lo)) {
            Some(&y) => y,
            None => mercator(lat, 0.0).1,
        }
    }
}

/// The lamp key of a chain: its first point `(lon, lat)` quantised to 1e-6
/// degrees. A chain the merge stores as pieces (more than 32 points)
/// starts a walk at each joint: still the same lamps in every texture.
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

/// The texel centres of a texture ([`TexelCentres`], the ones
/// `CpuLitGenerator::generate` samples) and what the raster derives from
/// them.
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
        let TexelCentres { lats, xs, ys, .. } = TexelCentres::of(name, size);
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

/// Hands `f` the lamps of a polyline (Mercator metres), in order: one at
/// every `spacing_m` of ground arc length, each lit or not by a hash of
/// the chain's key and its index.
fn lamp_positions(pts: &[(f64, f64)], key: i64, l: &LampParams, mut f: impl FnMut((f64, f64))) {
    let spacing = f64::from(l.spacing_m).max(MIN_LAMP_SPACING_M);
    let lit_share = f64::from(l.lit_share);
    let (mut along, mut next, mut k) = (0.0f64, 0.0f64, 0i64);
    for w in pts.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        // Not `hypot`: linked against glibc 2.35 it needs `hypot@GLIBC_2.35`,
        // above the release's glibc floor (clippy.toml). Mercator metres are
        // far from overflowing when squared.
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt() * cos_lat_of(y0);
        while next <= along + len {
            if unit(hash2(key, k, LAMP_SEED)) < lit_share {
                let t = if len > 0.0 { (next - along) / len } else { 0.0 };
                f((x0 + t * (x1 - x0), y0 + t * (y1 - y0)));
            }
            k += 1;
            next += spacing;
        }
        along += len;
    }
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

    /// The lamps of a polyline (Mercator metres) of class `p`, each a point
    /// glow with the class sigma (or the lamp floor, when wider) at the
    /// class intensity scaled by the lamp's.
    fn draw_lamps(&mut self, pts: &[(f64, f64)], key: i64, l: &LampParams, p: ClassParams) {
        let sigma = f64::from(l.sigma_m.max(p.sigma_m));
        let intensity = p.intensity * l.intensity;
        lamp_positions(pts, key, l, |at| self.stamp(at, at, 0.0, sigma, intensity));
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
/// any texture, up to rounding: the squarings multiply a one-ULP change of
/// the first term (2^-24 just below 1) by about 1024, so an output can
/// move by up to about 6.1e-5 when the `f32` distance moves by one ULP.
/// Two textures whose `f64` squared distances for the same ground round to
/// neighbouring `f32` values can therefore differ by that much, about 60%
/// of the 1e-4 the zoom-agreement tests allow.
fn falloff(u: f32) -> f32 {
    let u = u.clamp(0.0, 3.0);
    let mut e = 1.0 - u * u * (0.5 / (1u32 << FALLOFF_SQUARINGS) as f32);
    for _ in 0..FALLOFF_SQUARINGS {
        e *= e;
    }
    e
}

/// Degrees to add to the longitudes of tile `lon0`'s roads to draw them
/// beside `fp`: 0, or 360 either way for a tile across the antimeridian
/// (a footprint by the line asks for the wrapped tile across it).
///
/// It relies on a footprint never spanning the line itself, which holds
/// for every Mercator texture name (a texture's columns stop at 180): the
/// footprint's middle is then on one side, and the shift brings the tile
/// to that side.
pub(crate) fn antimeridian_shift(lon0: i32, fp: &Footprint) -> f64 {
    let middle = (fp.west + fp.east) / 2.0;
    360.0 * ((middle - (f64::from(lon0) + 0.5)) / 360.0).round()
}

/// The box a road of tile `lon0` must meet to light a footprint: the
/// footprint grown by the glow reach, in absolute degrees of the tile's
/// own longitudes (shifted by [`antimeridian_shift`] for a tile across the
/// line), and the shift that draws the tile's roads beside the footprint.
/// The raster culls with it and the has-light probe asks
/// [`crosses_footprint`], so the two cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RoadWindow {
    pub shift: f64,
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

impl RoadWindow {
    pub(crate) fn new(fp: &Footprint, reach: f64, lon0: i32) -> Self {
        let shift = antimeridian_shift(lon0, fp);
        Self {
            shift,
            south: fp.south - reach,
            west: fp.west - reach - shift,
            north: fp.north + reach,
            east: fp.east + reach - shift,
        }
    }
}

/// Whether a road of `roads` lies within `reach` degrees of `fp`, across
/// the antimeridian too: the has-light probe's road test, over the box the
/// raster culls with ([`RoadWindow`]).
pub fn crosses_footprint(roads: &MergedRoads, fp: &Footprint, reach: f64) -> bool {
    let w = RoadWindow::new(fp, reach, roads.tile.1);
    roads.crosses(w.south, w.west, w.north, w.east)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fuse::TextureKind;
    use crate::night::roads::class::RoadClass;
    use crate::night::roads::dsf::{Chains, DsfRoads};
    use crate::night::roads::merge::Merger;
    use crate::night::roads::model::LampParams;
    use crate::night::Footprint;

    /// The default model with the continuous glow instead of lamps.
    const GLOW: RoadModel = RoadModel {
        lamps: None,
        ..RoadModel::DEFAULT
    };

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
    fn roads_across_the_antimeridian_light_the_texture_at_it() {
        // The ZL16 texture whose east edge is the line, at Taveuni, and a
        // road 30 m past the line, in tile (-17, -180).
        let mut name = name_at_zoom(-16.8, 179.95, 16);
        name.col = (1 << 16) - 16;
        let f = Footprint::from_name(&name);
        assert_eq!(f.east, 180.0);
        let road = |lon: f64| vec![(100, vec![(lon, f.north + 0.01), (lon, f.south - 0.01)])];
        let across = merged((-17, -180), &road(-179.999_7));
        let field = RoadField::raster(&name, 256, &[across], &RoadModel::DEFAULT);
        assert!(field.stats.lit_texels > 0, "the glow crosses the line");
        // The texels it lights are those a road 30 m inside the line lights
        // in their mirror image: the east columns, not the west ones.
        let lit_cols = |field: &RoadField| {
            (0..256u32)
                .filter_map(|i| field.row(i))
                .flat_map(|row| row.iter().enumerate().filter(|(_, &v)| v > 0.0))
                .map(|(j, _)| j)
                .collect::<Vec<_>>()
        };
        assert!(lit_cols(&field).iter().all(|&j| j >= 192));
        // The westmost texture is lit by a road just across in tile 179.
        name.col = 0;
        let f = Footprint::from_name(&name);
        let road = vec![(
            100,
            vec![(179.999_7, f.north + 0.01), (179.999_7, f.south - 0.01)],
        )];
        let across = merged((-17, 179), &road);
        let field = RoadField::raster(&name, 256, &[across], &RoadModel::DEFAULT);
        assert!(field.stats.lit_texels > 0);
        assert!(lit_cols(&field).iter().all(|&j| j < 64));
    }

    /// A footprint with the given edges, at zoom 16.
    fn edges(south: f64, north: f64, west: f64, east: f64) -> Footprint {
        Footprint {
            north,
            south,
            west,
            east,
            zoom: 16,
        }
    }

    #[test]
    fn the_antimeridian_shift_brings_a_tile_to_the_footprints_side() {
        // A footprint whose east edge is the line.
        let east = edges(-16.7, -16.6, 179.9945, 180.0);
        assert_eq!(antimeridian_shift(179, &east), 0.0, "its own tile");
        assert_eq!(antimeridian_shift(-180, &east), 360.0, "the tile across");
        // A footprint whose west edge is the line.
        let west = edges(-16.7, -16.6, -180.0, -179.9945);
        assert_eq!(antimeridian_shift(-180, &west), 0.0, "its own tile");
        assert_eq!(antimeridian_shift(179, &west), -360.0, "the tile across");
        // Away from the line, no tile is shifted.
        let geneva = edges(46.2, 46.21, 6.14, 6.15);
        for lon0 in [5, 6, 7] {
            assert_eq!(antimeridian_shift(lon0, &geneva), 0.0, "{lon0}");
        }
        // Nor at the prime meridian.
        let greenwich = edges(51.47, 51.48, -0.005, 0.0);
        assert_eq!(antimeridian_shift(-1, &greenwich), 0.0);
        assert_eq!(antimeridian_shift(0, &greenwich), 0.0);
    }

    #[test]
    fn the_road_window_is_the_grown_footprint_in_the_tiles_longitudes() {
        let east = edges(-16.7, -16.6, 179.9945, 180.0);
        assert_eq!(
            RoadWindow::new(&east, 0.01, 179),
            RoadWindow {
                shift: 0.0,
                south: -16.71,
                west: 179.9945 - 0.01,
                north: -16.59,
                east: 180.01,
            }
        );
        // Across the line the window sits at the tile's west edge.
        let w = RoadWindow::new(&east, 0.01, -180);
        assert_eq!(w.shift, 360.0);
        assert_eq!((w.south, w.north), (-16.71, -16.59));
        assert!((w.west - (179.9945 - 0.01 - 360.0)).abs() < 1e-9, "{w:?}");
        assert!((w.east - (-180.0 + 0.01)).abs() < 1e-9, "{w:?}");
        // And the other way, at the tile's east edge.
        let west = edges(-16.7, -16.6, -180.0, -179.9945);
        let w = RoadWindow::new(&west, 0.01, 179);
        assert_eq!(w.shift, -360.0);
        assert!((w.west - (180.0 - 0.01)).abs() < 1e-9, "{w:?}");
        assert!((w.east - (-179.9945 + 0.01 + 360.0)).abs() < 1e-9, "{w:?}");
    }

    /// The ZL16 textures by the line at Taveuni: the one whose east edge is
    /// the line, and the one whose west edge is.
    fn textures_at_the_antimeridian() -> (Footprint, Footprint) {
        let mut name = name_at_zoom(-16.8, 179.95, 16);
        name.col = (1 << 16) - 16;
        let east = Footprint::from_name(&name);
        name.col = 0;
        let west = Footprint::from_name(&name);
        assert_eq!((east.east, west.west), (180.0, -180.0));
        (east, west)
    }

    /// A north-south road at `lon` in `tile`, past `fp`'s north and south.
    fn road_past(tile: (i32, i32), fp: &Footprint, lon: f64) -> Arc<MergedRoads> {
        merged(
            tile,
            &[(100, vec![(lon, fp.north + 0.01), (lon, fp.south - 0.01)])],
        )
    }

    #[test]
    fn given_a_road_just_across_the_line_the_footprint_is_crossed() {
        let (east, west) = textures_at_the_antimeridian();
        // 30 m east of the line, in tile -180; 30 m west of it, in 179.
        let reach = 0.001;
        assert!(crosses_footprint(
            &road_past((-17, -180), &east, -179.999_7),
            &east,
            reach
        ));
        assert!(crosses_footprint(
            &road_past((-17, 179), &west, 179.999_7),
            &west,
            reach
        ));
    }

    #[test]
    fn given_no_road_near_the_line_the_footprint_is_not_crossed() {
        let (east, west) = textures_at_the_antimeridian();
        let reach = 0.001;
        // Roads across the line but beyond the reach.
        assert!(!crosses_footprint(
            &road_past((-17, -180), &east, -179.9),
            &east,
            reach
        ));
        assert!(!crosses_footprint(
            &road_past((-17, 179), &west, 179.9),
            &west,
            reach
        ));
        // No road at all in the tile across.
        assert!(!crosses_footprint(&merged((-17, -180), &[]), &east, reach));
        assert!(!crosses_footprint(&merged((-17, 179), &[]), &west, reach));
    }

    #[test]
    fn the_northing_table_equals_the_projection_at_every_step() {
        // Footprints inside the tile, on its edges and past them.
        for (tile, south, north) in [
            ((46, 6), 46.4, 46.43),
            ((46, 6), 45.9, 46.01),
            ((46, 6), 46.99, 47.2),
            ((-34, -71), -33.5, -33.2),
        ] {
            let roads = merged(
                tile,
                &[(
                    100,
                    vec![
                        (f64::from(tile.1) + 0.5, south),
                        (f64::from(tile.1) + 0.5, north),
                    ],
                )],
            );
            let (lat0, lon0) = (f64::from(tile.0), f64::from(tile.1));
            let mut checked = 0;
            roads.for_each_chain_near(lat0, lon0, lat0 + 1.0, lon0 + 1.0, |chain| {
                let table = Northings::new(&chain, south, north);
                assert!(!table.ys.is_empty());
                for step in 0..=u16::MAX {
                    let lat = chain.lat_of(step);
                    let want = mercator(lat, 0.0).1;
                    assert_eq!(
                        table.at(step, lat).to_bits(),
                        want.to_bits(),
                        "{tile:?} step {step}"
                    );
                }
                checked += 1;
            });
            assert_eq!(checked, 1);
        }
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
    fn the_reach_window_misses_no_texel_the_whole_grid_would_light() {
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

    /// One mark of the reference raster: a segment `a` to `b` (Mercator
    /// metres, a point when equal) and its glow.
    struct Mark {
        a: (f64, f64),
        b: (f64, f64),
        half_width: f64,
        sigma_m: f64,
        intensity: f64,
    }

    /// A raster written apart from the fast path, in plain `f64`: texel
    /// centres and Web Mercator from their formulas, every mark tested
    /// against every texel whose centre lies within its reach box, the exact
    /// distance and `exp`. Only lamp placement is shared (placement is not
    /// what it checks). Returns the profile and, per texel, whether some
    /// mark's distance is within 1e-6 relative of its reach (the cut).
    fn reference_raster(
        name: &DdsFilename,
        size: u32,
        roads: &MergedRoads,
        model: &RoadModel,
    ) -> (Vec<f64>, Vec<bool>) {
        use std::f64::consts::{FRAC_PI_4, PI, TAU};
        let r = EARTH_RADIUS_M;
        let n = f64::from(1u32 << name.zoom);
        let side = f64::from(TILES_PER_TEXTURE);
        let px = f64::from(size);
        let project = |lon: f64, lat: f64| {
            (
                r * lon.to_radians(),
                r * (FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln(),
            )
        };
        // The marks of every chain of the tile.
        let mut marks = Vec::new();
        let (lat0, lon0) = (f64::from(roads.tile.0), f64::from(roads.tile.1));
        roads.for_each_chain_near(lat0, lon0, lat0 + 1.0, lon0 + 1.0, |chain| {
            let deg: Vec<(f64, f64)> = chain.points().collect();
            let pts: Vec<(f64, f64)> = deg.iter().map(|&(x, y)| project(x, y)).collect();
            let p = model.classes[chain.class().index()];
            match model.lamps {
                // A point glow per lamp, the class's sigma unless the lamp
                // floor is wider, at the class intensity scaled by the lamp's.
                Some(l) => lamp_positions(&pts, chain_key(deg[0]), &l, |at| {
                    marks.push(Mark {
                        a: at,
                        b: at,
                        half_width: 0.0,
                        sigma_m: f64::from(l.sigma_m).max(f64::from(p.sigma_m)),
                        intensity: f64::from(p.intensity) * f64::from(l.intensity),
                    })
                }),
                None => {
                    for w in pts.windows(2) {
                        marks.push(Mark {
                            a: w[0],
                            b: w[1],
                            half_width: f64::from(p.half_width_m),
                            sigma_m: f64::from(p.sigma_m),
                            intensity: f64::from(p.intensity),
                        });
                    }
                }
            }
        });
        let size = size as usize;
        let (mut out, mut edge) = (vec![0.0f64; size * size], vec![false; size * size]);
        // Column of a Mercator easting, unrounded (centres at whole values).
        let col_of = |x: f64| ((x / r + PI) / TAU * n - f64::from(name.col)) * px / side - 0.5;
        for i in 0..size {
            let lat = (PI * (1.0 - 2.0 * (f64::from(name.row) + (i as f64 + 0.5) / px * side) / n))
                .sinh()
                .atan();
            let y = r * (FRAC_PI_4 + lat / 2.0).tan().ln();
            let cos = lat.cos();
            let texel = r * TAU * side / (n * px) * cos;
            for m in &marks {
                let sigma = m.sigma_m.max(texel);
                let reach = m.half_width + 3.0 * sigma;
                let reach_mercator = reach / cos;
                if y < m.a.1.min(m.b.1) - reach_mercator || y > m.a.1.max(m.b.1) + reach_mercator {
                    continue;
                }
                let lo = col_of(m.a.0.min(m.b.0) - reach_mercator).ceil().max(0.0) as usize;
                let hi = col_of(m.a.0.max(m.b.0) + reach_mercator).floor();
                if hi < 0.0 {
                    continue;
                }
                for j in lo..=(hi as usize).min(size - 1) {
                    let lon = (f64::from(name.col) + (j as f64 + 0.5) / px * side) / n * TAU - PI;
                    let x = r * lon;
                    let (vx, vy) = (m.b.0 - m.a.0, m.b.1 - m.a.1);
                    let len2 = vx * vx + vy * vy;
                    let t = if len2 > 0.0 {
                        (((x - m.a.0) * vx + (y - m.a.1) * vy) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let (ex, ey) = (x - (m.a.0 + t * vx), y - (m.a.1 + t * vy));
                    let d = (ex * ex + ey * ey).sqrt() * cos;
                    let v = if d <= m.half_width {
                        m.intensity
                    } else if d <= reach {
                        let u = (d - m.half_width) / sigma;
                        m.intensity * (-u * u / 2.0).exp()
                    } else {
                        0.0
                    };
                    let k = i * size + j;
                    out[k] = out[k].max(v);
                    edge[k] |= (d - reach).abs() <= 1e-6 * reach;
                }
            }
        }
        (out, edge)
    }

    /// `count` random chains of `subtypes` over the footprint of `name`
    /// and a little beyond: points, short and long segments, some with a
    /// third point.
    fn random_roads(
        name: &DdsFilename,
        subtypes: &[u8],
        count: i64,
        seed: u64,
    ) -> Arc<MergedRoads> {
        use crate::night::generator::noise::{hash2, unit};
        let f = Footprint::from_name(name);
        let (w, h) = (f.east - f.west, f.north - f.south);
        let roads: Vec<(u8, Vec<(f64, f64)>)> = (0..count)
            .map(|k| {
                let u = |m: i64| unit(hash2(k, m, seed));
                let at = |m: i64| {
                    (
                        f.west + w * (u(m) * 1.2 - 0.1),
                        f.south + h * (u(m + 1) * 1.2 - 0.1),
                    )
                };
                let len = [0.0, 0.02, 0.1, 0.5][(k % 4) as usize] * w;
                let angle = u(5) * std::f64::consts::TAU;
                let a = at(1);
                let mut pts = vec![a, (a.0 + len * angle.cos(), a.1 + len * angle.sin())];
                if k % 3 == 0 {
                    pts.push(at(3));
                }
                (subtypes[(k as usize) % subtypes.len()], pts)
            })
            .collect();
        let f0 = Footprint::from_name(name);
        merged((f0.south.floor() as i32, f0.west.floor() as i32), &roads)
    }

    #[test]
    fn the_raster_matches_an_independent_reference_in_plain_f64() {
        let sparse = RoadModel {
            lamps: Some(LampParams::TEST),
            ..RoadModel::DEFAULT
        };
        // Each class alone in the continuous glow, so the tolerance is its
        // own intensity, then the default lamps on every glowing class, and
        // sparse lamps whose sigma floor is wider than some classes'.
        let mut cases: Vec<(Vec<u8>, RoadModel)> = [100u8, 120, 10, 30, 50, 70, 75]
            .into_iter()
            .map(|s| (vec![s], GLOW))
            .collect();
        cases.push((vec![100, 120, 10, 30], RoadModel::DEFAULT));
        cases.push((vec![100, 120, 10, 30, 50], sparse));
        // Texels from 3.3 m to 13 m: below and above every class sigma.
        for (zoom, size) in [(17u8, 256u32), (17, 512), (18, 256), (18, 512)] {
            let name = name_at_zoom(46.5, 6.5, zoom);
            for (c, (subtypes, model)) in cases.iter().enumerate() {
                let roads = random_roads(&name, subtypes, 40, 11 + c as u64);
                let fast = RoadField::raster(&name, size, std::slice::from_ref(&roads), model);
                let (want, edge) = reference_raster(&name, size, &roads, model);
                let intensity = subtypes
                    .iter()
                    .map(|&s| {
                        let class = RoadClass::of(s).unwrap();
                        model.classes[class.index()].intensity
                    })
                    .fold(0.0f32, f32::max)
                    * model.lamps.map_or(1.0, |l| l.intensity);
                let tolerance = 3e-4 * f64::from(intensity) + 1e-6;
                let (mut lit, mut edges, mut worst) = (0, 0, 0.0f64);
                for i in 0..size {
                    for j in 0..size {
                        let k = (i * size + j) as usize;
                        if edge[k] {
                            edges += 1;
                            continue;
                        }
                        let got = f64::from(fast.at(i, j));
                        let diff = (got - want[k]).abs();
                        worst = worst.max(diff);
                        assert!(
                            diff <= tolerance,
                            "ZL{zoom} {size} px case {c} texel {i},{j}: fast {got}, reference {}",
                            want[k]
                        );
                        assert_eq!(
                            got > 0.0,
                            want[k] > 0.0,
                            "ZL{zoom} {size} px case {c} texel {i},{j}: lit"
                        );
                        lit += usize::from(want[k] > 0.0);
                    }
                }
                let texels = (size * size) as usize;
                assert!(
                    lit > texels / 50,
                    "ZL{zoom} {size} px case {c}: only {lit} lit"
                );
                assert!(
                    edges < texels / 1000,
                    "ZL{zoom} {size} px case {c}: {edges} at the cut"
                );
                assert!(worst > 0.0 || lit == 0);
            }
        }
    }

    #[test]
    fn the_field_is_the_same_in_both_tile_layouts() {
        // A street grid over the texture, and long diagonals that span many
        // cells (the boxed list of an indexed tile).
        let name = name_at_zoom(46.2, 6.15, 16);
        let f = Footprint::from_name(&name);
        let tile = (46, 6);
        let rel = |v: f64, o: i32| (v - f64::from(o)) as f32;
        let mut c = Chains::new();
        street_grid(
            &mut c,
            (rel(f.south, tile.0), rel(f.north, tile.0)),
            (rel(f.west, tile.1), rel(f.east, tile.1)),
            120.0,
            30,
        );
        let (s, w) = (rel(f.south - 0.05, 46), rel(f.west - 0.05, 6));
        let (n, e) = (rel(f.north + 0.05, 46), rel(f.east + 0.05, 6));
        c.push(100, &[(w, s), (e, n)]);
        c.push(30, &[(w, n), (e, s)]);
        let layouts = [false, true].map(|index| {
            let mut m = Merger::new(tile);
            m.add(&DsfRoads {
                tile,
                overlay: true,
                exclusions: vec![],
                chains: c.clone(),
            });
            Arc::new(m.finish_with(index))
        });
        let size = 256;
        let [a, b] = layouts.map(|r| RoadField::raster(&name, size, &[r], &RoadModel::DEFAULT));
        assert!(a.stats.lit_texels > size as usize * 8, "{:?}", a.stats);
        assert_eq!(a.stats.lit_texels, b.stats.lit_texels);
        for row in 0..size {
            assert_eq!(a.row(row), b.row(row), "row {row}");
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
        let f = RoadField::raster(&n, 256, &[crossing(&n)], &GLOW);
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

    /// FNV-1a over the bits of every texel of `f`.
    fn field_digest(f: &RoadField) -> u64 {
        (0..f.size())
            .flat_map(|i| (0..f.size()).map(move |j| (i, j)))
            .fold(0xcbf2_9ce4_8422_2325u64, |h, (i, j)| {
                f.at(i, j)
                    .to_bits()
                    .to_le_bytes()
                    .iter()
                    .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
            })
    }

    #[test]
    fn without_lamps_roads_get_the_continuous_glow_byte_for_byte() {
        // The digest of the continuous glow as it was drawn before lamps
        // became the default (cf0e147): a street grid of every class and
        // a motorway across it.
        let n = name_at_zoom(46.5, 6.5, 16);
        let roads = [street_grid_over(&n, 150.0), crossing(&n)];
        let f = RoadField::raster(&n, 512, &roads, &GLOW);
        assert!(f.stats.lit_texels > 512 * 8, "{:?}", f.stats);
        assert_eq!(
            field_digest(&f),
            0x5aca_5cf7_6567_3b7e,
            "{:016x}",
            field_digest(&f)
        );
    }

    #[test]
    fn every_glowing_class_is_drawn_as_a_chain_of_lamps() {
        let n = name_at_zoom(46.5, 6.5, 16);
        let size = 1024;
        for subtype in [100u8, 120, 10, 30] {
            let class = RoadClass::of(subtype).unwrap();
            let intensity = RoadModel::DEFAULT.classes[class.index()].intensity;
            let roads = [across(&n, subtype)];
            let glow = RoadField::raster(&n, size, &roads, &GLOW);
            let lamps = RoadField::raster(&n, size, &roads, &RoadModel::DEFAULT);
            // The road runs north to south: the brightest texel of each row,
            // away from the edges.
            let peaks: Vec<f32> = (8..size - 8)
                .map(|i| (0..size).map(|j| lamps.at(i, j)).fold(0.0, f32::max))
                .collect();
            let hi = peaks.iter().copied().fold(0.0, f32::max);
            let lo = peaks.iter().copied().fold(f32::MAX, f32::min);
            assert!(hi > 0.8 * intensity, "{class:?}: a lamp at {hi}");
            assert!(lo < 0.7 * hi, "{class:?}: {lo} between lamps, {hi} on one");
            assert!(lamps.stats.lit_texels < glow.stats.lit_texels, "{class:?}");
        }
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
        // A lamp is as wide as its class's glow without the core, unless
        // its sigma floor is wider than every class reach.
        assert_eq!(reach_deg(&z16, 1024, &GLOW), fine);
        let wide = RoadModel {
            lamps: Some(LampParams {
                sigma_m: 40.0,
                ..LampParams::DEFAULT
            }),
            ..m
        };
        assert!(reach_deg(&z16, 1024, &wide) > fine);
        // Never divides by zero near a pole.
        let polar = name_at_zoom(84.9, 6.5, 12);
        assert!(reach_deg(&polar, 64, &m).is_finite());
    }

    #[test]
    fn a_lamp_has_its_class_sigma_unless_its_floor_is_wider() {
        // ZL18 at 512 px: 3.3 m texels, below the class sigma of 8 m.
        let n = name_at_zoom(46.5, 6.5, 18);
        let p = RoadModel::DEFAULT.classes[RoadClass::Secondary.index()];
        // One lamp (the chain is shorter than the spacing) on a texel centre:
        // its value there and three texels east, and that distance.
        let draw = |l: LampParams| {
            let mut c = Canvas::new(Grid::new(&n, 512));
            let at = (c.grid.xs[256], c.grid.ys[256]);
            c.draw_lamps(&[at, at], 0, &l, p);
            let metres = (c.grid.xs[259] - c.grid.xs[256]) * c.grid.cos[256];
            let row = &c.profile[256 * 512..257 * 512];
            (row[256], f64::from(row[259]), metres)
        };
        let lamp = LampParams {
            spacing_m: 1000.0,
            lit_share: 1.0,
            sigma_m: 2.0,
            intensity: 0.5,
        };
        let (peak, side, metres) = draw(lamp);
        assert_eq!(peak, p.intensity * 0.5, "class intensity times the lamp's");
        let want = |sigma: f64| f64::from(peak) * (-(metres / sigma).powi(2) / 2.0).exp();
        assert!((side - want(8.0)).abs() < 1e-3, "{side} for {}", want(8.0));
        let (_, wide, _) = draw(LampParams {
            sigma_m: 20.0,
            ..lamp
        });
        assert!(
            (wide - want(20.0)).abs() < 1e-3,
            "{wide} for {}",
            want(20.0)
        );
    }

    #[test]
    fn lamps_land_on_the_same_ground_at_any_zoom() {
        let n16 = name_at_zoom(46.5, 6.5, 16);
        let f = Footprint::from_name(&n16);
        let w = f.east - f.west;
        // A slanted road of every glowing class through the north-west
        // quarter.
        let roads = merged(
            (46, 6),
            &[100u8, 120, 10, 30]
                .iter()
                .enumerate()
                .map(|(k, &s)| {
                    let x = f.west + (0.05 + 0.1 * k as f64) * w;
                    (s, vec![(x, f.north + 0.01), (x + 0.1 * w, f.south - 0.01)])
                })
                .collect::<Vec<_>>(),
        );
        let f16 = RoadField::raster(&n16, 512, &[roads.clone()], &RoadModel::DEFAULT);
        let n17 = DdsFilename {
            row: n16.row * 2,
            col: n16.col * 2,
            zoom: 17,
            ..n16.clone()
        };
        let f17 = RoadField::raster(&n17, 256, &[roads], &RoadModel::DEFAULT);
        let mut lit = 0;
        for i in 0..256 {
            for j in 0..256 {
                assert!((f16.at(i, j) - f17.at(i, j)).abs() < 1e-4, "texel {i},{j}");
                lit += usize::from(f17.at(i, j) > 0.0);
            }
        }
        assert!(lit > 80, "the shared quarter holds lamps: {lit}");
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
        let at = |subtype| RoadField::raster(&n, 256, &[across(&n, subtype)], &GLOW).at(128, 128);
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
        for (label, model) in [("glow", GLOW), ("lamps", RoadModel::DEFAULT)] {
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
