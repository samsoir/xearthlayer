//! Measurements over the developer's X-Plane installation, which is
//! only read; every test returns at once where it is absent. Release
//! build, one test per process so memory readings are its own:
//! `cargo test --release -p xearthlayer --lib --
//! night::roads::service::real::<name> --exact --ignored
//! --nocapture`.

use super::*;
use crate::xworld_pro::SCENERY_PACKS_FILE;

const INSTALL: &str = "/media/FlightSim/X-Plane 12-beta";
const GENEVA: Tile = (46, 6);
const TOKYO: Tile = (35, 139);
/// No DSF covers it (South Pacific): readying it builds the pack
/// index alone.
const OCEAN: Tile = (-50, -130);

/// Whether the installation is there; says so when it is not.
fn installed() -> bool {
    let ini = Path::new(INSTALL)
        .join("Custom Scenery")
        .join(SCENERY_PACKS_FILE);
    let ok = ini.is_file();
    if !ok {
        println!("no X-Plane installation at {INSTALL}; skipped");
    }
    ok
}

/// XEL's own mount, which the service is told to leave out.
fn xel_mounts() -> Vec<PathBuf> {
    vec![Path::new(INSTALL).join("Custom Scenery/zzXEL_ortho")]
}

/// Every simHeaven pack, and XEL's mount: what is left of a tile is
/// its other packs and Global Scenery.
fn without_simheaven() -> Vec<PathBuf> {
    let cs = Path::new(INSTALL).join("Custom Scenery");
    let mut skip = xel_mounts();
    for e in std::fs::read_dir(&cs).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("simHeaven_") {
            skip.push(cs.join(e.file_name()));
        }
    }
    skip
}

/// A service over the installation whose waits never cut a
/// measurement short.
fn service(skip: Vec<PathBuf>, disk: Option<Arc<dyn Cache>>) -> Arc<RoadService> {
    RoadService::with_config(
        INSTALL.into(),
        skip,
        disk,
        RoadServiceConfig {
            probe_wait: Duration::from_secs(600),
            generate_wait: Duration::from_secs(600),
            ..RoadServiceConfig::default()
        },
    )
}

/// A disk tier in memory, large enough never to evict.
fn memory_disk() -> Arc<dyn Cache> {
    Arc::new(crate::cache::MemoryCacheProvider::new(4 << 30, None))
}

/// Prints the service's `Night roads:` events (debug) to stdout.
fn print_road_events() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("xearthlayer::night::roads=debug")
        .with_target(false)
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stdout)
        .try_init();
}

/// `tiles` made ready for generation, and the wall time in ms. Every
/// tile must be ready, or the numbers measure nothing.
async fn ready(svc: &Arc<RoadService>, tiles: &[Tile]) -> (FootprintRoads, f64) {
    let started = Instant::now();
    let roads = svc.footprint(tiles, Purpose::Generate).await;
    assert!(roads.complete, "every tile ready");
    (roads, started.elapsed().as_secs_f64() * 1e3)
}

/// The merged roads of `tiles` from the installation (XEL's mount
/// left out, no disk tier), or `None` without it.
pub(crate) fn merged_roads(tiles: &[Tile]) -> Option<Vec<Arc<MergedRoads>>> {
    if !installed() {
        return None;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let roads = rt.block_on(async {
        let svc = service(xel_mounts(), None);
        let roads = svc.footprint(tiles, Purpose::Generate).await;
        assert!(roads.complete, "every tile ready");
        roads.tiles
    });
    Some(roads)
}

fn mib(bytes: impl Into<f64>) -> String {
    format!("{:.1} MiB", bytes.into() / f64::from(1u32 << 20))
}

/// Resident memory of the process, in bytes.
fn resident() -> f64 {
    memory_stats::memory_stats().map_or(f64::NAN, |s| s.physical_mem as f64)
}

/// The process's peak resident memory (Linux `VmHWM`), in bytes.
fn peak_resident() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
            let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
            Some(kb * 1024.0)
        })
        .unwrap_or(f64::NAN)
}

/// Readiness of Geneva and Tokyo with every installed pack, without
/// and then with a disk tier, and the probe's crossing query on
/// Tokyo's merged roads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn tiles_ready_over_the_real_install() {
    if !installed() {
        return;
    }
    print_road_events();
    let index = tokio::task::spawn_blocking(|| {
        let started = Instant::now();
        let (idx, s) = PackIndex::build(Path::new(INSTALL), &xel_mounts());
        (s, idx.bytes(), started.elapsed().as_secs_f64() * 1e3)
    })
    .await
    .unwrap();
    println!(
        "pack index, built alone: {:?}, {} bytes ({}), {:.1} ms",
        index.0,
        index.1,
        mib(index.1 as f64),
        index.2
    );

    let mut tokyo = None;
    for (label, disk) in [("no disk tier", None), ("disk tier", Some(memory_disk()))] {
        let svc = service(xel_mounts(), disk.clone());
        let (_, ms) = ready(&svc, &[OCEAN]).await;
        println!("{label}: pack index through the service {ms:.1} ms");
        for tile in [GENEVA, TOKYO] {
            let considered = svc.source_paths(tile).await.len();
            let (roads, ms) = ready(&svc, &[tile]).await;
            let r = &roads.tiles[0];
            println!(
                "{label}: tile {tile:?} cold {ms:.1} ms, {considered} sources, \
                 {} merged segments, {} removed, {} bytes ({})",
                r.len(),
                r.removed,
                r.bytes(),
                mib(r.bytes() as f64)
            );
            if tile == TOKYO {
                tokyo = Some(Arc::clone(r));
            }
        }
        if let Some(d) = &disk {
            svc.writes_settled().await;
            d.gc().await.unwrap();
            println!(
                "{label}: {} entries, {} bytes on the tier",
                d.entry_count(),
                d.size_bytes()
            );
        }
        println!("{label}: {:?}", svc.stats_snapshot());
    }

    // The probe asks whether any road box meets a ZL16 footprint
    // grown by the glow reach; time it over random footprints.
    let tokyo = tokyo.unwrap();
    let boxes: Vec<_> = (0..10_000)
        .map(|i| {
            use crate::night::generator::noise::{hash2, unit};
            use crate::night::roads::field::{reach_deg, tests::name_at_zoom};
            let lat = f64::from(TOKYO.0) + unit(hash2(i, 0, 1));
            let lon = f64::from(TOKYO.1) + unit(hash2(i, 1, 1));
            let n = name_at_zoom(lat, lon, 16);
            let f = crate::night::Footprint::from_name(&n);
            let r = reach_deg(&n, 1024, &crate::night::roads::RoadModel::DEFAULT);
            (f.south - r, f.west - r, f.north + r, f.east + r)
        })
        .collect();
    let started = Instant::now();
    let crossed = boxes
        .iter()
        .filter(|&&(s, w, n, e)| std::hint::black_box(&tokyo).crosses(s, w, n, e))
        .count();
    let ns = started.elapsed().as_nanos() as f64 / boxes.len() as f64;
    println!(
        "crosses on Tokyo ({} merged segments): {ns:.0} ns per call, {crossed} of {} crossed",
        tokyo.len(),
        boxes.len()
    );
}

/// Geneva without its simHeaven packs falls back to the packs below
/// and Global Scenery's 7z mesh: cold (decompression, a cache miss
/// that writes the mesh's entry), then a second service reading the
/// same disk tier (hits only).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn global_scenery_fallback_for_geneva() {
    if !installed() {
        return;
    }
    print_road_events();
    let skip = without_simheaven();
    println!("{} packs left out", skip.len());
    let d = memory_disk();
    for pass in ["cold", "warm"] {
        let svc = service(skip.clone(), Some(Arc::clone(&d)));
        ready(&svc, &[OCEAN]).await;
        let sources = svc.source_paths(GENEVA).await;
        let (roads, ms) = ready(&svc, &[GENEVA]).await;
        let r = &roads.tiles[0];
        svc.writes_settled().await;
        let s = svc.stats_snapshot();
        println!(
            "{pass}: {ms:.1} ms, {} sources, {} merged segments, {} bytes, \
             cache hits {}, misses {}",
            sources.len(),
            r.len(),
            r.bytes(),
            s.cache_hits,
            s.cache_misses
        );
        for path in &sources {
            let key = cache_key(GENEVA, &path.to_string_lossy());
            if let Some(entry) = d.get(&key).await.unwrap() {
                println!("  entry {} for {}", entry.len(), path.display());
            }
        }
        // Global Scenery is the last source, and it was read: only
        // a read 7z leaves an entry.
        let global = sources.last().expect("Geneva has sources");
        assert!(
            global.starts_with(Path::new(INSTALL).join("Global Scenery")),
            "{}",
            global.display()
        );
        let key = cache_key(GENEVA, &global.to_string_lossy());
        assert!(d.get(&key).await.unwrap().is_some(), "the mesh was read");
        match pass {
            "cold" => assert!(s.cache_misses > 0, "the cold pass decompressed"),
            _ => assert_eq!(s.cache_misses, 0, "the warm pass read entries only"),
        }
    }
}

/// Returns freed memory the allocator still holds to the system, so
/// what remains resident is live.
fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only releases free memory.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Starts rayon's global pool. `run` has it running long before a
/// road tile is asked for (the ortho union index and the cache
/// scans use it), so its threads are not the pack index's cost.
fn warm_rayon() {
    use rayon::prelude::*;
    let n: usize = (0..100_000usize)
        .into_par_iter()
        .map(|k| std::hint::black_box(vec![k as u8; 64]).len())
        .sum();
    std::hint::black_box(n);
}

/// Resident memory around building the pack index and readying the
/// 3 x 3 tiles centred on `centre`, then after the service is
/// dropped, and after the allocator gives back what it holds free.
/// The allocator is configured as `run` configures it, and rayon's
/// pool is already running.
async fn memory_of_a_block(centre: Tile) {
    if !installed() {
        return;
    }
    crate::metrics::configure_allocator();
    warm_rayon();
    // And a blocking-pool thread, which `run` has too.
    tokio::task::spawn_blocking(|| ()).await.unwrap();
    let start = resident();
    let svc = service(xel_mounts(), None);
    ready(&svc, &[OCEAN]).await;
    let indexed = resident();
    let index = svc.index().await.bytes();
    let tiles: Vec<Tile> = (-1..=1)
        .flat_map(|a| (-1..=1).map(move |b| (centre.0 + a, centre.1 + b)))
        .collect();
    let (roads, ms) = ready(&svc, &tiles).await;
    let merged: usize = roads.tiles.iter().map(|r| r.bytes()).sum();
    svc.tiles.run_pending_tasks().await;
    let held = svc.tiles.weighted_size();
    let readied = resident();
    let peak = peak_resident();
    drop(roads);
    drop(svc);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let dropped = resident();
    trim();
    let trimmed = resident();
    println!(
        "3 x 3 around {centre:?}: ready in {ms:.0} ms; merged {} (tile cache holds {}); \
         pack index {}; resident: start {}, pack index +{}, tiles +{}, \
         after drop +{} over start, after trim +{}; peak {}",
        mib(merged as f64),
        mib(held as f64),
        mib(index as f64),
        mib(start),
        mib(indexed - start),
        mib(readied - indexed),
        mib(dropped - start),
        mib(trimmed - start),
        mib(peak)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn memory_of_a_3x3_block_around_geneva() {
    memory_of_a_block(GENEVA).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn memory_of_a_3x3_block_around_tokyo() {
    memory_of_a_block(TOKYO).await;
}
