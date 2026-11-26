//! The CPU lit generator.
//!
//! Each texel of the first visible level is a function of its ground
//! position alone: the recipe field gives the class weights (soft, warped
//! class edges) and the radiance (bilinear brightness), the radiance becomes
//! a gain, and the class patterns are sampled by stochastic tiling in Web
//! Mercator metres. Nothing depends on the texture's zoom or on 1 degree
//! tile borders, so neighbouring and nested textures agree on shared ground.
//!
//! Neighbourhood patches then turn some texels LED white (`led`), by class,
//! brightness and region.
//!
//! Road glow (`LitRequest::roads`) is added last, in linear light, only on
//! the texels it reaches: an empty road field leaves every byte as it was.

use std::sync::Arc;

use image::{Rgba, RgbaImage};
use tracing::debug;

use super::field::{FieldSampler, RecipeField, TexelAxes};
use super::led::{self, LedRegion, TonePath};
use super::tiling::{sample_cached, CornerCache};
use super::{gain, LitGenerator, LitRequest};
use crate::night::footprint::{Footprint, TexelCentres};
use crate::night::laminar::LitPattern;
use crate::night::math::floor_i64;
use crate::night::roads::field::RoadField;

/// Palette indexes per tile: a class cell is one byte.
const PALETTE_SLOTS: usize = 256;

/// Generates lit pixels on the CPU.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuLitGenerator;

impl LitGenerator for CpuLitGenerator {
    fn generate(&self, request: &LitRequest<'_>) -> RgbaImage {
        self.generate_with(request, &led::Toner::new(request.led), Shortcuts::ALL)
    }
}

/// Work the generator skips where it cannot change a byte (#438).
/// Production takes every one; tests and timings turn them off to compare
/// with the full path.
#[derive(Debug, Clone, Copy)]
struct Shortcuts {
    /// The sampler's class mask: no class lookup where no class is near.
    class_mask: bool,
    /// No tone path for a texel whose pattern sum is black.
    black_stop: bool,
}

impl Shortcuts {
    const ALL: Self = Self {
        class_mask: true,
        black_stop: true,
    };
    #[cfg(test)]
    const NONE: Self = Self {
        class_mask: false,
        black_stop: false,
    };
}

impl CpuLitGenerator {
    /// The generator with the tone path `toner` and `shortcuts`: the
    /// request's [`led::Toner`] and all of them in production; tests pass
    /// the reference path, or one that records what reaches it, and turn
    /// shortcuts off, to compare.
    fn generate_with<T: TonePath>(
        &self,
        request: &LitRequest<'_>,
        toner: &T,
        shortcuts: Shortcuts,
    ) -> RgbaImage {
        let name = request.name;
        let size = request.size;
        let TexelCentres { lons, lats, xs, ys } = TexelCentres::of(name, size);

        let field = RecipeField::new(&Footprint::from_name(name), request.recipes);
        let mut sampler = FieldSampler::with_class_mask(
            &field,
            TexelAxes {
                lats: &lats,
                lons: &lons,
            },
            shortcuts.class_mask,
        );
        // Patterns by (tile slot, palette index), resolved on first use.
        let mut patterns: Vec<Option<Option<Arc<LitPattern>>>> =
            vec![None; field.slot_count() * PALETTE_SLOTS];
        // Per (tile slot, palette index): industrial or not. Per slot: region.
        let mut industrial: Vec<Option<bool>> = vec![None; field.slot_count() * PALETTE_SLOTS];
        let mut regions: Vec<Option<LedRegion>> = vec![None; field.slot_count()];
        let mut weights = Vec::with_capacity(4);
        let mut corners = CornerCache::default();
        // A patch is about 53 ZL16 texels wide: neighbouring texels reuse
        // its jittered centres instead of hashing them again.
        let mut patches = led::PatchCache::default();
        let mut road_light = RoadLight::new(request);
        // A field rastered for another size would put glow on the wrong
        // texels: it is left out rather than misplaced.
        let no_roads = RoadField::empty(size);
        let roads = if request.roads.is_empty() || request.roads.size() == size {
            request.roads
        } else {
            debug!(
                field = request.roads.size(),
                size, "Night roads: road field size differs from the texture; no road light"
            );
            &no_roads
        };
        let mut img = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 255]));

        for (i, (&lat, &y)) in lats.iter().zip(&ys).enumerate() {
            // `None` when the whole field is empty: one branch per row.
            let road_row = roads.row(i as u32);
            for (j, (&lon, &x)) in lons.iter().zip(&xs).enumerate() {
                let road = road_row.map_or(0.0, |r| r[j]);
                sampler.class_weights(i, j, &mut weights);
                if weights.is_empty() && road <= 0.0 {
                    continue;
                }
                let radiance = sampler.radiance(i, j);
                // No class here: only the road can light the texel, and the
                // gain is not needed.
                let g = if weights.is_empty() {
                    0.0
                } else {
                    gain(radiance, request.brightness)
                };
                if g <= 0.0 {
                    if road > 0.0 {
                        let texel = RoadTexel {
                            at: (lat, lon),
                            xy: (x, y),
                            radiance,
                            bright: led::bright_weight(radiance),
                            patch: None,
                        };
                        let bytes = road_light.over([0; 3], road, &texel);
                        img.put_pixel(j as u32, i as u32, opaque(bytes));
                    }
                    continue;
                }
                let bright = led::bright_weight(radiance);
                let mut rgb = [0.0f32; 3];
                let (mut wsum, mut share, mut kelvin) = (0.0f64, 0.0f64, 0.0f64);
                for w in &weights {
                    let at = w.slot * PALETTE_SLOTS + usize::from(w.index);
                    let pattern = patterns[at].get_or_insert_with(|| {
                        field
                            .palette_name_in(w.slot, w.index)
                            .and_then(|class| (request.patterns)(class))
                    });
                    let Some(p) = pattern else {
                        continue;
                    };
                    let c = sample_cached(p, x, y, &mut corners);
                    for ch in 0..3 {
                        rgb[ch] += w.weight as f32 * c[ch];
                    }
                    // Skipped for weights without a pattern (above), so wsum
                    // covers only the lit weights, as the pattern sum does.
                    let ind = *industrial[at].get_or_insert_with(|| {
                        field
                            .palette_name_in(w.slot, w.index)
                            .is_some_and(led::is_industrial)
                    });
                    let region = *regions[w.slot]
                        .get_or_insert_with(|| (request.regions)(w.tile.0, w.tile.1));
                    let tone = request.led.tone(ind, bright, region);
                    wsum += w.weight;
                    share += w.weight * tone.share;
                    kelvin += w.weight * tone.kelvin;
                }
                // Lit art is bright points on black: where every tap is
                // black (or no weight has a pattern, `wsum == 0`, which
                // leaves `rgb` zero too), the tone path gives [0, 0, 0]
                // (luminance 0 is never toned, recoloured or raised), so it
                // is skipped. Compared exactly: a small non-zero sum can
                // still round to LED bytes above zero. The road draws the
                // texel's patch itself, the one `patch_at_cached` gives.
                if shortcuts.black_stop && rgb == [0.0; 3] {
                    if road > 0.0 {
                        let texel = RoadTexel {
                            at: (lat, lon),
                            xy: (x, y),
                            radiance,
                            bright,
                            patch: None,
                        };
                        let bytes = road_light.over([0; 3], road, &texel);
                        img.put_pixel(j as u32, i as u32, opaque(bytes));
                    }
                    continue;
                }
                let (mut led_kelvin, mut patch_here) = (None, None);
                if wsum > 0.0 && share > 0.0 {
                    let patch = led::patch_at_cached(x, y, &mut patches);
                    patch_here = Some(patch);
                    if patch.u < share / wsum {
                        led_kelvin = Some(led::patch_kelvin(kelvin / wsum, &patch));
                    }
                }
                let mut bytes = toner.bytes(rgb, led_kelvin, g as f32);
                if road > 0.0 {
                    let texel = RoadTexel {
                        at: (lat, lon),
                        xy: (x, y),
                        radiance,
                        bright,
                        patch: patch_here,
                    };
                    bytes = road_light.over(bytes, road, &texel);
                }
                img.put_pixel(j as u32, i as u32, opaque(bytes));
            }
        }
        img
    }
}

fn opaque([r, g, b]: [u8; 3]) -> Rgba<u8> {
    Rgba([r, g, b, 255])
}

/// Road glow added over a texel's pattern bytes, in linear light. The level
/// follows the glow, the radiance (from the rural floor `country` up to full
/// at `full_nw`) and `night.brightness`; the colour is the white of the
/// texel's LED patch, or the road sodium toned like the pattern's, at
/// luminance 1. Holds what is fixed for a texture, the last region looked up
/// (a texel costs no lookup while it stays in one 1 degree tile) and the
/// last patch cell (no hashing while it stays in one cell).
struct RoadLight<'r> {
    request: &'r LitRequest<'r>,
    linear: &'static [f32; 256],
    encoder: &'static led::ByteEncoder,
    /// The road sodium, toned and at luminance 1: fixed per request.
    sodium: [f32; 3],
    /// The last 1 degree tile looked up and its region.
    region: Option<((i32, i32), LedRegion)>,
    patches: led::PatchCache,
    /// The last LED temperature (its bits) and its white at luminance 1:
    /// a patch keeps one temperature wherever its brightness is settled.
    white: Option<(u64, [f32; 3])>,
}

/// Where a road texel is, and what the pattern pass already knows of it.
#[derive(Debug, Clone, Copy)]
struct RoadTexel {
    /// Latitude and longitude.
    at: (f64, f64),
    /// Web Mercator metres.
    xy: (f64, f64),
    radiance: f64,
    /// `led::bright_weight(radiance)`.
    bright: f64,
    /// The texel's LED patch, when the pattern pass drew it.
    patch: Option<led::Patch>,
}

impl<'r> RoadLight<'r> {
    fn new(request: &'r LitRequest<'r>) -> Self {
        let sodium = led::decode(led::desaturate(
            request.road_model.sodium,
            &request.led.sodium,
        ));
        Self {
            request,
            linear: led::linear_table(),
            encoder: led::ByteEncoder::get(),
            sodium: unit_luminance(sodium),
            region: None,
            patches: led::PatchCache::default(),
            white: None,
        }
    }

    /// `unit_luminance(led::white(kelvin))`, from the last one when the
    /// temperature has not changed.
    fn white(&mut self, kelvin: f64) -> [f32; 3] {
        match self.white {
            Some((k, w)) if k == kelvin.to_bits() => w,
            _ => {
                let w = unit_luminance(led::white(kelvin));
                self.white = Some((kelvin.to_bits(), w));
                w
            }
        }
    }

    fn region(&mut self, tile: (i32, i32)) -> LedRegion {
        match self.region {
            Some((t, r)) if t == tile => r,
            _ => {
                let r = (self.request.regions)(tile.0, tile.1);
                self.region = Some((tile, r));
                r
            }
        }
    }

    /// The bytes of `texel`, whose pattern gave `below`, with `road` glow
    /// (above 0).
    fn over(&mut self, below: [u8; 3], road: f32, texel: &RoadTexel) -> [u8; 3] {
        let RoadTexel {
            at: (lat, lon),
            xy: (x, y),
            radiance,
            bright,
            patch,
        } = *texel;
        let r = self.request;
        let m = r.road_model;
        let density = (radiance / m.full_nw).clamp(0.0, 1.0) as f32;
        let level = m.peak * road * (m.country + (1.0 - m.country) * density) * r.brightness as f32;
        let region = self.region((floor_i64(lat) as i32, floor_i64(lon) as i32));
        let tone = r.led.tone(false, bright, region);
        // `patch.u` is below 1, so no share means sodium without a patch.
        let patch = (tone.share > 0.0)
            .then(|| patch.unwrap_or_else(|| led::patch_at_cached(x, y, &mut self.patches)));
        let colour = match patch {
            Some(p) if p.u < tone.share => {
                // `white` is linear light at luminance 1, as `recolour`
                // relies on when it scales it by a linear luminance.
                self.white(led::patch_kelvin(tone.kelvin, &p))
            }
            _ => self.sodium,
        };
        let base = below.map(|v| self.linear[usize::from(v)]);
        [0, 1, 2].map(|k| self.encoder.byte(base[k] + colour[k] * level))
    }
}

/// A linear colour scaled to luminance 1 (black stays black).
fn unit_luminance(c: [f32; 3]) -> [f32; 3] {
    let l = led::luminance(c);
    if l > 0.0 {
        c.map(|v| v / l)
    } else {
        c
    }
}

#[cfg(test)]
mod texel_timing;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{DdsFilename, TextureKind};
    use crate::night::generator::field::tests_support::synthetic;
    use crate::night::generator::led::{LedModel, LedRegion, RegionTone, Tone};
    use crate::night::generator::noise::{hash2, unit};
    use crate::night::generator::tiling::mercator;
    use crate::night::parse_recipe;
    use crate::night::recipe::Recipe;
    use crate::night::roads::field::tests::crossing_field;
    use crate::night::roads::RoadModel;
    use std::f64::consts::PI;

    const URBAN: &[u8] = include_bytes!("../../../tests/fixtures/night/urban.oxn");

    pub(super) fn name_at(lat: f64, lon: f64) -> DdsFilename {
        name_at_zoom(lat, lon, 16)
    }

    fn name_at_zoom(lat: f64, lon: f64, zoom: u8) -> DdsFilename {
        let n = f64::from(1u32 << zoom);
        let x = ((lon + 180.0) / 360.0 * n) as u32;
        let lat_r = lat.to_radians();
        let y = ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
            as u32;
        DdsFilename {
            row: y / 16 * 16,
            col: x / 16 * 16,
            zoom,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    fn digest(img: &RgbaImage) -> u64 {
        img.as_raw().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// The generator's output before the LED stage, for the golden block
    /// with the speckled pattern. With `LedModel::OFF` it must not move.
    const SODIUM_DIGEST: u64 = 0xe5dc_c430_c9ea_f9ee;

    #[test]
    fn the_sodium_output_is_pinned() {
        let img = generate(&name_at(37.20, -122.725), 128, golden, speckled, 1.0);
        assert_eq!(digest(&img), SODIUM_DIGEST, "digest {:#018x}", digest(&img));
    }

    /// LED-on output for the golden block with the speckled pattern and no
    /// roads, under `LedModel::DEFAULT` and `full_share_model()` in region
    /// Na. Captured at d2fd12f, before road compositing and the `patch_at`
    /// cache: generation without roads must not move.
    const LED_DEFAULT_DIGEST: u64 = 0x313c_df53_ad1d_c310;
    const LED_FULL_SHARE_DIGEST: u64 = 0x6f7d_fa2c_f75a_3b14;

    #[test]
    fn the_led_output_is_pinned() {
        for (model, want) in [
            (LedModel::DEFAULT, LED_DEFAULT_DIGEST),
            (full_share_model(), LED_FULL_SHARE_DIGEST),
        ] {
            let img = generate_led(
                &name_at(37.20, -122.725),
                128,
                golden,
                speckled,
                1.0,
                &model,
                LedRegion::Na,
            );
            assert!(img.pixels().any(is_led), "the pin covers LED texels");
            assert_eq!(digest(&img), want, "digest {:#018x}", digest(&img));
        }
    }

    /// Road-lit output: the `crossing_field` road over a varied synthetic
    /// recipe in its tile (classes in irregular bands, brightness from 0 up
    /// across the cells) with the speckled pattern for every class, under
    /// `LedModel::DEFAULT` in region Na. Captured at 2a51a20, before #437
    /// hoisted per-texel work: the road pass reads radiance and LED patches
    /// too, and must not move.
    const ROAD_LED_DIGEST: u64 = 0xb267_c12f_e5c9_e022;

    #[test]
    fn the_road_lit_output_is_pinned() {
        let n = name_at(46.5, 6.5);
        let roads = crossing_field(&n, 128);
        let varied = synthetic(
            46,
            6,
            120,
            |r, c| u8::from((r / 3 + c / 5) % 3 != 0),
            120,
            |r, c| ((r * 37 + c * 11) % 256) as u8,
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (46, 6)).then(|| varied.clone());
        let any_speckled = |_: &str| speckled("lib/g10/terrain10/urban_tmp_sdry3.ter");
        let request = |roads| LitRequest {
            name: &n,
            size: 128,
            brightness: 1.0,
            recipes: &recipes,
            patterns: &any_speckled,
            regions: &|_, _| LedRegion::Na,
            led: &LedModel::DEFAULT,
            roads,
            road_model: &RoadModel::DEFAULT,
        };
        let img = CpuLitGenerator.generate(&request(&roads));
        let without = CpuLitGenerator.generate(&request(&RoadField::empty(128)));
        // The pin covers road glow over lit pattern texels and over black
        // ground (where the road pass draws its own patch), and LED texels.
        let (mut over_lit, mut over_black) = (0, 0);
        for (a, b) in without.pixels().zip(img.pixels()) {
            if a != b {
                if a.0[..3] == [0; 3] {
                    over_black += 1;
                } else {
                    over_lit += 1;
                }
            }
        }
        assert!(over_lit > 0 && over_black > 0, "{over_lit} {over_black}");
        assert!(img.pixels().any(is_led), "the pin covers LED texels");
        assert_eq!(
            digest(&img),
            ROAD_LED_DIGEST,
            "digest {:#018x}",
            digest(&img)
        );
    }

    #[test]
    fn a_supersampled_road_field_with_no_road_leaves_the_output_byte_for_byte() {
        // A capped texture's roads are rastered at twice its size (#431):
        // with no road near, the field is empty and nothing moves.
        use crate::night::roads::field::tests::{crossing, merged};
        let n = name_at(46.5, 6.5);
        let varied = synthetic(
            46,
            6,
            120,
            |_, _| 1,
            120,
            |r, c| ((r * 37 + c * 11) % 256) as u8,
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (46, 6)).then(|| varied.clone());
        let any_speckled = |_: &str| speckled("lib/g10/terrain10/urban_tmp_sdry3.ter");
        let generate = |roads: &RoadField| {
            CpuLitGenerator.generate(&LitRequest {
                name: &n,
                size: 128,
                brightness: 1.0,
                recipes: &recipes,
                patterns: &any_speckled,
                regions: &|_, _| LedRegion::Na,
                led: &LedModel::DEFAULT,
                roads,
                road_model: &RoadModel::DEFAULT,
            })
        };
        let without = generate(&RoadField::empty(128));
        let far = merged((47, 6), &[(100, vec![(6.5, 47.9), (6.6, 47.9)])]);
        for tiles in [vec![], vec![far]] {
            let field = RoadField::raster_supersampled(&n, 128, 2, &tiles, &RoadModel::DEFAULT);
            assert!(field.is_empty());
            assert!(generate(&field) == without, "{} tiles", tiles.len());
        }
        // And a road near it is drawn.
        let road = RoadField::raster_supersampled(&n, 128, 2, &[crossing(&n)], &RoadModel::DEFAULT);
        assert!(generate(&road) != without);
    }

    /// Every texel of the 128 px golden fixture samples the field and the
    /// pattern as the code before #437 did, bit for bit: class weights,
    /// radiance, and the speckled pattern at every class.
    #[test]
    fn the_fixture_samples_as_before_437() {
        use crate::night::generator::field::lookup_reference;
        use crate::night::generator::tiling::sample_reference;
        let n = name_at(37.20, -122.725);
        let TexelCentres { lons, lats, xs, ys } = TexelCentres::of(&n, 128);
        let field = RecipeField::new(&Footprint::from_name(&n), &golden);
        let mut sampler = FieldSampler::new(
            &field,
            TexelAxes {
                lats: &lats,
                lons: &lons,
            },
        );
        assert_eq!(
            sampler.has_tables(),
            (true, true),
            "the fixture reads tables"
        );
        let pattern = speckled("lib/g10/terrain10/urban_tmp_sdry3.ter").unwrap();
        let (mut cache, mut reference_cache) = (CornerCache::default(), CornerCache::default());
        let (mut got, mut want) = (Vec::new(), Vec::new());
        let mut weighted = 0;
        for (i, (&lat, &y)) in lats.iter().zip(&ys).enumerate() {
            for (j, (&lon, &x)) in lons.iter().zip(&xs).enumerate() {
                sampler.class_weights(i, j, &mut got);
                lookup_reference::class_weights(&field, lat, lon, &mut want);
                let bits = |w: &[super::super::field::ClassWeight]| {
                    w.iter()
                        .map(|w| (w.tile, w.slot, w.index, w.weight.to_bits()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(&got), bits(&want), "weights at {lat}, {lon}");
                let radiance = sampler.radiance(i, j);
                let reference = lookup_reference::radiance(&field, lat, lon);
                assert_eq!(radiance.to_bits(), reference.to_bits(), "at {lat}, {lon}");
                for _ in &want {
                    let c = sample_cached(&pattern, x, y, &mut cache);
                    let r = sample_reference::sample_cached(&pattern, x, y, &mut reference_cache);
                    assert_eq!(c.map(f32::to_bits), r.map(f32::to_bits), "at {x}, {y}");
                }
                weighted += usize::from(!want.is_empty());
            }
        }
        assert!(weighted > 128 * 128 / 4, "{weighted} weighted texels");
        assert_eq!(sampler.table_misses(), 0, "every read is in the tables");
    }

    /// The class mask (#438) skips only fixture texels the reference gives
    /// no class weight, and skips some.
    #[test]
    fn the_class_mask_skips_only_unweighted_fixture_texels() {
        use crate::night::generator::field::lookup_reference;
        let n = name_at(37.20, -122.725);
        let TexelCentres { lons, lats, .. } = TexelCentres::of(&n, 128);
        let field = RecipeField::new(&Footprint::from_name(&n), &golden);
        let sampler = FieldSampler::new(
            &field,
            TexelAxes {
                lats: &lats,
                lons: &lons,
            },
        );
        let (mut skipped, mut want) = (0, Vec::new());
        for (i, &lat) in lats.iter().enumerate() {
            for (j, &lon) in lons.iter().enumerate() {
                if sampler.skips_class(i, j) {
                    lookup_reference::class_weights(&field, lat, lon, &mut want);
                    assert!(want.is_empty(), "skipped a weighted texel at {lat}, {lon}");
                    skipped += 1;
                }
            }
        }
        assert!(skipped > 128 * 128 / 10, "{skipped} skipped");
    }

    /// A 32 px pattern black but for a lit square in one corner: sampled,
    /// many texels sum to exactly black.
    fn mostly_black(_: &str) -> Option<Arc<LitPattern>> {
        let rgb = (0..32 * 32)
            .map(|i| {
                if i % 32 < 8 && i / 32 < 8 {
                    [200, 120, 50]
                } else {
                    [0, 0, 0]
                }
            })
            .collect();
        Some(Arc::new(LitPattern::new(32, rgb, 4096.0)))
    }

    /// The generator's shortcuts (#438), the class mask and the stop on
    /// black pattern sums, give the bytes of the full path: with and
    /// without road glow, LED off, on and at full share, over classes whose
    /// pattern is all black and classes whose pattern is mostly black.
    #[test]
    fn the_shortcuts_give_the_bytes_of_the_full_path() {
        let n = name_at(46.5, 6.5);
        let roads = crossing_field(&n, 128);
        let mut banded = (*synthetic(
            46,
            6,
            120,
            |r, c| ((r / 3 + c / 5) % 3) as u8,
            120,
            |r, c| ((r * 37 + c * 11) % 256) as u8,
        ))
        .clone();
        banded.palette = vec![String::new(), "lit".into(), "black".into()];
        let banded = Arc::new(banded);
        let recipes = move |a: i32, b: i32| ((a, b) == (46, 6)).then(|| banded.clone());
        let black = Arc::new(LitPattern::new(4, vec![[0, 0, 0]; 16], 4096.0));
        let all_black = {
            let black = black.clone();
            move |_: &str| Some(black.clone())
        };
        let some_black = move |class: &str| match class {
            "black" => Some(black.clone()),
            _ => mostly_black(class),
        };
        let mut stopped = 0;
        for patterns in [
            &some_black as &(dyn Fn(&str) -> Option<Arc<LitPattern>> + Sync),
            &all_black,
        ] {
            for model in [LedModel::OFF, LedModel::DEFAULT, full_share_model()] {
                for field in [&roads, &RoadField::empty(128)] {
                    let request = LitRequest {
                        name: &n,
                        size: 128,
                        brightness: 1.0,
                        recipes: &recipes,
                        patterns,
                        regions: &|_, _| LedRegion::Na,
                        led: &model,
                        roads: field,
                        road_model: &RoadModel::DEFAULT,
                    };
                    let record = || Recording {
                        toner: led::Toner::new(&model),
                        inputs: Default::default(),
                    };
                    let (fast, full) = (record(), record());
                    let got = CpuLitGenerator.generate_with(&request, &fast, Shortcuts::ALL);
                    let want = CpuLitGenerator.generate_with(&request, &full, Shortcuts::NONE);
                    assert!(got == want, "{model:?}, roads {}", !field.is_empty());
                    let black = |r: &Recording| {
                        r.inputs
                            .borrow()
                            .iter()
                            .filter(|(s, ..)| *s == [0.0; 3])
                            .count()
                    };
                    assert_eq!(black(&fast), 0, "black sums stop before the tone path");
                    stopped += black(&full);
                }
            }
        }
        assert!(stopped > 0, "the full path met black sums");
        // With every pattern black, the road lights texels whose class is
        // weighted under a gain: the stop's road branch is compared above.
        let TexelCentres { lons, lats, .. } = TexelCentres::of(&n, 128);
        let field = RecipeField::new(&Footprint::from_name(&n), &recipes);
        let mut sampler = FieldSampler::new(
            &field,
            TexelAxes {
                lats: &lats,
                lons: &lons,
            },
        );
        let (mut weights, mut under_road) = (Vec::new(), 0);
        for i in 0..128 {
            let Some(row) = roads.row(i as u32) else {
                continue;
            };
            for (j, &road) in row.iter().enumerate() {
                sampler.class_weights(i, j, &mut weights);
                let lit = gain(sampler.radiance(i, j), 1.0) > 0.0;
                under_road += usize::from(road > 0.0 && lit && !weights.is_empty());
            }
        }
        assert!(under_road > 0, "no weighted texel under the road");
    }

    /// The pinned generations' sampling never leaves the sampler's tables
    /// (at their size and at larger ones), so the fallback to the field
    /// stays a fallback: a table reach that stopped covering the warp
    /// (`WARP_AMPLITUDE_CELLS` raised past it) fails here.
    #[test]
    fn pinned_generations_read_only_their_tables() {
        // The road pin's recipe.
        let varied = synthetic(
            46,
            6,
            120,
            |r, c| u8::from((r / 3 + c / 5) % 3 != 0),
            120,
            |r, c| ((r * 37 + c * 11) % 256) as u8,
        );
        let road_recipes = move |a: i32, b: i32| ((a, b) == (46, 6)).then(|| varied.clone());
        let inputs: [(DdsFilename, crate::night::generator::RecipeLookup<'_>); 2] = [
            (name_at(37.20, -122.725), &golden),
            (name_at(46.5, 6.5), &road_recipes),
        ];
        for (n, recipes) in &inputs {
            for size in [128, 512] {
                let TexelCentres { lons, lats, .. } = TexelCentres::of(n, size);
                let field = RecipeField::new(&Footprint::from_name(n), *recipes);
                let mut sampler = FieldSampler::new(
                    &field,
                    TexelAxes {
                        lats: &lats,
                        lons: &lons,
                    },
                );
                assert_eq!(sampler.has_tables(), (true, true), "{n:?} at {size}");
                let mut weights = Vec::new();
                for i in 0..lats.len() {
                    for j in 0..lons.len() {
                        sampler.class_weights(i, j, &mut weights);
                        sampler.radiance(i, j);
                    }
                }
                assert_eq!(sampler.table_misses(), 0, "{n:?} at {size}");
            }
        }
    }

    /// The generator's tone path is byte for byte the `powf` path it
    /// replaced (#436), at several models, brightnesses and regions.
    #[test]
    fn the_generator_gives_the_powf_tone_path_bytes() {
        let strong_tone = LedModel {
            sodium: led::SodiumTone {
                mix: 1.0,
                kelvin: 5000.0,
                energy: 1.3,
            },
            ..full_share_model()
        };
        let n = name_at(37.20, -122.725);
        for model in [
            LedModel::DEFAULT,
            LedModel::OFF,
            full_share_model(),
            strong_tone,
        ] {
            for (brightness, region) in [(1.0, LedRegion::Na), (0.5, LedRegion::Af)] {
                let request = LitRequest {
                    name: &n,
                    size: 128,
                    brightness,
                    recipes: &golden,
                    patterns: &speckled,
                    regions: &move |_, _| region,
                    led: &model,
                    roads: &RoadField::empty(128),
                    road_model: &RoadModel::DEFAULT,
                };
                let img = CpuLitGenerator.generate(&request);
                let reference = CpuLitGenerator.generate_with(
                    &request,
                    &led::powf_reference::PowfToner(&model),
                    Shortcuts::ALL,
                );
                assert!(lit_texels(&img) > 0);
                assert!(img == reference, "{model:?} at {brightness} in {region:?}");
            }
        }
    }

    fn golden(lat: i32, lon: i32) -> Option<Arc<Recipe>> {
        ((lat, lon) == (37, -123)).then(|| Arc::new(parse_recipe(URBAN).unwrap()))
    }

    fn red(name: &str) -> Option<Arc<LitPattern>> {
        (name == "lib/g10/terrain10/urban_tmp_sdry3.ter")
            .then(|| Arc::new(LitPattern::new(4, vec![[255, 0, 0]; 16], 4096.0)))
    }

    fn generate(
        name: &DdsFilename,
        size: u32,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        patterns: impl Fn(&str) -> Option<Arc<LitPattern>> + Sync,
        brightness: f64,
    ) -> RgbaImage {
        generate_led(
            name,
            size,
            recipes,
            patterns,
            brightness,
            &LedModel::OFF,
            LedRegion::Unknown,
        )
    }

    fn generate_led(
        name: &DdsFilename,
        size: u32,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        patterns: impl Fn(&str) -> Option<Arc<LitPattern>> + Sync,
        brightness: f64,
        led: &LedModel,
        region: LedRegion,
    ) -> RgbaImage {
        CpuLitGenerator.generate(&LitRequest {
            name,
            size,
            brightness,
            recipes: &recipes,
            patterns: &patterns,
            regions: &move |_, _| region,
            led,
            roads: &RoadField::empty(size),
            road_model: &RoadModel::DEFAULT,
        })
    }

    fn generate_roads(
        name: &DdsFilename,
        size: u32,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        patterns: impl Fn(&str) -> Option<Arc<LitPattern>> + Sync,
        roads: &RoadField,
        led: &LedModel,
    ) -> RgbaImage {
        generate_roads_with(
            name,
            size,
            recipes,
            patterns,
            roads,
            led,
            &RoadModel::DEFAULT,
        )
    }

    /// [`generate_roads`] with another road model.
    fn generate_roads_with(
        name: &DdsFilename,
        size: u32,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        patterns: impl Fn(&str) -> Option<Arc<LitPattern>> + Sync,
        roads: &RoadField,
        led: &LedModel,
        road_model: &RoadModel,
    ) -> RgbaImage {
        CpuLitGenerator.generate(&LitRequest {
            name,
            size,
            brightness: 1.0,
            recipes: &recipes,
            patterns: &patterns,
            regions: &|_, _| LedRegion::Unknown,
            led,
            roads,
            road_model,
        })
    }

    /// Every tile is dark: no class, radiance 0.
    fn dark(lat: i32, lon: i32) -> Option<Arc<Recipe>> {
        Some(synthetic(lat, lon, 4, |_, _| 0, 4, |_, _| 0))
    }

    #[test]
    fn a_road_field_of_another_size_is_ignored() {
        let name = name_at(46.5, 6.5);
        let wrong = crossing_field(&name, 32);
        assert!(!wrong.is_empty());
        let img = generate_roads(&name, 64, dark, red, &wrong, &LedModel::OFF);
        assert_eq!(lit_texels(&img), 0, "no glow at the wrong places");
    }

    #[test]
    fn countryside_roads_are_lit_where_viirs_reads_zero() {
        let name = name_at(46.5, 6.5);
        let field = crossing_field(&name, 64);
        let img = generate_roads(&name, 64, dark, red, &field, &LedModel::OFF);
        assert!(lit_texels(&img) > 0);
        let none = generate_roads(&name, 64, dark, red, &RoadField::empty(64), &LedModel::OFF);
        assert_eq!(lit_texels(&none), 0);
    }

    #[test]
    fn road_light_is_never_dimmer_than_the_pattern_below_it() {
        // The tile under the road lit in east-west stripes (so the road
        // crosses lit and dark ground) at a middling gain.
        let name = name_at(46.5, 6.5);
        let half = synthetic(46, 6, 120, |r, _| u8::from(r % 2 == 0), 4, |_, _| 60);
        let recipes = move |a: i32, b: i32| ((a, b) == (46, 6)).then(|| half.clone());
        let field = crossing_field(&name, 64);
        for model in [LedModel::OFF, LedModel::DEFAULT] {
            let empty = RoadField::empty(64);
            let base = generate_roads(&name, 64, &recipes, orange, &empty, &model);
            let with = generate_roads(&name, 64, &recipes, orange, &field, &model);
            let mut brighter = 0;
            for (a, b) in base.pixels().zip(with.pixels()) {
                assert!(lum(b) + 1e-3 >= lum(a), "{b:?} dimmer than {a:?}");
                brighter += usize::from(lum(b) > lum(a) && a[0] > 0);
            }
            assert!(brighter > 0, "the road crosses lit pattern texels");
        }
    }

    #[test]
    fn road_hue_follows_the_led_patch() {
        let name = name_at(46.5, 6.5);
        let field = crossing_field(&name, 128);
        // Every road texel LED, whatever its region.
        let all_led = LedModel {
            regions: [RegionTone {
                share_scale: 1.0,
                kelvin_shift: 0.0,
            }; LedRegion::COUNT],
            ..full_share_model()
        };
        // Bright enough over dark ground that the road's bytes clear the
        // rounding of the faint edge; the hue rule does not depend on it.
        let bright = RoadModel {
            peak: 0.12,
            ..RoadModel::DEFAULT
        };
        let gen = |led: &LedModel| generate_roads_with(&name, 128, dark, red, &field, led, &bright);
        let img = gen(&all_led);
        let warm = gen(&sodium_only());
        let blue_red = |p: &image::Rgba<u8>| f32::from(p[2]) / f32::from(p[0]);
        let mut compared = 0;
        for (led, sodium) in img.pixels().zip(warm.pixels()) {
            // Away from the faint edge, where bytes round to a few levels.
            if sodium[0] >= 32 {
                compared += 1;
                assert!(
                    blue_red(led) > blue_red(sodium),
                    "LED {led:?} not whiter than sodium {sodium:?}"
                );
            }
        }
        assert!(compared > 0, "the road is lit");
    }

    /// The composition rule written out plainly, with no table or memo.
    fn composite_reference(
        below: [u8; 3],
        road: f32,
        radiance: f64,
        (lat, lon): (f64, f64),
        (x, y): (f64, f64),
        r: &LitRequest<'_>,
    ) -> ([u8; 3], bool) {
        let m = r.road_model;
        let density = (radiance / m.full_nw).clamp(0.0, 1.0) as f32;
        let level = m.peak * road * (m.country + (1.0 - m.country) * density) * r.brightness as f32;
        let region = (r.regions)(lat.floor() as i32, lon.floor() as i32);
        let tone = r.led.tone(false, led::bright_weight(radiance), region);
        let patch = led::patch_at(x, y);
        let is_led = patch.u < tone.share;
        let mut colour = if is_led {
            led::white(led::patch_kelvin(tone.kelvin, &patch))
        } else {
            led::decode(led::desaturate(m.sodium, &r.led.sodium))
        };
        let l = led::luminance(colour);
        if l > 0.0 {
            colour = colour.map(|c| c / l);
        }
        let base = led::decode(below.map(f32::from));
        let out = led::encode([0, 1, 2].map(|k| base[k] + colour[k] * level));
        (out.map(|v| v.round().clamp(0.0, 255.0) as u8), is_led)
    }

    #[test]
    fn road_light_matches_the_plain_composition_rule() {
        // Regions change by tile, so the region memo is exercised across
        // degree lines; positions sweep many LED patches.
        let regions = |lat: i32, lon: i32| match (lat + lon).rem_euclid(3) {
            0 => LedRegion::Na,
            1 => LedRegion::Af,
            _ => LedRegion::Eu,
        };
        let name = name_at(46.5, 6.5);
        let roads = RoadField::empty(1);
        for (brightness, model) in [(1.0, LedModel::DEFAULT), (1.5, LedModel::OFF)] {
            let request = LitRequest {
                name: &name,
                size: 1,
                brightness,
                recipes: &|_, _| None,
                patterns: &|_| None,
                regions: &regions,
                led: &model,
                roads: &roads,
                road_model: &RoadModel::DEFAULT,
            };
            let mut light = RoadLight::new(&request);
            let (mut whites, mut sodiums) = (0, 0);
            for k in 0..4000u64 {
                let h = hash2(k as i64, 0, 7);
                let below = [
                    (h & 255) as u8,
                    ((h >> 8) & 255) as u8,
                    ((h >> 16) & 63) as u8,
                ];
                let road = unit(h >> 24) as f32;
                let radiance = unit(h >> 32) * 120.0;
                // A walk of about 0.004 degrees a step through three degree lines.
                let (lat, lon) = (45.9 + k as f64 * 0.0004, 5.8 + k as f64 * 0.0006);
                let (x, y) = mercator(lat, lon);
                // Half the steps hand over the bright weight and the patch,
                // as the generator does where the pattern was lit.
                let patch = (k % 2 == 0).then(|| led::patch_at(x, y));
                let bright = led::bright_weight(radiance);
                let texel = RoadTexel {
                    at: (lat, lon),
                    xy: (x, y),
                    radiance,
                    bright,
                    patch,
                };
                let got = light.over(below, road, &texel);
                let (want, is_led) =
                    composite_reference(below, road, radiance, (lat, lon), (x, y), &request);
                assert_eq!(got, want, "step {k}");
                (whites, sodiums) = if is_led {
                    (whites + 1, sodiums)
                } else {
                    (whites, sodiums + 1)
                };
            }
            assert!(sodiums > 0, "the sodium colour is compared");
            if model == LedModel::DEFAULT {
                assert!(whites > 0, "the LED white is compared");
            }
        }
    }

    fn lit_texels(img: &RgbaImage) -> usize {
        img.pixels()
            .filter(|p| p[0] > 0 || p[1] > 0 || p[2] > 0)
            .count()
    }

    #[test]
    fn a_lit_block_lights_only_its_cells() {
        // The golden urban block (lat 37.15..37.25, lon -122.75..-122.70)
        // sits under radiance 230 (gain 1 with the provisional curve).
        let img = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(img.dimensions(), (64, 64));
        let lit = lit_texels(&img);
        assert!(lit > 0, "the block lights");
        assert!(lit < 64 * 64, "cells outside the block stay dark");
        assert!(img.pixels().all(|p| p[1] == 0 && p[2] == 0 && p[3] == 255));
    }

    #[test]
    fn countryside_and_unlit_classes_are_black() {
        let img = generate(&name_at(37.60, -122.20), 32, golden, red, 1.0);
        assert_eq!(lit_texels(&img), 0);
        let img = generate(&name_at(37.20, -122.725), 32, golden, |_| None, 1.0);
        assert_eq!(lit_texels(&img), 0, "no pattern resolves");
    }

    #[test]
    fn zero_radiance_is_black() {
        let dark = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| dark.clone());
        let img = generate(&name_at(37.5, -122.5), 32, recipes, white, 1.0);
        assert_eq!(lit_texels(&img), 0);
    }

    /// A white 4 x 4 pattern: stochastic tiling of a solid colour is that colour,
    /// so a texel's value is its class weight times the gain times 255.
    fn white(_: &str) -> Option<Arc<LitPattern>> {
        Some(Arc::new(LitPattern::new(
            4,
            vec![[255, 255, 255]; 16],
            4096.0,
        )))
    }

    fn speckled(class: &str) -> Option<Arc<LitPattern>> {
        (class == "lib/g10/terrain10/urban_tmp_sdry3.ter").then(|| {
            let rgb = (0..32 * 32)
                .map(|i| {
                    let h = hash2(i, 0, 99);
                    [(unit(h) * 255.0) as u8, (unit(h >> 8) * 255.0) as u8, 0]
                })
                .collect();
            Arc::new(LitPattern::new(32, rgb, 4096.0))
        })
    }

    pub(super) fn name(row: u32, col: u32, zoom: u8) -> DdsFilename {
        DdsFilename {
            row,
            col,
            zoom,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    #[test]
    fn the_same_ground_gives_the_same_light_at_any_zoom() {
        // A ZL15 texture at 2N px contains the ZL16 texture at N px as one of
        // its quarters; their texel centres coincide, so their pixels match.
        let z16 = name_at(37.20, -122.725);
        let z15 = name(z16.row / 32 * 16, z16.col / 32 * 16, 15);
        let big = generate(&z15, 128, golden, speckled, 1.0);
        let small = generate(&z16, 64, golden, speckled, 1.0);
        assert!(
            lit_texels(&small) > 64 * 64 / 20,
            "the compared ground holds the lit block: {}",
            lit_texels(&small)
        );
        let x0 = (z16.col - 2 * z15.col) / 16 * 64;
        let y0 = (z16.row - 2 * z15.row) / 16 * 64;
        let mut worst = 0i32;
        for y in 0..64 {
            for x in 0..64 {
                let (a, b) = (big.get_pixel(x0 + x, y0 + y), small.get_pixel(x, y));
                for ch in 0..3 {
                    worst = worst.max((i32::from(a[ch]) - i32::from(b[ch])).abs());
                }
            }
        }
        assert!(worst <= 2, "largest difference {worst}");
    }

    #[test]
    fn no_brightness_step_at_a_degree_line() {
        // Texture col 10544 at ZL16 spans lon -122.080..-121.992, across -122.
        // The west tile is brighter than the east one. Brightness 100 is
        // radiance 8 (gain 0.36); nearest-cell brightness would step from
        // full light to that at the line, bilinear brightness ramps.
        let west = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 255);
        let east = synthetic(37, -122, 120, |_, _| 1, 4, |_, _| 100);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        // Row 25392 spans lat 37.440..37.510.
        let img = generate(&name(25392, 10544, 16), 64, recipes, white, 1.0);
        let (mut lo, mut hi, mut step) = (255u8, 0u8, 0i32);
        for y in 0..64 {
            for x in 0..64 {
                let v = img.get_pixel(x, y)[0];
                lo = lo.min(v);
                hi = hi.max(v);
                if x < 63 {
                    let next = img.get_pixel(x + 1, y)[0];
                    step = step.max((i32::from(next) - i32::from(v)).abs());
                }
            }
        }
        assert!(hi - lo >= 10, "both brightnesses show: {lo}..{hi}");
        assert!(step <= 3, "largest step between neighbours {step}");
    }

    #[test]
    fn a_first_tile_without_brightness_does_not_darken_its_neighbour() {
        // Texture col 10560 at ZL16 spans lon -121.992..-121.904, so its
        // field, grown by the margin, starts in tile (37, -123), which has
        // no brightness layer; (37, -122) is lit. East of the west tile's
        // class reach, the texture is lit as it is without the west recipe
        // (#434); before, the west recipe's missing layer set the field's
        // brightness grid and the east tile read dark.
        let east = synthetic(
            37,
            -122,
            120,
            |r, c| u8::from((r + c) % 4 != 0),
            240,
            |r, c| (128 + (r * 7 + c * 3) % 128) as u8,
        );
        let mut west = (*synthetic(37, -123, 120, |_, _| 1, 240, |_, _| 0)).clone();
        west.brightness = crate::night::recipe::Grid::empty();
        let west = Arc::new(west);
        let both = {
            let (w, e) = (west.clone(), east.clone());
            move |a: i32, b: i32| match (a, b) {
                (37, -123) => Some(w.clone()),
                (37, -122) => Some(e.clone()),
                _ => None,
            }
        };
        let alone = move |a: i32, b: i32| ((a, b) == (37, -122)).then(|| east.clone());
        let n = name(25392, 10560, 16);
        assert_eq!(
            Footprint::from_name(&n)
                .expanded(super::super::field::FIELD_MARGIN_DEG)
                .tiles(),
            vec![(37, -123), (37, -122)]
        );
        let (a, b) = (
            generate(&n, 128, both, speckled_any, 1.0),
            generate(&n, 128, alone, speckled_any, 1.0),
        );
        let lons = TexelCentres::of(&n, 128).lons;
        let mut lit = 0;
        for (j, &lon) in lons.iter().enumerate().filter(|(_, &l)| l > -121.98) {
            for i in 0..128 {
                let (p, q) = (a.get_pixel(j as u32, i), b.get_pixel(j as u32, i));
                assert_eq!(p, q, "at column {j} (lon {lon}), row {i}");
                lit += usize::from(p[0] > 0);
            }
        }
        assert!(lit > 0, "the east tile is lit");
    }

    /// The speckled pattern for every class.
    fn speckled_any(_: &str) -> Option<Arc<LitPattern>> {
        speckled("lib/g10/terrain10/urban_tmp_sdry3.ter")
    }

    #[test]
    fn class_edges_are_soft() {
        // West half of tile +37-123 lit; a ZL14 texture straddling lon -122.5.
        let half = synthetic(37, -123, 120, |_, c| u8::from(c < 60), 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| half.clone());
        // ZL14 texture 6336_2608 spans lat 37.440..37.719, lon -122.695..-122.344.
        let img = generate(&name(6336, 2608, 14), 256, recipes, white, 1.0);
        let partial = img.pixels().filter(|p| p[0] > 12 && p[0] < 243).count();
        assert!(
            partial > 256,
            "a ramp along the edge, got {partial} partial texels"
        );
    }

    fn orange(_: &str) -> Option<Arc<LitPattern>> {
        Some(Arc::new(LitPattern::new(
            4,
            vec![[200, 120, 50]; 16],
            4096.0,
        )))
    }

    /// A whole tile of class 1 under bright radiance.
    fn uniform() -> impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync + Clone {
        let r = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 200);
        move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone())
    }

    fn is_led(p: &image::Rgba<u8>) -> bool {
        // Orange 200,120,50 has blue/red 0.25; any LED white is far bluer.
        p[0] > 0 && f32::from(p[2]) / f32::from(p[0]) > 0.4
    }

    /// No LED, but the default sodium stage: what the generator writes for
    /// texels the LED stage leaves alone.
    fn sodium_only() -> LedModel {
        LedModel {
            sodium: LedModel::DEFAULT.sodium,
            ..LedModel::OFF
        }
    }

    fn saturation(p: &image::Rgba<u8>) -> f32 {
        let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
        f32::from(mx - mn) / f32::from(mx)
    }

    fn lum(p: &image::Rgba<u8>) -> f32 {
        let l = |v: u8| (f32::from(v) / 255.0).powf(crate::night::generator::SRGB_GAMMA);
        0.2126 * l(p[0]) + 0.7152 * l(p[1]) + 0.0722 * l(p[2])
    }

    #[test]
    fn the_off_model_keeps_the_sodium_output() {
        let img = generate_led(
            &name_at(37.20, -122.725),
            128,
            golden,
            speckled,
            1.0,
            &LedModel::OFF,
            LedRegion::Na,
        );
        assert_eq!(digest(&img), SODIUM_DIGEST);
    }

    /// Every urban tone at share 1 and 4000 K.
    fn full_share_model() -> LedModel {
        LedModel {
            urban_bright: Tone {
                share: 1.0,
                kelvin: 4000.0,
            },
            urban_dim: Tone {
                share: 1.0,
                kelvin: 4000.0,
            },
            ..LedModel::DEFAULT
        }
    }

    #[test]
    fn full_share_makes_every_lit_texel_led() {
        let img = generate_led(
            &name_at(37.5, -122.5),
            64,
            uniform(),
            orange,
            1.0,
            &full_share_model(),
            LedRegion::Na,
        );
        assert!(img.pixels().filter(|p| p[0] > 0).all(is_led));
    }

    #[test]
    fn led_texels_are_never_dimmer_than_sodium() {
        let name = name_at(37.5, -122.5);
        let sodium = generate_led(
            &name,
            128,
            uniform(),
            orange,
            1.0,
            &sodium_only(),
            LedRegion::Na,
        );
        let led = generate_led(
            &name,
            128,
            uniform(),
            orange,
            1.0,
            &LedModel::DEFAULT,
            LedRegion::Na,
        );
        let mut changed = 0;
        for (s, l) in sodium.pixels().zip(led.pixels()) {
            if s != l {
                changed += 1;
                assert!(lum(l) >= lum(s) * 1.1, "LED {l:?} not brighter than {s:?}");
            }
        }
        assert!(changed > 0, "some patches are LED");
    }

    #[test]
    fn the_default_model_desaturates_the_sodium_it_keeps() {
        let name = name_at(37.5, -122.5);
        let laminar = generate_led(
            &name,
            64,
            uniform(),
            orange,
            1.0,
            &LedModel::OFF,
            LedRegion::Na,
        );
        let kept = generate_led(
            &name,
            64,
            uniform(),
            orange,
            1.0,
            &sodium_only(),
            LedRegion::Na,
        );
        let mut lit = 0;
        for (a, b) in laminar.pixels().zip(kept.pixels()) {
            if a[0] == 0 {
                continue;
            }
            lit += 1;
            assert!(
                saturation(b) < saturation(a),
                "{b:?} not less saturated than {a:?}"
            );
            assert!(!is_led(b), "{b:?} reads as LED");
        }
        assert!(lit > 0, "the tile is lit");
    }

    /// The sodium stage never writes a texel dimmer than Laminar's bytes,
    /// from the gain floor up and at every brightness setting.
    #[test]
    fn desaturated_sodium_is_never_dimmer_after_rounding_to_bytes() {
        let name = name_at(37.5, -122.5);
        let mut changed = 0;
        for rgb in [[200u8, 120, 50], [255, 160, 60], [90, 40, 10], [56, 36, 18]] {
            let pattern = move |_: &str| Some(Arc::new(LitPattern::new(4, vec![rgb; 16], 4096.0)));
            for byte in (1u8..=255).step_by(2).chain([255]) {
                let r = synthetic(37, -123, 120, |_, _| 1, 4, move |_, _| byte);
                let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
                for brightness in [0.5, 1.0, 1.5] {
                    let run = |m: &LedModel| {
                        generate_led(
                            &name,
                            16,
                            recipes.clone(),
                            pattern,
                            brightness,
                            m,
                            LedRegion::Na,
                        )
                    };
                    let (laminar, kept) = (run(&LedModel::OFF), run(&sodium_only()));
                    for (a, b) in laminar.pixels().zip(kept.pixels()) {
                        if a != b {
                            changed += 1;
                            assert!(
                                lum(b) >= lum(a),
                                "{rgb:?}, byte {byte}, brightness {brightness}: \
                                 {b:?} dimmer than {a:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(changed > 0, "the sweep reaches desaturated texels");
    }

    /// Every texel the LED stage changes is at least as bright as the
    /// sodium texel it replaced, measured on the bytes that are written,
    /// from the gain floor up and at every brightness setting.
    #[test]
    fn led_is_never_dimmer_after_rounding_to_bytes() {
        let name = name_at(37.5, -122.5);
        let colours: [[u8; 3]; 5] = [
            [200, 120, 50],
            [255, 195, 0],
            [255, 160, 60],
            [255, 255, 255],
            [90, 40, 10],
        ];
        let mut changed = 0;
        for kelvin in [3200.0, 4000.0, 5700.0] {
            let tone = Tone { share: 1.0, kelvin };
            let all = LedModel {
                industrial: tone,
                urban_bright: tone,
                urban_dim: tone,
                regions: [RegionTone {
                    share_scale: 1.0,
                    kelvin_shift: 0.0,
                }; LedRegion::COUNT],
                ..LedModel::DEFAULT
            };
            for rgb in colours {
                let pattern =
                    move |_: &str| Some(Arc::new(LitPattern::new(4, vec![rgb; 16], 4096.0)));
                for byte in (1u8..=255).step_by(2).chain([255]) {
                    let r = synthetic(37, -123, 120, |_, _| 1, 4, move |_, _| byte);
                    let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
                    for brightness in [0.5, 1.0, 1.5] {
                        let run = |m: &LedModel| {
                            generate_led(
                                &name,
                                16,
                                recipes.clone(),
                                pattern,
                                brightness,
                                m,
                                LedRegion::Na,
                            )
                        };
                        let (sodium, led) = (run(&sodium_only()), run(&all));
                        for (s, l) in sodium.pixels().zip(led.pixels()) {
                            if s != l {
                                changed += 1;
                                assert!(
                                    lum(l) >= lum(s),
                                    "{rgb:?} at {kelvin} K, byte {byte}, brightness \
                                     {brightness}: LED {l:?} dimmer than sodium {s:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(changed > 0, "the sweep reaches LED texels");
    }

    #[test]
    fn the_led_share_matches_the_model() {
        // A ZL13 texture is about 78 km across: thousands of patches, so the
        // fraction is a good estimate of the share.
        let name = name_at_zoom(37.5, -122.5, 13);
        let img = generate_led(
            &name,
            256,
            uniform(),
            orange,
            1.0,
            &LedModel::DEFAULT,
            LedRegion::Na,
        );
        let lit = img.pixels().filter(|p| p[0] > 0).count();
        assert!(lit > 0, "the texture is over the uniform tile");
        let share = img.pixels().filter(|p| is_led(p)).count() as f64 / lit as f64;
        assert!(
            (share - 0.75).abs() < 0.15,
            "urban bright NA share 0.75, got {share}"
        );
    }

    #[test]
    fn led_patches_are_coherent() {
        let img = generate_led(
            &name_at(37.5, -122.5),
            256,
            uniform(),
            orange,
            1.0,
            &LedModel::DEFAULT,
            LedRegion::Na,
        );
        let (mut pairs, mut flips) = (0, 0);
        for y in 0..256 {
            for x in 0..255 {
                let (a, b) = (img.get_pixel(x, y), img.get_pixel(x + 1, y));
                if a[0] > 0 && b[0] > 0 {
                    pairs += 1;
                    flips += usize::from(is_led(a) != is_led(b));
                }
            }
        }
        assert!(
            flips * 10 < pairs,
            "patch edges only: {flips} flips in {pairs} pairs"
        );
    }

    #[test]
    fn led_decisions_agree_across_zoom() {
        // ZL16 texture and the ZL17 texture covering its north-west quarter.
        let z16 = name_at(37.5, -122.5);
        let z17 = DdsFilename {
            row: z16.row * 2,
            col: z16.col * 2,
            zoom: 17,
            ..z16.clone()
        };
        let a = generate_led(
            &z16,
            256,
            uniform(),
            orange,
            1.0,
            &LedModel::DEFAULT,
            LedRegion::Na,
        );
        let b = generate_led(
            &z17,
            256,
            uniform(),
            orange,
            1.0,
            &LedModel::DEFAULT,
            LedRegion::Na,
        );
        // ZL17 texel (2i+1, 2j+1) shares its centre region with ZL16 texel (i, j)
        // in the quarter; compare away from patch edges by majority over the image.
        let (mut same, mut total) = (0, 0);
        for j in 0..128 {
            for i in 0..128 {
                let (p, q) = (a.get_pixel(i, j), b.get_pixel(2 * i + 1, 2 * j + 1));
                if p[0] > 0 && q[0] > 0 {
                    total += 1;
                    same += usize::from(is_led(p) == is_led(q));
                }
            }
        }
        assert!(total > 0);
        assert!(same * 100 >= total * 97, "{same} of {total} agree");
    }

    #[test]
    fn region_moves_the_share() {
        let name = name_at_zoom(37.5, -122.5, 13);
        let count = |r| {
            generate_led(&name, 128, uniform(), orange, 1.0, &LedModel::DEFAULT, r)
                .pixels()
                .filter(|p| is_led(p))
                .count()
        };
        assert!(
            count(LedRegion::Na) > count(LedRegion::Af),
            "NA adopts more LED than AF"
        );
    }

    /// The directory holding the installed packages, for the release-build
    /// timings over real textures. Read by these tests only.
    const PACKAGES_VAR: &str = "XEL_NIGHT_PACKAGES";
    /// X-Plane's `Resources/default scenery/1000 world terrain`, for the
    /// same timings. Read by these tests only.
    const ART_VAR: &str = "XEL_LAMINAR_ART";

    /// The installation the release-build timings read, from
    /// [`PACKAGES_VAR`] and [`ART_VAR`]: the packages directory and the
    /// Laminar art. Says on stderr what is missing, so a skipped timing
    /// never passes silently.
    pub(super) fn real_install() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let dir = |var: &str, what: &str| match std::env::var_os(var) {
            Some(v) if std::path::Path::new(&v).is_dir() => Some(std::path::PathBuf::from(v)),
            Some(v) => {
                eprintln!("{var}={v:?} is not a directory; skipped");
                None
            }
            None => {
                eprintln!("set {var} to {what}; skipped");
                None
            }
        };
        let packages = dir(PACKAGES_VAR, "the directory holding the installed packages");
        let art = dir(
            ART_VAR,
            "X-Plane's \"Resources/default scenery/1000 world terrain\"",
        );
        Some((packages?, art?))
    }

    /// The package `pkg` of the installation, or `None` (said on stderr)
    /// when it is not installed.
    fn real_package(packages: &std::path::Path, pkg: &str) -> Option<std::path::PathBuf> {
        let path = packages.join(pkg);
        if path.is_dir() {
            Some(path)
        } else {
            eprintln!("no {pkg} in {}; skipped", packages.display());
            None
        }
    }

    /// Release-build timing of one Mexico City ZL16 texture at 1024 px on
    /// the installation [`real_install`] names; skipped elsewhere. Run with
    /// `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
    /// xearthlayer --lib time_a_real_zl16_texture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn time_a_real_zl16_texture() {
        let Some((packages, art)) = real_install() else {
            return;
        };
        let Some(pkg) = real_package(&packages, "zzXEL_na-mexico-central_ortho") else {
            return;
        };
        let store = crate::night::RecipeStore::new(vec![pkg]);
        let art = crate::night::LaminarArt::new(art);
        let n = name_at(19.4326, -99.1332);
        // Cold: the first call inflates the recipes and decodes the patterns.
        // Warm: the second call finds them cached, as a flight usually does
        // after the probe.
        for pass in ["cold", "warm"] {
            let started = std::time::Instant::now();
            let img = generate_led(
                &n,
                1024,
                |a, b| store.get(a, b),
                |c| art.pattern(c),
                1.0,
                &LedModel::DEFAULT,
                LedRegion::Na,
            );
            let ms = started.elapsed().as_millis();
            let digest = img.as_raw().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
            println!("ZL16 1024 px, {pass}: {ms} ms, pixels {digest:016x}");
            assert!(img.pixels().any(|p| p[0] > 0));
        }
        // The probe runs on every lit stat; time it warm at ZL16 and ZL12.
        let lookup = |a: i32, b: i32| store.get(a, b);
        for n in [
            n.clone(),
            name(n.row / 16 / 16 * 16, n.col / 16 / 16 * 16, 12),
        ] {
            let started = std::time::Instant::now();
            let lit = RecipeField::new(&Footprint::from_name(&n), &lookup)
                .has_light(|c| art.lit_terrain(c).is_some());
            let us = started.elapsed().as_micros();
            println!("probe ZL{}: {us} us, lit {lit}", n.zoom);
        }
    }

    /// Release-build timing of road light on the installation
    /// [`real_install`] names (Geneva, ZL16 at 1024 px, eu-west night
    /// recipes); skipped elsewhere. Run with `XEL_NIGHT_PACKAGES=...
    /// XEL_LAMINAR_ART=... cargo test --release -p xearthlayer --lib
    /// time_road_light_on_a_real_zl16_texture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn time_road_light_on_a_real_zl16_texture() {
        use crate::night::roads::field::tests::{crossing, street_grid_over};
        let Some((packages, art)) = real_install() else {
            return;
        };
        let Some(pkg) = real_package(&packages, "zzXEL_eu-west_ortho") else {
            return;
        };
        let store = crate::night::RecipeStore::new(vec![pkg]);
        let art = crate::night::LaminarArt::new(art);
        let n = name_at(46.20, 6.14);
        let size = 1024;
        let model = RoadModel::DEFAULT;
        let empty = RoadField::empty(size);
        let sparse = RoadField::raster(&n, size, &[crossing(&n)], &model);
        let grid = street_grid_over(&n, 100.0);
        let started = std::time::Instant::now();
        let dense = RoadField::raster(&n, size, &[grid], &model);
        println!(
            "100 m street grid raster: {:.1} ms, {} segments, {} lit texels",
            started.elapsed().as_secs_f64() * 1e3,
            dense.stats.segments,
            dense.stats.lit_texels
        );
        let run = |roads: &RoadField| {
            CpuLitGenerator.generate(&LitRequest {
                name: &n,
                size,
                brightness: 1.0,
                recipes: &|a, b| store.get(a, b),
                patterns: &|c| art.pattern(c),
                regions: &|_, _| LedRegion::Na,
                led: &LedModel::DEFAULT,
                roads,
                road_model: &model,
            })
        };
        // Inflates the recipes and decodes the patterns.
        run(&empty);
        for (label, roads) in [
            ("no roads", &empty),
            ("one motorway", &sparse),
            ("100 m street grid", &dense),
        ] {
            let mut ms = Vec::new();
            let mut img = None;
            for _ in 0..5 {
                let started = std::time::Instant::now();
                img = Some(run(roads));
                ms.push(started.elapsed().as_secs_f64() * 1e3);
            }
            ms.sort_by(f64::total_cmp);
            let img = img.unwrap();
            println!(
                "ZL16 1024 px, {label} ({} road texels): min {:.1} ms, median {:.1} ms, \
                 lit {}, pixels {:016x}",
                roads.stats.lit_texels,
                ms[0],
                ms[2],
                lit_texels(&img),
                digest(&img)
            );
        }
    }

    /// Release-build timing of road light from the real road network, at
    /// 1024 px: the Geneva texture above, and the Tokyo ZL16 texture with
    /// the most road points in its tile, each generated without roads and
    /// with the roads the installation's DSFs give (read and merged by the
    /// road service). The packages and the art are those [`real_install`]
    /// names; the X-Plane installation the road service reads is
    /// `roads::service::real`'s. Skipped without any of them. Run with
    /// `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
    /// xearthlayer --lib time_real_roads_on_real_zl16_textures -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn time_real_roads_on_real_zl16_textures() {
        use crate::night::roads::service::real;
        let Some((packages, art)) = real_install() else {
            return;
        };
        let art = crate::night::LaminarArt::new(art);
        let model = RoadModel::DEFAULT;
        let size = 1024;
        for (label, pkg, tile) in [
            ("Geneva", "zzXEL_eu-west_ortho", (46, 6)),
            ("Tokyo", "zzXEL_as-asia-pacific_ortho", (35, 139)),
        ] {
            let Some(pkg) = real_package(&packages, pkg) else {
                continue;
            };
            let store = crate::night::RecipeStore::new(vec![pkg.clone()]);
            if store.get(tile.0, tile.1).is_none() {
                eprintln!(
                    "{label}: no night recipe for {tile:?} in {}; skipped",
                    pkg.display()
                );
                continue;
            }
            let n = if label == "Geneva" {
                name_at(46.20, 6.14)
            } else {
                let Some(t) = real::merged_roads(&[tile]) else {
                    return;
                };
                densest_zl16(&t[0])
            };
            // The tiles the service would ask roads for.
            let tiles = crate::night::service::road_tiles(&n, size, &model);
            let Some(roads) = real::merged_roads(&tiles) else {
                return;
            };
            let regions = |a: i32, b: i32| {
                store
                    .get_with_region(a, b)
                    .map_or(LedRegion::Unknown, |(_, r)| r)
            };
            let run = |field: &RoadField| {
                CpuLitGenerator.generate(&LitRequest {
                    name: &n,
                    size,
                    brightness: 1.0,
                    recipes: &|a, b| store.get(a, b),
                    patterns: &|c| art.pattern(c),
                    regions: &regions,
                    led: &LedModel::DEFAULT,
                    roads: field,
                    road_model: &model,
                })
            };
            let raster = || RoadField::raster(&n, size, &roads, &model);
            /// Wall time of `f` in ms.
            fn ms(f: impl FnOnce()) -> f64 {
                let started = std::time::Instant::now();
                f();
                started.elapsed().as_secs_f64() * 1e3
            }
            // Inflates the recipes and decodes the patterns.
            let empty = RoadField::empty(size);
            let dark = run(&empty);
            let field = raster();
            let lit = run(&field);
            // Nine rounds of the four timings in turn, so a change in the
            // machine's load reaches all four alike; minimum and median.
            let rounds: [[f64; 4]; 9] = std::array::from_fn(|_| {
                [
                    ms(|| drop(std::hint::black_box(raster()))),
                    ms(|| drop(std::hint::black_box(run(&empty)))),
                    ms(|| drop(std::hint::black_box(run(&field)))),
                    ms(|| drop(std::hint::black_box(run(&raster())))),
                ]
            });
            let [(raster_min, raster_med), (none_min, none_med), (gen_min, gen_med), (all_min, all_med)] =
                std::array::from_fn(|k| {
                    let mut r = rounds.map(|round| round[k]);
                    r.sort_by(f64::total_cmp);
                    (r[0], r[4])
                });
            let texels = f64::from(size * size);
            println!(
                "{label} {}_{}_ZL{} ({} tiles, {} merged segments): {} segments rastered, \
                 {} road texels ({:.1}% of the texture); lit texels {} without roads, {} with; \
                 pixels {:016x} without roads, {:016x} with",
                n.row,
                n.col,
                n.zoom,
                tiles.len(),
                roads.iter().map(|r| r.len()).sum::<usize>(),
                field.stats.segments,
                field.stats.lit_texels,
                field.stats.lit_texels as f64 / texels * 100.0,
                lit_texels(&dark),
                lit_texels(&lit),
                digest(&dark),
                digest(&lit)
            );
            println!(
                "{label}: raster min {raster_min:.1} ms, median {raster_med:.1} ms; \
                 generation without roads min {none_min:.1} ms, median {none_med:.1} ms; \
                 with the field min {gen_min:.1} ms, median {gen_med:.1} ms; \
                 raster and generation min {all_min:.1} ms, median {all_med:.1} ms \
                 ({:+.1}% on the median)",
                (all_med / none_med - 1.0) * 100.0
            );
        }
    }

    /// What reached the tone path: pattern sum, LED temperature, gain.
    type ToneInput = ([f32; 3], Option<f64>, f32);

    /// A tone path that records every texel that reaches it, and gives the
    /// production bytes.
    struct Recording {
        toner: led::Toner,
        inputs: std::cell::RefCell<Vec<ToneInput>>,
    }

    impl TonePath for Recording {
        fn bytes(&self, sodium: [f32; 3], led_kelvin: Option<f64>, s: f32) -> [u8; 3] {
            self.inputs.borrow_mut().push((sodium, led_kelvin, s));
            self.toner.bytes(sodium, led_kelvin, s)
        }
    }

    /// Release-build timing of the tone path before and after #436 over
    /// real textures: the 5 x 5 ZL16 grids at 1024 px around Mexico City,
    /// Geneva, Paris and Montreal (those the probe lights), with the night
    /// recipes of every `*_ortho` package in [`PACKAGES_VAR`], the Laminar
    /// art in [`ART_VAR`], `LedModel::DEFAULT`, no roads, warm, one thread.
    /// Per texture it checks the generator gives the same image with the
    /// `powf` path, times both generations, then records the texels that
    /// reach the tone path and times both paths over them alone. Skipped,
    /// saying so on stderr, without the installation. Run with
    /// `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
    /// xearthlayer --lib time_the_tone_path_on_real_zl16_textures --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn time_the_tone_path_on_real_zl16_textures() {
        use std::hint::black_box;
        use std::time::Instant;
        let Some((packages, art)) = real_install() else {
            return;
        };
        let mut roots: Vec<_> = std::fs::read_dir(&packages)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.to_string_lossy().ends_with("_ortho"))
            .collect();
        roots.sort();
        let store = crate::night::RecipeStore::new(roots);
        let art = crate::night::LaminarArt::new(art);
        let model = LedModel::DEFAULT;
        let reference = led::powf_reference::PowfToner(&model);
        let toner = led::Toner::new(&model);
        let size = 1024;
        let recipes = |a: i32, b: i32| store.get(a, b);
        let regions = |a: i32, b: i32| {
            store
                .get_with_region(a, b)
                .map_or(LedRegion::Unknown, |(_, r)| r)
        };
        let ms = |started: Instant| started.elapsed().as_secs_f64() * 1e3;
        // Totals: textures, toned texels, generation and tone path ms,
        // before (`powf`) and after.
        let (mut textures, mut toned) = (0usize, 0usize);
        let (mut gen_before, mut gen_after, mut tone_before, mut tone_after) = (0.0, 0.0, 0.0, 0.0);
        for (city, lat, lon) in [
            ("Mexico City", 19.4326, -99.1332),
            ("Geneva", 46.2044, 6.1432),
            ("Paris", 48.8566, 2.3522),
            ("Montreal", 45.5019, -73.5674),
        ] {
            let centre = name_at(lat, lon);
            let (mut city_textures, mut city_tone) = (0usize, [0.0f64; 2]);
            for dr in -2i32..=2 {
                for dc in -2i32..=2 {
                    let n = name(
                        centre.row.saturating_add_signed(dr * 16),
                        centre.col.saturating_add_signed(dc * 16),
                        16,
                    );
                    if !RecipeField::new(&Footprint::from_name(&n), &recipes)
                        .has_light(|c| art.lit_terrain(c).is_some())
                    {
                        continue;
                    }
                    let request = LitRequest {
                        name: &n,
                        size,
                        brightness: 1.0,
                        recipes: &recipes,
                        patterns: &|c| art.pattern(c),
                        regions: &regions,
                        led: &model,
                        roads: &RoadField::empty(size),
                        road_model: &RoadModel::DEFAULT,
                    };
                    // Warm: inflates the recipes and decodes the patterns.
                    let after = CpuLitGenerator.generate(&request);
                    let before =
                        CpuLitGenerator.generate_with(&request, &reference, Shortcuts::ALL);
                    assert!(after == before, "{city}: {n:?} differs from the powf path");
                    // Two rounds each, alternated; the faster of each.
                    let (mut b, mut a) = (f64::MAX, f64::MAX);
                    for _ in 0..2 {
                        let started = Instant::now();
                        black_box(CpuLitGenerator.generate_with(
                            &request,
                            &reference,
                            Shortcuts::ALL,
                        ));
                        b = b.min(ms(started));
                        let started = Instant::now();
                        black_box(CpuLitGenerator.generate(&request));
                        a = a.min(ms(started));
                    }
                    gen_before += b;
                    gen_after += a;
                    let recording = Recording {
                        toner,
                        inputs: Default::default(),
                    };
                    CpuLitGenerator.generate_with(&request, &recording, Shortcuts::ALL);
                    let inputs = recording.inputs.into_inner();
                    let (mut b, mut a) = (f64::MAX, f64::MAX);
                    for _ in 0..3 {
                        let started = Instant::now();
                        for &(sodium, k, s) in &inputs {
                            black_box(reference.bytes(black_box(sodium), k, s));
                        }
                        b = b.min(ms(started));
                        let started = Instant::now();
                        for &(sodium, k, s) in &inputs {
                            black_box(toner.bytes(black_box(sodium), k, s));
                        }
                        a = a.min(ms(started));
                    }
                    for &(sodium, k, s) in &inputs {
                        assert_eq!(
                            toner.bytes(sodium, k, s),
                            reference.bytes(sodium, k, s),
                            "{sodium:?} at {k:?} K, gain {s:e}"
                        );
                    }
                    city_textures += 1;
                    city_tone[0] += b;
                    city_tone[1] += a;
                    toned += inputs.len();
                }
            }
            if city_textures > 0 {
                let per = city_textures as f64;
                println!(
                    "{city}: {city_textures} textures, tone path {:.1} ms -> {:.1} ms per texture",
                    city_tone[0] / per,
                    city_tone[1] / per
                );
            } else {
                eprintln!("{city}: no lit texture in the installed packages; skipped");
            }
            textures += city_textures;
            tone_before += city_tone[0];
            tone_after += city_tone[1];
        }
        if textures == 0 {
            eprintln!("no lit texture in {}; skipped", packages.display());
            return;
        }
        let per = textures as f64;
        println!(
            "{textures} textures, {:.0} toned texels per texture: tone path {:.1} ms -> {:.1} ms \
             per texture ({:.1} ms saved); generation {:.1} ms -> {:.1} ms per texture \
             ({:+.1}%); every byte identical",
            toned as f64 / per,
            tone_before / per,
            tone_after / per,
            (tone_before - tone_after) / per,
            gen_before / per,
            gen_after / per,
            (gen_after / gen_before - 1.0) * 100.0
        );
    }

    /// The ZL16 texture of `roads`' tile that holds the most road points.
    fn densest_zl16(roads: &crate::night::roads::merge::MergedRoads) -> DdsFilename {
        let mut counts = std::collections::HashMap::<(u32, u32), usize>::new();
        let (lat0, lon0) = (f64::from(roads.tile.0), f64::from(roads.tile.1));
        roads.for_each_chain_near(lat0, lon0, lat0 + 1.0, lon0 + 1.0, |chain| {
            for (lon, lat) in chain.points() {
                let n = name_at(lat, lon);
                *counts.entry((n.row, n.col)).or_default() += 1;
            }
        });
        let ((row, col), _) = counts
            .into_iter()
            .max_by_key(|&(k, c)| (c, k))
            .expect("a tile with roads");
        name(row, col, 16)
    }

    /// What the service answers for a lit `stat` of `name`.
    fn probe(
        name: &DdsFilename,
        recipes: &(dyn Fn(i32, i32) -> Option<Arc<Recipe>> + Sync),
    ) -> bool {
        RecipeField::new(&Footprint::from_name(name), recipes).has_light(|_| true)
    }

    /// Names at `zoom` whose footprints cover lat `lat0..lat1`, lon `lon0..lon1`.
    fn names_over(zoom: u8, lat0: f64, lat1: f64, lon0: f64, lon1: f64) -> Vec<DdsFilename> {
        let n = f64::from(1u32 << zoom);
        let col = |lon: f64| ((lon + 180.0) / 360.0 * n) as u32 / 16 * 16;
        let row = |lat: f64| {
            let r = f64::to_radians(lat);
            ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / PI) / 2.0 * n) as u32 / 16 * 16
        };
        let mut out = Vec::new();
        for r in (row(lat1)..=row(lat0)).step_by(16) {
            for c in (col(lon0)..=col(lon1)).step_by(16) {
                out.push(name(r, c, zoom));
            }
        }
        out
    }

    /// The probe contract: wherever the generator lights a texel of a
    /// footprint, the probe answers lit for it (over-approximating is fine,
    /// it costs only a full-size black file).
    fn assert_probe_covers(
        label: &str,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        window: (f64, f64, f64, f64),
    ) {
        let (lat0, lat1, lon0, lon1) = window;
        let (mut lit, mut dark, mut missed) = (0, 0, Vec::new());
        for zoom in [14u8, 16, 17, 18] {
            for n in names_over(zoom, lat0, lat1, lon0, lon1) {
                let generated = lit_texels(&generate(&n, 64, &recipes, white, 1.0)) > 0;
                let probed = probe(&n, &recipes);
                lit += usize::from(generated);
                dark += usize::from(!probed);
                if generated && !probed {
                    missed.push(format!("{}_{}_ZL{}", n.row, n.col, n.zoom));
                }
            }
        }
        assert!(lit > 0, "{label}: the sweep holds lit footprints");
        assert!(
            dark > 0,
            "{label}: the probe still answers some footprints dark"
        );
        assert!(
            missed.is_empty(),
            "{label}: lit by the generator, dark by the probe: {missed:?}"
        );
    }

    #[test]
    fn the_probe_covers_light_spilling_off_a_lit_block() {
        // Lit class west of lon -122.5, under brightness 0; the brightness
        // cell east of it is 200, so bilinear brightness lights the block's
        // east strip although no lit cell sits on a bright cell.
        let r = synthetic(
            37,
            -123,
            120,
            |_, c| u8::from(c < 60),
            4,
            |_, c| if c >= 2 { 200 } else { 0 },
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        assert_probe_covers("block edge", recipes, (37.45, 37.55, -122.62, -122.38));
    }

    #[test]
    fn the_probe_covers_light_across_a_degree_line() {
        // West tile: lit class, no radiance. East tile: no class, bright.
        let west = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let east = synthetic(37, -122, 120, |_, _| 0, 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        assert_probe_covers("longitude line", recipes, (37.45, 37.55, -122.3, -121.9));
    }

    #[test]
    fn the_probe_covers_light_across_a_latitude_line() {
        // South tile: lit class, no radiance. North tile: no class, bright.
        let south = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let north = synthetic(38, -123, 120, |_, _| 0, 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(south.clone()),
            (38, -123) => Some(north.clone()),
            _ => None,
        };
        assert_probe_covers("latitude line", recipes, (37.8, 38.1, -122.55, -122.45));
    }

    #[test]
    fn the_probe_covers_light_on_real_sized_grids() {
        // 1200 class and 240 brightness cells per degree, as OXO writes them.
        // A lit block (rows 600..660, cols 600..660) under zero brightness,
        // with one bright brightness cell just past each of its corners.
        let r = synthetic(
            37,
            -123,
            1200,
            |r, c| u8::from((600..660).contains(&r) && (600..660).contains(&c)),
            240,
            |r, c| {
                if (r == 119 || r == 132) && (c == 119 || c == 132) {
                    255
                } else {
                    0
                }
            },
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        assert_probe_covers("real grids", recipes, (37.40, 37.55, -122.55, -122.40));
    }

    #[test]
    fn generation_is_deterministic() {
        let a = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        let b = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(a.as_raw(), b.as_raw());
    }
}
