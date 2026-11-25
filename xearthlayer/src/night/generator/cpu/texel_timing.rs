//! Release-build timing of the per-texel parts #437 hoisted, before and
//! after, on real textures from the developer's installation, which is
//! only read: the 5 x 5 ZL16 grids at 1024 px around Mexico City, Geneva,
//! Paris and Montreal that the probe lights, as #436 timed. The inputs are
//! those [`super::tests::real_install`] names. Run with
//! `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
//! xearthlayer --lib time_the_hoisted_texel_parts -- --ignored --nocapture`.
//!
//! Each part is timed over the same texels in raster order, the code before
//! #437 (the `*_reference` copies, and `patch_at` without a cache) beside
//! today's, and every result is compared bit for bit:
//!
//! - class weights, at every texel (today's includes building the
//!   sampler's tables);
//! - radiance, at every weighted texel;
//! - pattern sampling, for every class weight with a pattern;
//! - the LED patch, where the generator draws it.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use super::tests::{name, name_at, real_install};
use super::*;
use crate::night::generator::field::lookup_reference;
use crate::night::generator::led::LedModel;
use crate::night::generator::tiling::sample_reference;
use crate::night::roads::RoadModel;

/// A class weight with its weight as bits, for exact comparison.
type WeightBits = ((i32, i32), usize, u8, u64);

fn weight_bits(out: &[super::super::field::ClassWeight]) -> Vec<WeightBits> {
    out.iter()
        .map(|w| (w.tile, w.slot, w.index, w.weight.to_bits()))
        .collect()
}

/// The least of `runs` timings of `f`, in ms.
fn least_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    (0..runs)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed().as_secs_f64() * 1e3
        })
        .fold(f64::MAX, f64::min)
}

/// Before and after times of the four parts, in ms, for one texture.
#[derive(Debug, Default, Clone, Copy)]
struct Parts {
    weights: [f64; 2],
    radiance: [f64; 2],
    patterns: [f64; 2],
    patches: [f64; 2],
    generation: f64,
}

/// The four parts of `request`'s texture, before and after, each the
/// least of `runs`; asserts every result is the same bits.
fn time_parts(request: &LitRequest<'_>, runs: usize) -> Parts {
    let TexelCentres { lons, lats, xs, ys } = TexelCentres::of(request.name, request.size);
    let field = RecipeField::new(&Footprint::from_name(request.name), request.recipes);
    let mut parts = Parts::default();

    // Class weights at every texel; keep the weighted ones.
    let mut weighted: Vec<(usize, usize, Vec<super::super::field::ClassWeight>)> = Vec::new();
    let mut out = Vec::with_capacity(4);
    parts.weights[0] = least_ms(runs, || {
        for &lat in &lats {
            for &lon in &lons {
                lookup_reference::class_weights(&field, lat, lon, &mut out);
                black_box(&out);
            }
        }
    });
    parts.weights[1] = least_ms(runs, || {
        let mut sampler = FieldSampler::new(
            &field,
            TexelAxes {
                lats: &lats,
                lons: &lons,
            },
        );
        for i in 0..lats.len() {
            for j in 0..lons.len() {
                sampler.class_weights(i, j, &mut out);
                black_box(&out);
            }
        }
    });
    let mut sampler = FieldSampler::new(
        &field,
        TexelAxes {
            lats: &lats,
            lons: &lons,
        },
    );
    let mut want = Vec::with_capacity(4);
    for (i, &lat) in lats.iter().enumerate() {
        for (j, &lon) in lons.iter().enumerate() {
            sampler.class_weights(i, j, &mut out);
            lookup_reference::class_weights(&field, lat, lon, &mut want);
            assert_eq!(weight_bits(&out), weight_bits(&want), "at {lat}, {lon}");
            if !want.is_empty() {
                weighted.push((i, j, want.clone()));
            }
        }
    }

    // Radiance at every weighted texel.
    parts.radiance[0] = least_ms(runs, || {
        for &(i, j, _) in &weighted {
            black_box(lookup_reference::radiance(&field, lats[i], lons[j]));
        }
    });
    parts.radiance[1] = least_ms(runs, || {
        for &(i, j, _) in &weighted {
            black_box(sampler.radiance(i, j));
        }
    });
    for &(i, j, _) in &weighted {
        assert_eq!(
            sampler.radiance(i, j).to_bits(),
            lookup_reference::radiance(&field, lats[i], lons[j]).to_bits()
        );
    }

    // Pattern sampling for every class weight with a pattern, and the
    // texels where the generator draws an LED patch.
    let mut taps: Vec<(f64, f64, Arc<LitPattern>)> = Vec::new();
    let mut patch_sites: Vec<(f64, f64)> = Vec::new();
    for (i, j, weights) in &weighted {
        let (x, y) = (xs[*j], ys[*i]);
        let radiance = sampler.radiance(*i, *j);
        if gain(radiance, request.brightness) <= 0.0 {
            continue;
        }
        let bright = led::bright_weight(radiance);
        let (mut wsum, mut share) = (0.0, 0.0);
        for w in weights {
            let Some(class) = field.palette_name_in(w.slot, w.index) else {
                continue;
            };
            let Some(p) = (request.patterns)(class) else {
                continue;
            };
            taps.push((x, y, p));
            let region = (request.regions)(w.tile.0, w.tile.1);
            let tone = request.led.tone(led::is_industrial(class), bright, region);
            wsum += w.weight;
            share += w.weight * tone.share;
        }
        if wsum > 0.0 && share > 0.0 {
            patch_sites.push((x, y));
        }
    }
    parts.patterns[0] = least_ms(runs, || {
        let mut cache = CornerCache::default();
        for (x, y, p) in &taps {
            black_box(sample_reference::sample_cached(p, *x, *y, &mut cache));
        }
    });
    parts.patterns[1] = least_ms(runs, || {
        let mut cache = CornerCache::default();
        for (x, y, p) in &taps {
            black_box(sample_cached(p, *x, *y, &mut cache));
        }
    });
    let (mut cache, mut reference_cache) = (CornerCache::default(), CornerCache::default());
    for (x, y, p) in &taps {
        assert_eq!(
            sample_cached(p, *x, *y, &mut cache).map(f32::to_bits),
            sample_reference::sample_cached(p, *x, *y, &mut reference_cache).map(f32::to_bits)
        );
    }

    // The LED patch where the generator draws it.
    parts.patches[0] = least_ms(runs, || {
        for &(x, y) in &patch_sites {
            black_box(led::patch_at(x, y));
        }
    });
    parts.patches[1] = least_ms(runs, || {
        let mut cache = led::PatchCache::default();
        for &(x, y) in &patch_sites {
            black_box(led::patch_at_cached(x, y, &mut cache));
        }
    });
    let mut cache = led::PatchCache::default();
    for &(x, y) in &patch_sites {
        assert_eq!(led::patch_at_cached(x, y, &mut cache), led::patch_at(x, y));
    }

    parts.generation = least_ms(runs, || drop(black_box(CpuLitGenerator.generate(request))));
    parts
}

#[test]
#[ignore]
fn time_the_hoisted_texel_parts() {
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
    let size = 1024;
    let recipes = |a: i32, b: i32| store.get(a, b);
    let patterns = |c: &str| art.pattern(c);
    let regions = |a: i32, b: i32| {
        store
            .get_with_region(a, b)
            .map_or(LedRegion::Unknown, |(_, r)| r)
    };
    let roads = RoadField::empty(size);
    let (mut textures, mut total) = (0usize, Parts::default());
    for (lat, lon) in [
        (19.4326, -99.1332),
        (46.2044, 6.1432),
        (48.8566, 2.3522),
        (45.5019, -73.5674),
    ] {
        let centre = name_at(lat, lon);
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
                    patterns: &patterns,
                    regions: &regions,
                    led: &model,
                    roads: &roads,
                    road_model: &RoadModel::DEFAULT,
                };
                // Warm: inflates the recipes and decodes the patterns.
                CpuLitGenerator.generate(&request);
                let p = time_parts(&request, 2);
                for (t, v) in [
                    (&mut total.weights, p.weights),
                    (&mut total.radiance, p.radiance),
                    (&mut total.patterns, p.patterns),
                    (&mut total.patches, p.patches),
                ] {
                    t[0] += v[0];
                    t[1] += v[1];
                }
                total.generation += p.generation;
                textures += 1;
            }
        }
    }
    if textures == 0 {
        eprintln!("no lit texture in {}; skipped", packages.display());
        return;
    }
    let per = textures as f64;
    let mut saved = 0.0;
    println!("{textures} textures at {size} px, ms per texture, before -> after (saved):");
    for (label, t) in [
        ("class weights, every texel", total.weights),
        ("radiance, weighted texels", total.radiance),
        ("pattern sampling", total.patterns),
        ("LED patch at its call sites", total.patches),
    ] {
        let (b, a) = (t[0] / per, t[1] / per);
        saved += b - a;
        println!("  {label}: {b:.1} -> {a:.1} ({:.1})", b - a);
    }
    println!(
        "  total saved {saved:.1}; generation now {:.1} ms per texture; every result identical",
        total.generation / per
    );
}
