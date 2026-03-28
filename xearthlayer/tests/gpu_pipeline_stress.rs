//! GPU pipeline stress test with idle gaps.
//!
//! Simulates long-running GPU usage patterns: bursts of compression
//! requests separated by idle periods, mimicking flight sim behavior
//! (active tile generation at DSF boundaries, idle during cruise).
//!
//! Run with: cargo test -p xearthlayer --features gpu-encode
//!           --test gpu_pipeline_stress -- --ignored --nocapture
//!
//! Environment variables:
//!   GPU_STRESS_DURATION_SECS  — total test duration (default: 120)
//!   GPU_STRESS_BURST_SIZE     — requests per burst (default: 15)
//!   GPU_STRESS_IDLE_MS        — idle gap between bursts in ms (default: 3000)
//!   GPU_STRESS_IMAGE_SIZE     — image width/height (default: 256, use 4096 for realistic)

#[cfg(feature = "gpu-encode")]
mod gpu_stress {
    use image::RgbaImage;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use xearthlayer::dds::gpu_channel::*;
    use xearthlayer::dds::{DdsFormat, ImageCompressor};

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

    #[tokio::test]
    #[ignore]
    async fn stress_test_gpu_pipeline_with_idle_gaps() {
        let duration = Duration::from_secs(env_or("GPU_STRESS_DURATION_SECS", 120));
        let burst_size: usize = env_or("GPU_STRESS_BURST_SIZE", 15);
        let idle_ms: u64 = env_or("GPU_STRESS_IDLE_MS", 3000);
        let image_size: u32 = env_or("GPU_STRESS_IMAGE_SIZE", 256);

        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channel, _worker) = match create_gpu_encoder_channel("integrated", shutdown) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping stress test: {e}");
                return;
            }
        };

        let channel = Arc::new(channel);
        let start = Instant::now();
        let mut total_requests = 0u64;
        let mut total_successes = 0u64;
        let mut total_failures = 0u64;
        let mut burst_count = 0u64;
        let mut all_latencies_ms: Vec<f64> = Vec::new();
        let initial_rss = rss_mb();

        eprintln!(
            "GPU stress test: duration={duration:?}, burst_size={burst_size}, \
             idle_ms={idle_ms}, image_size={image_size}x{image_size}, \
             initial_rss={initial_rss:.1}MB"
        );

        while start.elapsed() < duration {
            burst_count += 1;
            let burst_start = Instant::now();
            eprintln!(
                "Burst {burst_count}: submitting {burst_size} requests \
                 (elapsed: {:.1}s, ok={total_successes}, err={total_failures}, \
                 rss={:.1}MB)",
                start.elapsed().as_secs_f64(),
                rss_mb()
            );

            // Submit burst of concurrent requests
            let mut handles = Vec::with_capacity(burst_size);
            for _ in 0..burst_size {
                let ch = Arc::clone(&channel);
                let size = image_size;
                handles.push(tokio::task::spawn_blocking(move || {
                    let t = Instant::now();
                    let result = {
                        let image = RgbaImage::new(size, size);
                        ch.compress(&image, DdsFormat::BC1)
                    };
                    (result, t.elapsed())
                }));
            }

            // Collect results with timeout
            let mut burst_latencies = Vec::new();
            for handle in handles {
                total_requests += 1;
                match tokio::time::timeout(Duration::from_secs(30), handle).await {
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
                        eprintln!("  TIMEOUT: request hung for 30s — possible deadlock");
                    }
                }
            }

            // Report burst latency stats
            if !burst_latencies.is_empty() {
                burst_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let min = burst_latencies[0];
                let max = burst_latencies[burst_latencies.len() - 1];
                let avg: f64 = burst_latencies.iter().sum::<f64>() / burst_latencies.len() as f64;
                let p99_idx = ((burst_latencies.len() as f64) * 0.99) as usize;
                let p99 = burst_latencies[p99_idx.min(burst_latencies.len() - 1)];
                let burst_wall = burst_start.elapsed().as_secs_f64() * 1000.0;
                eprintln!(
                    "  Latency: min={min:.1}ms avg={avg:.1}ms p99={p99:.1}ms \
                     max={max:.1}ms | burst_wall={burst_wall:.1}ms"
                );
                all_latencies_ms.extend_from_slice(&burst_latencies);
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

        if !all_latencies_ms.is_empty() {
            all_latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg: f64 = all_latencies_ms.iter().sum::<f64>() / all_latencies_ms.len() as f64;
            let p50 = all_latencies_ms[all_latencies_ms.len() / 2];
            let p99_idx = ((all_latencies_ms.len() as f64) * 0.99) as usize;
            let p99 = all_latencies_ms[p99_idx.min(all_latencies_ms.len() - 1)];
            eprintln!("Latency:      avg={avg:.1}ms p50={p50:.1}ms p99={p99:.1}ms");
        }

        assert_eq!(
            total_failures, 0,
            "Expected zero failures, got {total_failures} out of {total_requests}"
        );
        assert!(
            channel.is_connected(),
            "GPU worker should still be alive after stress test"
        );
    }
}
