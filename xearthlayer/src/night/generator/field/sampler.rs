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

/// Cells of slack around those a table must hold. The tables hold every
/// cell a texel can read with no slack at all (see `FieldSampler::build`);
/// the slack absorbs rounding. A read outside a table is still answered,
/// from the field, with the same bits.
const TABLE_SLACK_CELLS: i64 = 2;

/// How far, in class cells, from a texel's own class cell the class mask
/// looks for class (#438). The warped sample `g + amplitude * noise - 0.5`,
/// noise in [-1, 1], lies in `[g - 1, g]`, so its bilinear corners are
/// within one cell of `floor(g)`; the second cell is slack for rounding.
const MASK_REACH_CELLS: usize = 2;

/// The most table cells per texel. A texture coarser than the recipe grid
/// (a low zoom) reads a small share of the cells it spans, so a table
/// covering them would cost more than it saves; it reads the field instead.
const MAX_TABLE_CELLS_PER_TEXEL: usize = 1;

/// One texel column's (or row's) class sample position: the class grid
/// coordinate, its cell and the warp noise coordinate.
#[derive(Debug, Clone, Copy)]
struct ClassAxis {
    g: f64,
    cell: i64,
    w: f64,
}

impl ClassAxis {
    fn of(g: f64) -> Self {
        Self {
            g,
            cell: floor_i64(g),
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
        if r1 < r0 || c1 < c0 {
            return empty;
        }
        let (nr, nc) = (
            r1.abs_diff(r0).saturating_add(1),
            c1.abs_diff(c0).saturating_add(1),
        );
        let n = match nr.checked_mul(nc) {
            Some(n) if n <= budget as u64 => n as usize,
            _ => return empty,
        };
        let mut cells = Vec::with_capacity(n);
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

impl CellTable<u32> {
    /// The class mask (#438): per cell of this class table, whether a cell
    /// with class (a non-zero word) lies within `MASK_REACH_CELLS` of it
    /// each way. A cell whose neighbourhood runs past the table is marked
    /// too: the cells there are read from the field, not known here.
    fn near_class(&self) -> CellTable<bool> {
        let (rows, cols) = (self.rows as usize, self.cols as usize);
        let n = MASK_REACH_CELLS;
        // Within reach along the row, then along the column of that.
        let mut across = vec![false; self.cells.len()];
        for r in 0..rows {
            let row = &self.cells[r * cols..(r + 1) * cols];
            for c in 0..cols {
                across[r * cols + c] =
                    c < n || c + n >= cols || row[c - n..=c + n].iter().any(|&w| w != 0);
            }
        }
        let mut near = vec![false; self.cells.len()];
        for r in 0..rows {
            for c in 0..cols {
                near[r * cols + c] =
                    r < n || r + n >= rows || (r - n..=r + n).any(|k| across[k * cols + c]);
            }
        }
        CellTable {
            row0: self.row0,
            col0: self.col0,
            rows: self.rows,
            cols: self.cols,
            cells: near,
        }
    }
}

/// The lowest and highest of `values`, or `None` for none.
fn span(values: impl Iterator<Item = i64>) -> Option<(i64, i64)> {
    values.fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

/// `span` grown by `below` cells down and `above` cells up (either may be
/// negative, in tests), saturating at the ends of `i64`: a texel at an
/// extreme position gets no table rather than an overflow.
fn widen(span: Option<(i64, i64)>, below: i64, above: i64) -> Option<(i64, i64)> {
    span.map(|(lo, hi)| (lo.saturating_sub(below), hi.saturating_add(above)))
}

/// A class cell as one word: `slot << 8 | index`, 0 for no weight (no
/// recipe, another grid, or class 0, which carries no weight). A slot is
/// below 2^24: a field holds a few tiles each way.
fn pack(cell: Option<(usize, u8)>) -> u32 {
    match cell {
        Some((slot, index)) if index != 0 => {
            debug_assert!(slot < 1 << 24, "slot {slot} does not fit a class word");
            (slot as u32) << 8 | u32::from(index)
        }
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
    /// `cells.near_class()`; empty (so it skips nothing) without the mask.
    near: CellTable<bool>,
}

struct BrightnessLayer {
    cols: Vec<BrightnessAxis>,
    rows: Vec<BrightnessAxis>,
    /// Bytes; 0 where no recipe covers the cell.
    cells: CellTable<u8>,
}

/// Where a [`FieldSampler`] samples: the latitude of each texel row (north
/// first) and the longitude of each texel column (west first). Named
/// fields rather than two slices, so a call cannot swap them unseen; the
/// sampler's methods take `(row, col)`.
#[derive(Debug, Clone, Copy)]
pub struct TexelAxes<'a> {
    pub lats: &'a [f64],
    pub lons: &'a [f64],
}

/// A [`RecipeField`] sampled at the texel centres of one texture: rows at
/// `axes.lats` and columns at `axes.lons`. Gives the bits of the field's
/// per-point `RecipeField::class_weights` and `RecipeField::radiance` (test
/// only) at texel `(row, col)`, which are this sampler over one point.
///
/// The warp noise keeps the corner values of the last lattice cell it
/// read, so texels sampled in raster order hash once per cell (about 20 to
/// 25 texels each way at ZL16).
///
/// With the class mask (#438), a texel whose own class cell has no class
/// cell within `MASK_REACH_CELLS` gets its empty weights without the warp
/// noise or a cell read: about a third of all texels on real textures.
pub struct FieldSampler<'f> {
    field: &'f RecipeField,
    class: Option<ClassLayer>,
    brightness: Option<BrightnessLayer>,
    warp: NoisePair,
    /// Reads that fell outside a built table and went to the field.
    #[cfg(test)]
    misses: std::cell::Cell<u64>,
}

impl<'f> FieldSampler<'f> {
    /// The sampler, with the class mask.
    pub fn new(field: &'f RecipeField, axes: TexelAxes<'_>) -> Self {
        Self::with_class_mask(field, axes, true)
    }

    /// The sampler, with the class mask or without it: the same weights
    /// either way (tests and timings compare the two).
    pub fn with_class_mask(field: &'f RecipeField, axes: TexelAxes<'_>, mask: bool) -> Self {
        Self::build(field, axes, TABLE_SLACK_CELLS, mask)
    }

    /// The sampler with `slack` cells around the cells its tables must
    /// hold; tests pass a negative slack to make reads miss the tables.
    fn build(field: &'f RecipeField, axes: TexelAxes<'_>, slack: i64, mask: bool) -> Self {
        let TexelAxes { lats, lons } = axes;
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
                widen(
                    span(axes.iter().map(|a| floor_i64(a.g))),
                    1 + slack,
                    1 + slack,
                )
            };
            let cells = CellTable::build(reach(&rows), reach(&cols), budget, |r, c| {
                pack(field.layer_cell(Layer::Class, r, c))
            });
            let near = if mask {
                cells.near_class()
            } else {
                CellTable::build(None, None, 0, |_, _| true)
            };
            ClassLayer {
                cols,
                rows,
                cells,
                near,
            }
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
            let reach =
                |axes: &[BrightnessAxis]| widen(span(axes.iter().map(|a| a.c0)), slack, 1 + slack);
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
            #[cfg(test)]
            misses: std::cell::Cell::new(0),
        }
    }

    /// Counts a read outside a built table (tests only).
    #[inline]
    fn missed(&self, table_built: bool) {
        #[cfg(test)]
        self.misses.set(self.misses.get() + u64::from(table_built));
        #[cfg(not(test))]
        let _ = table_built;
    }

    /// The class weights at texel `(row, col)`, into `out`: the bilinear
    /// weights of the four class cell centres around the warped position,
    /// merged per class. Class 0 and cells without a recipe carry no entry,
    /// so the weights sum to less than 1 near the edge of classified ground.
    pub fn class_weights(&mut self, row: usize, col: usize, out: &mut Vec<ClassWeight>) {
        out.clear();
        let Some(class) = &self.class else {
            return;
        };
        let (x, y) = (class.cols[col], class.rows[row]);
        if class.near.get(y.cell, x.cell) == Some(false) {
            return;
        }
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
            let word = class.cells.get(r, c).unwrap_or_else(|| {
                self.missed(!class.cells.cells.is_empty());
                pack(self.field.layer_cell(Layer::Class, r, c))
            });
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
                self.missed(!b.cells.cells.is_empty());
                self.field
                    .layer_cell(Layer::Brightness, r, c)
                    .map_or(0, |(_, v)| v)
            }))
        };
        let top = at(y.c0, x.c0) * x.rest + at(y.c0, x.c0 + 1) * x.f;
        let bottom = at(y.c0 + 1, x.c0) * x.rest + at(y.c0 + 1, x.c0 + 1) * x.f;
        self.field.radiance_of(top * y.rest + bottom * y.f)
    }

    /// A sampler whose tables hold `slack` cells around the cells it must
    /// read (negative: fewer), for tests of reads outside a table.
    #[cfg(test)]
    pub(crate) fn with_slack(field: &'f RecipeField, axes: TexelAxes<'_>, slack: i64) -> Self {
        Self::build(field, axes, slack, true)
    }

    /// Whether the class mask gives texel `(row, col)` its empty weights
    /// without sampling.
    #[cfg(test)]
    pub(crate) fn skips_class(&self, row: usize, col: usize) -> bool {
        self.class
            .as_ref()
            .is_some_and(|c| c.near.get(c.rows[row].cell, c.cols[col].cell) == Some(false))
    }

    /// Reads so far that fell outside a built table and went to the field.
    #[cfg(test)]
    pub(crate) fn table_misses(&self) -> u64 {
        self.misses.get()
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
        let inverted = CellTable::build(Some((2, 1)), Some((5, 8)), 100, |r, c| r * 100 + c);
        assert!(inverted.cells.is_empty(), "an empty range: no table");
    }

    #[test]
    fn the_class_mask_marks_cells_within_reach_of_class() {
        // A 9 x 9 table with class at (14, 24) only (table cell (4, 4)).
        let t = CellTable::build(Some((10, 18)), Some((20, 28)), 81, |r, c| {
            u32::from((r, c) == (14, 24)) << 8 | u32::from((r, c) == (14, 24))
        });
        let near = t.near_class();
        for r in 10i64..=18 {
            for c in 20i64..=28 {
                let edge = !(12..=16).contains(&r) || !(22..=26).contains(&c);
                let within = r.abs_diff(14) <= 2 && c.abs_diff(24) <= 2;
                assert_eq!(near.get(r, c), Some(edge || within), "{r}, {c}");
            }
        }
        assert_eq!(near.get(9, 24), None, "outside the table");
        // No class: only the cells whose reach runs past the table.
        let none = CellTable::build(Some((0, 8)), Some((0, 8)), 81, |_, _| 0u32).near_class();
        assert_eq!(none.cells.iter().filter(|&&m| !m).count(), 5 * 5);
        // A table narrower than the reach marks every cell.
        let thin = CellTable::build(Some((0, 3)), Some((0, 30)), 200, |_, _| 0u32).near_class();
        assert!(thin.cells.iter().all(|&m| m));
    }

    #[test]
    fn extreme_ranges_neither_overflow_nor_build() {
        let all = Some((i64::MIN, i64::MAX));
        let t = CellTable::build(all, all, usize::MAX, |_, _| 0u8);
        assert!(t.cells.is_empty());
        assert_eq!(t.get(0, 0), None);
        assert_eq!(widen(Some((i64::MIN, i64::MAX)), 3, 3), all);
        assert_eq!(widen(Some((i64::MIN + 1, i64::MAX - 1)), 3, 3), all);
        assert_eq!(widen(Some((10, 20)), 3, 4), Some((7, 24)));
        assert_eq!(widen(Some((10, 20)), -3, -4), Some((13, 16)));
        assert_eq!(widen(None, 3, 3), None);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "does not fit")]
    fn a_slot_past_24_bits_is_caught() {
        pack(Some((1 << 24, 1)));
    }
}
