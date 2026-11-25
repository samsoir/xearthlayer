# GPU-Accelerated DDS Encoding

This document describes XEarthLayer's GPU encoding pipeline for offloading BC1 DDS texture compression to a GPU via wgpu compute shaders.

## Motivation

CPU saturation (100% across all logical cores) at DSF boundaries during DDS texture encoding causes micro-stutters in X-Plane. The ISPC SIMD encoder improved throughput ~10x over the pure-Rust encoder, but CPU contention between XEarthLayer and X-Plane remains the bottleneck at DSF boundaries.

Many systems have an idle integrated GPU (e.g., AMD Radeon on Ryzen processors) that can handle DDS encoding, freeing CPU cores entirely. The primary benefit is **CPU freedom** -- encoding no longer competes with X-Plane for CPU time.

## Feature Gate

> **Note:** The `gpu-encode` feature flag was removed in v0.4.2. GPU encoding is now compiled unconditionally into every XEarthLayer binary. Select it at runtime via `texture.compressor = gpu` — no special build flags required.

The `block_compression` crate provides ISPC compression kernels ported to WGSL compute shaders, tested on Vulkan/Metal/DX12.

## Configuration

Two keys in the `[texture]` section:

```ini
[texture]
compressor = ispc           # software | ispc | gpu | hybrid
gpu_device = integrated     # integrated | discrete | <name substring>
```

- `compressor` (default: `ispc`) -- Selects the compression backend. `hybrid` is `gpu` with overflow to ISPC (see [Overflow to ISPC](#overflow-to-ispc-hybrid))
- `gpu_device` (default: `integrated`) -- GPU adapter selection via `wgpu::AdapterInfo::device_type`:
  - `integrated` -> `DeviceType::IntegratedGpu`
  - `discrete` -> `DeviceType::DiscreteGpu`
  - Any other string -> case-insensitive substring match against `AdapterInfo::name`

### Error Behaviour

All GPU configuration errors are fatal at startup (exit non-zero with actionable message):
1. `gpu_device` matches no adapter (lists all available adapters)
2. No Vulkan/Metal/DX12 runtime available

## Architecture

### Trait Hierarchy

The encoding pipeline uses two trait layers, reflecting different execution contracts:

```rust
/// Single-level compression (CPU backends)
pub trait ImageCompressor: Send + Sync {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;
}

/// Full-pipeline compression (GPU backend)
pub trait MipmapCompressor: Send + Sync {
    fn compress_mipmap_chain(
        &self,
        image: RgbaImage,
        format: DdsFormat,
        mipmap_count: usize,
    ) -> Result<Vec<u8>, DdsError>;
}
```

- `SoftwareCompressor` and `IspcCompressor` implement `ImageCompressor`
- `GpuEncoderChannel` implements `MipmapCompressor`

The `format` parameter is the extension point for future formats. Only BC1 is implemented: every backend rejects any other format with `DdsError::UnsupportedFormat`, and `GpuEncoderChannel` does so before the request is sent to the worker, so the worker maps BC1 only. See [Supported formats](dds-implementation.md#supported-formats).

`DdsEncoder` dispatches via a `CompressorBackend` enum:

```rust
pub enum CompressorBackend {
    Image(Arc<dyn ImageCompressor>),
    Mipmap(Arc<dyn MipmapCompressor>),
}
```

- `Image`: Fused `MipmapStream` loop -- iterate levels, compress each, drop immediately
- `Mipmap`: Pass owned source image to GPU worker -- one channel round-trip per tile

### Channel-Based GPU Worker

Rather than wrapping the GPU compressor in a `Mutex` (which serializes all GPU work), a dedicated worker thread receives requests via an `mpsc` channel:

```
BuildAndCacheDds tasks ──► bounded mpsc channel (32) ──► GPU Worker (single thread)
```

**Message type:**

```rust
struct GpuEncodeRequest {
    image: RgbaImage,       // source image, moved (not cloned)
    format: DdsFormat,
    mipmap_count: usize,
    sent_at: Instant,       // the caller starting its send: start of the queue wait
    response: oneshot::Sender<GpuEncodeReply>, // the result and the tile's GpuTileTiming
}
```

**The loop and the tile work are apart (#419).** `spawn_tile_worker` runs the
loop (receive, shutdown, `catch_unwind`, drain, reply, timing) around any
`TileProcessor`, whose one method encodes one tile and fills the tile's
stage times. `WgpuTileProcessor` (device, queue, `GpuBlockCompressor`) is the
production processor, and `spawn_gpu_worker` starts the loop with it. Tests
drive the real loop with processors that panic, sleep, fail or record their
order, so the loop's failure handling, shutdown and drain are tested without
a GPU.

**Worker loop:**
1. Receive `GpuEncodeRequest` with owned source image
2. Create `MipmapStream::new(image, mipmap_count)` -- worker owns the iteration
3. For each level: upload -> dispatch compute -> readback -> append to output
4. While GPU compresses level N, CPU downsamples to produce level N+1 (natural pipeline overlap)
5. Return concatenated compressed bytes on the oneshot

**GPU resource reuse:** Output and readback buffers are pre-allocated for level 0 size and reused across all mipmap levels within a tile. Input textures are created per level due to wgpu's immutable texture sizing.

### Pipeline Overlap (Approach 3)

The worker uses an adaptive overlap strategy for inter-tile pipelining:
- When more requests are queued: defer readback to overlap with the next upload (pipeline depth 2)
- When queue is empty: complete immediately to avoid deadlock on single/last request

VRAM budget: pipeline depth 2 with 4096x4096 tiles ~ 160MB peak.

### Shared Resource Factory

`create_gpu_resources()` is a shared factory for `wgpu::Device`, `wgpu::Queue`, and `GpuBlockCompressor` creation, used by both the worker and diagnostics.

### Hardening

- `map_async` errors propagated via `std::sync::mpsc` (not silently ignored)
- Worker panic recovery via `catch_unwind` (logs error, sends failure, drains queue, breaks loop)
- `device.on_uncaptured_error()` registered for device loss logging
- Structured tracing on buffer lifecycle and error paths

### Overflow to ISPC (`hybrid`)

The single worker sets the GPU path's throughput (see [Performance](#performance)): under a burst a `gpu` tile spends almost all its time in the queue. With `texture.compressor = hybrid` no tile waits for room in the queue. What a tile does when the queue is full is `WhenBusy` (`Wait` for `gpu`, `EncodeWithIspc` for `hybrid`), passed to `create_gpu_encoder_channel`, which builds handle and channel together with `GpuEncoderChannel::with_policy`:

- `with_policy` sizes the channel with the policy, so a handle never overflows from a deep queue: `CHANNEL_CAPACITY` (32) to wait in, 1 to overflow from, so one tile waits besides the one the worker encodes. A deeper queue only adds queue wait, as ISPC on one thread is about as fast as the worker.
- With `EncodeWithIspc`, `compress_mipmap_chain` submits with `try_send`. On `Full`, or `Closed` (the worker stopped), it takes its request back and encodes the tile with ISPC on the calling thread, through `encoder::compress_chain`, which runs the same level loop (`compress_chain_into`) `DdsEncoder` runs for `ispc`: the bytes are those `ispc` writes behind its header, which a test checks against `DdsEncoder` itself. A tile the worker accepts is waited for as with `gpu`. ISPC and the GPU kernel give different bytes for the same tile; both are valid BC1, and nothing depends on which one a tile got.
- The calling thread is a tile task's, which holds a CPU-pool permit, so the CPU pool stays the only limit on concurrent encodes: no second pool, no extra threads.
- An overflowed tile is counted with `GpuEncodeStats::record_overflowed` when it encodes (as `failed` when it does not) and appears as `ispc_overflow` in the `GPU encode sample` line, apart from the worker's `tiles`. It never enters the queue depth: with `EncodeWithIspc` a request is counted from its acceptance, with `Wait` from before its send, which blocks on a full channel. Counting after `try_send` leaves a short window in which the worker may already hold requests not yet counted (one per sender caught between `try_send` and the count), so `depth_max` can read low; it is a log figure, and counting before the send would count every overflowed tile instead.
- A worker that panics closes its queue before draining it, so a tile sent meanwhile overflows rather than failing with the drain. It drains with `blocking_recv`, not `try_recv`: `try_send` takes its slot before it sends, and `recv` returns `None` only once no sender holds a slot, so a request sent just after the close is still answered instead of waiting forever. A shutdown drains the same way, dropping the requests.
- The dashboard's Status pane names hybrid as `GPU ... + ISPC` on the Tile Processing Engine line.

`gpu` is unchanged: `CHANNEL_CAPACITY` (32), `blocking_send`, and an error when the worker has stopped.

### Stage Timing and Queue Depth (#419)

Each tile's `GpuTileTiming` comes back in its `GpuEncodeReply`:

| Stage | What it covers |
|---|---|
| `queue_wait` | From the caller starting its send, time blocked on a full channel included, to the worker taking the request. A lone tile waits mostly for the idle worker's poll, up to 10 ms |
| `downsample` | CPU mip generation (`MipmapStream`, which works one level ahead) and the padding of the sub-block tail |
| `upload` | The tile's output and readback buffers, each level's texture and `queue.write_texture` |
| `gpu` | Command recording, the compute dispatch, submit, `map_async` and `poll(Wait)`: wall time, driver included (no timestamp queries) |
| `readback` | Receiving the map, copying out of the mapped range and growing the result `Vec` |
| `process` | The processor's whole call, on the worker |
| total | On the caller, from its send to the reply |

They appear in three places:

- **The profile trace** (`run --profile`): spans `gpu_encode` (caller), `gpu_tile` (worker), and per level `gpu_downsample` (`computes_level`), `gpu_buffers`, `gpu_upload`, `gpu_submit_poll` and `gpu_readback`, plus `dds_header_assembly` in `DdsEncoder`.
- **The debug log**: one `GPU encode: tile stages` line per tile. Header assembly, which copies the payload behind the header, is in the encoder's own debug line (`assemble_us`), not in this one or the sample: removing that copy belongs with #423.
- **A `GPU encode sample` line every 60 s** at info, only when the window saw a tile, an overflowed tile or a failure. It gives p50, p95 and max per stage, tiles, `ispc_overflow` (tiles `hybrid` encoded with ISPC instead), `failed` (every request that came back without a tile: worker errors, a worker not running, a dropped reply) and the queue depth, now and the window's maximum. The service builder starts it, so a channel built in a test has no logger taking its samples. A window keeps at most 10,000 tiles (`WINDOW_CAP`) and counts the rest as `unsampled`.

The queue depth (`GpuEncodeStats`, `dds/gpu_stats.rs`) is raised before `blocking_send` (with `hybrid`, once `try_send` succeeds) and lowered when the reply or the failure is back, by a guard that drops on every path. It counts callers blocked on a full channel, which the channel's own capacity (32 buffered requests) does not show.

## Memory Optimization

### MipmapStream Iterator

`MipmapStream` yields one mipmap level at a time, eliminating intermediate clones:

```rust
pub struct MipmapStream {
    base: RgbaImage,
    current_level: u8,
}

impl Iterator for MipmapStream {
    type Item = (u8, RgbaImage);
}
```

- Stores only base image + downsampling state
- Each `next()` yields one level with no clones
- Memory footprint: ~64 MB (base) + ~32 MB (current level) max
- Proper cleanup in `Drop` if iteration is interrupted early

### GPU Path Zero-Clone

The `MipmapCompressor` trait accepts an owned `RgbaImage`, which is moved (not cloned) into the channel request. Combined with worker-side mipmap iteration, this eliminates the 64 MB per-level clone that previously occurred at the trait boundary.

| Metric | Before | After |
|--------|--------|-------|
| `image.clone()` in channel send | 64 MB/level | 0 (moved) |
| Channel round-trips per tile | 5 | 1 |
| Oneshot channels per tile | 5 pairs | 1 pair |
| GPU buffers allocated per tile | 5 pairs | 1 (reused) |

### Profiling Results (PR #122)

**ISPC path (heaptrack, 2 GB memory cache):**

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| Peak heap | 8.55 GB | 6.06 GB | **-29%** |
| Peak RSS | 20.86 GB | 16.58 GB | **-21%** |
| Mipmap in peak consumers? | #1 at 3.76 GB | Gone | Eliminated |

**GPU path:**

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| Peak heap | 7.06 GB | 5.61 GB | **-21%** |
| Peak RSS | 19.75 GB | 11.15 GB | **-44%** |
| GPU clone in peak consumers? | #2 at 1.88 GB | Gone | Eliminated |
| Memory leaked | 23.14 MB | 6.71 MB | **-71%** |

## Performance

Measured on 2026-11-24 with `tests/gpu_pipeline_stress.rs` (#419), on a quiet Framework 16 laptop (Ryzen AI 9 HX 370, 24 threads, on AC power) with an integrated Radeon 890M and the laptop RTX 5070. **These are reference figures from one laptop, not representative of most systems**: a desktop GPU, CPU or driver can rank the stages and the two paths differently. Use them to compare stages and changes on the same machine, and rerun the stress test on the hardware in question. The tile is a textured 4096×4096 image with its full 13-level chain, encoded to BC1 in a release build. "Lone" is 10 tiles, one at a time, after a warm-up. "Burst" is 15 tiles submitted at once, then 1 s idle, for 40 s. Times are p50 per tile in ms.

| Stage | Radeon 890M (integrated, RADV) lone | burst | RTX 5070 Laptop (discrete) lone | burst, first bursts |
|---|---|---|---|---|
| queue wait | 5.5 | 131 | 5.6 | 175 |
| CPU downsample | 6.9 | 6.4 | 7.1 | 6.4 |
| upload (buffers, textures, `write_texture`) | 4.3 | 4.3 | 8.2 | 8.2 |
| GPU (submit to poll, 13 levels) | 5.9 | 5.8 | 4.0 | 4.5 |
| readback (map, copy, `Vec` growth) | 1.3 | 0.9 | 1.4 | 1.0 |
| worker total | 18.5 | 17.9 | 20.7 | 20.6 |
| caller total | 24.3 | 149 | 26.5 | 199 |
| throughput | | 54 tiles/s | | 48–50 tiles/s |

ISPC on the same machine and tile, through `DdsEncoder` (`ispc_reference_on_the_same_tile`): 32 ms for a lone tile on one thread, and 167 tiles/s for a burst of 15 across 24 threads.

What the numbers show:

- **The single worker sets the GPU path's throughput.** Throughput is about 1 / worker total, about 55 tiles/s at best, a third of ISPC across all cores. Under a burst, a tile's latency is almost all queue wait, growing with its place in the queue.
- **A lone tile is only a little faster than ISPC on one thread:** about 25 ms against 32 ms. Of that, 5 to 6 ms is the idle worker's 10 ms poll (`try_recv` and sleep).
- **The CPU downsample, which runs on the worker thread, is its largest stage on the integrated GPU.** Upload is not free there either (4.3 ms, shared memory); on the discrete GPU it is the largest stage (8.2 ms over PCIe).
- **On this laptop, the NVIDIA driver's power management cut the discrete GPU's rate by two thirds under bursty work.** After about 7 bursts with 1 s idle gaps, the driver moved the GPU from P0 (12,001 MHz memory, PCIe gen 4) to P5 and P8 (810 and 405 MHz memory, PCIe gen 1 to 2) at 38 °C. Upload rose to 31 ms and submit-to-poll to 21 ms, and throughput fell to 14 tiles/s, in two runs out of two. Bursts back to back stayed at about 45 tiles/s. Whether a desktop GPU or another driver does the same is not known; a GPU that renders nothing else between tiles would see this load pattern in flight.
- **Memory:** process RSS grew about 200 MB on the NVIDIA driver over a run, and between 3 and 110 MB on RADV.

The GPU figure is wall time from submit to poll, driver included: timestamp queries are not used. Header assembly in `DdsEncoder` is not in the table.

## Diagnostics

`xearthlayer diagnostics` output:

```
GPU Adapters:
  [0] NVIDIA GeForce RTX 5090 (Discrete, Vulkan)
  [1] AMD Radeon Graphics (Integrated, Vulkan)
  Active compressor: gpu (AMD Radeon Graphics)
```

## Service Wiring

`service/builder.rs::create_encoder()` reads `config.texture().compressor()`:

- `"software"` -> `SoftwareCompressor`
- `"ispc"` -> `IspcCompressor` (default)
- `"gpu"` -> `GpuEncoderChannel` via `create_gpu_resources()` with `WhenBusy::Wait`; hard error on failure.
- `"hybrid"` -> the same with `WhenBusy::EncodeWithIspc`.

For both, the builder also starts the channel's 60 s `GPU encode sample` logger, which ends with the service's shutdown token or the channel

No changes to task, executor, or job code. The compressor is injected via the existing `DdsEncoder` API.

## Key Files

| File | Purpose |
|------|---------|
| `dds/compressor.rs` | `ImageCompressor` and `MipmapCompressor` traits, `SoftwareCompressor`, `IspcCompressor`, `create_gpu_resources` |
| `dds/gpu_channel.rs` | `GpuEncoderChannel` (implements `MipmapCompressor`), the worker loop (`spawn_tile_worker`), `TileProcessor` and `WgpuTileProcessor`, `GpuTileTiming` |
| `dds/gpu_stats.rs` | `GpuEncodeStats`: stage windows, failures, queue depth, the `GPU encode sample` line |
| `tests/gpu_pipeline_stress.rs` | Ignored GPU benchmark: lone tiles and bursts on the production chain, with the ISPC reference |
| `dds/mipmap.rs` | `MipmapStream` iterator |
| `dds/encoder.rs` | `DdsEncoder`, `CompressorBackend` dispatch |
| `service/builder.rs` | Compressor construction and wiring |
| `config/keys.rs` | `TextureCompressor`, `TextureGpuDevice` config keys |

## Future Considerations

- **GPU mipmap generation**: Add a downsample compute shader if CPU mipmap becomes a bottleneck (currently ~5ms, unlikely)
- **Auto-selection**: `compressor = auto` could probe for GPU availability and fall back to ISPC
- **Mac Apple Silicon**: iGPU reports as `IntegratedGpu` but is the only GPU; works transparently via Metal backend
