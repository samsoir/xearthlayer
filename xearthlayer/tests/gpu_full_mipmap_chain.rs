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

/// Whether the adapter `selector` names gives a device that can time its
/// kernels (#476), its device requested as the worker's is
/// (`create_gpu_resources`); `None` when the host has no such adapter.
fn kernel_timing_on(selector: &str) -> Option<bool> {
    use xearthlayer::dds::gpu_timestamps::{requested_features, KernelTimer};
    let adapters = xearthlayer::system::enumerate_gpus_raw();
    let adapter = xearthlayer::system::find_gpu(&adapters, selector).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("xel-test-kernel-timing"),
        required_features: requested_features(adapter.features()),
        ..Default::default()
    }))
    .ok()?;
    Some(KernelTimer::available(&device, &queue))
}

/// Sends one tile of `size` x `size`, `levels` levels, in `file`, on
/// `channel`, from a blocking thread.
async fn encode_on(
    channel: &Arc<xearthlayer::dds::gpu_channel::GpuEncoderChannel>,
    size: u32,
    levels: usize,
    file: Vec<u8>,
) -> Result<ChainOutcome, xearthlayer::dds::DdsError> {
    let ch = Arc::clone(channel);
    tokio::task::spawn_blocking(move || {
        let image = RgbaImage::from_pixel(size, size, image::Rgba([96, 120, 64, 255]));
        ch.compress_mipmap_chain(image, DdsFormat::BC1, levels, file)
    })
    .await
    .expect("join")
}

/// The production wiring of the kernel time (#476): on a device that can
/// time its kernels, every tile the worker encodes carries a kernel time
/// and a mip time (#424), within its `gpu` stage, whatever its size and level count, and so does
/// the tile after a failed one, for which the worker made its resources,
/// kernel timer included, again (#423's `AfterTile`).
#[tokio::test]
#[ignore]
async fn every_tile_carries_its_kernel_time_on_a_device_with_timestamps() {
    match kernel_timing_on(&gpu_device()) {
        None => {
            eprintln!("Skipping: no usable GPU");
            return;
        }
        Some(false) => {
            eprintln!("Skipping: the device cannot time its kernels (no timestamp queries)");
            return;
        }
        Some(true) => {}
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (channels, mut worker, selection) =
        xearthlayer::dds::gpu_channel::create_gpu_encoder_channels(
            &gpu_device(),
            xearthlayer::dds::gpu_channel::CHANNEL_CAPACITY,
            shutdown,
        )
        .expect("the GPU that was just opened");
    eprintln!("Adapter: {}", selection.adapter);
    let channel = Arc::new(channels.demand);

    // One tile's sample: its kernel time is there and within its `gpu`.
    let kernel_within_gpu = |case: &str| {
        let sample = channel
            .stats()
            .take_sample()
            .unwrap_or_else(|| panic!("{case}: the tile is in the sample"));
        assert_eq!((sample.tiles(), sample.failed), (1, 0), "{case}");
        let kernel = sample
            .gpu_kernel
            .unwrap_or_else(|| panic!("{case}: the tile carries a kernel time"));
        let gpu = sample.gpu.expect("the gpu stage");
        // Every case has levels below level 0, which the GPU builds (#424).
        let mips = sample
            .gpu_mips
            .unwrap_or_else(|| panic!("{case}: the tile carries a mip time"));
        eprintln!(
            "{case}: kernel {} us and mips {} us within gpu {} us",
            kernel.max_us, mips.max_us, gpu.max_us
        );
        assert_eq!((kernel.count, mips.count), (1, 1), "{case}");
        assert!(
            kernel.max_us + mips.max_us <= gpu.max_us,
            "{case}: kernel and mips within gpu"
        );
        assert_eq!(sample.kernel_timing(), "timestamps", "{case}");
    };

    let full = |size: u32| MipmapGenerator::full_chain_count(size, size);
    // A full chain; the same size truncated, on the kept resources; a
    // smaller tile, on resources made for it.
    for (case, size, levels) in [
        ("4096, full chain", 4096, full(4096)),
        ("4096, 5 levels", 4096, 5),
        ("1024, full chain", 1024, full(1024)),
    ] {
        gpu_bytes(
            encode_on(&channel, size, levels, file(size, size, levels))
                .await
                .unwrap_or_else(|e| panic!("{case}: {e}")),
        );
        kernel_within_gpu(case);
    }

    // A tile of the same size that fails: its file is one block longer
    // than its chain, which the worker refuses before any GPU work. The worker
    // drops its kept resources after it, and the next tile makes them again.
    let mut long = file(1024, 1024, full(1024));
    long.extend_from_slice(&[0; 8]);
    assert!(
        encode_on(&channel, 1024, full(1024), long).await.is_err(),
        "a file longer than its chain fails the tile"
    );
    let sample = channel.stats().take_sample().expect("the failure");
    assert_eq!((sample.tiles(), sample.failed), (0, 1));

    gpu_bytes(
        encode_on(&channel, 1024, full(1024), file(1024, 1024, full(1024)))
            .await
            .expect("the tile after the failed one"),
    );
    kernel_within_gpu("1024, after a failed tile");

    worker.stop(std::time::Duration::from_secs(10)).await;
}
