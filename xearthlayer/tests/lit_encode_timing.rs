//! Release-build timing and peak memory of the lit encode, before #439 and
//! #440 and now, on real textures from the developer's installation, which
//! is only read. The inputs are named by environment variables read here
//! only: `XEL_NIGHT_PACKAGES`, the packages directory (every `*_ortho`
//! package under it is used), and `XEL_LAMINAR_ART`, Laminar's `1000 world
//! terrain` directory. Without them the test says so and times nothing.
//! `XEL_NIGHT_PACKAGES=... XEL_LAMINAR_ART=... cargo test --release -p
//! xearthlayer --test lit_encode_timing -- --ignored --nocapture`.
//!
//! "Before" is the encoder before #439 (`support/lit_encode_before.rs`),
//! whose memory is also #439's: the whole chain at once, a copy of the
//! first level and a linear copy of it with four channels. Peak memory is
//! measured by [`CountingAllocator`], which this binary, and no other but
//! `lit_encode_memory`, makes its global allocator; it counts per thread.

#[path = "support/counting_alloc.rs"]
mod counting_alloc;
#[path = "support/lit_encode_before.rs"]
mod lit_encode_before;

use image::RgbaImage;

use counting_alloc::{held_at_peak, CountingAllocator};
use lit_encode_before::encode_visible_before;
use xearthlayer::fuse::{DdsFilename, TextureKind};
use xearthlayer::night::generator::encode_visible;
use xearthlayer::night::laminar::ArtRecord;
use xearthlayer::night::roads::field::RoadField;
use xearthlayer::night::roads::RoadModel;
use xearthlayer::night::{
    CpuLitGenerator, LaminarArt, LedModel, LedRegion, LitGenerator, LitRequest, RecipeStore,
};

#[global_allocator]
static COUNTING: CountingAllocator = CountingAllocator;

/// The ZL16 lit texture whose footprint holds `(lat, lon)`.
fn name_at(lat: f64, lon: f64) -> DdsFilename {
    let n = f64::from(1u32 << 16);
    let x = ((lon + 180.0) / 360.0 * n) as u32;
    let lat_r = lat.to_radians();
    let y =
        ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
    DdsFilename {
        row: y / 16 * 16,
        col: x / 16 * 16,
        zoom: 16,
        map_type: "BI".into(),
        kind: TextureKind::Lit,
    }
}

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

/// One encoder's least time over the runs, in ms, and its peak, in bytes
/// held beyond what its caller holds (the input not included).
#[derive(Clone, Copy, Default)]
struct Cost {
    ms: f64,
    peak: usize,
}

/// The cost of encoding `img` by the encoder before #439 and #440 and by
/// `encode_visible`, the least time of `runs` and the largest peak; asserts
/// both payloads are the same bytes.
fn encode_costs(img: &RgbaImage, runs: usize) -> (Cost, Cost) {
    let mut costs = [Cost {
        ms: f64::MAX,
        peak: 0,
    }; 2];
    let mut record = |which: usize, ms: f64, peak: usize| {
        costs[which].ms = costs[which].ms.min(ms);
        costs[which].peak = costs[which].peak.max(peak);
    };
    for _ in 0..runs {
        let (ms, (old, peak)) = timed(|| held_at_peak(|| encode_visible_before(img)));
        record(0, ms, peak);
        let owned = img.clone();
        let (ms, (new, peak)) = timed(|| held_at_peak(|| encode_visible(owned).unwrap()));
        record(1, ms, peak);
        assert!(new[..] == old[..], "encode_visible changed a byte");
    }
    (costs[0], costs[1])
}

/// Release-build time and peak memory of the lit encode before #439 and
/// #440 and now, on real ZL16 textures at 1024 px and on a black one, one
/// thread. Every payload is also checked byte for byte against the encoder
/// before #439. Inputs as the module says.
#[test]
#[ignore]
fn time_the_lit_encode() {
    let (Some(packages), Some(art)) = (
        std::env::var_os("XEL_NIGHT_PACKAGES"),
        std::env::var_os("XEL_LAMINAR_ART"),
    ) else {
        eprintln!(
            "time_the_lit_encode: set XEL_NIGHT_PACKAGES (the packages \
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
    let store = RecipeStore::with_regions(roots.clone());
    let roots: Vec<std::path::PathBuf> = roots.into_iter().map(|(r, _)| r).collect();
    let art = LaminarArt::new(art.into());
    let regions = |a: i32, b: i32| {
        store
            .get_with_region(a, b)
            .map_or(LedRegion::Unknown, |(_, r)| r)
    };
    let model = RoadModel::DEFAULT;
    let roads = RoadField::empty(SIZE);
    // The lookups' doubts decide disk writes in the service; unused here.
    let record = ArtRecord::default();
    let mut textures = Vec::new();
    for name in real_texture_names(&roots, 40) {
        let img = CpuLitGenerator.generate(&LitRequest {
            name: &name,
            size: SIZE,
            brightness: 1.0,
            recipes: &|a, b| store.get(a, b),
            patterns: &|c| art.pattern_recorded(c, &record),
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
        eprintln!("time_the_lit_encode: no lit texture under {roots:?}; nothing timed");
        return;
    }
    const MIB: f64 = (1 << 20) as f64;
    let report = |label: &str, images: &[RgbaImage]| {
        let (mut before, mut after) = (Cost::default(), Cost::default());
        for img in images {
            let (b, a) = encode_costs(img, RUNS);
            before.ms += b.ms / images.len() as f64;
            after.ms += a.ms / images.len() as f64;
            before.peak = before.peak.max(b.peak);
            after.peak = after.peak.max(a.peak);
        }
        println!(
            "{label}: per texture, before -> after: {:.2} -> {:.2} ms; \
             peak beyond the input {:.2} -> {:.2} MiB",
            before.ms,
            after.ms,
            before.peak as f64 / MIB,
            after.peak as f64 / MIB,
        );
    };
    report(
        &format!("{} real ZL16 textures at {SIZE} px", textures.len()),
        &textures,
    );
    report(
        &format!("black at {SIZE} px (the shortcut after)"),
        &[RgbaImage::from_pixel(
            SIZE,
            SIZE,
            image::Rgba([0, 0, 0, 255]),
        )],
    );
}
