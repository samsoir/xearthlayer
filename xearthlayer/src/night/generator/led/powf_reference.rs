//! The tone path as it was before #436, kept verbatim for tests: every
//! texel decoded by `desaturate` and again by `recolour`, every byte
//! luminance by `powf`, and the floor of each `no_dimmer` step recomputed.
//! The production path ([`super::Toner`]) must give the same bytes.

use super::super::SRGB_GAMMA;
use super::{decode, encode, keep_light, luminance, white, LedModel, SodiumTone, TonePath};

/// The luminance of written bytes: sRGB decoded, Rec.709 weights.
pub(crate) fn byte_luminance(c: [u8; 3]) -> f32 {
    luminance(c.map(|v| (f32::from(v) / 255.0).powf(SRGB_GAMMA)))
}

pub(crate) fn texel_bytes(
    sodium: [f32; 3],
    desaturated: Option<[f32; 3]>,
    led: Option<[f32; 3]>,
    s: f32,
) -> [u8; 3] {
    let quantise = |c: [f32; 3], f: fn(f32) -> f32| c.map(|v| f(v * s).clamp(0.0, 255.0) as u8);
    let no_dimmer = |below: [u8; 3], over: [f32; 3]| {
        let floor = byte_luminance(below);
        let nearest = quantise(over, f32::round);
        if byte_luminance(nearest) >= floor {
            return nearest;
        }
        let up = quantise(over, f32::ceil);
        if byte_luminance(up) >= floor {
            up
        } else {
            below
        }
    };
    let mut bytes = quantise(sodium, f32::round);
    if let Some(d) = desaturated {
        bytes = no_dimmer(bytes, d);
    }
    if let Some(l) = led {
        bytes = no_dimmer(bytes, l);
    }
    bytes
}

pub(crate) fn desaturate(stored: [f32; 3], tone: &SodiumTone) -> [f32; 3] {
    let m = tone.mix.clamp(0.0, 1.0);
    if m <= 0.0 {
        return stored;
    }
    let linear = decode(stored);
    let y = luminance(linear);
    if y <= 0.0 {
        return stored;
    }
    let w = white(tone.kelvin);
    let gain = 1.0 + m * (tone.energy - 1.0);
    let out = [0, 1, 2].map(|c| (linear[c] * (1.0 - m) + w[c] * y * m) * gain);
    encode(keep_light(out, y))
}

pub(crate) fn recolour(stored: [f32; 3], kelvin: f64, energy: f32) -> [f32; 3] {
    let y = luminance(decode(stored));
    if y <= 0.0 {
        return stored;
    }
    encode(keep_light(white(kelvin).map(|c| c * y * energy), y))
}

/// The generator's tone step as it was: `desaturate` when the model tones
/// sodium, `recolour` for an LED texel, then `texel_bytes`.
pub(crate) struct PowfToner<'m>(pub(crate) &'m LedModel);

impl TonePath for PowfToner<'_> {
    fn bytes(&self, sodium: [f32; 3], led_kelvin: Option<f64>, s: f32) -> [u8; 3] {
        let model = self.0;
        let toned = (model.sodium.mix > 0.0).then(|| desaturate(sodium, &model.sodium));
        let led = led_kelvin.map(|k| recolour(sodium, k, model.energy));
        texel_bytes(sodium, toned, led, s)
    }
}
