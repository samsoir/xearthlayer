//! Road light model: per class glow, rural floor, colour, optional lamps.
//! Internal like `LedModel`; starting values from OrthoStudio's measured
//! ratios, tuned by flight.

use sha2::{Digest, Sha256};

use super::class::RoadClass;

/// Bump when the meaning of a `RoadModel` parameter changes, so the same
/// values would draw different light: it is hashed into
/// [`RoadModel::digest`] with the values. A change to how roads are read,
/// merged or drawn is `ROADS_VERSION`'s.
pub const ROAD_MODEL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassParams {
    /// Half the lit core's width, ground metres.
    pub half_width_m: f32,
    /// Glow fall-off beyond the core, ground metres (at least one texel when drawn).
    pub sigma_m: f32,
    /// Relative intensity, 0..1.
    pub intensity: f32,
}

/// Sparse lamps instead of glow on every non-motorway class (testing option).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LampParams {
    pub spacing_m: f32,
    pub lit_share: f32,
    pub sigma_m: f32,
    pub intensity: f32,
}

impl LampParams {
    pub const TEST: Self = Self {
        spacing_m: 30.0,
        lit_share: 0.3,
        sigma_m: 4.0,
        intensity: 1.0,
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
    pub rural_lamps: Option<LampParams>,
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
        rural_lamps: None,
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
        match self.rural_lamps {
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
        with("rural_lamps".into(), &|m| {
            m.rural_lamps = Some(LampParams::TEST)
        });
        let lamp = |f: fn(&mut LampParams)| {
            move |m: &mut RoadModel| {
                let mut l = LampParams::TEST;
                f(&mut l);
                m.rural_lamps = Some(l);
            }
        };
        let lamps_on = {
            let mut m = RoadModel::DEFAULT;
            m.rural_lamps = Some(LampParams::TEST);
            m.digest()
        };
        let mut lamp_changes: Vec<(&str, RoadModel)> = vec![];
        for (what, f) in [
            ("spacing_m", lamp(|l| l.spacing_m += 1.0)),
            ("lit_share", lamp(|l| l.lit_share += 0.1)),
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
            assert_ne!(m.digest(), lamps_on, "lamp {what}");
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
    fn default_has_no_rural_lamps_and_brightest_motorways() {
        let m = RoadModel::DEFAULT;
        assert!(m.rural_lamps.is_none());
        let i = |c: RoadClass| m.classes[c.index()].intensity;
        assert!(i(RoadClass::Motorway) >= i(RoadClass::Primary));
        assert!(i(RoadClass::Primary) > i(RoadClass::Local));
        assert!(i(RoadClass::Local) > i(RoadClass::LitFootway));
    }
}
