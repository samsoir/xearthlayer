//! LED street lighting: which neighbourhoods of a lit texture are LED, at
//! what colour temperature, and how a sodium texel becomes white.
//!
//! The ground is cut into jittered cells (patches) in Web Mercator metres.
//! Each patch draws one value `u`; a texel is LED when `u` is below the share
//! its category and region give. The recolour keeps the texel's luminance,
//! times `LedModel::energy`, so Laminar's street pattern survives and LED
//! never reads dimmer than the sodium it replaces.

use super::noise::{hash2, unit};
use super::SRGB_GAMMA;

/// Where LED adoption and colour temperature come from: the continent prefix
/// of the package region code the recipe was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
///
/// The discriminants are load-bearing: `index` selects the row of
/// `LedModel::regions` and `tag` goes into the disk fingerprint, so they are
/// explicit and pinned by tests.
#[repr(u8)]
pub enum LedRegion {
    Na = 0,
    Sa = 1,
    Eu = 2,
    Af = 3,
    As = 4,
    Oc = 5,
    /// A patch, a code without a known continent, or no recipe.
    Unknown = 6,
}

impl LedRegion {
    pub const COUNT: usize = 7;

    /// The region of a package code: its first `-` segment, any case.
    pub fn from_code(code: &str) -> Self {
        let head = code.split('-').next().unwrap_or("");
        match head.to_ascii_uppercase().as_str() {
            "NA" => Self::Na,
            "SA" => Self::Sa,
            "EU" => Self::Eu,
            "AF" => Self::Af,
            "AS" => Self::As,
            "OC" => Self::Oc,
            _ => Self::Unknown,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    /// A stable byte for the disk fingerprint.
    pub fn tag(self) -> u8 {
        self as u8
    }
}

/// How likely a texel is to be LED, and the colour temperature it takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tone {
    pub share: f64,
    pub kelvin: f64,
}

/// A region's scale on the share and shift on the temperature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionTone {
    pub share_scale: f64,
    pub kelvin_shift: f64,
}

/// The whole LED model. Provisional values, tuned in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct LedModel {
    pub industrial: Tone,
    pub urban_bright: Tone,
    pub urban_dim: Tone,
    /// Indexed by `LedRegion::index`.
    pub regions: [RegionTone; LedRegion::COUNT],
    /// Luminance multiplier of an LED texel over the sodium it replaces.
    /// Above 1: saturated orange looks brighter than white of equal
    /// luminance, and LED should read brighter, not merely equal.
    pub energy: f32,
}

const fn region(share_scale: f64, kelvin_shift: f64) -> RegionTone {
    RegionTone {
        share_scale,
        kelvin_shift,
    }
}

impl LedModel {
    pub const DEFAULT: Self = Self {
        industrial: Tone {
            share: 0.80,
            kelvin: 4500.0,
        },
        urban_bright: Tone {
            share: 0.75,
            kelvin: 4000.0,
        },
        urban_dim: Tone {
            share: 0.45,
            kelvin: 3200.0,
        },
        // Na, Sa, Eu, Af, As, Oc, Unknown
        regions: [
            region(1.0, 400.0),
            region(0.7, 400.0),
            region(0.9, -400.0),
            region(0.5, 400.0),
            region(0.8, -400.0),
            region(0.9, -400.0),
            region(0.75, 0.0),
        ],
        energy: 1.25,
    };

    /// No LED anywhere: the generator's output is the sodium-only one.
    pub const OFF: Self = Self {
        industrial: Tone {
            share: 0.0,
            kelvin: 4500.0,
        },
        urban_bright: Tone {
            share: 0.0,
            kelvin: 4000.0,
        },
        urban_dim: Tone {
            share: 0.0,
            kelvin: 3200.0,
        },
        ..Self::DEFAULT
    };

    /// The tone of one class at a texel: industrial, or urban blended from
    /// dim to bright by `bright` (0..1), then the region applied.
    pub fn tone(&self, industrial: bool, bright: f64, region: LedRegion) -> Tone {
        let base = if industrial {
            self.industrial
        } else {
            let b = bright.clamp(0.0, 1.0);
            Tone {
                share: self.urban_dim.share + (self.urban_bright.share - self.urban_dim.share) * b,
                kelvin: self.urban_dim.kelvin
                    + (self.urban_bright.kelvin - self.urban_dim.kelvin) * b,
            }
        };
        let r = self.regions[region.index()];
        Tone {
            share: (base.share * r.share_scale).clamp(0.0, 1.0),
            kelvin: base.kelvin + r.kelvin_shift,
        }
    }
}

/// Radiance (nW/cm2/sr) at and below which urban counts as dim.
pub const URBAN_BRIGHT_LO_NW: f64 = 25.0;
/// Radiance at and above which urban counts as bright (downtown).
pub const URBAN_BRIGHT_HI_NW: f64 = 80.0;

/// How bright (0 dim, 1 downtown) urban ground is: a smoothstep in `ln(R)`.
pub fn bright_weight(radiance_nw: f64) -> f64 {
    if radiance_nw.is_nan() || radiance_nw <= 0.0 {
        return 0.0;
    }
    let t = ((radiance_nw.ln() - URBAN_BRIGHT_LO_NW.ln())
        / (URBAN_BRIGHT_HI_NW.ln() - URBAN_BRIGHT_LO_NW.ln()))
    .clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Whether a recipe palette name is a Laminar industrial terrain: its file
/// stem, trailing digits removed, ends in `_ind`.
pub fn is_industrial(palette_name: &str) -> bool {
    let file = palette_name.rsplit('/').next().unwrap_or(palette_name);
    let stem = file.strip_suffix(".ter").unwrap_or(file);
    stem.trim_end_matches(|c: char| c.is_ascii_digit())
        .ends_with("_ind")
}

/// Width of a patch cell in Web Mercator metres (about 53 texels, 13 BC1
/// blocks, at the ZL16 first visible level).
pub const PATCH_SIZE_M: f64 = 400.0;
/// How far a cell's centre may move from the cell middle, as a fraction of
/// the cell (both axes, centred), so patches are irregular.
const PATCH_JITTER: f64 = 0.8;
const PATCH_SEED_X: u64 = 0x4C45_445F_5041_5458;
const PATCH_SEED_Y: u64 = 0x4C45_445F_5041_5459;
const PATCH_SEED_U: u64 = 0x4C45_445F_5041_5455;
const PATCH_SEED_J: u64 = 0x4C45_445F_5041_544A;

/// One patch's draws: `u` in `[0, 1)` decides LED, `jitter` in `[-1, 1]`
/// moves its temperature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Patch {
    pub u: f64,
    pub jitter: f64,
}

/// The patch at Web Mercator `(x, y)`: the nearest of the 3 x 3 jittered
/// cell centres around it. A function of position only.
pub fn patch_at(x: f64, y: f64) -> Patch {
    let (gx, gy) = (x / PATCH_SIZE_M, y / PATCH_SIZE_M);
    let (cx, cy) = (gx.floor() as i64, gy.floor() as i64);
    let mut best = (f64::INFINITY, cx, cy);
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (ix, iy) = (cx + dx, cy + dy);
            let ox = (unit(hash2(ix, iy, PATCH_SEED_X)) - 0.5) * PATCH_JITTER;
            let oy = (unit(hash2(ix, iy, PATCH_SEED_Y)) - 0.5) * PATCH_JITTER;
            let (px, py) = (ix as f64 + 0.5 + ox, iy as f64 + 0.5 + oy);
            let d = (gx - px).powi(2) + (gy - py).powi(2);
            if d < best.0 {
                best = (d, ix, iy);
            }
        }
    }
    let (_, ix, iy) = best;
    Patch {
        u: unit(hash2(ix, iy, PATCH_SEED_U)),
        jitter: unit(hash2(ix, iy, PATCH_SEED_J)) * 2.0 - 1.0,
    }
}

/// Coolest and warmest LED temperatures, and the per-patch jitter.
pub const KELVIN_MIN: f64 = 2700.0;
pub const KELVIN_MAX: f64 = 5700.0;
pub const KELVIN_JITTER: f64 = 300.0;

/// A patch's temperature: the tone's, moved by the patch jitter, clamped.
pub fn patch_kelvin(tone_kelvin: f64, patch: &Patch) -> f64 {
    (tone_kelvin + patch.jitter * KELVIN_JITTER).clamp(KELVIN_MIN, KELVIN_MAX)
}

/// Linear sRGB (D65) of a blackbody, luminance 1, every 100 K from
/// `KELVIN_MIN` to `KELVIN_MAX` (Kim et al. Planckian locus).
const WHITE_TABLE: [[f32; 3]; 31] = [
    [1.9294, 0.8052, 0.1930], // 2700 K
    [1.8708, 0.8200, 0.2194], // 2800 K
    [1.8167, 0.8333, 0.2460], // 2900 K
    [1.7667, 0.8455, 0.2726], // 3000 K
    [1.7202, 0.8566, 0.2992], // 3100 K
    [1.6770, 0.8668, 0.3258], // 3200 K
    [1.6366, 0.8761, 0.3523], // 3300 K
    [1.5989, 0.8847, 0.3787], // 3400 K
    [1.5635, 0.8926, 0.4049], // 3500 K
    [1.5302, 0.8998, 0.4310], // 3600 K
    [1.4988, 0.9065, 0.4569], // 3700 K
    [1.4692, 0.9128, 0.4826], // 3800 K
    [1.4412, 0.9185, 0.5080], // 3900 K
    [1.4147, 0.9238, 0.5333], // 4000 K
    [1.3890, 0.9290, 0.5584], // 4100 K
    [1.3650, 0.9336, 0.5827], // 4200 K
    [1.3422, 0.9380, 0.6066], // 4300 K
    [1.3207, 0.9420, 0.6301], // 4400 K
    [1.3002, 0.9458, 0.6532], // 4500 K
    [1.2808, 0.9492, 0.6759], // 4600 K
    [1.2624, 0.9525, 0.6983], // 4700 K
    [1.2448, 0.9555, 0.7202], // 4800 K
    [1.2282, 0.9583, 0.7417], // 4900 K
    [1.2123, 0.9608, 0.7628], // 5000 K
    [1.1971, 0.9633, 0.7835], // 5100 K
    [1.1827, 0.9655, 0.8039], // 5200 K
    [1.1689, 0.9676, 0.8238], // 5300 K
    [1.1557, 0.9695, 0.8434], // 5400 K
    [1.1431, 0.9713, 0.8626], // 5500 K
    [1.1311, 0.9730, 0.8814], // 5600 K
    [1.1195, 0.9746, 0.8998], // 5700 K
];

/// The white of `kelvin` (clamped to the table), interpolated.
pub fn white(kelvin: f64) -> [f32; 3] {
    let t = ((kelvin.clamp(KELVIN_MIN, KELVIN_MAX) - KELVIN_MIN) / 100.0) as f32;
    let i = (t.floor() as usize).min(WHITE_TABLE.len() - 2);
    let f = t - i as f32;
    let (a, b) = (WHITE_TABLE[i], WHITE_TABLE[i + 1]);
    [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * f)
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The luminance of written bytes: sRGB decoded, Rec.709 weights.
fn byte_luminance(c: [u8; 3]) -> f32 {
    luminance(c.map(|v| (f32::from(v) / 255.0).powf(SRGB_GAMMA)))
}

/// The bytes of a texel after the gain `s`. A sodium texel is `sodium * s`
/// rounded to nearest. An LED texel is `led * s`, which must not read dimmer
/// than those sodium bytes: rounding to nearest can lose a little luminance
/// (near black, or where the colour sits at full scale), so it falls back to
/// rounding up, then to the sodium bytes themselves.
pub fn texel_bytes(sodium: [f32; 3], led: Option<[f32; 3]>, s: f32) -> [u8; 3] {
    let quantise = |c: [f32; 3], f: fn(f32) -> f32| c.map(|v| f(v * s).clamp(0.0, 255.0) as u8);
    let base = quantise(sodium, f32::round);
    let Some(led) = led else {
        return base;
    };
    let floor = byte_luminance(base);
    let nearest = quantise(led, f32::round);
    if byte_luminance(nearest) >= floor {
        return nearest;
    }
    let up = quantise(led, f32::ceil);
    if byte_luminance(up) >= floor {
        up
    } else {
        base
    }
}

/// A sodium texel (stored values, 0..255) as LED white at `kelvin`: its
/// luminance times `energy`. A channel past full scales the colour down
/// with its hue kept; if that leaves it dimmer than the input, it is mixed
/// towards full white until it is not.
pub fn recolour(stored: [f32; 3], kelvin: f64, energy: f32) -> [f32; 3] {
    let linear = stored.map(|v| (v / 255.0).clamp(0.0, 1.0).powf(SRGB_GAMMA));
    let y = luminance(linear);
    if y <= 0.0 {
        return stored;
    }
    let mut out = white(kelvin).map(|c| c * y * energy);
    let peak = out.iter().copied().fold(0.0f32, f32::max);
    if peak > 1.0 {
        out = out.map(|c| c / peak);
    }
    let l = luminance(out);
    if l < y {
        let t = (y - l) / (1.0 - l);
        out = out.map(|c| c + t * (1.0 - c));
    }
    out.map(|c| c.clamp(0.0, 1.0).powf(1.0 / SRGB_GAMMA) * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: [f32; 3]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    fn decode(stored: [f32; 3]) -> [f32; 3] {
        stored.map(|v| (v / 255.0).powf(SRGB_GAMMA))
    }

    #[test]
    fn region_codes_parse_by_their_first_segment() {
        assert_eq!(LedRegion::from_code("NA-USA-MX-CENTRAL"), LedRegion::Na);
        assert_eq!(LedRegion::from_code("na"), LedRegion::Na);
        assert_eq!(LedRegion::from_code("eu-west"), LedRegion::Eu);
        assert_eq!(LedRegion::from_code("OC"), LedRegion::Oc);
        assert_eq!(LedRegion::from_code("AS-JAPAN"), LedRegion::As);
        assert_eq!(LedRegion::from_code("sa"), LedRegion::Sa);
        assert_eq!(LedRegion::from_code("AF"), LedRegion::Af);
        assert_eq!(LedRegion::from_code("ANTARCTICA"), LedRegion::Unknown);
        assert_eq!(LedRegion::from_code(""), LedRegion::Unknown);
        assert_eq!(
            LedRegion::from_code("_patches/KLAX_Mesh"),
            LedRegion::Unknown
        );
    }

    #[test]
    fn region_tags_are_distinct() {
        let all = [
            LedRegion::Na,
            LedRegion::Sa,
            LedRegion::Eu,
            LedRegion::Af,
            LedRegion::As,
            LedRegion::Oc,
            LedRegion::Unknown,
        ];
        let tags: std::collections::HashSet<u8> = all.iter().map(|r| r.tag()).collect();
        assert_eq!(tags.len(), LedRegion::COUNT);
        assert!(all.iter().all(|r| r.index() < LedRegion::COUNT));
    }

    #[test]
    fn region_tags_and_indexes_are_pinned() {
        let all = [
            (LedRegion::Na, 0),
            (LedRegion::Sa, 1),
            (LedRegion::Eu, 2),
            (LedRegion::Af, 3),
            (LedRegion::As, 4),
            (LedRegion::Oc, 5),
            (LedRegion::Unknown, 6),
        ];
        for (region, n) in all {
            assert_eq!(region.tag(), n, "{region:?}");
            assert_eq!(region.index(), usize::from(n), "{region:?}");
        }
    }

    #[test]
    fn every_region_row_follows_the_spec_table() {
        // (region, share scale, kelvin shift), from the spec.
        let table = [
            (LedRegion::Na, 1.0, 400.0),
            (LedRegion::Sa, 0.7, 400.0),
            (LedRegion::Eu, 0.9, -400.0),
            (LedRegion::Af, 0.5, 400.0),
            (LedRegion::As, 0.8, -400.0),
            (LedRegion::Oc, 0.9, -400.0),
            (LedRegion::Unknown, 0.75, 0.0),
        ];
        let m = LedModel::DEFAULT;
        for (region, scale, shift) in table {
            // Urban bright: share 0.75, kelvin 4000.
            let t = m.tone(false, 1.0, region);
            assert!((t.share - 0.75 * scale).abs() < 1e-12, "{region:?} share");
            assert!(
                (t.kelvin - (4000.0 + shift)).abs() < 1e-9,
                "{region:?} kelvin"
            );
        }
    }

    #[test]
    fn texel_bytes_keep_led_at_least_as_bright_as_sodium() {
        let lum_b = |c: [u8; 3]| byte_luminance(c);
        // Near black: nearest rounding would give (0, 0, 0) for LED.
        let sodium = [200.0, 120.0, 50.0];
        let led = Some(recolour(sodium, 4000.0, 1.25));
        let s = 0.0025;
        let base = texel_bytes(sodium, None, s);
        assert_eq!(base, [1, 0, 0]);
        assert!(lum_b(texel_bytes(sodium, led, s)) >= lum_b(base));
        // Clip branch: equal luminance by construction.
        let sodium = [255.0, 195.0, 0.0];
        let led = Some(recolour(sodium, 3200.0, 1.25));
        let s = 0.995;
        let base = texel_bytes(sodium, None, s);
        assert!(lum_b(texel_bytes(sodium, led, s)) >= lum_b(base));
        // No LED: exactly the rounded sodium bytes.
        assert_eq!(texel_bytes([10.4, 10.6, 0.0], None, 1.0), [10, 11, 0]);
    }

    #[test]
    fn industrial_is_named_by_an_ind_stem() {
        assert!(is_industrial("lib/g10/terrain10/urban_cld_wet_ind.ter"));
        assert!(is_industrial("lib/g10/terrain10/urban_hot_dry_ind3.ter"));
        assert!(is_industrial("lib/g8/terrain/north_city_irr_ind1.ter"));
        assert!(!is_industrial("lib/g10/terrain10/urban_tmp_sdry3.ter"));
        assert!(!is_industrial("lib/g8/terrain/temp_city_irr_in.ter"));
        assert!(!is_industrial("lit"));
    }

    #[test]
    fn brightness_splits_urban_smoothly() {
        assert_eq!(bright_weight(0.0), 0.0);
        assert_eq!(bright_weight(f64::NAN), 0.0);
        assert_eq!(bright_weight(URBAN_BRIGHT_LO_NW), 0.0);
        assert_eq!(bright_weight(URBAN_BRIGHT_HI_NW), 1.0);
        assert_eq!(bright_weight(10_000.0), 1.0);
        let mid = bright_weight((URBAN_BRIGHT_LO_NW * URBAN_BRIGHT_HI_NW).sqrt());
        assert!((mid - 0.5).abs() < 1e-9, "log-midpoint, got {mid}");
    }

    #[test]
    fn the_default_tones_follow_the_spec_tables() {
        let m = LedModel::DEFAULT;
        let t = m.tone(true, 0.0, LedRegion::Na);
        assert_eq!((t.share, t.kelvin), (0.80, 4900.0));
        let t = m.tone(false, 1.0, LedRegion::Eu);
        assert!((t.share - 0.675).abs() < 1e-12);
        assert_eq!(t.kelvin, 3600.0);
        let t = m.tone(false, 0.0, LedRegion::Af);
        assert!((t.share - 0.225).abs() < 1e-12);
        assert_eq!(t.kelvin, 3600.0);
        let t = m.tone(false, 0.5, LedRegion::Unknown);
        assert!((t.share - 0.45).abs() < 1e-12, "0.75 x mean(0.75, 0.45)");
        assert_eq!(t.kelvin, 3600.0);
        assert_eq!(m.energy, 1.25);
    }

    #[test]
    fn the_off_model_has_no_share() {
        for industrial in [false, true] {
            for bright in [0.0, 0.5, 1.0] {
                assert_eq!(
                    LedModel::OFF.tone(industrial, bright, LedRegion::Na).share,
                    0.0
                );
            }
        }
    }

    #[test]
    fn patches_are_deterministic_and_coherent() {
        let a = patch_at(1_000.0, 2_000.0);
        assert_eq!(a, patch_at(1_000.0, 2_000.0));
        assert!((0.0..1.0).contains(&a.u));
        assert!((-1.0..=1.0).contains(&a.jitter));
        // Points 1 m apart almost always share a patch; points far apart
        // take many different draws.
        let same = (0..1000)
            .filter(|&i| {
                let x = f64::from(i) * 37.0;
                patch_at(x, 0.0) == patch_at(x + 1.0, 0.0)
            })
            .count();
        assert!(same > 980, "neighbours share a patch, got {same}");
        let draws: std::collections::HashSet<u64> = (0..200)
            .map(|i| patch_at(f64::from(i) * PATCH_SIZE_M * 3.0, 0.0).u.to_bits())
            .collect();
        assert!(draws.len() > 190, "distant patches differ");
    }

    #[test]
    fn white_has_unit_luminance_and_cools_with_temperature() {
        let mut last_blue_ratio = 0.0;
        let mut k = KELVIN_MIN;
        while k <= KELVIN_MAX {
            let w = white(k);
            assert!((lum(w) - 1.0).abs() < 2e-3, "{k} K: {w:?}");
            let ratio = w[2] / w[0];
            assert!(ratio > last_blue_ratio, "{k} K bluer than below");
            last_blue_ratio = ratio;
            k += 50.0;
        }
        assert_eq!(white(1_000.0), white(KELVIN_MIN), "clamped below");
        assert_eq!(white(9_000.0), white(KELVIN_MAX), "clamped above");
    }

    #[test]
    fn recolour_keeps_the_light_and_adds_the_energy() {
        let sodium = [56.0, 36.0, 18.0];
        let y = lum(decode(sodium));
        let out = recolour(sodium, 4000.0, 1.25);
        assert!((lum(decode(out)) / y - 1.25).abs() < 1e-3);
        assert!(
            out[2] / out[0] > sodium[2] / sodium[0],
            "whiter than sodium"
        );
    }

    #[test]
    fn recolour_never_dims_a_bright_texel() {
        for stored in [
            [255.0, 255.0, 255.0],
            [255.0, 200.0, 120.0],
            [255.0, 160.0, 60.0],
        ] {
            let y = lum(decode(stored));
            for k in [KELVIN_MIN, 4000.0, KELVIN_MAX] {
                let out = recolour(stored, k, 1.25);
                assert!(out.iter().all(|&c| c <= 255.0 + 1e-3), "{out:?}");
                assert!(lum(decode(out)) >= y - 1e-4, "{stored:?} at {k} K dimmed");
            }
        }
    }

    #[test]
    fn recolour_leaves_black_black() {
        assert_eq!(recolour([0.0, 0.0, 0.0], 4000.0, 1.25), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn patch_kelvin_jitters_within_the_range() {
        let p = |jitter| Patch { u: 0.0, jitter };
        assert_eq!(patch_kelvin(4000.0, &p(0.0)), 4000.0);
        assert_eq!(patch_kelvin(4000.0, &p(1.0)), 4000.0 + KELVIN_JITTER);
        assert_eq!(patch_kelvin(2800.0, &p(-1.0)), KELVIN_MIN);
        assert_eq!(patch_kelvin(5600.0, &p(1.0)), KELVIN_MAX);
    }
}
