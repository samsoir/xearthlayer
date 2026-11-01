//! The radiance to light gain curve.

/// Radiance (nW/cm2/sr) at and below which the gain is 0. Set below 2 so
/// thin development around 2 nW keeps a glimmer (gain about 0.05) rather
/// than going black (flight test, 2026-10-17). Provisional.
pub const GAIN_FLOOR_NW: f64 = 1.6;

/// Radiance at and above which the gain is 1. Provisional.
pub const GAIN_CEILING_NW: f64 = 150.0;

/// Gamma applied to the gain ramp. Provisional.
pub const GAIN_GAMMA: f64 = 1.0;

/// Light multiplier for a radiance: a linear ramp in `ln(R)` from the floor
/// (0) to the ceiling (1), raised to the gamma, times `brightness`, clamped
/// to 0..1 (R10).
pub fn gain(radiance_nw: f64, brightness: f64) -> f64 {
    if radiance_nw.is_nan() || radiance_nw <= GAIN_FLOOR_NW {
        return 0.0;
    }
    let t = (radiance_nw.ln() - GAIN_FLOOR_NW.ln()) / (GAIN_CEILING_NW.ln() - GAIN_FLOOR_NW.ln());
    (t.clamp(0.0, 1.0).powf(GAIN_GAMMA) * brightness).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_ramps_from_floor_to_ceiling() {
        assert_eq!(gain(0.0, 1.0), 0.0);
        assert_eq!(gain(GAIN_FLOOR_NW, 1.0), 0.0);
        assert_eq!(gain(GAIN_CEILING_NW, 1.0), 1.0);
        assert_eq!(gain(1_000.0, 1.0), 1.0);
        let mid = gain((GAIN_FLOOR_NW * GAIN_CEILING_NW).sqrt(), 1.0);
        assert!((mid - 0.5).abs() < 1e-9, "log-midpoint is 0.5, got {mid}");
        assert!((gain((GAIN_FLOOR_NW * GAIN_CEILING_NW).sqrt(), 0.5) - 0.25).abs() < 1e-9);
        assert_eq!(gain(1_000.0, 1.5), 1.0, "clamped");
    }

    #[test]
    fn dim_development_at_2_nw_keeps_a_glimmer() {
        let g = gain(2.0, 1.0);
        assert!(
            (0.045..=0.055).contains(&g),
            "gain at 2 nW is about 0.05, got {g}"
        );
    }
}
