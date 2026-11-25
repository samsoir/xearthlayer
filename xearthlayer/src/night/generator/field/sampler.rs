//! The recipe field sampled at the texel centres of one texture.
//!
//! A texel's longitude depends on its column alone and its latitude on its
//! row alone, so everything a sample derives from one of them (grid
//! coordinates, warp noise coordinates, the brightness sample's first cell
//! and fraction) is computed once per column and once per row. The cells
//! the texels can reach are copied once into dense tables, so a cell read
//! is a subtraction and a multiply rather than the divisions that find a
//! global cell's tile. Every value is the bits the per-point code gives.

use super::{
    ClassWeight, Layer, RecipeField, WARP_AMPLITUDE_CELLS, WARP_SEED_X, WARP_SEED_Y,
    WARP_WAVELENGTH_CELLS,
};
use crate::night::generator::noise::NoisePair;
use crate::night::math::floor_i64;

/// Cells of slack around those a table must hold. Reads outside a table
/// are still answered (from the field), so this only keeps them rare.
const TABLE_SLACK_CELLS: i64 = 2;

/// The most table cells per texel. A texture coarser than the recipe grid
/// (a low zoom) reads a small share of the cells it spans, so a table
/// covering them would cost more than it saves; it reads the field instead.
const MAX_TABLE_CELLS_PER_TEXEL: usize = 1;

/// One texel column's (or row's) class sample position: the class grid
/// coordinate and the warp noise coordinate.
#[derive(Debug, Clone, Copy)]
struct ClassAxis {
    g: f64,
    w: f64,
}

impl ClassAxis {
    fn of(g: f64) -> Self {
        Self {
            g,
            w: g / WARP_WAVELENGTH_CELLS,
        }
    }
}

/// One texel column's (or row's) brightness sample: the first bilinear
/// cell, the fraction towards the next and one minus it.
#[derive(Debug, Clone, Copy)]
struct BrightnessAxis {
    c0: i64,
    f: f64,
    rest: f64,
}

impl BrightnessAxis {
    /// `u` in brightness grid units, less half a cell (cell centres).
    fn of(u: f64) -> Self {
        let c0 = floor_i64(u);
        let f = u - c0 as f64;
        Self {
            c0,
            f,
            rest: 1.0 - f,
        }
    }
}

/// A dense row-major rectangle of global cells; may be empty.
#[derive(Debug)]
struct CellTable<T> {
    row0: i64,
    col0: i64,
    rows: u64,
    cols: u64,
    cells: Vec<T>,
}

impl<T: Copy> CellTable<T> {
    /// The cells of the inclusive `rows` and `cols` ranges, each from `cell`;
    /// empty without a range or past `budget` cells.
    fn build(
        rows: Option<(i64, i64)>,
        cols: Option<(i64, i64)>,
        budget: usize,
        cell: impl Fn(i64, i64) -> T,
    ) -> Self {
        let empty = Self {
            row0: 0,
            col0: 0,
            rows: 0,
            cols: 0,
            cells: Vec::new(),
        };
        let (Some((r0, r1)), Some((c0, c1))) = (rows, cols) else {
            return empty;
        };
        let (nr, nc) = ((r1 - r0 + 1) as u64, (c1 - c0 + 1) as u64);
        if nr.saturating_mul(nc) > budget as u64 {
            return empty;
        }
        let mut cells = Vec::with_capacity((nr * nc) as usize);
        for r in r0..=r1 {
            cells.extend((c0..=c1).map(|c| cell(r, c)));
        }
        Self {
            row0: r0,
            col0: c0,
            rows: nr,
            cols: nc,
            cells,
        }
    }

    /// Global cell `(row, col)`, or `None` outside the table.
    #[inline]
    fn get(&self, row: i64, col: i64) -> Option<T> {
        let r = row.wrapping_sub(self.row0) as u64;
        let c = col.wrapping_sub(self.col0) as u64;
        (r < self.rows && c < self.cols).then(|| self.cells[(r * self.cols + c) as usize])
    }
}

/// The lowest and highest of `values`, or `None` for none.
fn span(values: impl Iterator<Item = i64>) -> Option<(i64, i64)> {
    values.fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

/// A class cell as one word: `slot << 8 | index`, 0 for no weight (no
/// recipe, another grid, or class 0, which carries no weight).
fn pack(cell: Option<(usize, u8)>) -> u32 {
    match cell {
        Some((slot, index)) if index != 0 => (slot as u32) << 8 | u32::from(index),
        _ => 0,
    }
}

fn unpack(word: u32) -> Option<(usize, u8)> {
    (word != 0).then_some(((word >> 8) as usize, word as u8))
}

struct ClassLayer {
    cols: Vec<ClassAxis>,
    rows: Vec<ClassAxis>,
    cells: CellTable<u32>,
}

struct BrightnessLayer {
    cols: Vec<BrightnessAxis>,
    rows: Vec<BrightnessAxis>,
    /// Bytes; 0 where no recipe covers the cell.
    cells: CellTable<u8>,
}

/// A [`RecipeField`] sampled at the texel centres of one texture: columns
/// at `lons` and rows at `lats`. Gives the bits of the field's per-point
/// [`RecipeField::class_weights`] and [`RecipeField::radiance`] at texel
/// `(row, col)`, which are this sampler over one point.
///
/// The warp noise keeps the corner values of the last lattice cell it
/// read, so texels sampled in raster order hash once per cell (about 20 to
/// 25 texels each way at ZL16).
pub struct FieldSampler<'f> {
    field: &'f RecipeField,
    class: Option<ClassLayer>,
    brightness: Option<BrightnessLayer>,
    warp: NoisePair,
}

impl<'f> FieldSampler<'f> {
    pub fn new(field: &'f RecipeField, lons: &[f64], lats: &[f64]) -> Self {
        let budget = lons.len() * lats.len() * MAX_TABLE_CELLS_PER_TEXEL;
        let class = field.dims(Layer::Class).map(|(ny, nx)| {
            let cols: Vec<ClassAxis> = lons
                .iter()
                .map(|&lon| ClassAxis::of((lon + 180.0) * nx as f64))
                .collect();
            let rows: Vec<ClassAxis> = lats
                .iter()
                .map(|&lat| ClassAxis::of((90.0 - lat) * ny as f64))
                .collect();
            // The warped position `g + amplitude * noise - 0.5`, noise in
            // [-1, 1], floors to within one cell below `floor(g)`, and the
            // bilinear corners reach one cell above that.
            let reach = |axes: &[ClassAxis]| {
                span(axes.iter().map(|a| floor_i64(a.g)))
                    .map(|(lo, hi)| (lo - 1 - TABLE_SLACK_CELLS, hi + 1 + TABLE_SLACK_CELLS))
            };
            let cells = CellTable::build(reach(&rows), reach(&cols), budget, |r, c| {
                pack(field.layer_cell(Layer::Class, r, c))
            });
            ClassLayer { cols, rows, cells }
        });
        let brightness = field.dims(Layer::Brightness).map(|(ny, nx)| {
            let cols: Vec<BrightnessAxis> = lons
                .iter()
                .map(|&lon| BrightnessAxis::of((lon + 180.0) * nx as f64 - 0.5))
                .collect();
            let rows: Vec<BrightnessAxis> = lats
                .iter()
                .map(|&lat| BrightnessAxis::of((90.0 - lat) * ny as f64 - 0.5))
                .collect();
            let reach = |axes: &[BrightnessAxis]| {
                span(axes.iter().map(|a| a.c0))
                    .map(|(lo, hi)| (lo - TABLE_SLACK_CELLS, hi + 1 + TABLE_SLACK_CELLS))
            };
            let cells = CellTable::build(reach(&rows), reach(&cols), budget, |r, c| {
                field
                    .layer_cell(Layer::Brightness, r, c)
                    .map_or(0, |(_, b)| b)
            });
            BrightnessLayer { cols, rows, cells }
        });
        Self {
            field,
            class,
            brightness,
            warp: NoisePair::new(WARP_SEED_X, WARP_SEED_Y),
        }
    }

    /// The class weights at texel `(row, col)`, into `out`: the bilinear
    /// weights of the four class cell centres around the warped position,
    /// merged per class (see [`RecipeField::class_weights`]).
    pub fn class_weights(&mut self, row: usize, col: usize, out: &mut Vec<ClassWeight>) {
        out.clear();
        let Some(class) = &self.class else {
            return;
        };
        let (x, y) = (class.cols[col], class.rows[row]);
        let (nx, ny) = self.warp.at(x.w, y.w);
        let u = x.g + WARP_AMPLITUDE_CELLS * nx - 0.5;
        let v = y.g + WARP_AMPLITUDE_CELLS * ny - 0.5;
        let (c0, r0) = (floor_i64(u), floor_i64(v));
        let (fx, fy) = (u - c0 as f64, v - r0 as f64);
        for (dr, dc, w) in [
            (0, 0, (1.0 - fx) * (1.0 - fy)),
            (0, 1, fx * (1.0 - fy)),
            (1, 0, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            if w <= 0.0 {
                continue;
            }
            let (r, c) = (r0 + dr, c0 + dc);
            let word = class
                .cells
                .get(r, c)
                .unwrap_or_else(|| pack(self.field.layer_cell(Layer::Class, r, c)));
            let Some((slot, index)) = unpack(word) else {
                continue;
            };
            match out.iter_mut().find(|e| e.slot == slot && e.index == index) {
                Some(e) => e.weight += w,
                None => out.push(ClassWeight {
                    tile: self.field.slot_tile(slot),
                    slot,
                    index,
                    weight: w,
                }),
            }
        }
    }

    /// Radiance (nW/cm2/sr) at texel `(row, col)`: the brightness byte,
    /// bilinear between cell centres, through the field's curve; 0 where
    /// no recipe contributes.
    pub fn radiance(&self, row: usize, col: usize) -> f64 {
        let Some(b) = &self.brightness else {
            return 0.0;
        };
        let (x, y) = (b.cols[col], b.rows[row]);
        let at = |r: i64, c: i64| {
            f64::from(b.cells.get(r, c).unwrap_or_else(|| {
                self.field
                    .layer_cell(Layer::Brightness, r, c)
                    .map_or(0, |(_, v)| v)
            }))
        };
        let top = at(y.c0, x.c0) * x.rest + at(y.c0, x.c0 + 1) * x.f;
        let bottom = at(y.c0 + 1, x.c0) * x.rest + at(y.c0 + 1, x.c0 + 1) * x.f;
        self.field.radiance_of(top * y.rest + bottom * y.f)
    }

    /// Whether the class and brightness tables were built: for tests of
    /// the path that reads the field instead.
    #[cfg(test)]
    pub(crate) fn has_tables(&self) -> (bool, bool) {
        (
            self.class
                .as_ref()
                .is_some_and(|l| !l.cells.cells.is_empty()),
            self.brightness
                .as_ref()
                .is_some_and(|l| !l.cells.cells.is_empty()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_cell_packs_and_unpacks() {
        assert_eq!(unpack(pack(None)), None);
        assert_eq!(
            unpack(pack(Some((3, 0)))),
            None,
            "class 0 carries no weight"
        );
        for cell in [(0, 1), (0, 255), (7, 2), (1 << 20, 9)] {
            assert_eq!(unpack(pack(Some(cell))), Some(cell));
        }
    }

    #[test]
    fn a_table_answers_inside_and_declines_outside() {
        let t = CellTable::build(Some((-3, -1)), Some((5, 8)), 12, |r, c| r * 100 + c);
        assert_eq!(t.get(-3, 5), Some(-295));
        assert_eq!(t.get(-1, 8), Some(-92));
        for (r, c) in [
            (-4, 5),
            (0, 5),
            (-2, 4),
            (-2, 9),
            (i64::MIN, 5),
            (-2, i64::MAX),
        ] {
            assert_eq!(t.get(r, c), None, "{r}, {c}");
        }
        let over = CellTable::build(Some((-3, -1)), Some((5, 8)), 11, |r, c| r * 100 + c);
        assert_eq!(over.get(-2, 6), None, "past the budget: no table");
        let none = CellTable::build(None, Some((5, 8)), 100, |r, c| r * 100 + c);
        assert_eq!(none.get(0, 5), None);
    }
}
