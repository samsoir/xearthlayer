//! `class_weights` and `radiance` as they were before #437, kept verbatim
//! for tests: every cell found with `div_euclid` and `rem_euclid`, every
//! floor by `f64::floor`, the warp noise evaluated once per seed, a new
//! class entry's tile found again, and the radiance logarithm taken on
//! every call. The production path ([`super::FieldSampler`]) must give the
//! same bits.

use super::{
    tile_of, ClassWeight, Layer, RecipeField, WARP_AMPLITUDE_CELLS, WARP_SEED_X, WARP_SEED_Y,
    WARP_WAVELENGTH_CELLS,
};
use crate::night::generator::noise::{hash2, unit};

/// `noise::value_noise` before #437.
pub(crate) fn value_noise(x: f64, y: f64, seed: u64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (smooth(x - x0), smooth(y - y0));
    let (ix, iy) = (x0 as i64, y0 as i64);
    let v = |dx: i64, dy: i64| unit(hash2(ix + dx, iy + dy, seed)) * 2.0 - 1.0;
    let top = v(0, 0) * (1.0 - sx) + v(1, 0) * sx;
    let bottom = v(0, 1) * (1.0 - sx) + v(1, 1) * sx;
    top * (1.0 - sy) + bottom * sy
}

/// `RecipeField::class_weights` before #437.
pub(crate) fn class_weights(field: &RecipeField, lat: f64, lon: f64, out: &mut Vec<ClassWeight>) {
    out.clear();
    let Some((ny, nx)) = field.dims(Layer::Class) else {
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
        let (slot, index) = match field.slot_cell(Layer::Class, ny, nx, row, col) {
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

/// `RecipeField::radiance` before #437.
pub(crate) fn radiance(field: &RecipeField, lat: f64, lon: f64) -> f64 {
    let b = brightness_byte(field, lat, lon);
    if b <= 0.0 {
        return 0.0;
    }
    let scale = field.scale;
    let span = (1.0 + scale.rmax_nw / scale.r0_nw).ln();
    scale.r0_nw * ((b / 255.0 * span).exp() - 1.0)
}

fn brightness_byte(field: &RecipeField, lat: f64, lon: f64) -> f64 {
    let Some((ny, nx)) = field.dims(Layer::Brightness) else {
        return 0.0;
    };
    let u = (lon + 180.0) * nx as f64 - 0.5;
    let v = (90.0 - lat) * ny as f64 - 0.5;
    let (c0, r0) = (u.floor(), v.floor());
    let (fx, fy) = (u - c0, v - r0);
    let (c0, r0) = (c0 as i64, r0 as i64);
    let at = |r: i64, c: i64| {
        field
            .cell(Layer::Brightness, ny, nx, r, c)
            .map_or(0.0, f64::from)
    };
    let top = at(r0, c0) * (1.0 - fx) + at(r0, c0 + 1) * fx;
    let bottom = at(r0 + 1, c0) * (1.0 - fx) + at(r0 + 1, c0 + 1) * fx;
    top * (1.0 - fy) + bottom * fy
}
