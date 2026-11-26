//! Calibration harness (#476, phase 1 of #475): the production encode
//! pipeline on a synthetic 4096 tile, for each selected GPU and for ISPC on
//! one thread and on every thread. It stands in for the benchmark tool
//! (#477) until that exists, and it is the before-and-after measure for
//! phase 2 (#424, #425, #467).
//!
//! - **GPU:** `create_gpu_encoder_channels` (the worker with
//!   `WgpuTileProcessor`) behind `DdsEncoder`, as the service builds it,
//!   the demand lane. Lone tiles (one at a time: latency), then bursts back
//!   to back (tiles/s), with the per-stage breakdown of each from the
//!   channel's `GpuEncodeStats`, including `gpu_kernel`, the GPU's own time
//!   from timestamp queries; then the same bursts on the prefetch lane,
//!   which never has two tiles in flight (#425).
//! - **ISPC:** `DdsEncoder` with `IspcCompressor`: lone tiles on one thread,
//!   and a burst on every thread, with the CPU chain and the BC1 of the
//!   levels timed apart on one thread.
//! - **The CPU while the GPU works:** ISPC lone tiles on one thread while
//!   the GPU encodes a steady stream, against the same on an idle machine,
//!   to show what sharing memory (an integrated GPU) costs each side.
//!
//! Requires a GPU for the GPU parts; ignored. Run in release, one
//! measurement at a time, on a quiet machine:
//!
//! ```text
//! cargo test --release -p xearthlayer --test gpu_calibration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Besides `XEL_BENCH_GPUS` (see `support/gpu_bench.rs`):
//! `XEL_BENCH_LONE` lone tiles per device (default 20),
//! `XEL_BENCH_BURST` tiles per burst (default 32), `XEL_BENCH_BURSTS`
//! bursts back to back (default 3) and `XEL_BENCH_STREAM_POOL` images
//! cloned beforehand for the GPU's stream while ISPC is timed (default 96,
//! 6 GiB), and `XEL_BENCH_STREAM_RATE` caps that stream's tiles per second
//! (default none), to compare the CPU's slowdown at the same GPU work. `XEL_BENCH_ISPC_BURST` sets the ISPC burst on every thread
//! (default four tiles a thread). Images are always cloned before they are
//! timed, and each is 64 MiB: the ISPC burst holds all of its images at
//! once, 8 GiB by default on 32 threads, so lower it on a host with less
//! memory to spare.

#[path = "support/gpu_bench.rs"]
#[allow(dead_code)]
mod gpu_bench;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpu_bench::*;
use image::RgbaImage;
use xearthlayer::dds::gpu_channel::{create_gpu_encoder_channels, CHANNEL_CAPACITY};
use xearthlayer::dds::gpu_stats::{GpuEncodeStats, GpuSample};
use xearthlayer::dds::{
    DdsEncoder, DdsFormat, IspcCompressor, MipmapCompressor, MipmapGenerator, MipmapStream,
};
use xearthlayer::metrics::LatencySummary;

const LEVELS: usize = 13;

fn summary(samples: &[Duration]) -> Option<LatencySummary> {
    let mut w = xearthlayer::metrics::LatencyWindow::default();
    for s in samples {
        w.record(s.as_micros() as u64);
    }
    w.take()
}

fn fig(s: Option<LatencySummary>) -> String {
    s.map_or_else(
        || "-".to_string(),
        |s| {
            format!(
                "p50 {:>7.2} p95 {:>7.2} max {:>7.2} ms",
                s.median_us as f64 / 1000.0,
                s.p95_us as f64 / 1000.0,
                s.max_us as f64 / 1000.0
            )
        },
    )
}

fn print_stages(s: &GpuSample) {
    eprintln!(
        "    tiles={} overlapped={} failed={} fallback={} depth_max={} kernel timing: {}",
        s.tiles(),
        s.overlapped,
        s.failed,
        s.fallback,
        s.depth_max,
        s.kernel_timing()
    );
    for (name, stage) in [
        ("queue_wait", s.queue_wait),
        ("upload", s.upload),
        ("gpu", s.gpu),
        ("gpu_mips", s.gpu_mips),
        ("gpu_kernel", s.gpu_kernel),
        ("readback", s.readback),
        ("process", s.process),
        ("total", s.total),
    ] {
        eprintln!("    {name:<11} {}", fig(stage));
    }
}

/// One device's headline figures, for the closing table.
struct Row {
    device: String,
    lone: Option<LatencySummary>,
    tiles_per_s: f64,
    stages: Option<GpuSample>,
}

/// Encodes `images` on `threads` threads, each taking the next image until
/// none is left; returns the wall time and each tile's latency.
fn run_burst(
    encoder: &DdsEncoder,
    images: Vec<RgbaImage>,
    threads: usize,
) -> (Duration, Vec<Duration>) {
    let queue = std::sync::Mutex::new(images);
    let start = Instant::now();
    let latencies = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let Some(img) = queue.lock().unwrap().pop() else {
                            break;
                        };
                        let t = Instant::now();
                        encoder.encode(img).expect("the tile encodes");
                        mine.push(t.elapsed());
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    (start.elapsed(), latencies)
}

/// `n` tiles one at a time on the calling thread: each tile's latency. The
/// images are cloned first, so no copy runs between the tiles.
fn run_lone(encoder: &DdsEncoder, source: &RgbaImage, n: usize) -> Vec<Duration> {
    let images: Vec<RgbaImage> = (0..n).map(|_| source.clone()).collect();
    images
        .into_iter()
        .map(|img| {
            let t = Instant::now();
            encoder.encode(img).expect("the tile encodes");
            t.elapsed()
        })
        .collect()
}

fn ispc_encoder() -> DdsEncoder {
    DdsEncoder::new(DdsFormat::BC1)
        .with_compressor(Arc::new(IspcCompressor))
        .with_mipmap_count(LEVELS)
}

fn take(stats: &GpuEncodeStats) -> Option<GpuSample> {
    stats.take_sample()
}

#[test]
#[ignore]
fn calibrate_the_encode_pipelines() {
    let lone_n: usize = env_or("XEL_BENCH_LONE", 20);
    let burst: usize = env_or("XEL_BENCH_BURST", 32);
    let bursts: usize = env_or("XEL_BENCH_BURSTS", 3);
    let stream_rate: Option<f64> = std::env::var("XEL_BENCH_STREAM_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r: &f64| *r > 0.0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    eprintln!("Machine: {}", machine_label());
    eprintln!(
        "Tile: synthetic ortho 4096 x 4096, {LEVELS} levels, BC1; lone {lone_n}, \
         bursts {bursts} x {burst} back to back"
    );
    let source = synthetic_ortho(4096);
    assert_eq!(
        MipmapGenerator::full_chain_count(4096, 4096),
        LEVELS,
        "the production chain"
    );
    let mut rows = Vec::new();

    // ISPC first, on an idle machine.
    let ispc = ispc_encoder();
    let pin = pin_cpu();
    let lone = on_cpu(pin, || {
        run_lone(&ispc, &source, 2);
        run_lone(&ispc, &source, lone_n)
    });
    let idle_lone = summary(&lone);
    eprintln!("\n== ISPC, one thread (pinned to CPU {pin:?}) ==");
    eprintln!("  lone tile  {}", fig(idle_lone));
    // Its parts, on the same core: the CPU chain and the BC1 of its levels.
    let levels: Vec<RgbaImage> = MipmapGenerator::generate_chain_with_count(&source, LEVELS)
        .iter()
        .map(pad_to_blocks)
        .collect();
    let (chain, clone, bc1) = on_cpu(pin, || {
        let chain = time_median(10, || {
            let mut stream = MipmapStream::new(source.clone(), LEVELS);
            let mut n = 0u32;
            while let Some(level) = stream.next_level() {
                n += level.width();
            }
            std::hint::black_box(n);
        });
        let clone = time_median(10, || {
            std::hint::black_box(source.clone());
        });
        let bc1 = time_median(10, || {
            for l in &levels {
                std::hint::black_box(ispc_bc1(l));
            }
        });
        (chain, clone, bc1)
    });
    eprintln!(
        "  parts: chain (MipmapStream) {:.2} ms, net of the image clone the test makes ({:.2} ms); \
         BC1 of every level {:.2} ms",
        ms(chain.saturating_sub(clone)),
        ms(clone),
        ms(bc1)
    );
    let one_thread_rate = idle_lone.map_or(0.0, |s| 1e6 / s.median_us as f64);
    eprintln!("  throughput {one_thread_rate:.1} tiles/s (1 / lone p50)");
    rows.push(Row {
        device: "ISPC, 1 thread".into(),
        lone: idle_lone,
        tiles_per_s: one_thread_rate,
        stages: None,
    });

    // The control for the measurement under each GPU below: ISPC on the
    // same core while one thread does the GPU worker's CPU work with no
    // GPU, back to back: the chain and an 85.33 MiB copy per tile
    // (`generate_chain_with_count` copies level 0 and keeps a copy of each
    // level, as `write_texture` copies every level into staging). Any
    // slowdown here is the CPU side's, not the GPU's.
    {
        let stop = AtomicBool::new(false);
        let done = AtomicUsize::new(0);
        let (lone, wall, during) = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::black_box(MipmapGenerator::generate_chain_with_count(
                        &source, LEVELS,
                    ));
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
            std::thread::sleep(Duration::from_millis(300));
            let started = done.load(Ordering::Relaxed);
            let t = Instant::now();
            let lone = on_cpu(pin, || run_lone(&ispc, &source, lone_n));
            let wall = t.elapsed();
            let during = done.load(Ordering::Relaxed) - started;
            stop.store(true, Ordering::Relaxed);
            (lone, wall, during)
        });
        eprintln!(
            "  control: ISPC lone tile {} while one thread does the GPU worker's CPU work \
             without a GPU ({:.1} tiles/s)",
            fig(summary(&lone)),
            during as f64 / wall.as_secs_f64()
        );
    }

    eprintln!("\n== ISPC, {threads} threads ==");
    // Every image of the burst is cloned before it is timed, 64 MiB each:
    // four a thread is 8 GiB on 32 threads (`XEL_BENCH_ISPC_BURST`).
    let ispc_burst: usize = env_or("XEL_BENCH_ISPC_BURST", threads * 4).max(1);
    eprintln!(
        "  holding {ispc_burst} images, {:.1} GiB",
        (ispc_burst * source.as_raw().len()) as f64 / f64::from(1u32 << 30)
    );
    let images: Vec<RgbaImage> = (0..ispc_burst).map(|_| source.clone()).collect();
    let (wall, lat) = run_burst(&ispc, images, threads);
    let rate = ispc_burst as f64 / wall.as_secs_f64();
    eprintln!(
        "  burst of {ispc_burst}: {:.1} ms, {rate:.1} tiles/s; tile {}",
        ms(wall),
        fig(summary(&lat))
    );
    rows.push(Row {
        device: format!("ISPC, {threads} threads"),
        lone: None,
        tiles_per_s: rate,
        stages: None,
    });

    for selector in gpu_selectors() {
        let Some(info) = adapter_info(&selector) else {
            eprintln!("\nSkipping `{selector}`: no such adapter");
            continue;
        };
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channels, mut worker, _selection) =
            match create_gpu_encoder_channels(&selector, CHANNEL_CAPACITY, shutdown) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("\nSkipping `{selector}`: {e}");
                    continue;
                }
            };
        eprintln!("\n== GPU `{selector}`: {} ==", adapter_label(&info));
        let prefetch_channel = Arc::new(channels.prefetch);
        let channel = Arc::new(channels.demand);
        let stats = Arc::clone(channel.stats());
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_compressor(
                Arc::clone(&channel) as Arc<dyn MipmapCompressor>,
                Arc::new(IspcCompressor),
            )
            .with_mipmap_count(LEVELS);
        let nv = || nvidia_state().unwrap_or_else(|| "-".into());

        // Warm-up: shader compilation, the kept resources.
        run_lone(&encoder, &source, 3);
        let _ = take(&stats);

        eprintln!("  NVIDIA state before lone tiles: {}", nv());
        let lone = run_lone(&encoder, &source, lone_n);
        eprintln!("  lone tile (caller) {}", fig(summary(&lone)));
        if let Some(s) = take(&stats) {
            print_stages(&s);
        }

        eprintln!("  NVIDIA state before bursts: {}", nv());
        let mut tiles = 0;
        let mut wall = Duration::ZERO;
        for b in 0..bursts {
            let images: Vec<RgbaImage> = (0..burst).map(|_| source.clone()).collect();
            let (w, _) = run_burst(&encoder, images, burst);
            eprintln!(
                "  burst {}: {burst} tiles in {:.1} ms, {:.1} tiles/s; NVIDIA: {}",
                b + 1,
                ms(w),
                burst as f64 / w.as_secs_f64(),
                nv()
            );
            tiles += burst;
            wall += w;
        }
        let rate = tiles as f64 / wall.as_secs_f64();
        eprintln!("  bursts: {rate:.1} tiles/s");
        let burst_stages = take(&stats);
        if let Some(s) = &burst_stages {
            print_stages(s);
        }
        rows.push(Row {
            device: format!("GPU {}", info.name),
            lone: summary(&lone),
            tiles_per_s: rate,
            stages: burst_stages,
        });

        // The same bursts on the prefetch lane: the worker never has two
        // prefetch tiles in flight (#425), so this is the serial rate.
        {
            let prefetch = DdsEncoder::new(DdsFormat::BC1)
                .with_mipmap_compressor(
                    Arc::clone(&prefetch_channel) as Arc<dyn MipmapCompressor>,
                    Arc::new(IspcCompressor),
                )
                .with_mipmap_count(LEVELS);
            let mut tiles = 0;
            let mut wall = Duration::ZERO;
            for _ in 0..bursts {
                let images: Vec<RgbaImage> = (0..burst).map(|_| source.clone()).collect();
                let (w, _) = run_burst(&prefetch, images, burst);
                tiles += burst;
                wall += w;
            }
            let rate = tiles as f64 / wall.as_secs_f64();
            eprintln!("  prefetch-lane bursts: {rate:.1} tiles/s");
            if let Some(s) = prefetch_channel.stats().take_sample() {
                print_stages(&s);
            }
            rows.push(Row {
                device: format!("GPU {}, prefetch lane", info.name),
                lone: None,
                tiles_per_s: rate,
                stages: None,
            });
        }

        // ISPC on one thread while the GPU encodes a steady stream: four
        // callers keep the GPU's queue from running dry. Their images are
        // cloned beforehand, so the only CPU work beside ISPC is the GPU
        // path's own (the worker's upload and readback).
        let pool = std::sync::Mutex::new(
            (0..env_or("XEL_BENCH_STREAM_POOL", 96usize))
                .map(|_| source.clone())
                .collect::<Vec<_>>(),
        );
        let stop = AtomicBool::new(false);
        let streamed = AtomicUsize::new(0);
        // With `XEL_BENCH_STREAM_RATE`, tile `n` of the stream is sent no
        // sooner than `n / rate` seconds in: the same GPU work per second
        // on any build, so the CPU's slowdown compares like for like.
        let paced = stream_rate.map(|rate| (Instant::now(), rate, AtomicUsize::new(0)));
        let (cpu_under_load, stream_wall) = std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some((start, rate, sent)) = &paced {
                            let n = sent.fetch_add(1, Ordering::Relaxed);
                            let at = *start + Duration::from_secs_f64(n as f64 / rate);
                            std::thread::sleep(at.saturating_duration_since(Instant::now()));
                        }
                        let Some(img) = pool.lock().unwrap().pop() else {
                            break;
                        };
                        encoder.encode(img).expect("the tile encodes");
                        streamed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            // Let the stream settle.
            std::thread::sleep(Duration::from_millis(300));
            let _ = take(&stats);
            let started = streamed.load(Ordering::Relaxed);
            let t_cpu = Instant::now();
            let lone = on_cpu(pin, || run_lone(&ispc, &source, lone_n));
            let cpu_wall = t_cpu.elapsed();
            let during = streamed.load(Ordering::Relaxed) - started;
            stop.store(true, Ordering::Relaxed);
            (lone, (during, cpu_wall))
        });
        let (during, cpu_wall) = stream_wall;
        let left = pool.lock().unwrap().len();
        eprintln!(
            "  ISPC one thread while this GPU encodes{}{}:",
            stream_rate.map_or(String::new(), |r| format!(" at most {r} tiles/s")),
            if left == 0 {
                " (the stream's images ran out before the end: raise XEL_BENCH_STREAM_POOL)"
            } else {
                ""
            }
        );
        eprintln!(
            "    ISPC lone tile {} (idle: {})",
            fig(summary(&cpu_under_load)),
            fig(idle_lone)
        );
        eprintln!(
            "    GPU meanwhile: {:.1} tiles/s",
            during as f64 / cpu_wall.as_secs_f64()
        );
        if let Some(s) = take(&stats) {
            print_stages(&s);
        }
        eprintln!("  NVIDIA state after: {}", nv());

        drop(encoder);
        drop(prefetch_channel);
        worker.stop_blocking(Duration::from_secs(10));
    }

    eprintln!("\n== Summary ({}) ==", machine_label());
    eprintln!("Per-stage figures are the GPU bursts' p50, in ms.");
    eprintln!(
        "{:<44} {:>9} {:>8} | {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "device",
        "lone p50",
        "tiles/s",
        "queue",
        "upload",
        "gpu",
        "mips",
        "kernel",
        "readbk",
        "process"
    );
    let p50 = |s: Option<LatencySummary>| {
        s.map_or("-".to_string(), |s| {
            format!("{:.2}", s.median_us as f64 / 1000.0)
        })
    };
    for r in rows {
        let st = r.stages.unwrap_or_default();
        eprintln!(
            "{:<44} {:>9} {:>8.1} | {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
            r.device,
            p50(r.lone),
            r.tiles_per_s,
            p50(st.queue_wait),
            p50(st.upload),
            p50(st.gpu),
            p50(st.gpu_mips),
            p50(st.gpu_kernel),
            p50(st.readback),
            p50(st.process),
        );
    }
}
