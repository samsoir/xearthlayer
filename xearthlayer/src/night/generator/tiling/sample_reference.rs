//! `sample_cached` and `LitPattern::sample` as they were before #437, kept
//! verbatim for tests: every floor by `f64::floor` and every pattern tap
//! wrapped with `rem_euclid`. The production path ([`super::sample_cached`])
//! must give the same bits.

use super::{barycentric, CornerCache, BLEND_SHARPNESS, SKEW, UNSKEW};
use crate::night::laminar::LitPattern;

/// `tiling::sample_cached` before #437.
pub(crate) fn sample_cached(
    pattern: &LitPattern,
    x: f64,
    y: f64,
    cache: &mut CornerCache,
) -> [f32; 3] {
    let cell = pattern.ground_scale_m;
    let (a, b) = (x / cell, y / cell);
    let s = (a + b) * SKEW;
    let (i, j) = ((a + s).floor(), (b + s).floor());
    let t = (i + j) * UNSKEW;
    let (x0, y0) = (a - (i - t), b - (j - t));
    let (i1, j1) = if x0 > y0 { (1.0, 0.0) } else { (0.0, 1.0) };
    let corners = [(i, j), (i + i1, j + j1), (i + 1.0, j + 1.0)];
    let unskewed = corners.map(|(ci, cj)| {
        let t = (ci + cj) * UNSKEW;
        (ci - t, cj - t)
    });
    let weights = barycentric((a, b), unskewed).map(|w| w.max(0.0).powi(BLEND_SHARPNESS));
    let total: f64 = weights.iter().sum();
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        let w = (weights[k] / total) as f32;
        if w == 0.0 {
            continue;
        }
        let corner = cache.get(cell, corners[k], unskewed[k]);
        let (du, dv) = ((x - corner.vx) * corner.k, -(y - corner.vy) * corner.k);
        let (u, v) = match corner.turns {
            0 => (du, dv),
            1 => (-dv, du),
            2 => (-du, -dv),
            _ => (dv, -du),
        };
        let c = pattern_sample(pattern, u + corner.ou, v + corner.ov);
        for ch in 0..3 {
            out[ch] += w * c[ch];
        }
    }
    out
}

/// `LitPattern::sample` before #437.
pub(crate) fn pattern_sample(p: &LitPattern, u: f64, v: f64) -> [f32; 3] {
    let n = p.size as f64;
    let (x, y) = (u * n - 0.5, v * n - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
    let wrap = |i: f64| (i as i64).rem_euclid(p.size as i64) as usize;
    let (xa, xb, ya, yb) = (wrap(x0), wrap(x0 + 1.0), wrap(y0), wrap(y0 + 1.0));
    let at = |x: usize, y: usize| p.rgb[y * p.size as usize + x];
    let mut out = [0.0f32; 3];
    for (ch, o) in out.iter_mut().enumerate() {
        let top = at(xa, ya)[ch] as f32 * (1.0 - fx) + at(xb, ya)[ch] as f32 * fx;
        let bottom = at(xa, yb)[ch] as f32 * (1.0 - fx) + at(xb, yb)[ch] as f32 * fx;
        *o = top * (1.0 - fy) + bottom * fy;
    }
    out
}
