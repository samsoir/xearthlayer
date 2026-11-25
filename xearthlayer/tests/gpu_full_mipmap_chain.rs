//! GPU full mipmap chain integration test (issue #212).
//!
//! The GPU backend is the one path whose sub-block behaviour cannot be
//! covered by unit tests: the upstream `block_compression` kernel asserts
//! both dimensions are multiples of 4, and its panic handler shuts the
//! encoder worker down for the rest of the process lifetime. The 2x2 and 1x1
//! tail of a full chain therefore has to be exercised against real hardware.
//!
//! Requires a GPU. Run with:
//!   cargo test -p xearthlayer --test gpu_full_mipmap_chain -- --ignored --nocapture
//!
//! `XEL_TEST_GPU_DEVICE` picks the adapter as `texture.gpu_device` does
//! (`integrated`, the default, `discrete`, or part of the adapter's name).

use image::RgbaImage;
use std::sync::Arc;
use xearthlayer::dds::{DdsFormat, MipmapCompressor, MipmapGenerator};

/// The adapter these tests run on; read by the test, never the library.
fn gpu_device() -> String {
    std::env::var("XEL_TEST_GPU_DEVICE").unwrap_or_else(|_| "integrated".to_string())
}

/// Expected payload bytes for a BC1 chain, walking levels the way the
/// encoder does. Levels below 4x4 still occupy one whole 8-byte block.
fn expected_bc1_payload(width: u32, height: u32, levels: usize) -> usize {
    let (mut w, mut h) = (width, height);
    let mut total = 0;
    for _ in 0..levels {
        total += (w.div_ceil(4) as usize) * (h.div_ceil(4) as usize) * 8;
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    total
}

#[tokio::test]
#[ignore]
async fn gpu_emits_full_mipmap_chain_for_4096_tile() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (channel, _worker, _adapter) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channel(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::WhenBusy::Wait,
            shutdown,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };

    let levels = MipmapGenerator::full_chain_count(4096, 4096);
    assert_eq!(levels, 13, "4096x4096 must yield 13 levels");

    // spawn_blocking: compress_mipmap_chain blocks on the worker round trip.
    let channel = Arc::new(channel);
    let ch = Arc::clone(&channel);
    let data = tokio::task::spawn_blocking(move || {
        ch.compress_mipmap_chain(RgbaImage::new(4096, 4096), DdsFormat::BC1, levels)
    })
    .await
    .expect("join")
    .expect("GPU compression of the full chain must not fail");

    assert_eq!(
        data.len(),
        expected_bc1_payload(4096, 4096, levels),
        "GPU payload must hold exactly 13 levels worth of blocks"
    );
    assert_eq!(
        data.len() + 128,
        11_184_952,
        "header + payload must match the Ortho4XP reference size"
    );

    // Every stage of the full chain is timed (#419).
    let sample = channel
        .stats()
        .take_sample()
        .expect("the tile is in the sample");
    eprintln!("{sample:#?}");
    assert_eq!((sample.tiles(), sample.failed), (1, 0));
    for (stage, summary) in [
        ("downsample", sample.downsample),
        ("upload", sample.upload),
        ("gpu", sample.gpu),
        ("readback", sample.readback),
        ("process", sample.process),
        ("total", sample.total),
    ] {
        let summary = summary.unwrap_or_else(|| panic!("{stage} is reported"));
        assert!(summary.max_us > 0, "{stage} took measurable time");
    }
    assert_eq!(channel.stats().depth(), 0, "nothing is left in the channel");
}

/// The worker must survive the sub-block tail and keep serving afterwards.
///
/// A panic inside the worker is caught, but the handler drains the queue and
/// breaks the loop — so a second request after the tail is what actually
/// proves the worker is still alive.
#[tokio::test]
#[ignore]
async fn gpu_worker_survives_sub_block_levels() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (channel, _worker, _adapter) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channel(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::WhenBusy::Wait,
            shutdown,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };

    let channel = Arc::new(channel);

    // 64x64 -> 7 levels, reaching 4x4, 2x2 and 1x1.
    let levels = MipmapGenerator::full_chain_count(64, 64);
    let ch = Arc::clone(&channel);
    let first = tokio::task::spawn_blocking(move || {
        ch.compress_mipmap_chain(RgbaImage::new(64, 64), DdsFormat::BC1, levels)
    })
    .await
    .expect("join")
    .expect("sub-block levels must not kill the GPU worker");

    assert_eq!(first.len(), expected_bc1_payload(64, 64, levels));

    // Second request proves the worker did not break out of its loop.
    let ch = Arc::clone(&channel);
    let second = tokio::task::spawn_blocking(move || {
        ch.compress_mipmap_chain(RgbaImage::new(256, 256), DdsFormat::BC1, 9)
    })
    .await
    .expect("join")
    .expect("worker must still serve requests after the sub-block tail");

    assert_eq!(second.len(), expected_bc1_payload(256, 256, 9));
}
