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
use xearthlayer::dds::{ChainOutcome, DdsFormat, MipmapCompressor, MipmapGenerator};

/// The adapter these tests run on; read by the test, never the library.
fn gpu_device() -> String {
    std::env::var("XEL_TEST_GPU_DEVICE").unwrap_or_else(|_| "integrated".to_string())
}

/// The DDS file of a `width` x `height` BC1 encode of `levels` levels, its
/// header written: what the encoder hands the GPU to fill (#423).
fn file(width: u32, height: u32, levels: usize) -> Vec<u8> {
    use xearthlayer::dds::{layout, DdsHeader};
    let levels = levels as u32;
    let mut file = vec![0; layout::file_len(width, height, levels, DdsFormat::BC1)];
    file[..layout::HEADER_LEN]
        .copy_from_slice(&DdsHeader::new(width, height, levels, DdsFormat::BC1).to_bytes());
    file
}

/// The GPU's chain, after the file's header; a tile the GPU handed back to
/// the CPU fails the test.
fn gpu_bytes(outcome: ChainOutcome) -> Vec<u8> {
    match outcome {
        ChainOutcome::Compressed(mut file) => file.split_off(xearthlayer::dds::layout::HEADER_LEN),
        ChainOutcome::Returned { reason, .. } => panic!("the GPU handed the tile back: {reason}"),
        ChainOutcome::Cancelled => panic!("nothing cancels these tiles"),
    }
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
    let (channels, mut worker, _adapter) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channels(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::CHANNEL_CAPACITY,
            shutdown,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };
    let channel = channels.demand;

    let levels = MipmapGenerator::full_chain_count(4096, 4096);
    assert_eq!(levels, 13, "4096x4096 must yield 13 levels");

    // spawn_blocking: compress_mipmap_chain blocks on the worker round trip.
    let channel = Arc::new(channel);
    let ch = Arc::clone(&channel);
    let data = gpu_bytes(
        tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(
                RgbaImage::new(4096, 4096),
                DdsFormat::BC1,
                levels,
                file(4096, 4096, levels),
            )
        })
        .await
        .expect("join")
        .expect("GPU compression of the full chain must not fail"),
    );

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
    // The worker's thread is not the runtime's: end it before the process.
    worker.stop(std::time::Duration::from_secs(10)).await;
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
    let (channels, mut worker, _adapter) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channels(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::CHANNEL_CAPACITY,
            shutdown,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return;
            }
        };

    let channel = Arc::new(channels.demand);

    // 64x64 -> 7 levels, reaching 4x4, 2x2 and 1x1.
    let levels = MipmapGenerator::full_chain_count(64, 64);
    let ch = Arc::clone(&channel);
    let first = gpu_bytes(
        tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(
                RgbaImage::new(64, 64),
                DdsFormat::BC1,
                levels,
                file(64, 64, levels),
            )
        })
        .await
        .expect("join")
        .expect("sub-block levels must not kill the GPU worker"),
    );

    assert_eq!(first.len(), expected_bc1_payload(64, 64, levels));

    // Second request proves the worker did not break out of its loop.
    let ch = Arc::clone(&channel);
    let second = gpu_bytes(
        tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(
                RgbaImage::new(256, 256),
                DdsFormat::BC1,
                9,
                file(256, 256, 9),
            )
        })
        .await
        .expect("join")
        .expect("worker must still serve requests after the sub-block tail"),
    );

    assert_eq!(second.len(), expected_bc1_payload(256, 256, 9));
    worker.stop(std::time::Duration::from_secs(10)).await;
}

/// A deterministic 4096 tile with texture, `seed` telling two apart.
fn textured(seed: u64) -> RgbaImage {
    RgbaImage::from_fn(4096, 4096, |x, y| {
        let mut h = (u64::from(x) << 32 | u64::from(y)) ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 31;
        let noise = (h & 0x3F) as u32;
        image::Rgba([
            ((x * 255 / 4096) + noise).min(255) as u8,
            ((y * 255 / 4096) + noise / 2).min(255) as u8,
            (h >> 40) as u8,
            255,
        ])
    })
}

/// Encodes `images` back to back, full chain, on one new GPU worker.
async fn on_one_worker(images: Vec<RgbaImage>) -> Option<Vec<Vec<u8>>> {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (channels, mut worker, _adapter) =
        match xearthlayer::dds::gpu_channel::create_gpu_encoder_channels(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::CHANNEL_CAPACITY,
            shutdown.clone(),
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping: no usable GPU ({e})");
                return None;
            }
        };
    let channel = Arc::new(channels.demand);
    let levels = MipmapGenerator::full_chain_count(4096, 4096);
    let mut out = Vec::new();
    for image in images {
        let ch = Arc::clone(&channel);
        out.push(gpu_bytes(
            tokio::task::spawn_blocking(move || {
                ch.compress_mipmap_chain(image, DdsFormat::BC1, levels, file(4096, 4096, levels))
            })
            .await
            .expect("join")
            .expect("the GPU encodes the tile"),
        ));
    }
    shutdown.cancel();
    worker.wait(std::time::Duration::from_secs(10)).await;
    Some(out)
}

/// The acceptance test of #423 for the worker's persistent resources:
/// two different 4096 tiles encoded back to back on one worker, the
/// second reusing the first's buffers and textures, each match the same
/// tile encoded on a fresh worker, byte for byte.
#[tokio::test]
#[ignore]
async fn kept_resources_do_not_change_a_tile() {
    let (first, second) = (textured(1), textured(2));
    let Some(back_to_back) = on_one_worker(vec![first.clone(), second.clone()]).await else {
        return;
    };
    let fresh_first = on_one_worker(vec![first]).await.expect("the GPU again");
    let fresh_second = on_one_worker(vec![second]).await.expect("the GPU again");
    assert!(back_to_back[0] != back_to_back[1], "the tiles differ");
    assert!(back_to_back[0] == fresh_first[0], "the first tile, bytes");
    assert!(back_to_back[1] == fresh_second[0], "the second tile, bytes");
}
