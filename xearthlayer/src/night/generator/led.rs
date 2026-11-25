//! LED street lighting: which neighbourhoods of a lit texture are LED, at
//! what colour temperature, and how a sodium texel becomes white.
//!
//! The ground is cut into jittered cells (patches) in Web Mercator metres.
//! Each patch draws one value `u`; a texel is LED when `u` is below the share
//! its category and region give. The recolour keeps the texel's luminance,
//! times `LedModel::energy`, so Laminar's street pattern survives and LED
//! never reads dimmer than the sodium it replaces.

use std::sync::OnceLock;

use super::noise::{floor_i64, hash2, unit};
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

/// How the sodium texels the LED stage leaves alone are toned (#363).
///
/// Laminar's sodium is about as saturated as a 2700 K blackbody, which is
/// physically fair for high-pressure sodium, but seen from the air a city's
/// sodium districts read less orange than the art. A texel is mixed `mix`
/// of the way towards the white of `kelvin` at its own luminance, then its
/// luminance is raised by `mix` of `energy`, because a less saturated light
/// of equal luminance looks dimmer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SodiumTone {
    /// How far towards the white, 0 (Laminar's bytes) to 1.
    pub mix: f32,
    pub kelvin: f64,
    /// Luminance multiplier at a full mix; a partial mix gets that share.
    pub energy: f32,
}

impl SodiumTone {
    /// Laminar's sodium bytes, untouched.
    pub const NONE: Self = Self {
        mix: 0.0,
        kelvin: 3200.0,
        energy: 1.0,
    };
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
    /// The tone of the texels that stay sodium.
    pub sodium: SodiumTone,
}

const fn region(share_scale: f64, kelvin_shift: f64) -> RegionTone {
    RegionTone {
        share_scale,
        kelvin_shift,
    }
}

impl LedModel {
    pub const DEFAULT: Self = Self {
        // Shares raised in #363: from the air, cities that have converted
        // their street lighting read mostly white, residential included.
        // Whites cooled in #363: most conversions were 4000 K (US 4000 to
        // 5000 K before 2016, UK councils mostly 4000 K or more), with
        // newer residential streets warmer; at 3200 K a distant city
        // averaged to amber even when nine texels in ten were LED.
        industrial: Tone {
            share: 0.90,
            kelvin: 4500.0,
        },
        urban_bright: Tone {
            share: 0.90,
            kelvin: 4500.0,
        },
        urban_dim: Tone {
            share: 0.80,
            kelvin: 4000.0,
        },
        // Na, Sa, Eu, Af, As, Oc, Unknown. The shifts were halved in #363:
        // European guidance leans warm for residential streets, but its
        // practice and the US's converge on 4000 K; how much a region has
        // converted is the share's job, not the temperature's.
        regions: [
            region(1.0, 200.0),
            region(0.7, 200.0),
            region(0.9, -200.0),
            region(0.5, 200.0),
            region(0.8, -200.0),
            region(0.9, -200.0),
            region(0.75, 0.0),
        ],
        energy: 1.25,
        sodium: SodiumTone {
            mix: 0.30,
            kelvin: 3200.0,
            energy: 1.125,
        },
    };

    /// No LED anywhere and Laminar's sodium untouched: the generator's
    /// output is the sodium-only one.
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
        sodium: SodiumTone::NONE,
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
    // Outside the ramp the smoothstep is exactly 0 or 1: no logarithm.
    if radiance_nw.is_nan() || radiance_nw <= URBAN_BRIGHT_LO_NW {
        return 0.0;
    }
    if radiance_nw >= URBAN_BRIGHT_HI_NW {
        return 1.0;
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
    patch_at_cached(x, y, &mut PatchCache::default())
}

/// A jittered cell centre (in cells) and its cell.
type Centre = (f64, f64, i64, i64);

/// The 3 x 3 jittered centres around the last cell a texel fell in, and the
/// last patch drawn. A cell is about 53 ZL16 texels wide, so texels along a
/// row reuse its centres instead of hashing them again. The cached values
/// are the ones computed without a cache, so a patch is the same either way.
#[derive(Debug, Default)]
pub struct PatchCache {
    cell: Option<(i64, i64)>,
    /// In the order they are compared: the first nearest wins.
    centres: [Centre; 9],
    last: Option<((i64, i64), Patch)>,
}

/// `patch_at`, reusing the centres and the last patch from `cache`.
pub fn patch_at_cached(x: f64, y: f64, cache: &mut PatchCache) -> Patch {
    let (gx, gy) = (x / PATCH_SIZE_M, y / PATCH_SIZE_M);
    let (cx, cy) = (floor_i64(gx), floor_i64(gy));
    if cache.cell != Some((cx, cy)) {
        let mut k = 0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let (ix, iy) = (cx + dx, cy + dy);
                let ox = (unit(hash2(ix, iy, PATCH_SEED_X)) - 0.5) * PATCH_JITTER;
                let oy = (unit(hash2(ix, iy, PATCH_SEED_Y)) - 0.5) * PATCH_JITTER;
                cache.centres[k] = (ix as f64 + 0.5 + ox, iy as f64 + 0.5 + oy, ix, iy);
                k += 1;
            }
        }
        cache.cell = Some((cx, cy));
    }
    let mut best = (f64::INFINITY, cx, cy);
    for &(px, py, ix, iy) in &cache.centres {
        let d = (gx - px).powi(2) + (gy - py).powi(2);
        if d < best.0 {
            best = (d, ix, iy);
        }
    }
    let (_, ix, iy) = best;
    match cache.last {
        Some((cell, patch)) if cell == (ix, iy) => patch,
        _ => {
            let patch = Patch {
                u: unit(hash2(ix, iy, PATCH_SEED_U)),
                jitter: unit(hash2(ix, iy, PATCH_SEED_J)) * 2.0 - 1.0,
            };
            cache.last = Some(((ix, iy), patch));
            patch
        }
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
    // `t` is at least 0, so the cast is its floor.
    let i = (t as usize).min(WHITE_TABLE.len() - 2);
    let f = t - i as f32;
    let (a, b) = (WHITE_TABLE[i], WHITE_TABLE[i + 1]);
    [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * f)
}

pub(crate) fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Stored byte values (0..255) to linear light, exactly as [`decode`] gives
/// them (the same `powf` of the same 256 inputs), built once per process.
pub(crate) fn linear_table() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|v| decode([v as f32; 3])[0]))
}

/// The luminance of written bytes: sRGB decoded by `linear` (the
/// [`linear_table`], fetched once by the caller), Rec.709 weights.
fn byte_luminance(linear: &[f32; 256], c: [u8; 3]) -> f32 {
    luminance(c.map(|v| linear[usize::from(v)]))
}

/// The bytes of a texel after the gain `s`. Laminar's sodium is `sodium * s`
/// rounded to nearest. A desaturated sodium texel, and then an LED texel,
/// must each not read dimmer than the bytes below it: rounding to nearest
/// can lose a little luminance (near black, or where the colour sits at full
/// scale), so each falls back to rounding up, then to the bytes below.
pub fn texel_bytes(
    sodium: [f32; 3],
    desaturated: Option<[f32; 3]>,
    led: Option<[f32; 3]>,
    s: f32,
) -> [u8; 3] {
    let quantise = |c: [f32; 3], f: fn(f32) -> f32| c.map(|v| f(v * s).clamp(0.0, 255.0) as u8);
    let bytes = quantise(sodium, f32::round);
    if desaturated.is_none() && led.is_none() {
        return bytes;
    }
    let linear = linear_table();
    // Each step settles on bytes and returns their luminance with them:
    // the next step's floor, so no byte luminance is computed twice.
    let no_dimmer = |(below, floor): ([u8; 3], f32), over: [f32; 3]| {
        let nearest = quantise(over, f32::round);
        let y = byte_luminance(linear, nearest);
        if y >= floor {
            return (nearest, y);
        }
        let up = quantise(over, f32::ceil);
        let y = byte_luminance(linear, up);
        if y >= floor {
            (up, y)
        } else {
            (below, floor)
        }
    };
    let mut settled = (bytes, byte_luminance(linear, bytes));
    if let Some(d) = desaturated {
        settled = no_dimmer(settled, d);
    }
    if let Some(l) = led {
        settled = no_dimmer(settled, l);
    }
    settled.0
}

/// A texel's stored values (0..255) decoded once: its linear light and that
/// light's luminance, shared by the sodium tone and the LED recolour. Built
/// from the stored values only, so the three always agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Decoded {
    stored: [f32; 3],
    linear: [f32; 3],
    y: f32,
}

impl Decoded {
    pub(crate) fn new(stored: [f32; 3]) -> Self {
        let linear = decode(stored);
        Self {
            stored,
            linear,
            y: luminance(linear),
        }
    }
}

/// What [`desaturate`] derives from a [`SodiumTone`] alone, computed once
/// (per request in the generator): the clamped mix, the white of the
/// tone's temperature and the luminance gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PreparedTone {
    mix: f32,
    white: [f32; 3],
    gain: f32,
}

impl PreparedTone {
    pub(crate) fn new(tone: &SodiumTone) -> Self {
        let mix = tone.mix.clamp(0.0, 1.0);
        Self {
            mix,
            white: white(tone.kelvin),
            gain: 1.0 + mix * (tone.energy - 1.0),
        }
    }

    /// [`desaturate`] of a decoded texel, by the same operations.
    pub(crate) fn apply(&self, texel: &Decoded) -> [f32; 3] {
        let (m, y) = (self.mix, texel.y);
        if m <= 0.0 || y <= 0.0 {
            return texel.stored;
        }
        let (linear, w) = (texel.linear, self.white);
        let out = [0, 1, 2].map(|c| (linear[c] * (1.0 - m) + w[c] * y * m) * self.gain);
        encode(keep_light(out, y))
    }
}

/// A sodium texel (stored values, 0..255) toned by `tone`: mixed in linear
/// light towards the white of `tone.kelvin` at its own luminance, then that
/// luminance raised by `tone.mix` of `tone.energy`. No mix, or black, returns
/// the input exactly, so `SodiumTone::NONE` keeps Laminar's bytes. The
/// generator holds a [`PreparedTone`] and a [`Decoded`] texel instead.
pub fn desaturate(stored: [f32; 3], tone: &SodiumTone) -> [f32; 3] {
    let tone = PreparedTone::new(tone);
    if tone.mix <= 0.0 {
        return stored;
    }
    tone.apply(&Decoded::new(stored))
}

/// A sodium texel (stored values, 0..255) as LED white at `kelvin`: its
/// luminance times `energy`. A channel past full scales the colour down
/// with its hue kept; if that leaves it dimmer than the input, it is mixed
/// towards full white until it is not. Tests only: the generator calls
/// [`recolour_decoded`] with the texel it has already decoded.
#[cfg(test)]
pub(crate) fn recolour(stored: [f32; 3], kelvin: f64, energy: f32) -> [f32; 3] {
    recolour_decoded(&Decoded::new(stored), kelvin, energy)
}

/// A decoded sodium texel as LED white at `kelvin`: its luminance times
/// `energy`. A channel past full scales the colour down with its hue kept;
/// if that leaves it dimmer than the input, it is mixed towards full white
/// until it is not. Black returns the input exactly.
pub(crate) fn recolour_decoded(texel: &Decoded, kelvin: f64, energy: f32) -> [f32; 3] {
    let y = texel.y;
    if y <= 0.0 {
        return texel.stored;
    }
    encode(keep_light(white(kelvin).map(|c| c * y * energy), y))
}

/// How the generator turns a texel's pattern sum `sodium` (stored values,
/// 0..255) into bytes at gain `s`: toned as sodium, or recoloured to LED
/// white at `led_kelvin`, then [`texel_bytes`]. A trait so tests can run
/// the generator with the reference path and compare.
pub(crate) trait TonePath {
    fn bytes(&self, sodium: [f32; 3], led_kelvin: Option<f64>, s: f32) -> [u8; 3];
}

/// The tone path of one request: the model's sodium tone prepared once,
/// and each texel decoded at most once for both the tone and the LED white.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Toner {
    /// `None` when the model leaves Laminar's sodium untouched.
    sodium: Option<PreparedTone>,
    energy: f32,
}

impl Toner {
    pub(crate) fn new(model: &LedModel) -> Self {
        Self {
            sodium: (model.sodium.mix > 0.0).then(|| PreparedTone::new(&model.sodium)),
            energy: model.energy,
        }
    }
}

impl TonePath for Toner {
    fn bytes(&self, sodium: [f32; 3], led_kelvin: Option<f64>, s: f32) -> [u8; 3] {
        if self.sodium.is_none() && led_kelvin.is_none() {
            return texel_bytes(sodium, None, None, s);
        }
        let texel = Decoded::new(sodium);
        let toned = self.sodium.map(|tone| tone.apply(&texel));
        let led = led_kelvin.map(|k| recolour_decoded(&texel, k, self.energy));
        texel_bytes(sodium, toned, led, s)
    }
}

/// Stored values (0..255) to linear light.
pub(crate) fn decode(stored: [f32; 3]) -> [f32; 3] {
    stored.map(|v| (v / 255.0).clamp(0.0, 1.0).powf(SRGB_GAMMA))
}

/// Linear light to stored values (0..255).
pub(crate) fn encode(linear: [f32; 3]) -> [f32; 3] {
    linear.map(|c| c.clamp(0.0, 1.0).powf(1.0 / SRGB_GAMMA) * 255.0)
}

/// Linear light to the byte `encode` gives it, rounded to nearest and
/// clamped, exactly, for every input (NaN and anything at or below 0 give
/// 0), with no `powf`: [`ByteEncoder::byte`]. The tables are built once per
/// process; a caller encoding many values holds [`ByteEncoder::get`].
pub(crate) struct ByteEncoder {
    thresholds: &'static [f32; 255],
    /// Bucket of the first threshold (floats share a bucket when their
    /// top 16 bits do).
    first_bucket: usize,
    /// Per bucket from `first_bucket` up to 1.0, how many thresholds lie
    /// at or below the bucket's smallest float.
    starts: Vec<u8>,
}

impl ByteEncoder {
    pub(crate) fn get() -> &'static Self {
        static ENCODER: OnceLock<ByteEncoder> = OnceLock::new();
        ENCODER.get_or_init(|| {
            let thresholds = byte_thresholds();
            let first_bucket = (thresholds[0].to_bits() >> 16) as usize;
            let last_bucket = (1.0f32.to_bits() >> 16) as usize;
            let starts = (first_bucket..last_bucket)
                .map(|k| {
                    let lowest = f32::from_bits((k as u32) << 16);
                    thresholds.partition_point(|&x| x <= lowest) as u8
                })
                .collect();
            Self {
                thresholds,
                first_bucket,
                starts,
            }
        })
    }

    /// The number of thresholds at or below `linear`, counted on from
    /// where its bucket says the count starts (a step or two at most).
    pub(crate) fn byte(&self, linear: f32) -> u8 {
        if linear.is_nan() || linear <= 0.0 {
            return 0;
        }
        if linear >= 1.0 {
            return 255;
        }
        let bucket = (linear.to_bits() >> 16) as usize;
        let mut b = bucket
            .checked_sub(self.first_bucket)
            .map_or(0, |k| usize::from(self.starts[k]));
        while b < self.thresholds.len() && self.thresholds[b] <= linear {
            b += 1;
        }
        b as u8
    }
}

/// Threshold `b - 1` is the least float whose encoded value rounds to `b`
/// or more, found once by bisecting the floats from 0 to 1 (ordered by
/// their bits). Exact because the encode never decreases.
fn byte_thresholds() -> &'static [f32; 255] {
    static TABLE: OnceLock<[f32; 255]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let byte = |bits: u32| {
            encode([f32::from_bits(bits); 3])[0]
                .round()
                .clamp(0.0, 255.0) as u8
        };
        std::array::from_fn(|i| {
            let want = i as u8 + 1;
            let (mut lo, mut hi) = (0u32, 1.0f32.to_bits());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if byte(mid) >= want {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            f32::from_bits(lo)
        })
    })
}

/// A linear colour brought within full scale without reading dimmer than
/// luminance `y`: a channel past full scales all three down so the hue
/// holds, and if that leaves it dimmer than `y`, it is mixed towards full
/// white until it is not.
fn keep_light(mut out: [f32; 3], y: f32) -> [f32; 3] {
    let peak = out.iter().copied().fold(0.0f32, f32::max);
    if peak > 1.0 {
        out = out.map(|c| c / peak);
    }
    let l = luminance(out);
    if l < y {
        let t = (y - l) / (1.0 - l);
        out = out.map(|c| c + t * (1.0 - c));
    }
    out
}

#[cfg(test)]
pub(crate) mod powf_reference;

#[cfg(test)]
mod tests {
    use super::super::gain::{gain, GAIN_FLOOR_NW};
    use super::*;

    #[test]
    fn the_linear_table_is_the_decode() {
        let table = linear_table();
        for v in 0..=255u8 {
            assert_eq!(
                table[usize::from(v)].to_bits(),
                decode([f32::from(v); 3])[0].to_bits(),
                "stored {v}"
            );
        }
    }

    #[test]
    fn byte_luminance_is_its_powf_form_for_every_byte_of_each_channel() {
        for v in 0..=255u8 {
            let w = 255 - v;
            for c in [[v, 0, 0], [0, v, 0], [0, 0, v], [v, v, v], [v, w, v / 2]] {
                assert_eq!(
                    byte_luminance(linear_table(), c).to_bits(),
                    powf_reference::byte_luminance(c).to_bits(),
                    "{c:?}"
                );
            }
        }
    }

    /// splitmix64: a fixed, portable sequence for the equivalence tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        /// Uniform in [0, 1).
        fn unit(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u32 << 24) as f32
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A pattern sum: exact zeros, whole bytes (a single full weight) or
    /// any value in 0..255, with channels zeroed at random.
    fn sodium_sample(rng: &mut Rng) -> [f32; 3] {
        match rng.below(16) {
            0 => [0.0; 3],
            1 | 2 => [0; 3].map(|_: u8| (rng.next() % 256) as f32),
            _ => [0; 3].map(|_: u8| {
                if rng.below(16) == 0 {
                    0.0
                } else {
                    rng.unit() * 255.0
                }
            }),
        }
    }

    /// A gain the generator can pass: from just above the radiance floor
    /// (a tiny positive gain) to 1, spread over four decades.
    fn gain_sample(rng: &mut Rng) -> f32 {
        match rng.below(16) {
            0 => gain(GAIN_FLOOR_NW * (1.0 + 1e-9), 1.0) as f32,
            1 => gain(GAIN_FLOOR_NW.next_up(), 1.0) as f32,
            2 => 1.0,
            3 => rng.unit(),
            _ => 10f32.powf(-4.0 * rng.unit()),
        }
    }

    fn tone_models() -> [LedModel; 4] {
        let toned = |mix, kelvin, energy| LedModel {
            sodium: SodiumTone {
                mix,
                kelvin,
                energy,
            },
            ..LedModel::DEFAULT
        };
        [
            LedModel::DEFAULT,
            LedModel::OFF,
            toned(1.0, 5000.0, 1.3),
            toned(0.65, 2000.0, 0.9),
        ]
    }

    /// LED white or not, at temperatures inside, on and past the table.
    const LED_KELVINS: [Option<f64>; 8] = [
        None,
        Some(1800.0),
        Some(KELVIN_MIN),
        Some(3000.0),
        Some(4000.0),
        Some(4650.5),
        Some(KELVIN_MAX),
        Some(7000.0),
    ];

    fn bits(c: [f32; 3]) -> [u32; 3] {
        c.map(f32::to_bits)
    }

    #[test]
    fn the_tone_path_matches_its_powf_form_on_two_million_colours() {
        let models = tone_models();
        let toners: Vec<_> = models
            .iter()
            .map(|m| (Toner::new(m), powf_reference::PowfToner(m)))
            .collect();
        let mut rng = Rng(436);
        let mut led_texels = 0;
        for i in 0..2_000_000u32 {
            let sodium = sodium_sample(&mut rng);
            let s = gain_sample(&mut rng);
            let k = LED_KELVINS[rng.below(LED_KELVINS.len())];
            let (toner, reference) = &toners[i as usize % toners.len()];
            led_texels += usize::from(k.is_some());
            assert_eq!(
                toner.bytes(sodium, k, s),
                reference.bytes(sodium, k, s),
                "sodium {sodium:?}, gain {s:e}, LED {k:?}, model {}",
                i as usize % toners.len()
            );
        }
        assert!(led_texels > 1_500_000, "LED texels covered");
    }

    #[test]
    fn the_tone_steps_match_their_powf_forms() {
        let mut rng = Rng(0x436);
        for model in tone_models() {
            let tone = PreparedTone::new(&model.sodium);
            for _ in 0..25_000 {
                let sodium = sodium_sample(&mut rng);
                let decoded = Decoded::new(sodium);
                let want = powf_reference::desaturate(sodium, &model.sodium);
                assert_eq!(bits(desaturate(sodium, &model.sodium)), bits(want));
                if model.sodium.mix > 0.0 {
                    assert_eq!(bits(tone.apply(&decoded)), bits(want), "{sodium:?}");
                }
                let k = LED_KELVINS[1 + rng.below(LED_KELVINS.len() - 1)].unwrap();
                let want = powf_reference::recolour(sodium, k, model.energy);
                assert_eq!(bits(recolour(sodium, k, model.energy)), bits(want));
                assert_eq!(
                    bits(recolour_decoded(&decoded, k, model.energy)),
                    bits(want),
                    "{sodium:?} at {k} K"
                );
                let s = gain_sample(&mut rng);
                let (d, l) = (Some(desaturate(sodium, &model.sodium)), Some(want));
                for (d, l) in [(None, None), (d, None), (None, l), (d, l)] {
                    assert_eq!(
                        texel_bytes(sodium, d, l, s),
                        powf_reference::texel_bytes(sodium, d, l, s),
                        "{sodium:?} at gain {s:e}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_tone_path_leaves_exact_zeros_black() {
        for model in tone_models() {
            let toner = Toner::new(&model);
            for k in LED_KELVINS {
                for s in [f32::MIN_POSITIVE, 1e-3, 1.0] {
                    assert_eq!(toner.bytes([0.0; 3], k, s), [0; 3]);
                }
            }
        }
    }

    /// What a road texel's channel became before the byte table: encode,
    /// round to nearest, clamp.
    fn encode_round(x: f32) -> u8 {
        encode([x; 3])[0].round().clamp(0.0, 255.0) as u8
    }

    #[test]
    fn bright_weight_is_the_smoothstep_everywhere() {
        let smoothstep = |r: f64| {
            if r.is_nan() || r <= 0.0 {
                return 0.0;
            }
            let t = ((r.ln() - URBAN_BRIGHT_LO_NW.ln())
                / (URBAN_BRIGHT_HI_NW.ln() - URBAN_BRIGHT_LO_NW.ln()))
            .clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        let mut rs = vec![0.0, -1.0, f64::NAN, f64::INFINITY, 1e-300, 1e300];
        for edge in [URBAN_BRIGHT_LO_NW, URBAN_BRIGHT_HI_NW] {
            let mut r = edge;
            for _ in 0..1000 {
                r = r.next_down();
            }
            for _ in 0..2000 {
                rs.push(r);
                r = r.next_up();
            }
        }
        rs.extend((0..200_000).map(|k| f64::from(k) * 0.001));
        for r in rs {
            assert_eq!(bright_weight(r).to_bits(), smoothstep(r).to_bits(), "{r}");
        }
    }

    #[test]
    fn the_byte_encoder_is_encode_rounded_for_every_kind_of_input() {
        let mut inputs = vec![
            0.0,
            -0.0,
            1.0,
            1.5,
            -1.0,
            f32::MIN_POSITIVE,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::MAX,
        ];
        // Around every byte boundary: the float steps either side of the
        // linear value whose encoded value is exactly b + 0.5.
        for b in 0..256 {
            let t = ((b as f64 + 0.5) / 255.0).powf(f64::from(SRGB_GAMMA)) as f32;
            let mut x = t;
            for _ in 0..64 {
                x = f32::from_bits(x.to_bits().saturating_sub(1));
            }
            for _ in 0..128 {
                inputs.push(x);
                x = f32::from_bits(x.to_bits() + 1);
            }
        }
        // And a dense sweep of [0, 1.25].
        inputs.extend((0..=2_000_000).map(|i| i as f32 / 1_600_000.0));
        let encoder = ByteEncoder::get();
        for x in inputs {
            assert_eq!(
                encoder.byte(x),
                encode_round(x),
                "linear {x:e} ({:#x})",
                x.to_bits()
            );
        }
    }

    /// Every float from 0 to 1: the byte table assumes the encode is
    /// monotonic. Release only (about a billion `powf`):
    /// `cargo test --release -p xearthlayer --lib the_byte_encoder_exhaustive -- --ignored`.
    #[test]
    #[ignore]
    fn the_byte_encoder_exhaustive() {
        let (encoder, mut mismatches) = (ByteEncoder::get(), 0u64);
        for bits in 0..=1.0f32.to_bits() {
            let x = f32::from_bits(bits);
            mismatches += u64::from(encoder.byte(x) != encode_round(x));
        }
        assert_eq!(mismatches, 0);
    }

    fn lum(c: [f32; 3]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
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
            (LedRegion::Na, 1.0, 200.0),
            (LedRegion::Sa, 0.7, 200.0),
            (LedRegion::Eu, 0.9, -200.0),
            (LedRegion::Af, 0.5, 200.0),
            (LedRegion::As, 0.8, -200.0),
            (LedRegion::Oc, 0.9, -200.0),
            (LedRegion::Unknown, 0.75, 0.0),
        ];
        let m = LedModel::DEFAULT;
        for (region, scale, shift) in table {
            // Urban bright: share 0.90, kelvin 4500.
            let t = m.tone(false, 1.0, region);
            assert!((t.share - 0.90 * scale).abs() < 1e-12, "{region:?} share");
            assert!(
                (t.kelvin - (4500.0 + shift)).abs() < 1e-9,
                "{region:?} kelvin"
            );
        }
    }

    #[test]
    fn texel_bytes_keep_led_at_least_as_bright_as_sodium() {
        let lum_b = |c: [u8; 3]| byte_luminance(linear_table(), c);
        // Near black: nearest rounding would give (0, 0, 0) for LED.
        let sodium = [200.0, 120.0, 50.0];
        let led = Some(recolour(sodium, 4000.0, 1.25));
        let s = 0.0025;
        let base = texel_bytes(sodium, None, None, s);
        assert_eq!(base, [1, 0, 0]);
        assert!(lum_b(texel_bytes(sodium, None, led, s)) >= lum_b(base));
        // Clip branch: equal luminance by construction.
        let sodium = [255.0, 195.0, 0.0];
        let led = Some(recolour(sodium, 3200.0, 1.25));
        let s = 0.995;
        let base = texel_bytes(sodium, None, None, s);
        assert!(lum_b(texel_bytes(sodium, None, led, s)) >= lum_b(base));
        // No LED: exactly the rounded sodium bytes.
        assert_eq!(texel_bytes([10.4, 10.6, 0.0], None, None, 1.0), [10, 11, 0]);
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
        assert_eq!((t.share, t.kelvin), (0.90, 4700.0));
        let t = m.tone(false, 1.0, LedRegion::Eu);
        assert!((t.share - 0.81).abs() < 1e-12);
        assert_eq!(t.kelvin, 4300.0);
        let t = m.tone(false, 0.0, LedRegion::Af);
        assert!((t.share - 0.40).abs() < 1e-12);
        assert_eq!(t.kelvin, 4200.0);
        let t = m.tone(false, 0.5, LedRegion::Unknown);
        assert!((t.share - 0.6375).abs() < 1e-12, "0.75 x mean(0.90, 0.80)");
        assert_eq!(t.kelvin, 4250.0, "mean(4500, 4000)");
        assert_eq!(m.energy, 1.25);
        assert_eq!(
            m.sodium,
            SodiumTone {
                mix: 0.30,
                kelvin: 3200.0,
                energy: 1.125,
            }
        );
    }

    #[test]
    fn the_off_model_leaves_sodium_alone() {
        assert_eq!(LedModel::OFF.sodium, SodiumTone::NONE);
        assert_eq!(SodiumTone::NONE.mix, 0.0);
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

    fn saturation(c: [f32; 3]) -> f32 {
        let (mx, mn) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
        (mx - mn) / mx
    }

    #[test]
    fn desaturate_without_a_mix_returns_the_input_exactly() {
        for stored in [[56.0, 36.0, 18.0], [255.0, 160.0, 60.0], [0.0, 0.0, 0.0]] {
            assert_eq!(desaturate(stored, &SodiumTone::NONE), stored);
        }
    }

    #[test]
    fn desaturate_lowers_saturation_and_keeps_the_compensated_light() {
        let tone = LedModel::DEFAULT.sodium;
        // Laminar's mean sodium texel and the generator tests' orange.
        for stored in [[56.0, 36.0, 18.0], [200.0, 120.0, 50.0]] {
            let out = desaturate(stored, &tone);
            assert!(
                saturation(out) < saturation(stored),
                "{stored:?} -> {out:?} not less saturated"
            );
            let want = 1.0 + tone.mix * (tone.energy - 1.0);
            let got = lum(decode(out)) / lum(decode(stored));
            assert!(
                (got - want).abs() < 1e-3,
                "{stored:?}: luminance x{got}, want x{want}"
            );
            // Still sodium, not LED: blue stays well under red.
            assert!(out[2] / out[0] < 0.4, "{stored:?} -> {out:?} reads as LED");
        }
    }

    #[test]
    fn desaturate_never_dims_a_bright_texel() {
        let tone = LedModel::DEFAULT.sodium;
        for stored in [
            [255.0, 255.0, 255.0],
            [255.0, 200.0, 120.0],
            [255.0, 160.0, 60.0],
        ] {
            let out = desaturate(stored, &tone);
            assert!(out.iter().all(|&c| c <= 255.0 + 1e-3), "{out:?}");
            assert!(
                lum(decode(out)) >= lum(decode(stored)) - 1e-4,
                "{stored:?} dimmed"
            );
        }
    }

    #[test]
    fn desaturate_leaves_black_black() {
        assert_eq!(
            desaturate([0.0, 0.0, 0.0], &LedModel::DEFAULT.sodium),
            [0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn texel_bytes_keep_desaturated_sodium_at_least_as_bright_as_laminar() {
        let lum_b = |c: [u8; 3]| byte_luminance(linear_table(), c);
        let tone = LedModel::DEFAULT.sodium;
        for laminar in [[200.0, 120.0, 50.0], [56.0, 36.0, 18.0], [90.0, 40.0, 10.0]] {
            let sodium = Some(desaturate(laminar, &tone));
            for s in [0.0025, 0.01, 0.05, 0.3, 1.0] {
                let raw = texel_bytes(laminar, None, None, s);
                let out = texel_bytes(laminar, sodium, None, s);
                assert!(
                    lum_b(out) >= lum_b(raw),
                    "{laminar:?} at gain {s}: {out:?} dimmer than {raw:?}"
                );
            }
        }
    }

    #[test]
    fn a_shared_patch_cache_gives_the_uncached_patches() {
        // The stale-hit guard: a cache shared by rows of ZL16 texels (about
        // 7.5 m apart) across many cells, then by jumps back and forth,
        // gives what a fresh cache gives. That the patches equal the ones
        // drawn before the cache existed is pinned by the LED-on goldens
        // (`cpu::tests::the_led_output_is_pinned`).
        let mut cache = PatchCache::default();
        let mut points = Vec::new();
        for row in 0..40 {
            let y = 5_000_000.0 + f64::from(row) * 97.0;
            points.extend((0..800).map(|i| (-1_200.0 + f64::from(i) * 7.5, y)));
        }
        points.extend((0..2000).map(|i| {
            let h = hash2(i, 1, 3);
            (unit(h) * 1e5 - 5e4, unit(h >> 16) * 1e5 - 5e4)
        }));
        for (x, y) in points {
            assert_eq!(
                patch_at_cached(x, y, &mut cache),
                patch_at(x, y),
                "at {x},{y}"
            );
        }
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
