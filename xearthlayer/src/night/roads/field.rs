//! Per-footprint road glow: for each texel the brightest class profile of
//! any nearby segment, in 0..=1 (class intensity included). Light comes
//! from geometry in metres, so it is world-anchored at every zoom.
//!
//! Cost: a tile is skipped whole when no chain nears the footprint, a chain
//! is culled by its box in tile-relative `f32`, a segment visits only the
//! texels in its reach window, and a texel is rejected by squared distance
//! before any square root or exponential. The profile (one `f32` per
//! texel) is allocated when the first segment reaches the grid, so a
//! texture with no road nearby costs no heap at all.

use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

use super::class::RoadClass;
use super::dsf::Chains;
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
            if !tile.crosses(
                fp.south - reach,
                fp.west - reach,
                fp.north + reach,
                fp.east + reach,
            ) {
                continue;
            }
            let canvas = canvas.get_or_insert_with(|| Canvas::new(Grid::new(name, size)));
            let (lat0, lon0) = (f64::from(tile.tile.0), f64::from(tile.tile.1));
            // The footprint and its reach as offsets from the tile corner.
            let want = [
                (fp.south - reach - lat0) as f32,
                (fp.west - reach - lon0) as f32,
                (fp.north + reach - lat0) as f32,
                (fp.east + reach - lon0) as f32,
            ];
            for (ci, b) in tile.bbox.iter().enumerate() {
                if b[2] < want[0] || b[0] > want[2] || b[3] < want[1] || b[1] > want[3] {
                    continue;
                }
                let class = tile.class[ci];
                pts.clear();
                pts.extend(tile.chains.points(ci).map(|p| {
                    mercator(
                        f64::from(tile.chains.lat[p]) + lat0,
                        f64::from(tile.chains.lon[p]) + lon0,
                    )
                }));
                segments += pts.len().saturating_sub(1);
                let lamps = model
                    .rural_lamps
                    .filter(|_| !matches!(class, RoadClass::Motorway | RoadClass::Ramp));
                match lamps {
                    None => canvas.draw_glow(&pts, model.classes[class.index()]),
                    Some(l) => canvas.draw_lamps(&pts, chain_key(&tile.chains, ci, lat0, lon0), &l),
                }
            }
        }
        let Some(canvas) = canvas else {
            return Self::empty(size);
        };
        let lit_texels = canvas.lit;
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

/// The lamp key of a chain: its first point quantised to 1e-6 degrees.
fn chain_key(chains: &Chains, ci: usize, lat0: f64, lon0: f64) -> i64 {
    let p = chains.points(ci).start;
    hash2(
        ((f64::from(chains.lon[p]) + lon0) * 1e6).round() as i64,
        ((f64::from(chains.lat[p]) + lat0) * 1e6).round() as i64,
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
            xs,
            ys,
            cos,
            texel,
        }
    }
}

/// A grid and the profile drawn on it (allocated by the first draw that
/// reaches the grid).
struct Canvas {
    grid: Grid,
    profile: Vec<f32>,
    /// Texels above zero so far.
    lit: usize,
}

impl Canvas {
    fn new(grid: Grid) -> Self {
        Self {
            grid,
            profile: Vec::new(),
            lit: 0,
        }
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
        let j0 = g.xs.partition_point(|&x| x < xmin);
        let j1 = g.xs.partition_point(|&x| x <= xmax);
        let i0 = g.ys.partition_point(|&y| y > ymax);
        let i1 = g.ys.partition_point(|&y| y >= ymin);
        if j0 >= j1 || i0 >= i1 {
            return;
        }
        if self.profile.is_empty() {
            self.profile = vec![0f32; g.size * g.size];
        }
        let (vx, vy) = (b.0 - a.0, b.1 - a.1);
        let len2 = vx * vx + vy * vy;
        let inv_len2 = if len2 > 0.0 { 1.0 / len2 } else { 0.0 };
        let core2 = half_width * half_width;
        for i in i0..i1 {
            let sigma = sigma_m.max(g.texel[i]);
            let reach = half_width + 3.0 * sigma;
            let reach2 = reach * reach;
            let neg_inv_two_s2 = -1.0 / (2.0 * sigma * sigma);
            let c2 = g.cos[i] * g.cos[i];
            let dy = g.ys[i] - a.1;
            let along_y = dy * vy;
            let row = &mut self.profile[i * g.size..(i + 1) * g.size];
            for (&x, cell) in g.xs[j0..j1].iter().zip(&mut row[j0..j1]) {
                let dx = x - a.0;
                let t = ((dx * vx + along_y) * inv_len2).clamp(0.0, 1.0);
                let (ex, ey) = (dx - t * vx, dy - t * vy);
                let d2 = (ex * ex + ey * ey) * c2;
                if d2 > reach2 || intensity <= *cell {
                    continue;
                }
                let v = if d2 <= core2 {
                    intensity
                } else {
                    let d = d2.sqrt() - half_width;
                    intensity * (d * d * neg_inv_two_s2).exp() as f32
                };
                if v > *cell {
                    self.lit += usize::from(*cell == 0.0);
                    *cell = v;
                }
            }
        }
    }
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
        let mut m = Merger::new(tile);
        m.add(&DsfRoads {
            tile,
            overlay: true,
            net_def: Some("n".into()),
            exclusions: vec![],
            chains: c,
        });
        Arc::new(m.finish())
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
    /// points per `pitch_m` cell, classes mixed by a hash of the cell.
    fn street_grid(
        c: &mut Chains,
        (south, north): (f32, f32),
        (west, east): (f32, f32),
        pitch_m: f64,
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
                c.push(subtype, &ew);
                c.push(subtype, &[(lon, lat), (lon, lat + dlat)]);
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
        street_grid(&mut c, (0.0, 1.0), (0.0, 1.0), 207.0);
        // ... and a dense core over the footprint at an 80 m pitch.
        let rel = |v: f64, o: i32| (v - f64::from(o)) as f32;
        street_grid(
            &mut c,
            (rel(f.south, 46), rel(f.north, 46)),
            (rel(f.west, 6), rel(f.east, 6)),
            80.0,
        );
        let chains = c.len();
        let mut m = Merger::new((46, 6));
        m.add(&DsfRoads {
            tile: (46, 6),
            overlay: true,
            net_def: Some("n".into()),
            exclusions: vec![],
            chains: c,
        });
        let tile = Arc::new(m.finish());
        println!("chains in the tile: {chains}");
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
}
