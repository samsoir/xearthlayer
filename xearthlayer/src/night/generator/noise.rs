//! Deterministic hashing and smooth value noise, anchored to integer lattices.

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

/// Smooth value noise in `[-1, 1]`: random values at integer lattice points,
/// blended with a smoothstep, so it is continuous everywhere.
pub fn value_noise(x: f64, y: f64, seed: u64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (smooth(x - x0), smooth(y - y0));
    let (ix, iy) = (x0 as i64, y0 as i64);
    let v = |dx: i64, dy: i64| unit(hash2(ix + dx, iy + dy, seed)) * 2.0 - 1.0;
    let top = v(0, 0) * (1.0 - sx) + v(1, 0) * sx;
    let bottom = v(0, 1) * (1.0 - sx) + v(1, 1) * sx;
    top * (1.0 - sy) + bottom * sy
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
}
