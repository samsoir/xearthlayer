# GPU Channel Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the Mutex-serialized `WgpuCompressor` with a channel-based GPU worker that batches multiple tiles into single GPU compute passes.

**Architecture:** Bounded mpsc channel → single GPU worker task → drain-and-batch → oneshot responses. Implements `BlockCompressor` trait for drop-in integration.

**Tech Stack:** `tokio::sync::mpsc`, `tokio::sync::oneshot`, `wgpu`, `block_compression`, `image`

**TDD Methodology:** Every task follows strict red-green-refactor. Write failing tests first, then implement the minimum code to make them pass.

---

## Task 1: Define `GpuEncodeRequest` and `GpuEncoderChannel` Types (TDD)

**Files:**
- Create: `xearthlayer/src/dds/gpu_channel.rs`
- Modify: `xearthlayer/src/dds/mod.rs`

**Step 1: Create the module file with types and test skeleton**

```rust
// xearthlayer/src/dds/gpu_channel.rs
//! GPU encoding channel with drain-and-batch worker.
//!
//! Replaces Mutex-serialized GPU access with a dedicated worker task
//! that batches multiple compression requests into single GPU passes.

#[cfg(feature = "gpu-encode")]
mod inner {
    use crate::dds::types::{DdsError, DdsFormat};
    use image::RgbaImage;
    use tokio::sync::{mpsc, oneshot};

    /// Maximum tiles to batch in a single GPU compute pass.
    ///
    /// 4 tiles × 6 mip levels = 24 compression tasks per pass.
    /// Conservative VRAM budget (~256MB for textures + output buffers).
    pub const MAX_BATCH: usize = 4;

    /// Channel capacity — provides backpressure when GPU is saturated.
    pub const CHANNEL_CAPACITY: usize = 32;

    /// A request to encode a set of mipmap images to DDS via the GPU.
    pub(crate) struct GpuEncodeRequest {
        /// Pre-generated mipmap chain (level 0 = full resolution).
        pub mipmaps: Vec<RgbaImage>,
        /// Target compression format.
        pub format: DdsFormat,
        /// Oneshot channel to send the result back to the caller.
        pub response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
    }

    /// Channel-based GPU encoder that sends work to a dedicated worker task.
    ///
    /// Implements `BlockCompressor` so it can be used as a drop-in replacement
    /// for `WgpuCompressor` in the `DdsEncoder` pipeline.
    pub struct GpuEncoderChannel {
        sender: mpsc::Sender<GpuEncodeRequest>,
    }

    impl GpuEncoderChannel {
        /// Create a new channel with the given sender half.
        pub fn new(sender: mpsc::Sender<GpuEncodeRequest>) -> Self {
            Self { sender }
        }

        /// Check if the GPU worker is still alive.
        pub fn is_connected(&self) -> bool {
            !self.sender.is_closed()
        }
    }
}

#[cfg(feature = "gpu-encode")]
pub use inner::*;
```

**Step 2: Write the failing tests**

```rust
#[cfg(test)]
#[cfg(feature = "gpu-encode")]
mod tests {
    use super::inner::*;
    use tokio::sync::mpsc;

    #[test]
    fn test_max_batch_constant() {
        assert_eq!(MAX_BATCH, 4);
    }

    #[test]
    fn test_channel_capacity_constant() {
        assert_eq!(CHANNEL_CAPACITY, 32);
    }

    #[test]
    fn test_gpu_encoder_channel_connected() {
        let (tx, _rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        assert!(channel.is_connected());
    }

    #[test]
    fn test_gpu_encoder_channel_disconnected_when_rx_dropped() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        drop(rx);
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_gpu_encode_request_roundtrip() {
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(1);
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

        let image = image::RgbaImage::new(4, 4);
        let req = GpuEncodeRequest {
            mipmaps: vec![image],
            format: crate::dds::types::DdsFormat::BC1,
            response: resp_tx,
        };

        tx.send(req).await.unwrap();
        let received = rx.recv().await.unwrap();

        assert_eq!(received.mipmaps.len(), 1);
        assert_eq!(received.mipmaps[0].width(), 4);

        // Send a mock response back
        received.response.send(Ok(vec![1, 2, 3])).unwrap();
        let result = resp_rx.await.unwrap();
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }
}
```

**Step 3: Add module to `dds/mod.rs`**

Add `pub mod gpu_channel;` to the module declarations (alongside existing `pub mod compressor;`).

**Step 4: Run tests to verify they pass**

Run: `cargo test --features gpu-encode dds::gpu_channel -v`
Expected: All 5 tests PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/dds/gpu_channel.rs xearthlayer/src/dds/mod.rs
git commit -m "feat(dds): define GpuEncoderChannel and GpuEncodeRequest types (#67)"
```

---

## Task 2: Implement `BlockCompressor` for `GpuEncoderChannel` (TDD)

**Files:**
- Modify: `xearthlayer/src/dds/gpu_channel.rs`

The `BlockCompressor::compress()` signature is `fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>`. It's synchronous (called from `spawn_blocking`). The implementation must:

1. Generate the mipmap chain (CPU work, already on blocking thread)
2. Send `GpuEncodeRequest` to the channel
3. Block on the oneshot response

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn test_block_compressor_sends_request() {
    use crate::dds::compressor::BlockCompressor;

    let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
    let channel = GpuEncoderChannel::new(tx);

    let image = image::RgbaImage::new(4, 4);

    // Spawn a mock worker that receives and responds
    let worker = tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        assert_eq!(req.format, crate::dds::types::DdsFormat::BC1);
        assert!(!req.mipmaps.is_empty());
        req.response.send(Ok(vec![0xDD, 0x55])).unwrap();
    });

    // Call compress from a blocking context (as it would be in production)
    let result = tokio::task::spawn_blocking(move || {
        channel.compress(&image, crate::dds::types::DdsFormat::BC1)
    })
    .await
    .unwrap();

    assert_eq!(result.unwrap(), vec![0xDD, 0x55]);
    worker.await.unwrap();
}

#[tokio::test]
async fn test_block_compressor_error_when_worker_dead() {
    use crate::dds::compressor::BlockCompressor;

    let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
    let channel = GpuEncoderChannel::new(tx);
    drop(rx); // Kill the worker

    let image = image::RgbaImage::new(4, 4);

    let result = tokio::task::spawn_blocking(move || {
        channel.compress(&image, crate::dds::types::DdsFormat::BC1)
    })
    .await
    .unwrap();

    assert!(result.is_err());
    assert!(
        format!("{:?}", result.unwrap_err()).contains("GPU worker"),
        "Error should mention GPU worker"
    );
}

#[test]
fn test_block_compressor_name() {
    use crate::dds::compressor::BlockCompressor;

    let (tx, _rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
    let channel = GpuEncoderChannel::new(tx);
    assert_eq!(channel.name(), "gpu-channel");
}
```

**Step 2: Run tests to verify they FAIL**

Run: `cargo test --features gpu-encode dds::gpu_channel::tests::test_block_compressor -v`
Expected: FAIL — `BlockCompressor` not implemented for `GpuEncoderChannel`

**Step 3: Implement `BlockCompressor` for `GpuEncoderChannel`**

```rust
use crate::dds::compressor::BlockCompressor;
use crate::dds::mipmap::MipmapGenerator;

impl BlockCompressor for GpuEncoderChannel {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        // Step 1: Generate mipmaps (CPU work — we're already in spawn_blocking)
        let mipmaps = MipmapGenerator::generate_chain_with_count(image, 5);

        // Step 2: Create oneshot channel for response
        let (resp_tx, resp_rx) = oneshot::channel();

        let request = GpuEncodeRequest {
            mipmaps,
            format,
            response: resp_tx,
        };

        // Step 3: Send to GPU worker (blocking send from sync context)
        self.sender.blocking_send(request).map_err(|_| {
            DdsError::CompressionFailed("GPU worker is not running".to_string())
        })?;

        // Step 4: Block on response
        resp_rx.blocking_recv().map_err(|_| {
            DdsError::CompressionFailed("GPU worker dropped response channel".to_string())
        })?
    }

    fn name(&self) -> &str {
        "gpu-channel"
    }
}
```

**Important design note:** `BlockCompressor::compress()` receives a single mip-level image and returns compressed bytes for that level. However, `GpuEncoderChannel` needs ALL mip levels to batch them. This means `GpuEncoderChannel` should NOT implement `BlockCompressor` directly — instead it needs to be a replacement at the `DdsEncoder` level (or use a different trait).

**Alternative:** `GpuEncoderChannel` implements a new method `encode_tile(&self, image: &RgbaImage, format: DdsFormat, mipmap_count: usize) -> Result<Vec<u8>, DdsError>` that returns complete DDS bytes (header + all mips). Then implement `TextureEncoder` (from `texture/mod.rs`) instead of `BlockCompressor`.

**Step 3 (revised): Check `TextureEncoder` trait**

Read `xearthlayer/src/texture/mod.rs` to find the right trait level for integration.

**Step 4: Run tests to verify they PASS**

Run: `cargo test --features gpu-encode dds::gpu_channel -v`
Expected: All tests PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/dds/gpu_channel.rs
git commit -m "feat(dds): implement BlockCompressor for GpuEncoderChannel (#67)"
```

---

## Task 3: Implement the GPU Worker Task (TDD)

**Files:**
- Modify: `xearthlayer/src/dds/gpu_channel.rs`

The worker is an async task that:
1. Receives requests from the channel
2. Drains additional requests (up to MAX_BATCH)
3. Processes the entire batch in one GPU pass
4. Sends results back via oneshot channels

**Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn test_worker_processes_single_request() {
    // Create channel pair
    // Spawn worker with mock GPU (software compressor fallback)
    // Send one request with a 4×4 image
    // Assert response contains valid compressed data
}

#[tokio::test]
async fn test_worker_batches_multiple_requests() {
    // Send 3 requests rapidly before worker drains
    // Verify all 3 get responses
    // Verify worker processed them in a batch (via metrics/logging)
}

#[tokio::test]
async fn test_worker_stops_when_channel_closed() {
    // Drop the sender
    // Assert worker task completes (JoinHandle resolves)
}

#[tokio::test]
async fn test_worker_respects_max_batch() {
    // Send MAX_BATCH + 2 requests
    // Verify all get responses (worker processes in multiple batches)
}

#[tokio::test]
async fn test_worker_handles_mixed_formats() {
    // Send BC1 and BC3 requests in same batch
    // Verify each gets correct output size
}
```

**Step 2: Run tests to verify they FAIL**

Expected: FAIL — worker function doesn't exist yet

**Step 3: Implement the worker function**

```rust
/// Spawn the GPU worker task.
///
/// Returns a `JoinHandle` that resolves when the channel is closed.
/// The worker owns the GPU device/queue and `GpuBlockCompressor`.
pub fn spawn_gpu_worker(
    device: wgpu::Device,
    queue: wgpu::Queue,
    mut compressor: GpuBlockCompressor,
    mut rx: mpsc::Receiver<GpuEncodeRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];

            // Drain up to MAX_BATCH-1 more without blocking
            while batch.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(req) => batch.push(req),
                    Err(_) => break,
                }
            }

            tracing::debug!(
                batch_size = batch.len(),
                "GPU worker processing batch"
            );

            process_batch(&device, &queue, &mut compressor, batch);
        }

        tracing::info!("GPU worker shutting down (channel closed)");
    })
}
```

**Step 4: Implement `process_batch`**

This is the core function that:
- For each request in the batch, for each mip level:
  - Creates GPU texture, uploads pixel data, creates output/readback buffers
  - Calls `compressor.add_compression_task()`
- Creates ONE command encoder with ONE compute pass
- Calls `compressor.compress(&mut pass)` — dispatches all tasks
- Copies all outputs to readback buffers
- Submits, polls, reads back
- Assembles DDS bytes (header + compressed mips) per request
- Sends results via oneshot channels

```rust
fn process_batch(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    compressor: &mut GpuBlockCompressor,
    batch: Vec<GpuEncodeRequest>,
) {
    // ... implementation that batches all mip levels from all requests
    // into a single GPU compute pass
}
```

**Step 5: Run tests to verify they PASS**

Run: `cargo test --features gpu-encode dds::gpu_channel -v`
Expected: All tests PASS

**Note:** Tests that need actual GPU hardware should be `#[ignore]` by default and only run
with `cargo test --features gpu-encode -- --ignored`. Unit tests should use `SoftwareCompressor`
or mock the GPU operations.

**Step 6: Commit**

```bash
git add xearthlayer/src/dds/gpu_channel.rs
git commit -m "feat(dds): implement GPU worker with drain-and-batch pattern (#67)"
```

---

## Task 4: Extract GPU Resource Creation from `WgpuCompressor` (Refactor)

**Files:**
- Modify: `xearthlayer/src/dds/compressor.rs`

**Step 1: Write a test for the extracted function**

```rust
#[test]
#[cfg(feature = "gpu-encode")]
fn test_create_gpu_resources_returns_device_and_queue() {
    let result = create_gpu_resources("integrated");
    // May fail on CI without GPU — use #[ignore] if needed
    if let Ok((device, queue, compressor, name)) = result {
        assert!(!name.is_empty());
    }
}
```

**Step 2: Extract `create_gpu_resources()` from `WgpuCompressor::try_new()`**

```rust
/// Create GPU device, queue, and block compressor for a selected adapter.
///
/// Shared by `WgpuCompressor` (direct) and `GpuEncoderChannel` (worker).
pub fn create_gpu_resources(
    gpu_device: &str,
) -> Result<(wgpu::Device, wgpu::Queue, GpuBlockCompressor, String), DdsError> {
    // ... extracted from WgpuCompressor::try_new()
}
```

Update `WgpuCompressor::try_new()` to call `create_gpu_resources()`.

**Step 3: Run ALL existing tests to verify no regressions**

Run: `cargo test --features gpu-encode dds -v`
Expected: All existing tests still pass

**Step 4: Commit**

```bash
git add xearthlayer/src/dds/compressor.rs
git commit -m "refactor(dds): extract create_gpu_resources() for shared GPU init (#67)"
```

---

## Task 5: Wire `GpuEncoderChannel` into Service Builder (TDD)

**Files:**
- Modify: `xearthlayer/src/service/builder.rs`
- Modify: `xearthlayer/src/dds/mod.rs` (re-exports)

**Step 1: Write the failing test**

```rust
#[test]
#[cfg(feature = "gpu-encode")]
fn test_create_encoder_gpu_returns_channel_compressor() {
    // Create a ServiceConfig with texture.compressor = "gpu"
    // Call create_encoder()
    // Verify it returns Ok (or appropriate error on CI without GPU)
}
```

**Step 2: Update `create_encoder()` in `service/builder.rs`**

Replace:
```rust
"gpu" => {
    use crate::dds::create_wgpu_compressor;
    let gpu_compressor = create_wgpu_compressor(config.texture().gpu_device())...;
    Arc::new(gpu_compressor)
}
```

With:
```rust
"gpu" => {
    use crate::dds::gpu_channel::{
        create_gpu_encoder_channel, CHANNEL_CAPACITY,
    };
    let (channel, worker_handle) = create_gpu_encoder_channel(
        config.texture().gpu_device(),
    )?;
    // Worker handle is dropped (task runs independently until channel closes)
    Arc::new(channel) as Arc<dyn BlockCompressor>
}
```

**Step 3: Run full test suite**

Run: `make pre-commit`
Expected: All tests pass, clippy clean

**Step 4: Commit**

```bash
git add xearthlayer/src/service/builder.rs xearthlayer/src/dds/mod.rs
git commit -m "feat(dds): wire GpuEncoderChannel into service builder (#67)"
```

---

## Task 6: Integration Test with Full Pipeline (TDD)

**Files:**
- Modify: `xearthlayer/src/dds/gpu_channel.rs`

**Step 1: Write integration test**

```rust
/// End-to-end test: submit a 4096×4096 image through the channel pipeline.
/// Uses software compression for CI compatibility.
#[tokio::test]
#[ignore] // Requires GPU hardware
async fn test_full_pipeline_4096x4096() {
    let (channel, _handle) = create_gpu_encoder_channel("integrated").unwrap();
    let image = image::RgbaImage::new(4096, 4096);

    let result = tokio::task::spawn_blocking(move || {
        use crate::dds::compressor::BlockCompressor;
        channel.compress(&image, DdsFormat::BC1)
    })
    .await
    .unwrap();

    let dds = result.unwrap();
    // Verify DDS magic
    assert_eq!(&dds[0..4], b"DDS ");
    // Verify reasonable size (header + 5 mip levels of BC1)
    assert!(dds.len() > 128);
}

/// Test concurrent submissions through the channel.
#[tokio::test]
#[ignore] // Requires GPU hardware
async fn test_concurrent_submissions() {
    let (channel, _handle) = create_gpu_encoder_channel("integrated").unwrap();
    let channel = Arc::new(channel);

    let mut handles = vec![];
    for _ in 0..8 {
        let ch = Arc::clone(&channel);
        handles.push(tokio::task::spawn_blocking(move || {
            use crate::dds::compressor::BlockCompressor;
            let image = image::RgbaImage::new(256, 256);
            ch.compress(&image, DdsFormat::BC1)
        }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}
```

**Step 2: Run integration tests**

Run: `cargo test --features gpu-encode dds::gpu_channel -- --ignored`
Expected: PASS on systems with GPU

**Step 3: Commit**

```bash
git add xearthlayer/src/dds/gpu_channel.rs
git commit -m "test(dds): add GPU channel integration tests (#67)"
```

---

## Task 7: Update Documentation

**Files:**
- Modify: `docs/configuration.md` (if any new config keys)
- Modify: `CLAUDE.md` (update DDS Compression section)

**Step 1: Update CLAUDE.md DDS section**

Add mention of channel-based GPU encoding architecture.

**Step 2: Commit**

```bash
git add docs/ CLAUDE.md
git commit -m "docs(dds): document GPU channel architecture (#67)"
```

---

## Execution Order

```
Task 1: Types + module ──► Task 2: BlockCompressor impl ──► Task 3: Worker ──► Task 4: Refactor
                                                                                      │
                                                                    Task 5: Wire builder ◄─┘
                                                                         │
                                                                    Task 6: Integration
                                                                         │
                                                                    Task 7: Docs
```

Tasks are sequential — each builds on the previous.

## Verification

1. `make pre-commit` — fmt + clippy + all tests pass
2. `cargo test --features gpu-encode dds::gpu_channel` — all channel tests pass
3. `cargo test --features gpu-encode dds::gpu_channel -- --ignored` — integration tests on GPU hardware
4. **Manual test**: `xearthlayer run` with `texture.compressor = gpu`, verify no freezes at DSF boundaries
5. **Verify batching**: Log analysis showing `batch_size > 1` during boundary crossings

## Design Note: BlockCompressor vs TextureEncoder Integration

The `BlockCompressor::compress()` trait operates on a **single image** and returns compressed bytes for that image. It's called once per mip level by `DdsEncoder::encode_with_mipmaps()`. This means `GpuEncoderChannel` as a `BlockCompressor` would still result in 6 separate GPU submissions per tile (one per mip level).

**To achieve true batching across mip levels**, `GpuEncoderChannel` should intercept at the `TextureEncoder` level instead — receiving the full image and producing complete DDS output (header + all mips). The implementation in Task 2 should evaluate both approaches during the red-green cycle:

- **Option A**: Implement `BlockCompressor` — simpler integration, batches across tiles but NOT within a tile's mip chain
- **Option B**: Implement `TextureEncoder` — full batching (all mips from all tiles), requires the channel to produce complete DDS output including header

Option B is preferred for maximum GPU efficiency. The tests in Task 2 should be written against whichever trait provides the better batching semantics.
