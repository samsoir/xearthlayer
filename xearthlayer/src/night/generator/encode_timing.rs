//! Release-build timing of the lit encode stages before and after #439,
//! on real textures from the developer's installation, which is only
//! read. The inputs are named by environment variables read here only:
//! `XEL_NIGHT_PACKAGES`, the packages directory (every `*_ortho` package
//! under it is used), and `XEL_LAMINAR_ART`, Laminar's `1000 world
//! terrain` directory. Without them the test says so and times nothing.
//! `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
//! xearthlayer --lib time_the_lit_encode_stages -- --ignored --nocapture`.

use image::RgbaImage;

use super::tests::{dim_reference, lit_chain_reference, name_at, solid};
use super::*;
use crate::night::roads::field::RoadField;
use crate::night::roads::RoadModel;

/// The ZL16 textures of the recipe tiles under `roots`, sampled evenly
/// to at most `tiles` tiles, three positions in each.
fn real_texture_names(roots: &[std::path::PathBuf], tiles: usize) -> Vec<DdsFilename> {
    let mut found: Vec<(i32, i32)> = Vec::new();
    for root in roots {
        let Ok(blocks) = std::fs::read_dir(root.join("night")) else {
            continue;
        };
        for block in blocks.flatten() {
            let Ok(files) = std::fs::read_dir(block.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".oxn")) else {
                    continue;
                };
                if stem.len() == 7 {
                    if let (Ok(lat), Ok(lon)) = (stem[..3].parse(), stem[3..].parse()) {
                        found.push((lat, lon));
                    }
                }
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    let step = found.len().div_ceil(tiles.max(1)).max(1);
    found
        .iter()
        .step_by(step)
        .flat_map(|&(lat, lon)| {
            [(0.5, 0.5), (0.25, 0.25), (0.75, 0.75)]
                .map(|(a, b)| name_at(f64::from(lat) + a, f64::from(lon) + b))
        })
        .collect()
}

/// Milliseconds `f` took, and its result.
fn timed<T>(f: impl FnOnce() -> T) -> (f64, T) {
    let started = std::time::Instant::now();
    let out = f();
    (started.elapsed().as_secs_f64() * 1e3, out)
}

/// `lit_chain`, `dim` and `bc1_levels` of `img`, in ms, by the encoder
/// before #439 and by today's, each the least of `runs`; asserts both
/// payloads and `encode_visible`'s are the same bytes.
fn stage_times(img: &RgbaImage, runs: usize) -> ([f64; 3], [f64; 3], f64) {
    let (mut before, mut after, mut visible) = ([f64::MAX; 3], [f64::MAX; 3], f64::MAX);
    let least = |acc: &mut [f64; 3], t: [f64; 3]| {
        for (a, t) in acc.iter_mut().zip(t) {
            *a = a.min(t);
        }
    };
    for _ in 0..runs {
        let (t0, mut chain) = timed(|| lit_chain_reference(img));
        let (t1, ()) = timed(|| {
            for (n, level) in chain.iter_mut().enumerate() {
                dim_reference(level, FADE_RAMP.get(n).copied().unwrap_or(1.0));
            }
        });
        let (t2, old) = timed(|| bc1_levels(&chain).unwrap());
        least(&mut before, [t0, t1, t2]);

        let (t0, mut chain) = timed(|| lit_chain(img));
        let (t1, ()) = timed(|| {
            for (n, level) in chain.iter_mut().enumerate() {
                dim(level, FADE_RAMP.get(n).copied().unwrap_or(1.0));
            }
        });
        let (t2, new) = timed(|| bc1_levels(&chain).unwrap());
        least(&mut after, [t0, t1, t2]);
        assert!(new == old, "the stages changed a byte");

        let owned = img.clone();
        let (t, payload) = timed(|| encode_visible(owned).unwrap());
        visible = visible.min(t);
        assert!(payload[..] == old[..], "encode_visible changed a byte");
    }
    (before, after, visible)
}

/// Release-build timing of the encode stages before and after #439, on
/// real ZL16 textures at 1024 px and on a black one, one thread. Every
/// payload is also checked byte for byte against the encoder before
/// #439. Inputs as the module says.
#[test]
#[ignore]
fn time_the_lit_encode_stages() {
    let (Some(packages), Some(art)) = (
        std::env::var_os("XEL_NIGHT_PACKAGES"),
        std::env::var_os("XEL_LAMINAR_ART"),
    ) else {
        eprintln!(
            "time_the_lit_encode_stages: set XEL_NIGHT_PACKAGES (the packages \
             directory; every *_ortho package under it is used) and \
             XEL_LAMINAR_ART (Laminar's `1000 world terrain`); nothing timed"
        );
        return;
    };
    const SIZE: u32 = 1024;
    const RUNS: usize = 3;
    // Every `*_ortho` package, with the LED region its name gives
    // (`zzXEL_eu-west_ortho` is `eu-west`).
    let mut roots: Vec<(std::path::PathBuf, LedRegion)> = std::fs::read_dir(&packages)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            let code = name.strip_suffix("_ortho")?;
            let code = code.split_once('_').map_or(code, |(_, c)| c);
            Some((entry.path(), LedRegion::from_code(code)))
        })
        .collect();
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    let store = crate::night::RecipeStore::with_regions(roots.clone());
    let roots: Vec<std::path::PathBuf> = roots.into_iter().map(|(r, _)| r).collect();
    let art = crate::night::LaminarArt::new(art.into());
    let regions = |a: i32, b: i32| {
        store
            .get_with_region(a, b)
            .map_or(LedRegion::Unknown, |(_, r)| r)
    };
    let model = RoadModel::DEFAULT;
    let roads = RoadField::empty(SIZE);
    let mut textures = Vec::new();
    for name in real_texture_names(&roots, 40) {
        let img = CpuLitGenerator.generate(&LitRequest {
            name: &name,
            size: SIZE,
            brightness: 1.0,
            recipes: &|a, b| store.get(a, b),
            patterns: &|c| art.pattern(c),
            regions: &regions,
            led: &LedModel::DEFAULT,
            roads: &roads,
            road_model: &model,
        });
        if img.pixels().any(|p| p.0[..3] != [0; 3]) {
            textures.push(img);
        }
    }
    if textures.is_empty() {
        eprintln!("time_the_lit_encode_stages: no lit texture under {roots:?}; nothing timed");
        return;
    }
    let report = |label: &str, images: &[RgbaImage]| {
        let (mut before, mut after, mut visible) = ([0.0; 3], [0.0; 3], 0.0);
        for img in images {
            let (b, a, v) = stage_times(img, RUNS);
            for c in 0..3 {
                before[c] += b[c] / images.len() as f64;
                after[c] += a[c] / images.len() as f64;
            }
            visible += v / images.len() as f64;
        }
        let total = |t: [f64; 3]| t.iter().sum::<f64>();
        println!(
            "{label}: ms per texture, before -> after: lit_chain {:.2} -> {:.2}, \
             dim {:.2} -> {:.2}, bc1_levels {:.2} -> {:.2}, total {:.2} -> {:.2}; \
             encode_visible {:.2}",
            before[0],
            after[0],
            before[1],
            after[1],
            before[2],
            after[2],
            total(before),
            total(after),
            visible
        );
    };
    report(
        &format!("{} real ZL16 textures at {SIZE} px", textures.len()),
        &textures,
    );
    report(
        &format!("black at {SIZE} px (stages without the shortcut)"),
        &[solid(SIZE, [0; 3])],
    );
}
