//! Road light model: per class glow, rural floor, colour, optional lamps.
//! Internal like `LedModel`; starting values from OrthoStudio's measured
//! ratios, tuned by flight.

use sha2::{Digest, Sha256};

use super::class::RoadClass;

/// Bump when the meaning of a `RoadModel` parameter changes, so the same
/// values would draw different light: it is hashed into
/// [`RoadModel::digest`] with the values. A change to how roads are read,
/// merged or drawn is `ROADS_VERSION`'s. 2: lamps light every class, with
/// the class sigma (the lamp's a floor) and intensity (the lamp's a factor).
pub const ROAD_MODEL_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassParams {
    /// Half the lit core's width, ground metres.
    pub half_width_m: f32,
    /// Glow fall-off beyond the core, ground metres (at least one texel when drawn).
    pub sigma_m: f32,
    /// Relative intensity, 0..1.
    pub intensity: f32,
}

/// Roads as chains of lamps instead of a continuous glow: one lamp every
/// `spacing_m` of ground along every glowing road, each a point glow with
/// its class's sigma and intensity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LampParams {
    /// Ground metres between lamps along a road.
    pub spacing_m: f32,
    /// Share of the lamps lit, 0..1 (by a hash of the road and the lamp).
    pub lit_share: f32,
    /// The least sigma a lamp has, ground metres: a lamp's glow fall-off is
    /// the larger of this and its class's `sigma_m`.
    pub sigma_m: f32,
    /// Factor on the class intensity.
    pub intensity: f32,
}

impl LampParams {
    /// Every lamp lit, as X-Plane lines its motorways and arterials. After
    /// the fourth flight (KCCR, 2026-11-14) the glow read a little dense:
    /// at 32 m the road light over 20 ZL16 textures around KCCR is 0.495 of
    /// the continuous glow's (0.547 at 26 m, 0.464 at 36 m; neighbouring
    /// lamps overlap, so the light falls slower than the spacing grows).
    /// The sigma floor is below every glowing class's.
    pub const DEFAULT: Self = Self {
        spacing_m: 32.0,
        lit_share: 1.0,
        sigma_m: 4.0,
        intensity: 1.0,
    };

    /// Sparse lamps whose sigma floor lies between the class sigmas.
    #[cfg(test)]
    pub const TEST: Self = Self {
        spacing_m: 30.0,
        lit_share: 0.3,
        sigma_m: 9.0,
        intensity: 0.8,
    };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoadModel {
    /// Indexed by `RoadClass::index`.
    pub classes: [ClassParams; RoadClass::COUNT],
    /// Linear light of a profile value of 1 at full density.
    pub peak: f32,
    /// Share of the light where VIIRS sees nothing (the rural floor).
    pub country: f32,
    /// Radiance (nW/cm2/sr) from which a road gets full light.
    pub full_nw: f64,
    /// Road sodium colour, stored values 0..255 (toned like the pattern).
    pub sodium: [f32; 3],
    /// Lamps along every glowing road, or the continuous glow when `None`.
    pub lamps: Option<LampParams>,
}

const fn c(half_width_m: f32, sigma_m: f32, intensity: f32) -> ClassParams {
    ClassParams {
        half_width_m,
        sigma_m,
        intensity,
    }
}

impl RoadModel {
    pub const DEFAULT: Self = Self {
        classes: [
            c(8.0, 10.0, 1.00), // Motorway
            c(4.0, 8.0, 0.90),  // Ramp
            c(6.0, 9.0, 0.95),  // Primary
            c(5.0, 8.0, 0.425), // Secondary
            c(3.0, 7.0, 0.60),  // Local
            c(2.0, 6.0, 0.45),  // Minor
            c(1.5, 5.0, 0.30),  // LitFootway
        ],
        // 0.12 at the first flight, three stops down at the second and half
        // a stop more at the fourth (KCCR, 2026-11-14).
        peak: 0.12 / 8.0 / std::f32::consts::SQRT_2,
        country: 0.2,
        full_nw: 10.0,
        sodium: [255.0, 178.0, 92.0],
        lamps: Some(LampParams::DEFAULT),
    };

    /// Digest of every parameter, for the lit session fingerprint.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(ROAD_MODEL_VERSION.to_le_bytes());
        for p in &self.classes {
            for v in [p.half_width_m, p.sigma_m, p.intensity] {
                h.update(v.to_bits().to_le_bytes());
            }
        }
        h.update(self.peak.to_bits().to_le_bytes());
        h.update(self.country.to_bits().to_le_bytes());
        h.update(self.full_nw.to_bits().to_le_bytes());
        for v in self.sodium {
            h.update(v.to_bits().to_le_bytes());
        }
        match self.lamps {
            None => h.update([0u8]),
            Some(l) => {
                h.update([1u8]);
                for v in [l.spacing_m, l.lit_share, l.sigma_m, l.intensity] {
                    h.update(v.to_bits().to_le_bytes());
                }
            }
        }
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_changes_with_every_parameter() {
        let base = RoadModel::DEFAULT.digest();
        assert_eq!(base, RoadModel::DEFAULT.digest());
        let mut changed: Vec<(String, RoadModel)> = vec![];
        let mut with = |what: String, f: &dyn Fn(&mut RoadModel)| {
            let mut m = RoadModel::DEFAULT;
            f(&mut m);
            changed.push((what, m));
        };
        for c in 0..RoadClass::COUNT {
            with(format!("half_width_m {c}"), &|m| {
                m.classes[c].half_width_m += 0.5
            });
            with(format!("sigma_m {c}"), &|m| m.classes[c].sigma_m += 0.5);
            with(format!("intensity {c}"), &|m| {
                m.classes[c].intensity += 0.01
            });
        }
        with("peak".into(), &|m| m.peak += 0.01);
        with("country".into(), &|m| m.country += 0.01);
        with("full_nw".into(), &|m| m.full_nw += 1.0);
        for ch in 0..3 {
            with(format!("sodium {ch}"), &|m| m.sodium[ch] -= 1.0);
        }
        with("lamps".into(), &|m| m.lamps = None);
        let lamp = |f: fn(&mut LampParams)| {
            move |m: &mut RoadModel| {
                let mut l = LampParams::DEFAULT;
                f(&mut l);
                m.lamps = Some(l);
            }
        };
        let mut lamp_changes: Vec<(&str, RoadModel)> = vec![];
        for (what, f) in [
            ("spacing_m", lamp(|l| l.spacing_m += 1.0)),
            ("lit_share", lamp(|l| l.lit_share -= 0.1)),
            ("sigma_m", lamp(|l| l.sigma_m += 1.0)),
            ("intensity", lamp(|l| l.intensity -= 0.1)),
        ] {
            let mut m = RoadModel::DEFAULT;
            f(&mut m);
            lamp_changes.push((what, m));
        }
        for (what, m) in &changed {
            assert_ne!(m.digest(), base, "{what}");
        }
        for (what, m) in &lamp_changes {
            assert_ne!(m.digest(), base, "lamp {what}");
        }
        // Each change is its own input, not just a different one.
        let mut all: Vec<[u8; 32]> = changed.iter().map(|(_, m)| m.digest()).collect();
        all.extend(lamp_changes.iter().map(|(_, m)| m.digest()));
        all.push(base);
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
    }

    #[test]
    fn the_road_glow_is_three_and_a_half_stops_below_its_first_flight() {
        // At KCCR the glow was 98.6% of the lit light, 10 to 20 times the
        // area light of X-Plane's 3D roads beside it, at a peak of 0.12.
        // Three stops in the second flight, half a stop more in the fourth.
        assert_eq!(
            RoadModel::DEFAULT.peak,
            0.12 / 8.0 / std::f32::consts::SQRT_2
        );
    }

    #[test]
    fn default_draws_every_glowing_road_as_a_chain_of_lit_lamps() {
        // Flight 4 (KCCR, 2026-11-14): X-Plane's roads read as dotted
        // chains of lamps, and the glow a little too dense.
        let m = RoadModel::DEFAULT;
        assert_eq!(m.lamps, Some(LampParams::DEFAULT));
        assert_eq!(LampParams::DEFAULT.spacing_m, 32.0, "half the glow's light");
        assert_eq!(LampParams::DEFAULT.lit_share, 1.0, "no random gaps");
        assert_eq!(LampParams::DEFAULT.intensity, 1.0);
        for class in RoadClass::ALL.into_iter().filter(|c| c.is_lit()) {
            assert!(
                LampParams::DEFAULT.sigma_m < m.classes[class.index()].sigma_m,
                "{class:?} lamps have their class sigma"
            );
        }
    }

    #[test]
    fn motorways_are_the_brightest_class() {
        let m = RoadModel::DEFAULT;
        let i = |c: RoadClass| m.classes[c.index()].intensity;
        assert!(i(RoadClass::Motorway) >= i(RoadClass::Primary));
        assert!(i(RoadClass::Primary) > i(RoadClass::Local));
        assert!(i(RoadClass::Local) > i(RoadClass::LitFootway));
    }
}
