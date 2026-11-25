//! GPU pipeline stress test with idle gaps, and the per-stage measurements
//! of the GPU encode path (#419).
//!
//! Simulates long-running GPU usage patterns: bursts of compression
//! requests separated by idle periods, mimicking flight sim behavior
//! (active tile generation at DSF boundaries, idle during cruise). Before
//! the bursts it encodes tiles one at a time, so the stages of a lone tile
//! and of a tile under a burst can be told apart. The defaults are the
//! production chain: a 4096 x 4096 tile with all 13 levels.
//!
//! Run with (release, as the measurements are about speed):
//!   cargo test --release -p xearthlayer
//!           --test gpu_pipeline_stress -- --ignored --nocapture --test-threads=1
//!
//! Environment variables (read by these tests, never by the library):
//!   XEL_TEST_GPU_DEVICE       — adapter, as `texture.gpu_device`: `integrated`
//!                               (default), `discrete`, or part of its name
//!   GPU_STRESS_DURATION_SECS  — total burst phase duration (default: 120)
//!   GPU_STRESS_BURST_SIZE     — requests per burst (default: 15)
//!   GPU_STRESS_IDLE_MS        — idle gap between bursts in ms (default: 3000)
//!   GPU_STRESS_IMAGE_SIZE     — image width/height (default: 4096)
//!   GPU_STRESS_LEVELS         — mip levels (default: the full chain, 13 at 4096)
//!   GPU_STRESS_SINGLES        — lone tiles timed before the bursts (default: 10)

mod gpu_stress {
    use image::RgbaImage;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use xearthlayer::dds::gpu_channel::*;
    use xearthlayer::dds::gpu_stats::GpuSample;
    use xearthlayer::dds::{
        DdsEncoder, DdsFormat, IspcCompressor, MipmapCompressor, MipmapGenerator,
    };
    use xearthlayer::metrics::LatencySummary;

    /// Read current process RSS from /proc/self/status (Linux only).
    fn rss_mb() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<f64>().ok())
            })
            .map(|kb| kb / 1024.0)
            .unwrap_or(0.0)
    }

    fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn gpu_device() -> String {
        std::env::var("XEL_TEST_GPU_DEVICE").unwrap_or_else(|_| "integrated".to_string())
    }

    fn image_size() -> u32 {
        env_or("GPU_STRESS_IMAGE_SIZE", 4096)
    }

    fn levels(size: u32) -> usize {
        env_or(
            "GPU_STRESS_LEVELS",
            MipmapGenerator::full_chain_count(size, size),
        )
    }

    /// A deterministic image with texture: smooth gradients under per-pixel
    /// noise, so neither encoder sees the flat blocks of an empty image.
    fn test_image(size: u32) -> RgbaImage {
        RgbaImage::from_fn(size, size, |x, y| {
            let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
            h ^= h >> 29;
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            h ^= h >> 32;
            let noise = (h & 0x3F) as u32;
            let r = ((x * 255 / size) + noise).min(255) as u8;
            let g = ((y * 255 / size) + noise / 2).min(255) as u8;
            let b = (((x + y) * 127 / size) + (noise >> 1)).min(255) as u8;
            image::Rgba([r, g, b, 255])
        })
    }

    fn ms(us: u64) -> f64 {
        us as f64 / 1000.0
    }

    fn stage_line(name: &str, s: Option<LatencySummary>) -> String {
        match s {
            Some(s) => format!(
                "  {name:<11} p50={:>8.2}ms p95={:>8.2}ms max={:>8.2}ms (n={})",
                ms(s.median_us),
                ms(s.p95_us),
                ms(s.max_us),
                s.count
            ),
            None => format!("  {name:<11} -"),
        }
    }

    fn print_sample(title: &str, s: &GpuSample) {
        eprintln!(
            "{title}: tiles={} (unsampled {}) failed={} depth_now={} depth_max={}",
            s.tiles(),
            s.unsampled,
            s.failed,
            s.depth_now,
            s.depth_max
        );
        for (name, stage) in [
            ("queue_wait", s.queue_wait),
            ("downsample", s.downsample),
            ("upload", s.upload),
            ("gpu", s.gpu),
            ("readback", s.readback),
            ("process", s.process),
            ("total", s.total),
        ] {
            eprintln!("{}", stage_line(name, stage));
        }
    }

    fn summary_of(us: Vec<u64>) -> Option<LatencySummary> {
        let mut w = xearthlayer::metrics::LatencyWindow::default();
        for u in us {
            w.record(u);
        }
        w.take()
    }

    #[tokio::test]
    #[ignore]
    async fn stress_test_gpu_pipeline_with_idle_gaps() {
        let duration = Duration::from_secs(env_or("GPU_STRESS_DURATION_SECS", 120));
        let burst_size: usize = env_or("GPU_STRESS_BURST_SIZE", 15);
        let idle_ms: u64 = env_or("GPU_STRESS_IDLE_MS", 3000);
        let singles: usize = env_or("GPU_STRESS_SINGLES", 10);
        let image_size = image_size();
        let levels = levels(image_size);
        let device = gpu_device();

        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channel, _worker, selection) =
            match create_gpu_encoder_channel(&device, WhenBusy::Wait, shutdown) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Skipping stress test: {e}");
                    return;
                }
            };

        let channel = Arc::new(channel);
        let stats = Arc::clone(channel.stats());
        let source = Arc::new(test_image(image_size));
        let initial_rss = rss_mb();

        eprintln!(
            "GPU stress test: adapter={}, duration={duration:?}, burst_size={burst_size}, \
             idle_ms={idle_ms}, image_size={image_size}x{image_size}, levels={levels}, \
             initial_rss={initial_rss:.1}MB",
            selection.adapter
        );

        // One tile to warm the pipeline (shader compilation, first
        // allocations), not counted.
        {
            let ch = Arc::clone(&channel);
            let img = (*source).clone();
            tokio::task::spawn_blocking(move || {
                ch.compress_mipmap_chain(img, DdsFormat::BC1, levels)
            })
            .await
            .unwrap()
            .expect("warm-up tile");
            let _ = stats.take_sample();
        }

        // Lone tiles: nothing else in the channel.
        for _ in 0..singles {
            let ch = Arc::clone(&channel);
            let img = (*source).clone();
            tokio::task::spawn_blocking(move || {
                ch.compress_mipmap_chain(img, DdsFormat::BC1, levels)
            })
            .await
            .unwrap()
            .expect("lone tile");
        }
        if let Some(s) = stats.take_sample() {
            print_sample("Lone tiles", &s);
        }

        let start = Instant::now();
        let mut total_requests = 0u64;
        let mut total_successes = 0u64;
        let mut total_failures = 0u64;
        let mut burst_count = 0u64;
        let mut all_latencies_ms: Vec<f64> = Vec::new();
        let mut burst_tiles = 0usize;
        let mut burst_wall = Duration::ZERO;
        let mut depth_left_after_bursts = 0u64;

        while start.elapsed() < duration {
            burst_count += 1;
            eprintln!(
                "Burst {burst_count}: submitting {burst_size} requests \
                 (elapsed: {:.1}s, ok={total_successes}, err={total_failures}, \
                 rss={:.1}MB)",
                start.elapsed().as_secs_f64(),
                rss_mb()
            );

            // The images are cloned before the burst starts, so the clone
            // is not part of any request's time.
            let images: Vec<RgbaImage> = (0..burst_size).map(|_| (*source).clone()).collect();
            let burst_start = Instant::now();

            // Submit burst of concurrent requests
            let mut handles = Vec::with_capacity(burst_size);
            for img in images {
                let ch = Arc::clone(&channel);
                handles.push(tokio::task::spawn_blocking(move || {
                    let t = Instant::now();
                    let result = ch.compress_mipmap_chain(img, DdsFormat::BC1, levels);
                    (result, t.elapsed())
                }));
            }

            // Collect results with timeout
            let mut burst_latencies = Vec::new();
            for handle in handles {
                total_requests += 1;
                match tokio::time::timeout(Duration::from_secs(60), handle).await {
                    Ok(Ok((Ok(_data), elapsed))) => {
                        total_successes += 1;
                        burst_latencies.push(elapsed.as_secs_f64() * 1000.0);
                    }
                    Ok(Ok((Err(e), _))) => {
                        total_failures += 1;
                        eprintln!("  Compression error: {e}");
                    }
                    Ok(Err(e)) => {
                        total_failures += 1;
                        eprintln!("  Task panic: {e}");
                    }
                    Err(_) => {
                        total_failures += 1;
                        eprintln!("  TIMEOUT: request hung for 60s — possible deadlock");
                    }
                }
            }
            let wall = burst_start.elapsed();
            burst_wall += wall;
            burst_tiles += burst_latencies.len();

            // Report burst latency stats
            if !burst_latencies.is_empty() {
                burst_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let min = burst_latencies[0];
                let max = burst_latencies[burst_latencies.len() - 1];
                let avg: f64 = burst_latencies.iter().sum::<f64>() / burst_latencies.len() as f64;
                let p99_idx = ((burst_latencies.len() as f64) * 0.99) as usize;
                let p99 = burst_latencies[p99_idx.min(burst_latencies.len() - 1)];
                eprintln!(
                    "  Latency: min={min:.1}ms avg={avg:.1}ms p99={p99:.1}ms \
                     max={max:.1}ms | burst_wall={:.1}ms | {:.2} tiles/s",
                    wall.as_secs_f64() * 1000.0,
                    burst_latencies.len() as f64 / wall.as_secs_f64()
                );
                all_latencies_ms.extend_from_slice(&burst_latencies);
            }
            // A burst whose callers all returned leaves the channel empty.
            // Checked at the end, after the failure summary: a caller that
            // timed out is still counted in the depth.
            if burst_latencies.len() == burst_size && stats.depth() != 0 {
                eprintln!("  Depth {} after a complete burst", stats.depth());
                depth_left_after_bursts += 1;
            }

            // Check worker is still alive
            if !channel.is_connected() {
                eprintln!("FATAL: GPU worker died after burst {burst_count}");
                break;
            }

            // Idle gap (simulates cruise period)
            if start.elapsed() + Duration::from_millis(idle_ms) < duration {
                eprintln!("  Idle for {idle_ms}ms...");
                tokio::time::sleep(Duration::from_millis(idle_ms)).await;
            }
        }

        let final_rss = rss_mb();
        eprintln!("\n=== GPU Stress Test Results ===");
        eprintln!("Adapter:      {}", selection.adapter);
        eprintln!("Duration:     {:.1}s", start.elapsed().as_secs_f64());
        eprintln!("Bursts:       {burst_count}");
        eprintln!("Requests:     {total_requests}");
        eprintln!("Successes:    {total_successes}");
        eprintln!("Failures:     {total_failures}");
        eprintln!("Worker alive: {}", channel.is_connected());
        eprintln!(
            "RSS:          {initial_rss:.1}MB → {final_rss:.1}MB (delta: {:.1}MB)",
            final_rss - initial_rss
        );
        if !burst_wall.is_zero() {
            eprintln!(
                "Throughput:   {:.2} tiles/s under bursts",
                burst_tiles as f64 / burst_wall.as_secs_f64()
            );
        }

        if !all_latencies_ms.is_empty() {
            all_latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg: f64 = all_latencies_ms.iter().sum::<f64>() / all_latencies_ms.len() as f64;
            let p50 = all_latencies_ms[all_latencies_ms.len() / 2];
            let p99_idx = ((all_latencies_ms.len() as f64) * 0.99) as usize;
            let p99 = all_latencies_ms[p99_idx.min(all_latencies_ms.len() - 1)];
            eprintln!("Latency:      avg={avg:.1}ms p50={p50:.1}ms p99={p99:.1}ms");
        }
        if let Some(s) = stats.take_sample() {
            print_sample("Burst tiles", &s);
        }

        assert_eq!(
            total_failures, 0,
            "Expected zero failures, got {total_failures} out of {total_requests}"
        );
        assert!(
            channel.is_connected(),
            "GPU worker should still be alive after stress test"
        );
        assert_eq!(
            depth_left_after_bursts, 0,
            "every complete burst must leave the queue depth at zero"
        );
    }

    /// The same tile through ISPC on this machine, for comparison: the full
    /// `DdsEncoder` (CPU mip chain and BC1), one tile at a time on one
    /// thread, then a burst of `GPU_STRESS_BURST_SIZE` tiles at once on the
    /// blocking pool.
    #[tokio::test]
    #[ignore]
    async fn ispc_reference_on_the_same_tile() {
        let burst_size: usize = env_or("GPU_STRESS_BURST_SIZE", 15);
        let singles: usize = env_or("GPU_STRESS_SINGLES", 10);
        let image_size = image_size();
        let levels = levels(image_size);
        let encoder = Arc::new(
            DdsEncoder::new(DdsFormat::BC1)
                .with_compressor(Arc::new(IspcCompressor))
                .with_mipmap_count(levels),
        );
        let source = Arc::new(test_image(image_size));

        let encode = |encoder: &DdsEncoder, img: RgbaImage| {
            let t = Instant::now();
            encoder.encode(img).expect("ISPC encode");
            t.elapsed().as_micros() as u64
        };

        // Warm-up, then lone tiles.
        encode(&encoder, (*source).clone());
        let mut lone = Vec::new();
        for _ in 0..singles {
            lone.push(encode(&encoder, (*source).clone()));
        }
        eprintln!("ISPC {image_size}x{image_size}, {levels} levels");
        eprintln!("{}", stage_line("lone tile", summary_of(lone)));

        let images: Vec<RgbaImage> = (0..burst_size).map(|_| (*source).clone()).collect();
        let start = Instant::now();
        let handles: Vec<_> = images
            .into_iter()
            .map(|img| {
                let enc = Arc::clone(&encoder);
                tokio::task::spawn_blocking(move || encode(&enc, img))
            })
            .collect();
        let mut burst = Vec::new();
        for h in handles {
            burst.push(h.await.unwrap());
        }
        let wall = start.elapsed();
        eprintln!("{}", stage_line("burst tile", summary_of(burst)));
        eprintln!(
            "  burst of {burst_size}: wall={:.1}ms, {:.2} tiles/s on {} threads",
            wall.as_secs_f64() * 1000.0,
            burst_size as f64 / wall.as_secs_f64(),
            std::thread::available_parallelism().map_or(0, |n| n.get())
        );
    }
}
