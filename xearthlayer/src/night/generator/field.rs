//! Recipe layers sampled as continuous functions of world position.
//!
//! Cells are indexed globally (row 0 at the north pole, column 0 at 180 W),
//! so a sample near a 1 degree edge reads the neighbouring tile's cells and
//! every texture computes the same value for the same ground.

use std::sync::Arc;

use super::gain::gain;
use super::RecipeLookup;
use crate::night::footprint::{wrap_tile_lon, Footprint};
use crate::night::math::floor_i64;
use crate::night::recipe::{Grid, RadianceCurve, Recipe};

mod sampler;

pub use sampler::{FieldSampler, TexelAxes};

/// How far beyond a footprint sampling can reach, in degrees: bilinear
/// neighbours and the class warp stay well inside it.
pub const FIELD_MARGIN_DEG: f64 = 0.01;

/// Slack, in cells, on the probe's index ranges, so a texel exactly on a
/// cell boundary cannot fall outside them by rounding.
const PROBE_SLACK_CELLS: f64 = 1e-6;

/// A recipe layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Class,
    Brightness,
}

/// The 1 degree tile `(lat0, lon0)` holding global cell `(row, col)` of a
/// layer with `ny` x `nx` cells per degree, the longitude wrapped into
/// [-180, 180): columns past either end of the world continue across the
/// antimeridian.
fn tile_of(row: i64, col: i64, ny: i64, nx: i64) -> (i64, i64) {
    (
        89 - row.div_euclid(ny),
        col.div_euclid(nx).rem_euclid(360) - 180,
    )
}

/// A layer's grid as a field reads it: cells per degree, and for the
/// brightness layer the scale its bytes are read with (the class layer's is
/// the default, unused). Tiles continue one another's cells only when their
/// grids are equal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LayerGrid {
    pub ny: usize,
    pub nx: usize,
    /// The brightness scale's curve, its logarithm taken once.
    pub curve: RadianceCurve,
}

impl LayerGrid {
    /// `layer` of `recipe` as read, or `None` when the recipe lacks it.
    fn of(recipe: &Recipe, layer: Layer) -> Option<Self> {
        let g = RecipeField::grid(recipe, layer);
        (!g.is_missing()).then(|| Self {
            ny: g.height,
            nx: g.width,
            curve: match layer {
                Layer::Class => RadianceCurve::default(),
                Layer::Brightness => recipe.scale.curve(),
            },
        })
    }

    /// Radiance of a (possibly fractional) brightness byte; 0 for none.
    fn radiance_of(&self, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        self.curve.radiance_at(b)
    }
}

/// One layer's grids across a field.
struct LayerGrids {
    /// Per slot: its tile's grid, `None` without a recipe or the layer.
    slots: Vec<Option<LayerGrid>>,
    /// The distinct grids, in slot order: one for version 1 recipes.
    distinct: Vec<LayerGrid>,
}

impl LayerGrids {
    fn new(slots: &[Option<Arc<Recipe>>], layer: Layer) -> Self {
        let slots: Vec<Option<LayerGrid>> = slots
            .iter()
            .map(|r| r.as_deref().and_then(|r| LayerGrid::of(r, layer)))
            .collect();
        let mut distinct: Vec<LayerGrid> = Vec::new();
        for g in slots.iter().flatten() {
            if !distinct.contains(g) {
                distinct.push(*g);
            }
        }
        Self { slots, distinct }
    }
}

/// The recipes one footprint (plus margin) touches, fetched once.
///
/// Each tile's layers are read on that tile's own grid and, for brightness,
/// with its own scale, so the values at a point do not depend on which
/// footprint asks (#434). A point samples a layer on the grid of its own
/// tile's layer or, where its tile lacks one, of the nearest tile that has
/// one (ties to the smaller tile key), and reads cells only from tiles on
/// that grid: across a line where grids differ the other tile reads as
/// missing, and a sample next to a missing tile reads its neighbour.
/// Version 1 recipes all share one grid and scale, so a field of them
/// reads on that one everywhere.
///
/// The recipes sit in a small row-major table of tile slots (a field spans
/// at most a few tiles each way), so a cell read is index arithmetic, not a
/// hash lookup. Slots run west to east from the field's west edge without
/// wrapping, so a field across the antimeridian is contiguous; the recipes
/// and the tiles the slots report are those of the wrapped longitudes.
/// Positions near the line keep their own longitudes, so the pattern
/// tiling and the warp noise (functions of position, not periodic around
/// the world) are not continuous across it.
pub struct RecipeField {
    footprint: Footprint,
    south0: i64,
    west0: i64,
    lats: i64,
    lons: i64,
    slots: Vec<Option<Arc<Recipe>>>,
    /// The tile `(lat0, lon0)` of each slot.
    tiles: Vec<(i32, i32)>,
    class: LayerGrids,
    brightness: LayerGrids,
}

impl RecipeField {
    pub fn new(footprint: &Footprint, lookup: RecipeLookup<'_>) -> Self {
        let (lat_range, lon_range) = footprint.expanded(FIELD_MARGIN_DEG).tile_ranges();
        let (south0, north0) = (i64::from(*lat_range.start()), i64::from(*lat_range.end()));
        let (west0, east0) = (i64::from(*lon_range.start()), i64::from(*lon_range.end()));
        let (lats, lons) = ((north0 - south0 + 1).max(0), (east0 - west0 + 1).max(0));
        let tiles: Vec<(i32, i32)> = (0..lats * lons)
            .map(|i| {
                let lon = (west0 + i % lons) as i32;
                ((south0 + i / lons) as i32, wrap_tile_lon(lon))
            })
            .collect();
        let slots: Vec<Option<Arc<Recipe>>> = tiles.iter().map(|&(a, b)| lookup(a, b)).collect();
        Self {
            tiles,
            footprint: *footprint,
            class: LayerGrids::new(&slots, Layer::Class),
            brightness: LayerGrids::new(&slots, Layer::Brightness),
            south0,
            west0,
            lats,
            lons,
            slots,
        }
    }

    fn grid(recipe: &Recipe, layer: Layer) -> &Grid {
        match layer {
            Layer::Class => &recipe.class,
            Layer::Brightness => &recipe.brightness,
        }
    }

    /// Number of tile slots; every `ClassWeight::slot` is below it.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// The slot of tile `(lat0, lon0)`, its longitude wrapped or not, or
    /// `None` outside the field.
    fn slot_of(&self, lat0: i64, lon0: i64) -> Option<usize> {
        let (dy, dx) = (lat0 - self.south0, (lon0 - self.west0).rem_euclid(360));
        ((0..self.lats).contains(&dy) && dx < self.lons).then(|| (dy * self.lons + dx) as usize)
    }

    /// The tile `(lat0, lon0)` of `slot` (below `slot_count`).
    fn slot_tile(&self, slot: usize) -> (i32, i32) {
        self.tiles[slot]
    }

    fn slot_recipe(&self, slot: usize) -> Option<&Arc<Recipe>> {
        self.slots.get(slot)?.as_ref()
    }

    pub(crate) fn recipe(&self, lat0: i32, lon0: i32) -> Option<&Arc<Recipe>> {
        self.slot_recipe(self.slot_of(lat0.into(), lon0.into())?)
    }

    fn layer(&self, layer: Layer) -> &LayerGrids {
        match layer {
            Layer::Class => &self.class,
            Layer::Brightness => &self.brightness,
        }
    }

    /// The distinct grids of `layer` across the field, in slot order: one
    /// for version 1 recipes, none without the layer.
    pub(crate) fn grids(&self, layer: Layer) -> &[LayerGrid] {
        &self.layer(layer).distinct
    }

    /// The grid `layer` is sampled on at a point: its own tile's, or where
    /// that tile lacks the layer, the nearest tile's that has it (by the
    /// larger of the latitude and longitude distances to the tile, ties to
    /// the smaller tile key); `None` when no tile of the field has it.
    ///
    /// A field reads the tiles within `FIELD_MARGIN_DEG` of every point of
    /// its footprint, so for grids fine enough that a sample reaches less
    /// than that (version 1's), any field holding a point chooses the same
    /// grid wherever the choice changes a value.
    pub(crate) fn grid_at(&self, layer: Layer, lat: f64, lon: f64) -> Option<LayerGrid> {
        let grids = self.layer(layer);
        match grids.distinct.as_slice() {
            [] => return None,
            [one] => return Some(*one),
            _ => {}
        }
        let own = self.slot_of(floor_i64(lat), floor_i64(lon));
        if let Some(g) = own.and_then(|s| grids.slots[s]) {
            return Some(g);
        }
        let gap = |x: f64, lo: f64| (lo - x).max(x - (lo + 1.0)).max(0.0);
        let distance = |slot: usize| {
            let (dy, dx) = (slot as i64 / self.lons, slot as i64 % self.lons);
            let (lat0, lon0) = ((self.south0 + dy) as f64, (self.west0 + dx) as f64);
            gap(lat, lat0).max(gap(lon, lon0))
        };
        grids
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, g)| Some((distance(slot), self.tiles[slot], (*g)?)))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, _, g)| g)
    }

    /// The slot and value of global cell `(row, col)` of `layer` on `grid`;
    /// `None` without a recipe or when that tile's layer is missing or on
    /// another grid.
    pub(crate) fn slot_cell(
        &self,
        layer: Layer,
        grid: &LayerGrid,
        row: i64,
        col: i64,
    ) -> Option<(usize, u8)> {
        let (ny, nx) = (grid.ny as i64, grid.nx as i64);
        let (lat0, lon0) = tile_of(row, col, ny, nx);
        let slot = self.slot_of(lat0, lon0)?;
        if self.layer(layer).slots[slot].as_ref() != Some(grid) {
            return None;
        }
        let g = Self::grid(self.slot_recipe(slot)?, layer);
        Some((
            slot,
            g.get(row.rem_euclid(ny) as usize, col.rem_euclid(nx) as usize),
        ))
    }

    /// The value of global cell `(row, col)` of `layer` on `grid`; `None`
    /// as for `slot_cell`.
    #[cfg(test)]
    pub(crate) fn cell(&self, layer: Layer, grid: &LayerGrid, row: i64, col: i64) -> Option<u8> {
        self.slot_cell(layer, grid, row, col).map(|(_, v)| v)
    }

    /// Radiance (nW/cm2/sr) at a point: the brightness byte, bilinear
    /// between cell centres; 0 where no recipe contributes. For one point,
    /// in tests; a texture samples through a [`FieldSampler`], which gives
    /// the same bits.
    #[cfg(test)]
    pub(crate) fn radiance(&self, lat: f64, lon: f64) -> f64 {
        FieldSampler::new(
            self,
            TexelAxes {
                lats: &[lat],
                lons: &[lon],
            },
        )
        .radiance(0, 0)
    }
}

/// Warp of the class sample position, in class cells (about 45 m at 3
/// arc-seconds): breaks the cell grid into organic edges (R12).
pub const WARP_AMPLITUDE_CELLS: f64 = 0.5;
/// Wavelength of the warp, in class cells.
pub const WARP_WAVELENGTH_CELLS: f64 = 2.0;
const WARP_SEED_X: u64 = 0x5741_5250_5F58_0001;
const WARP_SEED_Y: u64 = 0x5741_5250_5F59_0002;

/// How far, in class cells, a class cell's centre can be from a texel it
/// gives weight to. With `u = gx + amplitude * noise - 0.5` and noise in
/// `[-1, 1]`, cell `c` is one of the four bilinear corners only when
/// `c - 1 <= u < c + 1`, that is for texels with `gx` within
/// `1 + amplitude` of the cell centre `c + 0.5`.
const CLASS_REACH_CELLS: f64 = 1.0 + WARP_AMPLITUDE_CELLS;

/// One lit candidate at a point: a class of one tile's palette and its weight.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassWeight {
    pub tile: (i32, i32),
    /// The tile's slot in the field, for per-tile tables (below `slot_count`).
    pub slot: usize,
    pub index: u8,
    pub weight: f64,
}

impl RecipeField {
    /// The palette name of class `index` in `tile`.
    pub fn palette_name(&self, tile: (i32, i32), index: u8) -> Option<&str> {
        self.recipe(tile.0, tile.1)?
            .palette
            .get(usize::from(index))
            .map(String::as_str)
    }

    /// The palette name of class `index` in the tile at `slot`.
    pub fn palette_name_in(&self, slot: usize, index: u8) -> Option<&str> {
        self.slot_recipe(slot)?
            .palette
            .get(usize::from(index))
            .map(String::as_str)
    }

    /// The classes at a point with their weights: the bilinear weights of the
    /// four class cell centres around the warped position, merged per class.
    /// Class 0 and cells without a recipe carry no entry, so the weights sum
    /// to less than 1 near the edge of classified ground.
    ///
    /// For one point, in tests; a texture samples through a
    /// [`FieldSampler`], which gives the same bits.
    #[cfg(test)]
    pub(crate) fn class_weights(&self, lat: f64, lon: f64, out: &mut Vec<ClassWeight>) {
        FieldSampler::new(
            self,
            TexelAxes {
                lats: &[lat],
                lons: &[lon],
            },
        )
        .class_weights(0, 0, out);
    }

    /// True when the generator may light some texel of the footprint (R7:
    /// answers the file size from recipes alone, without generating pixels).
    ///
    /// It follows the generator's own sampling model, conservatively. A
    /// texel is lit only when a lit class (one `is_lit` accepts) gives it
    /// weight and its bilinear brightness gives a gain above zero. So the
    /// probe looks at every class cell that can give weight to a texel of
    /// the footprint (within `CLASS_REACH_CELLS`), and for each lit one, at
    /// every brightness cell that can enter the bilinear brightness of
    /// those texels (one brightness cell further). Bilinear brightness never
    /// exceeds its brightest corner and radiance grows with the byte, so if
    /// none of those cells gives a gain, no texel can be lit. The answer may
    /// be lit where the generator ends up black (that costs a full-size
    /// black file), never the reverse.
    ///
    /// With more than one grid in the field (#434), every pair of a class
    /// grid and a brightness grid is checked over the whole footprint: a
    /// texel samples on one of each, so this covers it.
    pub fn has_light(&self, is_lit: impl Fn(&str) -> bool) -> bool {
        // Per (slot, class index): unknown, lit or not.
        let mut lit_class: Vec<Option<bool>> = vec![None; self.slots.len() * 256];
        self.grids(Layer::Class).iter().any(|class| {
            self.grids(Layer::Brightness)
                .iter()
                .any(|brightness| self.has_light_on(class, brightness, &is_lit, &mut lit_class))
        })
    }

    /// `has_light` with the class cells of `class` and the brightness cells
    /// of `brightness` alone.
    fn has_light_on(
        &self,
        class: &LayerGrid,
        brightness: &LayerGrid,
        is_lit: &impl Fn(&str) -> bool,
        lit_class: &mut [Option<bool>],
    ) -> bool {
        let f = &self.footprint;
        let (cy, cx) = (class.ny as f64, class.nx as f64);
        let (by, bx) = (brightness.ny as f64, brightness.nx as f64);
        // The footprint in class grid units (gy grows southwards).
        let (gx_w, gx_e) = ((f.west + 180.0) * cx, (f.east + 180.0) * cx);
        let (gy_n, gy_s) = ((90.0 - f.north) * cy, (90.0 - f.south) * cy);
        // Cells whose centre (`index + 0.5`) is within reach of the footprint.
        let reach = CLASS_REACH_CELLS + 0.5 + PROBE_SLACK_CELLS;
        let rows = (gy_n - reach).floor() as i64..=(gy_s + reach).floor() as i64;
        let cols = (gx_w - reach).floor() as i64..=(gx_e + reach).floor() as i64;

        // The brightness cells (in brightness grid units, `u = g - 0.5`)
        // entering the bilinear sample of any point in `[lo, hi]`.
        let span = |lo: f64, hi: f64| {
            (lo - 0.5 - PROBE_SLACK_CELLS).floor() as i64
                ..=(hi - 0.5 + PROBE_SLACK_CELLS).floor() as i64 + 1
        };
        for row in rows {
            // Texels this class row can weight, clipped to the footprint.
            let ty0 = (row as f64 + 0.5 - CLASS_REACH_CELLS).max(gy_n);
            let ty1 = (row as f64 + 0.5 + CLASS_REACH_CELLS).min(gy_s);
            if ty0 > ty1 + PROBE_SLACK_CELLS {
                continue;
            }
            let brows = span(ty0 / cy * by, ty1 / cy * by);
            for col in cols.clone() {
                let Some((slot, index)) = self.slot_cell(Layer::Class, class, row, col) else {
                    continue;
                };
                if index == 0 {
                    continue;
                }
                let known = &mut lit_class[slot * 256 + usize::from(index)];
                let lit = *known
                    .get_or_insert_with(|| self.palette_name_in(slot, index).is_some_and(is_lit));
                if !lit {
                    continue;
                }
                let tx0 = (col as f64 + 0.5 - CLASS_REACH_CELLS).max(gx_w);
                let tx1 = (col as f64 + 0.5 + CLASS_REACH_CELLS).min(gx_e);
                if tx0 > tx1 + PROBE_SLACK_CELLS {
                    continue;
                }
                let bcols = span(tx0 / cx * bx, tx1 / cx * bx);
                for br in brows.clone() {
                    for bc in bcols.clone() {
                        let b = self.slot_cell(Layer::Brightness, brightness, br, bc);
                        if b.is_some_and(|(_, b)| {
                            gain(brightness.radiance_of(f64::from(b)), 1.0) > 0.0
                        }) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
pub(crate) mod lookup_reference;

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::night::recipe::{BrightnessScale, Grid, Recipe};
    use std::sync::Arc;

    /// A recipe for tile (lat0, lon0): `class(row, col)` on an n_class grid,
    /// `bright(row, col)` on an n_bright grid, palette ["", "lit"].
    pub(crate) fn synthetic(
        lat0: i32,
        lon0: i32,
        n_class: usize,
        class: impl Fn(usize, usize) -> u8,
        n_bright: usize,
        bright: impl Fn(usize, usize) -> u8,
    ) -> Arc<Recipe> {
        let grid = |n: usize, f: &dyn Fn(usize, usize) -> u8| Grid {
            width: n,
            height: n,
            cells: (0..n * n).map(|i| f(i / n, i % n)).collect(),
        };
        Arc::new(Recipe {
            tile: crate::night::tile_name(lat0, lon0),
            palette: vec![String::new(), "lit".into()],
            class: grid(n_class, &class),
            brightness: grid(n_bright, &bright),
            scale: BrightnessScale::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::synthetic;
    use super::*;
    use crate::night::recipe::BrightnessScale;

    fn footprint(north: f64, south: f64, west: f64, east: f64) -> Footprint {
        Footprint {
            north,
            south,
            west,
            east,
            zoom: 16,
        }
    }

    #[test]
    fn brightness_interpolates_between_cell_centres() {
        // One tile, 4 x 4 brightness: column 1 is 0, column 2 is 200.
        let r = synthetic(
            37,
            -123,
            4,
            |_, _| 1,
            4,
            |_, c| if c >= 2 { 200 } else { 0 },
        );
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        let scale = BrightnessScale::default();
        let mid = field.radiance(37.5, -122.5);
        assert!((mid - scale.radiance_at(100.0)).abs() < 1e-9, "{mid}");
        assert!((field.radiance(37.5, -122.375) - scale.radiance(200)).abs() < 1e-9);
    }

    #[test]
    fn brightness_is_continuous_across_a_tile_edge() {
        let west = synthetic(37, -123, 4, |_, _| 1, 4, |_, _| 150);
        let east = synthetic(37, -122, 4, |_, _| 1, 4, |_, _| 150);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.1, -121.9), &lookup);
        let expected = BrightnessScale::default().radiance(150);
        for lon in [-122.01, -122.0001, -122.0, -121.9999, -121.99] {
            assert!(
                (field.radiance(37.5, lon) - expected).abs() < 1e-9,
                "lon {lon}"
            );
        }
    }

    #[test]
    fn a_missing_neighbour_reads_as_zero() {
        let only = synthetic(37, -123, 4, |_, _| 1, 4, |_, _| 200);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| only.clone());
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.1, -121.9), &lookup);
        let at_edge = field.radiance(37.5, -122.0);
        let scale = BrightnessScale::default();
        assert!(
            (at_edge - scale.radiance_at(100.0)).abs() < 1e-9,
            "{at_edge}"
        );
        assert_eq!(field.radiance(37.5, -121.5), 0.0, "no recipe at all");
    }

    fn weight_sum(field: &RecipeField, lat: f64, lon: f64) -> f64 {
        let mut out = Vec::new();
        field.class_weights(lat, lon, &mut out);
        out.iter().map(|w| w.weight).sum()
    }

    #[test]
    fn deep_inside_a_class_the_weight_is_one() {
        let r = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        let mut out = Vec::new();
        field.class_weights(37.5, -122.5, &mut out);
        assert_eq!(out.len(), 1, "one class, merged");
        assert!((out[0].weight - 1.0).abs() < 1e-9);
        assert_eq!(field.palette_name(out[0].tile, out[0].index), Some("lit"));
    }

    #[test]
    fn class_zero_and_missing_tiles_carry_no_weight() {
        let r = synthetic(37, -123, 120, |_, _| 0, 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        assert_eq!(weight_sum(&field, 37.5, -122.5), 0.0);
        assert_eq!(weight_sum(&field, 36.5, -122.5), 0.0, "no recipe");
    }

    /// West half of the tile lit (columns < 60 of 120), east half class 0.
    fn half_lit() -> RecipeField {
        let r = synthetic(37, -123, 120, |_, c| u8::from(c < 60), 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup)
    }

    #[test]
    fn a_class_edge_is_a_ramp_not_a_step() {
        // The edge is at lon -122.5; a cell is 1/120 degree. Sample across it.
        let field = half_lit();
        let cell = 1.0 / 120.0;
        let ramp: Vec<f64> = (-30..=30)
            .map(|k| weight_sum(&field, 37.5003, -122.5 + f64::from(k) * cell / 10.0))
            .collect();
        let between = ramp.iter().filter(|w| **w > 0.05 && **w < 0.95).count();
        assert!(
            between >= 5,
            "at least half a cell of intermediate weights: {ramp:?}"
        );
        assert!(ramp.first().unwrap() > &0.95 && ramp.last().unwrap() < &0.05);
    }

    #[test]
    fn a_class_edge_is_not_straight() {
        // Where the weight crosses 0.5 moves along the edge, by the warp.
        let field = half_lit();
        let cell = 1.0 / 120.0;
        let crossings: Vec<f64> = (0..40)
            .map(|k| {
                let lat = 37.3 + f64::from(k) * cell * 0.7;
                (0..400)
                    .map(|s| -122.5 - 2.0 * cell + f64::from(s) * cell / 100.0)
                    .find(|&lon| weight_sum(&field, lat, lon) < 0.5)
                    .unwrap()
            })
            .collect();
        let spread = crossings.iter().cloned().fold(f64::MIN, f64::max)
            - crossings.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread > 0.3 * cell, "the 50% line wanders by {spread} deg");
        assert!(spread < 2.0 * cell, "but stays near the edge: {spread} deg");
    }

    #[test]
    fn brightness_is_continuous_across_a_latitude_line() {
        // Tile +37-123 is south of lat 38, +38-123 north of it. Cell rows run
        // north to south inside each tile, so the south tile's row 0 and the
        // north tile's last row meet at the line: global rows
        // `(89 - 38) * 4 + 3` and `(89 - 37) * 4 + 0`.
        let south = synthetic(
            37,
            -123,
            4,
            |_, _| 1,
            4,
            |r, _| if r == 0 { 200 } else { 0 },
        );
        let north = synthetic(
            38,
            -123,
            4,
            |_, _| 1,
            4,
            |r, _| if r == 3 { 100 } else { 0 },
        );
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(south.clone()),
            (38, -123) => Some(north.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(38.1, 37.9, -122.6, -122.4), &lookup);
        let grid = field.grids(Layer::Brightness)[0];
        assert_eq!((grid.ny, grid.nx), (4, 4));
        assert_eq!(
            field.cell(Layer::Brightness, &grid, (89 - 38) * 4 + 3, 4 * 57 + 1),
            Some(100)
        );
        assert_eq!(
            field.cell(Layer::Brightness, &grid, (89 - 37) * 4, 4 * 57 + 1),
            Some(200)
        );
        let scale = BrightnessScale::default();
        // On the line the byte is halfway between the two cell centres.
        let on = field.radiance(38.0, -122.5);
        assert!((on - scale.radiance_at(150.0)).abs() < 1e-9, "{on}");
        // At the cell centres, the cells' own values.
        assert!((field.radiance(38.125, -122.625) - scale.radiance(100)).abs() < 1e-9);
        assert!((field.radiance(37.875, -122.625) - scale.radiance(200)).abs() < 1e-9);
        // Continuous through the line.
        let mut last = field.radiance(38.1, -122.5);
        for k in 1..=200 {
            let lat = 38.1 - f64::from(k) * 0.001;
            let r = field.radiance(lat, -122.5);
            // A nearest-cell step at the line would be about 80 nW.
            assert!(
                (r - last).abs() < 1.0,
                "a step of {} at lat {lat}",
                r - last
            );
            last = r;
        }
    }

    #[test]
    fn class_weights_carry_each_tiles_own_palette() {
        // West tile class 1 is "west-lit", east tile class 1 is "east-lit".
        // On the shared line both tiles' class 1 take part, kept apart by
        // tile, and each resolves through its own palette.
        let named = |lat0: i32, lon0: i32, name: &str| {
            let mut r = (*synthetic(lat0, lon0, 120, |_, _| 1, 4, |_, _| 255)).clone();
            r.palette = vec![String::new(), name.into()];
            Arc::new(r)
        };
        let (west, east) = (named(37, -123, "west-lit"), named(37, -122, "east-lit"));
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.1, -121.9), &lookup);
        let mut out = Vec::new();
        let (mut both, mut west_only, mut east_only) = (0, 0, 0);
        for k in 0..400 {
            let lon = -122.02 + f64::from(k) * 0.0001;
            let lat = 37.5 + f64::from(k % 7) * 0.001;
            field.class_weights(lat, lon, &mut out);
            let names: Vec<&str> = out
                .iter()
                .map(|w| {
                    assert_eq!(
                        field.palette_name(w.tile, w.index),
                        field.palette_name_in(w.slot, w.index)
                    );
                    field.palette_name(w.tile, w.index).unwrap()
                })
                .collect();
            assert!((out.iter().map(|w| w.weight).sum::<f64>() - 1.0).abs() < 1e-9);
            for w in &out {
                let expect = if w.tile == (37, -123) {
                    "west-lit"
                } else {
                    "east-lit"
                };
                assert_eq!(field.palette_name(w.tile, w.index), Some(expect));
            }
            match (names.contains(&"west-lit"), names.contains(&"east-lit")) {
                (true, true) => both += 1,
                (true, false) => west_only += 1,
                (false, true) => east_only += 1,
                _ => unreachable!(),
            }
        }
        assert!(
            both > 0 && west_only > 0 && east_only > 0,
            "{both} {west_only} {east_only}"
        );
    }

    // Golden urban.oxn (tile +37-123), from the recipe format's Conformance table:
    // urban (index 2) rows 900-1019 cols 300-359 = lat 37.25..37.15, lon -122.75..-122.70,
    //   under brightness row 3 col 1 = 230 (lit);
    // urban rows 120-179 cols 120-239 = lat 37.90..37.85, lon -122.90..-122.80,
    //   under brightness row 0 col 0 = 0 (dark: no radiance);
    // industrial (index 1) rows 600-659 cols 600-719 = lat 37.50..37.45, lon -122.50..-122.40.

    const URBAN: &[u8] = include_bytes!("../../../tests/fixtures/night/urban.oxn");

    fn golden_only(lat: i32, lon: i32) -> impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync {
        let r = Arc::new(crate::night::parse_recipe(URBAN).unwrap());
        move |a, b| ((a, b) == (lat, lon)).then(|| r.clone())
    }

    fn urban_only(name: &str) -> bool {
        name == "lib/g10/terrain10/urban_tmp_sdry3.ter"
    }

    fn probe(
        f: Footprint,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        is_lit: impl Fn(&str) -> bool,
    ) -> bool {
        RecipeField::new(&f, &recipes).has_light(is_lit)
    }

    #[test]
    fn lit_where_an_urban_class_meets_radiance() {
        let f = footprint(37.24, 37.16, -122.74, -122.71);
        assert!(probe(f, golden_only(37, -123), urban_only));
    }

    #[test]
    fn lit_when_only_the_second_straddled_tile_has_light() {
        // Spans lon -123.02..-122.70: tiles (37,-124), which has no recipe, then
        // (37,-123), which holds the lit urban block.
        let f = footprint(37.24, 37.16, -123.02, -122.70);
        assert_eq!(f.tiles(), vec![(37, -124), (37, -123)]);
        assert!(probe(f, golden_only(37, -123), urban_only));
        assert!(!probe(f, golden_only(37, -124), urban_only));
    }

    #[test]
    fn dark_where_urban_has_no_radiance() {
        // The west end of the dark urban block, where every brightness cell
        // within reach is 0 (cells (0, 0), (1, 0), and missing neighbours).
        let f = footprint(37.89, 37.86, -122.899, -122.885);
        assert!(!probe(f, golden_only(37, -123), urban_only));
        // Further east the same block is lit: bilinear brightness reaches it
        // from cell (0, 1) = 255, centred at lon -122.625, so the generator
        // lights its east strip and the probe must say so.
        let f = footprint(37.89, 37.86, -122.89, -122.81);
        assert!(probe(f, golden_only(37, -123), urban_only));
    }

    #[test]
    fn dark_where_the_class_does_not_resolve_to_lit_art() {
        let f = footprint(37.49, 37.46, -122.49, -122.41);
        assert!(!probe(f, golden_only(37, -123), urban_only));
        assert!(probe(f, golden_only(37, -123), |_| true));
    }

    #[test]
    fn dark_over_countryside_and_without_a_recipe() {
        let f = footprint(37.62, 37.58, -122.22, -122.18);
        assert!(!probe(f, golden_only(37, -123), |_| true));
        let f = footprint(37.24, 37.16, -122.74, -122.71);
        assert!(!probe(f, |_, _| None, |_| true));
    }

    #[test]
    fn dim_development_below_the_gain_floor_is_dark() {
        // Byte 40 is radiance 1.4, under the 1.6 floor: the gain is 0.
        let r = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 40);
        assert!(BrightnessScale::default().radiance(40) <= super::super::GAIN_FLOOR_NW);
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        assert!(!probe(
            footprint(37.6, 37.4, -122.6, -122.4),
            recipes,
            |_| true
        ));
    }

    /// A class weight with its weight as bits, for exact comparison.
    type WeightBits = ((i32, i32), usize, u8, u64);

    fn weight_bits(out: &[ClassWeight]) -> Vec<WeightBits> {
        out.iter()
            .map(|w| (w.tile, w.slot, w.index, w.weight.to_bits()))
            .collect()
    }

    /// Asserts that the class weights and the radiance at every point of
    /// the `lats` x `lons` grid are those of the code before #437, bit for
    /// bit: through a sampler over the grid (its cell tables), and through
    /// the per-point calls (one-point samplers, which read the field). No
    /// read misses a table the sampler built. Returns the number of points
    /// with a class weight.
    fn assert_matches_reference(field: &RecipeField, lats: &[f64], lons: &[f64]) -> usize {
        let mut sampler = FieldSampler::new(field, TexelAxes { lats, lons });
        let weighted = assert_sampler_matches_reference(&mut sampler, field, lats, lons);
        assert_eq!(sampler.table_misses(), 0, "every read is in the tables");
        weighted
    }

    /// `assert_matches_reference` through `sampler`, built over the grid.
    fn assert_sampler_matches_reference(
        sampler: &mut FieldSampler<'_>,
        field: &RecipeField,
        lats: &[f64],
        lons: &[f64],
    ) -> usize {
        let (mut got, mut point, mut want) = (Vec::new(), Vec::new(), Vec::new());
        let mut weighted = 0;
        for (i, &lat) in lats.iter().enumerate() {
            for (j, &lon) in lons.iter().enumerate() {
                lookup_reference::class_weights(field, lat, lon, &mut want);
                sampler.class_weights(i, j, &mut got);
                assert_eq!(
                    weight_bits(&got),
                    weight_bits(&want),
                    "weights at {lat}, {lon}"
                );
                field.class_weights(lat, lon, &mut point);
                assert_eq!(weight_bits(&point), weight_bits(&want), "at {lat}, {lon}");
                weighted += usize::from(!want.is_empty());
                let reference = lookup_reference::radiance(field, lat, lon).to_bits();
                assert_eq!(
                    sampler.radiance(i, j).to_bits(),
                    reference,
                    "at {lat}, {lon}"
                );
                assert_eq!(
                    field.radiance(lat, lon).to_bits(),
                    reference,
                    "at {lat}, {lon}"
                );
            }
        }
        weighted
    }

    /// Six tiles around (0, 0), so coordinates run negative and the sweep
    /// crosses the equator, the prime meridian and lon 1: (-1, -1) sets the
    /// grids (120 class, 8 brightness cells per degree), (-1, 0) has another
    /// class grid, (0, 0) another brightness grid, (-1, 1) matches, and
    /// (0, -1) and (0, 1) have no recipe. Classes 0 to 4 in diagonal bands,
    /// so neighbouring cells merge and split; brightness from 0 to 255.
    fn mixed_field() -> RecipeField {
        let classes = |r: usize, c: usize| ((r * 7 + c * 3) % 5) as u8;
        let bright = |r: usize, c: usize| ((r * 53 + c * 29) % 256) as u8;
        let sw = synthetic(-1, -1, 120, classes, 8, bright);
        let other_class = synthetic(-1, 0, 60, classes, 8, bright);
        let other_bright = synthetic(0, 0, 120, classes, 4, bright);
        let east = synthetic(-1, 1, 120, classes, 8, bright);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (-1, -1) => Some(sw.clone()),
            (-1, 0) => Some(other_class.clone()),
            (0, 0) => Some(other_bright.clone()),
            (-1, 1) => Some(east.clone()),
            _ => None,
        };
        RecipeField::new(&footprint(0.4, -0.6, -0.5, 1.4), &lookup)
    }

    /// `n` evenly spaced values over `[lo, hi]`, with each of `lines`, its
    /// neighbours one ulp away, and `-0.0`.
    fn axis(lo: f64, hi: f64, n: usize, lines: &[f64]) -> Vec<f64> {
        let mut v: Vec<f64> = (0..n)
            .map(|k| lo + (hi - lo) * k as f64 / (n - 1) as f64)
            .collect();
        for &l in lines {
            v.extend([l, l.next_up(), l.next_down()]);
        }
        v.push(-0.0);
        v
    }

    #[test]
    fn sampling_matches_the_code_before_437_on_a_dense_sweep() {
        let field = mixed_field();
        let lats = axis(-0.62, 0.42, 420, &[0.0, -0.5, 0.25]);
        let lons = axis(-0.52, 1.42, 780, &[0.0, 1.0, -0.25]);
        assert_eq!(
            FieldSampler::new(
                &field,
                TexelAxes {
                    lats: &lats,
                    lons: &lons
                }
            )
            .has_tables(),
            (true, true)
        );
        let weighted = assert_matches_reference(&field, &lats, &lons);
        assert!(
            weighted > lats.len() * lons.len() / 3,
            "{weighted} weighted"
        );
    }

    #[test]
    fn reads_outside_a_table_give_the_bits_of_the_field() {
        // Tables cut short (negative slack) so reads at the edge of the
        // texels' reach miss them and go to the field: the same bits.
        let field = mixed_field();
        let lats = axis(-0.3, 0.1, 100, &[0.0]);
        let lons = axis(-0.2, 0.6, 200, &[0.0]);
        let axes = TexelAxes {
            lats: &lats,
            lons: &lons,
        };
        let mut full = FieldSampler::new(&field, axes);
        assert_sampler_matches_reference(&mut full, &field, &lats, &lons);
        assert_eq!(full.table_misses(), 0);
        for slack in [-1, -2] {
            let mut tight = FieldSampler::with_slack(&field, axes, slack);
            // The coarser brightness grid's table may be cut to nothing.
            assert!(tight.has_tables().0, "slack {slack}");
            let weighted = assert_sampler_matches_reference(&mut tight, &field, &lats, &lons);
            assert!(
                weighted > lats.len() * lons.len() / 3,
                "{weighted} weighted"
            );
            assert!(
                tight.table_misses() > 0,
                "slack {slack}: reads miss the tables"
            );
        }
    }

    #[test]
    fn sampling_without_tables_matches_the_code_before_437() {
        // Texels much coarser than the grid: the sampler reads the field.
        let field = mixed_field();
        let lats = axis(-0.62, 0.42, 30, &[0.0]);
        let lons = axis(-0.52, 1.42, 30, &[0.0, 1.0]);
        assert_eq!(
            FieldSampler::new(
                &field,
                TexelAxes {
                    lats: &lats,
                    lons: &lons
                }
            )
            .has_tables(),
            (false, true),
            "no class table; the 8 cell brightness grid still fits"
        );
        assert!(assert_matches_reference(&field, &lats, &lons) > 0);
    }

    /// Isolated class cells around the corner of four tiles at (47, 7),
    /// near both degree lines: (46, 6) and (47, 7) on a 120 cell grid,
    /// (46, 7) on a 60 cell grid, (47, 6) with no recipe.
    fn isolated_cells_field() -> RecipeField {
        let only = |cells: &'static [(usize, usize)]| {
            move |r: usize, c: usize| u8::from(cells.contains(&(r, c)))
        };
        let sw = synthetic(
            46,
            6,
            120,
            only(&[(0, 119), (3, 114), (1, 117)]),
            8,
            |_, _| 200,
        );
        let se = synthetic(46, 7, 60, only(&[(0, 0), (2, 1)]), 8, |_, _| 200);
        let ne = synthetic(47, 7, 120, only(&[(119, 0), (116, 4)]), 8, |_, _| 200);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (46, 6) => Some(sw.clone()),
            (46, 7) => Some(se.clone()),
            (47, 7) => Some(ne.clone()),
            _ => None,
        };
        RecipeField::new(&footprint(47.04, 46.96, 6.96, 7.04), &lookup)
    }

    #[test]
    fn the_class_mask_skips_only_texels_without_class_weight() {
        let field = isolated_cells_field();
        let dense = |lo: f64, line: f64| {
            let mut v: Vec<f64> = (0..480).map(|k| lo + f64::from(k) * 0.000_125).collect();
            v.extend([line, line.next_up(), line.next_down()]);
            v
        };
        let (lats, lons) = (dense(46.97, 47.0), dense(6.97, 7.0));
        let axes = TexelAxes {
            lats: &lats,
            lons: &lons,
        };
        let sampler = FieldSampler::new(&field, axes);
        assert_eq!(sampler.has_tables(), (true, true));
        let (mut skipped, mut weighted, mut want) = (0, 0, Vec::new());
        for (i, &lat) in lats.iter().enumerate() {
            for (j, &lon) in lons.iter().enumerate() {
                lookup_reference::class_weights(&field, lat, lon, &mut want);
                if sampler.skips_class(i, j) {
                    assert!(want.is_empty(), "skipped a weighted texel at {lat}, {lon}");
                    skipped += 1;
                }
                weighted += usize::from(!want.is_empty());
            }
        }
        assert!(skipped > lats.len() * lons.len() / 10, "{skipped} skipped");
        assert!(weighted > 0, "the isolated cells are weighted");
        // With the mask or without, the sampler gives the reference bits.
        assert!(assert_matches_reference(&field, &lats, &lons) > 0);
        let mut unmasked = FieldSampler::with_class_mask(&field, axes, false);
        assert!(!(0..lats.len()).any(|i| (0..lons.len()).any(|j| unmasked.skips_class(i, j))));
        assert_sampler_matches_reference(&mut unmasked, &field, &lats, &lons);
    }

    /// A recipe for `(lat0, lon0)` lit and bright everywhere, on 120 class
    /// and 8 brightness cells per degree.
    fn bright_tile(lat0: i32, lon0: i32) -> Arc<Recipe> {
        synthetic(lat0, lon0, 120, |_, _| 1, 8, |_, _| 200)
    }

    #[test]
    fn a_footprint_on_the_antimeridian_reads_the_tile_across_it() {
        // Taveuni, Fiji: tile (-17, -180) east of the line, nothing west.
        let across = bright_tile(-17, -180);
        let lookup = move |a: i32, b: i32| ((a, b) == (-17, -180)).then(|| across.clone());
        let east_edge = RecipeField::new(&footprint(-16.6, -16.7, 179.95, 180.0), &lookup);
        assert!(east_edge.recipe(-17, -180).is_some());
        // On the line, bilinear brightness is halfway between the last
        // cell west of it (no recipe) and the first east of it (200), whose
        // centres are 1/16 degree either side.
        let scale = BrightnessScale::default();
        let at_line = east_edge.radiance(-16.65, 180.0);
        assert!(
            (at_line - scale.radiance_at(100.0)).abs() < 1e-9,
            "{at_line}"
        );
        assert!(east_edge.radiance(-16.65, 179.999_999) > 0.0);
        assert_eq!(east_edge.radiance(-16.65, 179.9), 0.0, "no recipe west");
        // The class warp reaches the tile's westmost cells, under its own
        // key and palette.
        let mut out = Vec::new();
        east_edge.class_weights(-16.65, 179.999_9, &mut out);
        assert!(!out.is_empty(), "class weight across the line");
        for w in &out {
            assert_eq!(w.tile, (-17, -180));
            assert_eq!(east_edge.palette_name(w.tile, w.index), Some("lit"));
        }
        // The other way: a footprint on the line's east side reads 179.
        let west_tile = bright_tile(-17, 179);
        let lookup = move |a: i32, b: i32| ((a, b) == (-17, 179)).then(|| west_tile.clone());
        let west_edge = RecipeField::new(&footprint(-16.6, -16.7, -180.0, -179.95), &lookup);
        assert!(west_edge.radiance(-16.65, -179.999_999) > 0.0);
        west_edge.class_weights(-16.65, -179.999_9, &mut out);
        assert!(!out.is_empty() && out.iter().all(|w| w.tile == (-17, 179)));
        // The probe sees the light across the line.
        assert!(east_edge.has_light(|_| true));
        assert!(west_edge.has_light(|_| true));
    }

    /// `recipe` with `layer` missing, as the reader gives a recipe without it.
    fn without(recipe: &Arc<Recipe>, layer: Layer) -> Arc<Recipe> {
        let mut r = (**recipe).clone();
        match layer {
            Layer::Class => r.class = Grid::empty(),
            Layer::Brightness => {
                r.brightness = Grid::empty();
                r.scale = BrightnessScale::default();
            }
        }
        Arc::new(r)
    }

    /// Class weights as (tile, index, weight bits): comparable across
    /// fields, whose slots differ.
    fn ground_weights(out: &[ClassWeight]) -> Vec<((i32, i32), u8, u64)> {
        out.iter()
            .map(|w| (w.tile, w.index, w.weight.to_bits()))
            .collect()
    }

    /// The class weights and radiance of `a` and `b` agree, bit for bit,
    /// at every point of `lats` x `lons`; returns the lit points.
    fn assert_fields_agree(a: &RecipeField, b: &RecipeField, lats: &[f64], lons: &[f64]) -> usize {
        let (mut wa, mut wb, mut lit) = (Vec::new(), Vec::new(), 0);
        for &lat in lats {
            for &lon in lons {
                a.class_weights(lat, lon, &mut wa);
                b.class_weights(lat, lon, &mut wb);
                assert_eq!(ground_weights(&wa), ground_weights(&wb), "at {lat}, {lon}");
                let (ra, rb) = (a.radiance(lat, lon), b.radiance(lat, lon));
                assert_eq!(ra.to_bits(), rb.to_bits(), "at {lat}, {lon}");
                lit += usize::from(!wa.is_empty() && ra > 0.0);
            }
        }
        lit
    }

    #[test]
    fn a_first_tile_without_a_layer_hides_nothing_of_its_neighbours() {
        // (37, -123) has the smallest key; (37, -122) is lit. A field over
        // both, the first without one of its layers, reads that layer as a
        // field without the first recipe does: the neighbour's own.
        let east = synthetic(
            37,
            -122,
            120,
            |r, c| u8::from((r + c) % 3 != 0),
            240,
            |r, c| ((r * 41 + c * 13) % 256) as u8,
        );
        let west = synthetic(37, -123, 120, |_, c| u8::from(c > 60), 240, |_, _| 230);
        let f = footprint(37.6, 37.4, -122.08, -121.9);
        let lats: Vec<f64> = (0..60).map(|k| 37.4 + f64::from(k) * 0.2 / 59.0).collect();
        let mut lons: Vec<f64> = (0..600)
            .map(|k| -122.08 + f64::from(k) * 0.18 / 599.0)
            .collect();
        lons.extend([-122.0, (-122.0f64).next_up(), (-122.0f64).next_down()]);
        let (lats, lons) = (&lats[..], &lons[..]);
        for layer in [Layer::Class, Layer::Brightness] {
            let first = without(&west, layer);
            let (e, w) = (east.clone(), first.clone());
            let both = RecipeField::new(&f, &move |a, b| match (a, b) {
                (37, -123) => Some(w.clone()),
                (37, -122) => Some(e.clone()),
                _ => None,
            });
            let e = east.clone();
            let alone =
                RecipeField::new(&f, &move |a, b| ((a, b) == (37, -122)).then(|| e.clone()));
            // The missing layer is the same in both fields everywhere.
            let (mut wa, mut wb) = (Vec::new(), Vec::new());
            for &lat in lats {
                for &lon in lons {
                    match layer {
                        Layer::Class => {
                            both.class_weights(lat, lon, &mut wa);
                            alone.class_weights(lat, lon, &mut wb);
                            assert_eq!(ground_weights(&wa), ground_weights(&wb), "{lat}, {lon}");
                        }
                        Layer::Brightness => assert_eq!(
                            both.radiance(lat, lon).to_bits(),
                            alone.radiance(lat, lon).to_bits(),
                            "{lat}, {lon}"
                        ),
                    }
                }
            }
            // East of the first tile's class reach, both layers agree.
            let east_lons: Vec<f64> = lons.iter().copied().filter(|&l| l > -121.98).collect();
            assert!(
                assert_fields_agree(&both, &alone, lats, &east_lons) > 0,
                "{layer:?}"
            );
        }
    }

    #[test]
    fn adjacent_footprints_agree_along_their_edge_with_a_mixed_grid_recipe() {
        // Texture X (ZL16 col 10560) spans lon -121.992..-121.904, so its
        // field (grown by the margin) starts in tile (37, -123); texture Y
        // (col 10576) east of it lies in (37, -122) alone. (37, -122) is on
        // other class and brightness grids, with another scale.
        let zl16 = |col| {
            Footprint::from_name(&crate::fuse::DdsFilename {
                row: 25392,
                col,
                zoom: 16,
                map_type: "BI".into(),
                kind: crate::fuse::TextureKind::Lit,
            })
        };
        let (x, y) = (zl16(10560), zl16(10576));
        assert_eq!(
            x.expanded(FIELD_MARGIN_DEG).tiles(),
            vec![(37, -123), (37, -122)]
        );
        assert_eq!(y.expanded(FIELD_MARGIN_DEG).tiles(), vec![(37, -122)]);
        assert_eq!(x.east, y.west);
        let west = synthetic(37, -123, 120, |_, _| 1, 8, |_, _| 120);
        let mut odd = (*synthetic(
            37,
            -122,
            60,
            |r, c| u8::from((r / 2 + c) % 3 != 0),
            4,
            |r, c| ((r * 59 + c * 7) % 256) as u8,
        ))
        .clone();
        odd.scale = BrightnessScale {
            r0_nw: 2.0,
            rmax_nw: 500.0,
        };
        let odd = Arc::new(odd);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(odd.clone()),
            _ => None,
        };
        let (fx, fy) = (RecipeField::new(&x, &lookup), RecipeField::new(&y, &lookup));
        let lats: Vec<f64> = (0..400)
            .map(|k| y.south + (y.north - y.south) * f64::from(k) / 399.0)
            .collect();
        let edge = [x.east.next_down(), x.east, x.east.next_up()];
        assert!(
            assert_fields_agree(&fx, &fy, &lats, &edge) > 0,
            "the edge is lit"
        );
    }

    #[test]
    fn a_tile_is_read_with_its_own_scale() {
        let west = synthetic(37, -123, 120, |_, _| 1, 8, |_, _| 150);
        let mut east = (*synthetic(37, -122, 120, |_, _| 1, 8, |_, _| 150)).clone();
        let scale = BrightnessScale {
            r0_nw: 2.0,
            rmax_nw: 500.0,
        };
        east.scale = scale;
        let east = Arc::new(east);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.3, -121.7), &lookup);
        let want = scale.radiance(150);
        assert!((field.radiance(37.5, -121.8) - want).abs() < 1e-9);
        let default = BrightnessScale::default().radiance(150);
        assert!((field.radiance(37.5, -122.2) - default).abs() < 1e-9);
        assert!((want - default).abs() > 1.0, "the scales differ");
    }

    #[test]
    fn a_point_reads_its_own_tiles_grid_or_the_nearest_one() {
        // (37, -123) on 120 class cells, (37, -122) on 60, (38, -123) on
        // 30; (38, -122) has no recipe.
        let tile = |lat0, lon0, n| synthetic(lat0, lon0, n, |_, _| 1, 8, |_, _| 100);
        let (a, b, c) = (tile(37, -123, 120), tile(37, -122, 60), tile(38, -123, 30));
        let lookup = move |lat: i32, lon: i32| match (lat, lon) {
            (37, -123) => Some(a.clone()),
            (37, -122) => Some(b.clone()),
            (38, -123) => Some(c.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(38.5, 37.5, -122.5, -121.5), &lookup);
        let class = |lat, lon| field.grid_at(Layer::Class, lat, lon).map(|g| g.nx);
        assert_eq!(class(37.9, -121.9), Some(60), "its own tile");
        assert_eq!(class(37.9, -122.1), Some(120));
        assert_eq!(class(38.1, -122.1), Some(30));
        // In (38, -122): the nearest tile with a class layer.
        assert_eq!(class(38.01, -121.6), Some(60), "south");
        assert_eq!(class(38.4, -121.99), Some(30), "west");
        // At (38.125, -121.875) all three are 1/8 away (exactly): the
        // smallest key.
        assert_eq!(class(38.125, -121.875), Some(120));
        // Another field holding the point chooses the same.
        let other = RecipeField::new(&footprint(38.3, 38.001, -121.95, -121.8), &lookup);
        for lat in [38.001, 38.005, 38.009] {
            let at = |f: &RecipeField| f.grid_at(Layer::Class, lat, -121.9).map(|g| g.nx);
            assert_eq!(at(&other), Some(60));
            assert_eq!(at(&other), at(&field));
        }
        // Uniform fields have one grid everywhere.
        let only = RecipeField::new(
            &footprint(37.6, 37.4, -122.6, -122.4),
            &golden_only(37, -123),
        );
        assert_eq!(only.grids(Layer::Class).len(), 1);
        assert_eq!(
            only.grid_at(Layer::Class, 36.0, -120.0).map(|g| g.nx),
            Some(1200)
        );
    }

    #[test]
    fn the_probe_sees_light_on_a_tile_with_another_grid() {
        // The first tile is unlit (class 0) on 120 cells; the lit one east
        // of it is on 60, and the footprint straddles the line.
        let dark = synthetic(37, -123, 120, |_, _| 0, 8, |_, _| 255);
        let lit = synthetic(37, -122, 60, |_, _| 1, 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(dark.clone()),
            (37, -122) => Some(lit.clone()),
            _ => None,
        };
        assert!(probe(
            footprint(37.6, 37.4, -122.08, -121.99),
            lookup,
            |_| true
        ));
    }

    #[test]
    fn sampling_matches_the_code_before_437_without_any_recipe() {
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.6, -122.4), &|_, _| None);
        let lats = axis(37.39, 37.61, 40, &[]);
        let lons = axis(-122.61, -122.39, 40, &[]);
        assert_eq!(assert_matches_reference(&field, &lats, &lons), 0);
    }
}
