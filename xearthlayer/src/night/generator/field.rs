//! Recipe layers sampled as continuous functions of world position.
//!
//! Cells are indexed globally (row 0 at the north pole, column 0 at 180 W),
//! so a sample near a 1 degree edge reads the neighbouring tile's cells and
//! every texture computes the same value for the same ground.

use std::sync::Arc;

use super::gain::gain;
use super::noise::value_noise;
use super::RecipeLookup;
use crate::night::footprint::Footprint;
use crate::night::recipe::{BrightnessScale, Grid, Recipe};

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
/// layer with `ny` x `nx` cells per degree.
fn tile_of(row: i64, col: i64, ny: i64, nx: i64) -> (i64, i64) {
    (89 - row.div_euclid(ny), col.div_euclid(nx) - 180)
}

/// The recipes one footprint (plus margin) touches, fetched once.
///
/// Grid sizes and the brightness scale come from one recipe (the one with
/// the smallest tile key, so the choice is deterministic) and apply to the
/// whole field: version 1 recipes all share them, and a tile whose grid
/// differs reads as missing. Taking them from the field rather than from the
/// tile containing a point keeps a sample next to a missing tile correct.
///
/// The recipes sit in a small row-major table of tile slots (a field spans
/// at most a few tiles each way), so a cell read is index arithmetic, not a
/// hash lookup.
pub struct RecipeField {
    footprint: Footprint,
    south0: i64,
    west0: i64,
    lats: i64,
    lons: i64,
    slots: Vec<Option<Arc<Recipe>>>,
    class_dims: Option<(usize, usize)>,
    brightness_dims: Option<(usize, usize)>,
    scale: BrightnessScale,
}

impl RecipeField {
    pub fn new(footprint: &Footprint, lookup: RecipeLookup<'_>) -> Self {
        let tiles = footprint.expanded(FIELD_MARGIN_DEG).tiles();
        let (mut south0, mut north0, mut west0, mut east0) = (0i64, -1i64, 0i64, -1i64);
        if let Some(&(lat, lon)) = tiles.first() {
            (south0, north0, west0, east0) = (lat.into(), lat.into(), lon.into(), lon.into());
        }
        for &(lat, lon) in &tiles {
            south0 = south0.min(lat.into());
            north0 = north0.max(lat.into());
            west0 = west0.min(lon.into());
            east0 = east0.max(lon.into());
        }
        let (lats, lons) = (north0 - south0 + 1, east0 - west0 + 1);
        let mut slots: Vec<Option<Arc<Recipe>>> = vec![None; (lats * lons) as usize];
        for (lat, lon) in tiles {
            let i = (i64::from(lat) - south0) * lons + (i64::from(lon) - west0);
            slots[i as usize] = lookup(lat, lon);
        }
        // Slots run south to north, then west to east: the first filled one
        // is the smallest (lat, lon) key.
        let first = slots.iter().flatten().next();
        let dims = |g: &Grid| (g.height, g.width);
        Self {
            footprint: *footprint,
            class_dims: first.map(|r| dims(&r.class)),
            brightness_dims: first.map(|r| dims(&r.brightness)),
            scale: first.map(|r| r.scale).unwrap_or_default(),
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

    /// The slot of tile `(lat0, lon0)`, or `None` outside the field.
    fn slot_of(&self, lat0: i64, lon0: i64) -> Option<usize> {
        let (dy, dx) = (lat0 - self.south0, lon0 - self.west0);
        ((0..self.lats).contains(&dy) && (0..self.lons).contains(&dx))
            .then(|| (dy * self.lons + dx) as usize)
    }

    fn slot_recipe(&self, slot: usize) -> Option<&Arc<Recipe>> {
        self.slots.get(slot)?.as_ref()
    }

    pub(crate) fn recipe(&self, lat0: i32, lon0: i32) -> Option<&Arc<Recipe>> {
        self.slot_recipe(self.slot_of(lat0.into(), lon0.into())?)
    }

    /// Rows and columns per degree of `layer` across the field; `None` when
    /// the field has no recipe.
    pub(crate) fn dims(&self, layer: Layer) -> Option<(usize, usize)> {
        match layer {
            Layer::Class => self.class_dims,
            Layer::Brightness => self.brightness_dims,
        }
    }

    /// The slot and value of global cell `(row, col)` of a layer with
    /// `ny` x `nx` cells per degree; `None` without a recipe or when that
    /// tile's grid differs.
    fn slot_cell(
        &self,
        layer: Layer,
        ny: usize,
        nx: usize,
        row: i64,
        col: i64,
    ) -> Option<(usize, u8)> {
        let (ny_i, nx_i) = (ny as i64, nx as i64);
        let (lat0, lon0) = tile_of(row, col, ny_i, nx_i);
        let slot = self.slot_of(lat0, lon0)?;
        let g = Self::grid(self.slot_recipe(slot)?, layer);
        if g.height != ny || g.width != nx {
            return None;
        }
        Some((
            slot,
            g.get(row.rem_euclid(ny_i) as usize, col.rem_euclid(nx_i) as usize),
        ))
    }

    /// The value of global cell `(row, col)` of a layer with `ny` x `nx` cells
    /// per degree; `None` without a recipe or when that tile's grid differs.
    pub(crate) fn cell(
        &self,
        layer: Layer,
        ny: usize,
        nx: usize,
        row: i64,
        col: i64,
    ) -> Option<u8> {
        self.slot_cell(layer, ny, nx, row, col).map(|(_, v)| v)
    }

    /// The brightness byte at a point, bilinear between cell centres; 0 where
    /// no recipe covers a neighbour.
    fn brightness_byte(&self, lat: f64, lon: f64) -> f64 {
        let Some((ny, nx)) = self.dims(Layer::Brightness) else {
            return 0.0;
        };
        let u = (lon + 180.0) * nx as f64 - 0.5;
        let v = (90.0 - lat) * ny as f64 - 0.5;
        let (c0, r0) = (u.floor(), v.floor());
        let (fx, fy) = (u - c0, v - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        let at = |r: i64, c: i64| {
            self.cell(Layer::Brightness, ny, nx, r, c)
                .map_or(0.0, f64::from)
        };
        let top = at(r0, c0) * (1.0 - fx) + at(r0, c0 + 1) * fx;
        let bottom = at(r0 + 1, c0) * (1.0 - fx) + at(r0 + 1, c0 + 1) * fx;
        top * (1.0 - fy) + bottom * fy
    }

    /// Radiance of a (possibly fractional) brightness byte; 0 for none.
    fn radiance_of(&self, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        self.scale.radiance_at(b)
    }

    /// Radiance (nW/cm2/sr) at a point; 0 where no recipe contributes.
    pub fn radiance(&self, lat: f64, lon: f64) -> f64 {
        self.radiance_of(self.brightness_byte(lat, lon))
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
    pub fn class_weights(&self, lat: f64, lon: f64, out: &mut Vec<ClassWeight>) {
        out.clear();
        let Some((ny, nx)) = self.dims(Layer::Class) else {
            return;
        };
        let gx = (lon + 180.0) * nx as f64;
        let gy = (90.0 - lat) * ny as f64;
        let (wx, wy) = (gx / WARP_WAVELENGTH_CELLS, gy / WARP_WAVELENGTH_CELLS);
        let u = gx + WARP_AMPLITUDE_CELLS * value_noise(wx, wy, WARP_SEED_X) - 0.5;
        let v = gy + WARP_AMPLITUDE_CELLS * value_noise(wx, wy, WARP_SEED_Y) - 0.5;
        let (c0, r0) = (u.floor(), v.floor());
        let (fx, fy) = (u - c0, v - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        for (dr, dc, w) in [
            (0, 0, (1.0 - fx) * (1.0 - fy)),
            (0, 1, fx * (1.0 - fy)),
            (1, 0, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            if w <= 0.0 {
                continue;
            }
            let (row, col) = (r0 + dr, c0 + dc);
            let (slot, index) = match self.slot_cell(Layer::Class, ny, nx, row, col) {
                Some((s, i)) if i != 0 => (s, i),
                _ => continue,
            };
            match out.iter_mut().find(|e| e.slot == slot && e.index == index) {
                Some(e) => e.weight += w,
                None => {
                    let (lat0, lon0) = tile_of(row, col, ny as i64, nx as i64);
                    out.push(ClassWeight {
                        tile: (lat0 as i32, lon0 as i32),
                        slot,
                        index,
                        weight: w,
                    });
                }
            }
        }
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
    pub fn has_light(&self, is_lit: impl Fn(&str) -> bool) -> bool {
        let (Some((cny, cnx)), Some((bny, bnx))) =
            (self.dims(Layer::Class), self.dims(Layer::Brightness))
        else {
            return false;
        };
        let f = &self.footprint;
        let (cy, cx) = (cny as f64, cnx as f64);
        let (by, bx) = (bny as f64, bnx as f64);
        // The footprint in class grid units (gy grows southwards).
        let (gx_w, gx_e) = ((f.west + 180.0) * cx, (f.east + 180.0) * cx);
        let (gy_n, gy_s) = ((90.0 - f.north) * cy, (90.0 - f.south) * cy);
        // Cells whose centre (`index + 0.5`) is within reach of the footprint.
        let reach = CLASS_REACH_CELLS + 0.5 + PROBE_SLACK_CELLS;
        let rows = (gy_n - reach).floor() as i64..=(gy_s + reach).floor() as i64;
        let cols = (gx_w - reach).floor() as i64..=(gx_e + reach).floor() as i64;

        // Per (slot, class index): unknown, lit or not.
        let mut lit_class: Vec<Option<bool>> = vec![None; self.slots.len() * 256];
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
                let Some((slot, index)) = self.slot_cell(Layer::Class, cny, cnx, row, col) else {
                    continue;
                };
                if index == 0 {
                    continue;
                }
                let known = &mut lit_class[slot * 256 + usize::from(index)];
                let lit = *known
                    .get_or_insert_with(|| self.palette_name_in(slot, index).is_some_and(&is_lit));
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
                        let b = self.cell(Layer::Brightness, bny, bnx, br, bc);
                        if b.is_some_and(|b| gain(self.radiance_of(f64::from(b)), 1.0) > 0.0) {
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
        assert_eq!(
            field.cell(Layer::Brightness, 4, 4, (89 - 38) * 4 + 3, 4 * 57 + 1),
            Some(100)
        );
        assert_eq!(
            field.cell(Layer::Brightness, 4, 4, (89 - 37) * 4, 4 * 57 + 1),
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
}
