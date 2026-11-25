//! Deterministic hashing and smooth value noise, anchored to integer lattices.

use crate::night::math::floor_i64;

/// The SplitMix64 finaliser: a well mixed 64-bit hash of `z`.
pub fn mix(z: u64) -> u64 {
    let mut z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A hash of lattice point `(x, y)` under `seed`.
pub fn hash2(x: i64, y: i64, seed: u64) -> u64 {
    mix(mix(seed ^ x as u64).wrapping_add(y as u64))
}

/// A hash as a uniform value in `[0, 1)`.
pub fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// The values at the four corners of lattice cell `(ix, iy)` under `seed`,
/// in `[-1, 1)`: `(0, 0)`, `(1, 0)`, `(0, 1)`, `(1, 1)`.
fn corners(ix: i64, iy: i64, seed: u64) -> [f64; 4] {
    let v = |dx: i64, dy: i64| unit(hash2(ix + dx, iy + dy, seed)) * 2.0 - 1.0;
    [v(0, 0), v(1, 0), v(0, 1), v(1, 1)]
}

/// The corner values blended at smoothstep weights `(sx, sy)`.
fn blend(c: &[f64; 4], sx: f64, sy: f64) -> f64 {
    let top = c[0] * (1.0 - sx) + c[1] * sx;
    let bottom = c[2] * (1.0 - sx) + c[3] * sx;
    top * (1.0 - sy) + bottom * sy
}

/// The lattice cell holding `(x, y)` and the smoothstep weights inside it.
fn cell_of(x: f64, y: f64) -> ((i64, i64), f64, f64) {
    let (ix, iy) = (floor_i64(x), floor_i64(y));
    ((ix, iy), smooth(x - ix as f64), smooth(y - iy as f64))
}

/// Smooth value noise in `[-1, 1]`: random values at integer lattice points,
/// blended with a smoothstep, so it is continuous everywhere. The reference
/// [`NoisePair`] is tested against.
#[cfg(test)]
fn value_noise(x: f64, y: f64, seed: u64) -> f64 {
    let ((ix, iy), sx, sy) = cell_of(x, y);
    blend(&corners(ix, iy, seed), sx, sy)
}

/// A lattice cell and its corner values under each of two seeds.
type CellCorners = ((i64, i64), [[f64; 4]; 2]);

/// Two value noises under two seeds at the same points, from one lattice
/// lookup per point, keeping the corner values of the last lattice cell:
/// points that follow each other closely (a row of texels) share a cell
/// and hash nothing. Each value is the bits `value_noise` gives.
#[derive(Debug, Clone)]
pub struct NoisePair {
    seeds: [u64; 2],
    /// The last lattice cell and its corner values under each seed.
    last: Option<CellCorners>,
}

impl NoisePair {
    pub fn new(a: u64, b: u64) -> Self {
        Self {
            seeds: [a, b],
            last: None,
        }
    }

    /// `(value_noise(x, y, a), value_noise(x, y, b))`.
    pub fn at(&mut self, x: f64, y: f64) -> (f64, f64) {
        let (cell, sx, sy) = cell_of(x, y);
        let c = match &self.last {
            Some((k, c)) if *k == cell => c,
            _ => {
                let c = self.seeds.map(|seed| corners(cell.0, cell.1, seed));
                &self.last.insert((cell, c)).1
            }
        };
        (blend(&c[0], sx, sy), blend(&c[1], sx, sy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_bounded() {
        for i in 0..1000 {
            let (x, y) = (i as f64 * 0.37 - 150.0, i as f64 * 0.91 + 20.0);
            let a = value_noise(x, y, 7);
            assert_eq!(a, value_noise(x, y, 7));
            assert!((-1.0..=1.0).contains(&a), "{a}");
        }
    }

    #[test]
    fn noise_is_continuous() {
        for i in 0..1000 {
            let (x, y) = (i as f64 * 0.731, i as f64 * 0.177);
            assert!((value_noise(x, y, 3) - value_noise(x + 1e-7, y, 3)).abs() < 1e-5);
        }
    }

    #[test]
    fn noise_varies_and_seeds_differ() {
        let values: Vec<f64> = (0..200).map(|i| value_noise(i as f64, 0.0, 1)).collect();
        let mean = values.iter().sum::<f64>() / 200.0;
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / 200.0;
        assert!(var > 0.1, "variance {var}");
        assert_ne!(value_noise(10.5, 3.5, 1), value_noise(10.5, 3.5, 2));
    }

    #[test]
    fn a_noise_pair_gives_the_bits_of_two_value_noises() {
        let (a, b) = (0x5741_5250_5F58_0001, 0x5741_5250_5F59_0002);
        let mut pair = NoisePair::new(a, b);
        // A fine walk (cache hits), jumps back and forth (misses), both
        // signs, and points on and one ulp beside lattice lines.
        let mut points: Vec<(f64, f64)> = (0..4000)
            .map(|i| (f64::from(i) * 0.013 - 20.0, f64::from(i / 50) * 0.29 - 7.0))
            .collect();
        points.extend((0..2000).map(|i| {
            let h = hash2(i, 3, 5);
            (unit(h) * 2e6 - 1e6, unit(mix(h)) * 2e6 - 1e6)
        }));
        for k in -5..5 {
            let x = f64::from(k);
            points.extend([(x, x), (x.next_up(), x), (x.next_down(), -x), (-0.0, x)]);
        }
        for (x, y) in points {
            let (na, nb) = pair.at(x, y);
            assert_eq!(na.to_bits(), value_noise(x, y, a).to_bits(), "{x}, {y}");
            assert_eq!(nb.to_bits(), value_noise(x, y, b).to_bits(), "{x}, {y}");
            // And `value_noise` is the noise before #437.
            let before = super::super::field::lookup_reference::value_noise(x, y, a);
            assert_eq!(na.to_bits(), before.to_bits(), "{x}, {y}");
        }
    }
}
