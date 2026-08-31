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

use image::RgbaImage;
use xearthlayer::dds::{DdsFormat, MipmapCompressor, MipmapGenerator};

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
    let (channel, _worker) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channel("integrated", shutdown) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };

    let levels = MipmapGenerator::full_chain_count(4096, 4096);
    assert_eq!(levels, 13, "4096x4096 must yield 13 levels");

    // spawn_blocking: compress_mipmap_chain blocks on the worker round trip.
    let data = tokio::task::spawn_blocking(move || {
        channel.compress_mipmap_chain(RgbaImage::new(4096, 4096), DdsFormat::BC1, levels)
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
    let (channel, _worker) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channel("integrated", shutdown) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };

    let channel = std::sync::Arc::new(channel);

    // 64x64 -> 7 levels, reaching 4x4, 2x2 and 1x1.
    let levels = MipmapGenerator::full_chain_count(64, 64);
    let ch = std::sync::Arc::clone(&channel);
    let first = tokio::task::spawn_blocking(move || {
        ch.compress_mipmap_chain(RgbaImage::new(64, 64), DdsFormat::BC1, levels)
    })
    .await
    .expect("join")
    .expect("sub-block levels must not kill the GPU worker");

    assert_eq!(first.len(), expected_bc1_payload(64, 64, levels));

    // Second request proves the worker did not break out of its loop.
    let ch = std::sync::Arc::clone(&channel);
    let second = tokio::task::spawn_blocking(move || {
        ch.compress_mipmap_chain(RgbaImage::new(256, 256), DdsFormat::BC1, 9)
    })
    .await
    .expect("join")
    .expect("worker must still serve requests after the sub-block tail");

    assert_eq!(second.len(), expected_bc1_payload(256, 256, 9));
}
