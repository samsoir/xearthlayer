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
compressor = ispc           # software | ispc | gpu
gpu_device = integrated     # integrated | discrete | <name substring>
```

- `compressor` (default: `ispc`) -- Selects the compression backend
- `gpu_device` (default: `integrated`) -- GPU adapter selection via `wgpu::AdapterInfo::device_type`:
  - `integrated` -> `DeviceType::IntegratedGpu`
  - `discrete` -> `DeviceType::DiscreteGpu`
  - Any other string -> case-insensitive substring match against `AdapterInfo::name`

### Error Behaviour

All GPU configuration errors are fatal at startup (exit non-zero with actionable message):
1. `gpu_device` matches no adapter (lists all available adapters)
2. No Vulkan/Metal/DX12 runtime available

A GPU that fails once XEarthLayer is running is not fatal: every tile from then
on is encoded on the CPU with ISPC, for the rest of the session (#420). See
[Failure and Fallback](#failure-and-fallback-420).

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
    ) -> Result<ChainOutcome, DdsError>;
}

/// What a MipmapCompressor did with an image (#420)
pub enum ChainOutcome {
    Compressed(Vec<u8>),                          // every level, largest first
    Returned { image: RgbaImage, reason: String }, // not taken: encode it another way
}
```

- `SoftwareCompressor` and `IspcCompressor` implement `ImageCompressor`
- `GpuEncoderChannel` implements `MipmapCompressor`. When it hands an image back
  (`ChainOutcome::Returned`), `DdsEncoder` encodes it with its fallback
  `ImageCompressor`, which the service names explicitly: `IspcCompressor`. The
  bytes are then exactly those `texture.compressor = ispc` writes.

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
demand lane   BuildAndCacheDds ──► demand channel (32)          ──┐
                                                                  ├─► GPU worker (thread xel-gpu-encoder)
prefetch lane BuildAndCacheDds ──► prefetch channel (CPU pool)  ──┘   demand first
```

**Two lanes, one GPU (#421).** Each executor lane has its own encoder and
channel (`GpuEncoderChannels`, `gpu_lanes`), so nothing on the demand side can
name the prefetch channel. The worker takes from the demand channel whenever it
is free and from the prefetch channel only when the demand channel is empty
(`next_take`, a pure function of the two queues' states). The tile in progress
is never preempted, so a demand tile waits behind at most one prefetch tile.
The queue-age valve and the hand-back behind a late tile apply to the demand
lane only (`GpuLane::has_deadline`); hand-back after a GPU failure and the drain
cover both. See the adaptive prefetch design, "What the lanes share".

**Awaited, with the job's cancellation (#422).** `BuildAndCacheDdsTask` awaits
the GPU reply (`MipmapCompressor::submit_mipmap_chain`) instead of parking a
blocking thread in `blocking_send`/`blocking_recv`, so a tile waiting for the
GPU holds its CPU permit but no blocking-pool thread. Each request carries its
job's cancellation token. Worker and caller race for a request through a claim
(`RequestClaim`, decided by `admit`): a tile not yet started when its job is
cancelled is skipped by the worker and its caller returns `Cancelled` at once;
a started tile is finished and cached, as #385 keeps complete tiles (never under
executor shutdown). An image handed back to the CPU is encoded with ISPC as
blocking work under the task's CPU permit; one handed back to a cancelled job is
not encoded and counts as cancelled.

**Kept resources, output in place, no idle poll (#423).** The worker keeps its
GPU resources (textures, output and readback buffers) across tiles, keyed by size
and format, and discards them after any failed tile (`AfterTile`), so a late or
failed tile never poisons the next ones: a readback buffer left mid-map by a
timed-out poll made every later submit fail before this. `DdsEncoder` allocates
the DDS file once at its exact size and moves it through the request
(`GpuEncodeRequest::output`); the worker writes each level into its slice, so an
encoded tile is one allocation with no copy behind the header, as on the CPU path
since #393. When both channels are empty the worker blocks until a request, a
shutdown or a GPU failure (`SharedGpuHealth::failed()`) wakes it: there is no
poll interval. It runs on its own named thread (`WORKER_THREAD`,
`xel-gpu-encoder`) behind a `GpuWorkerHandle`, which the service stops at
shutdown; a stop ends the worker after the tile in progress.

**Message type:**

```rust
struct GpuEncodeRequest {
    image: RgbaImage,       // source image, moved (not cloned)
    format: DdsFormat,
    mipmap_count: usize,
    sent_at: Instant,       // the caller starting its send: start of the queue wait
    output: Vec<u8>,        // the DDS file at its exact size, header written, levels to fill (#423)
    // plus the lane, the job's cancellation token and the claim (#421, #422),
    // and the oneshot the GpuEncodeReply (result and GpuTileTiming) comes back on
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
1. Take a request, demand first (blocking while both channels are empty)
2. Admit it: skip it if its job was cancelled before it started, hand it back if it waited past the deadline (demand only)
3. Create `MipmapStream::new(image, mipmap_count)` -- worker owns the iteration
4. For each level: upload -> dispatch compute -> readback into the level's slice of the output file
5. Return the file on the oneshot

The CPU downsample of level N+1 does not overlap the GPU work of level N today: `MipmapStream` computes the next level before it returns the current one, and each level is a synchronous round trip. Taking that work off the worker thread is #424.

**GPU resource reuse:** the worker's textures and output and readback buffers are kept across tiles of the same size and format and dropped after a failed tile (#423); within a tile, one set serves every level.

### Pipeline Overlap (Approach 3)

The worker uses an adaptive overlap strategy for inter-tile pipelining:
- When more requests are queued: defer readback to overlap with the next upload (pipeline depth 2)
- When queue is empty: complete immediately to avoid deadlock on single/last request

VRAM budget: pipeline depth 2 with 4096x4096 tiles ~ 160MB peak.

### Shared Resource Factory

`create_gpu_resources()` is a shared factory for `wgpu::Device`, `wgpu::Queue`, and `GpuBlockCompressor` creation, used by both the worker and diagnostics.

### Hardening

- `map_async` errors propagated via `std::sync::mpsc` (not silently ignored)
- Worker panic recovery via `catch_unwind`: the tile in progress fails, the
  GPU turns Failed, and every queued image is handed back (see below)
- `device.set_device_lost_callback()` registered for device loss. wgpu never
  reports device loss to `on_uncaptured_error` (it returns early for
  `DeviceLost`), so that handler only logs validation and out-of-memory errors
- Structured tracing on buffer lifecycle and error paths

### Failure and Fallback (#420)

`SharedGpuHealth` (`dds/gpu_health.rs`) starts Healthy and turns Failed once,
for good, keeping the first reason. There is no respawn: once failed, the GPU
stays off for the session. It turns Failed on:

- **A panic** the worker loop caught.
- **A lost device**, reported by the device-lost callback with
  `DeviceLostReason::Unknown` (a driver reset or fault). `Destroyed` follows
  only an explicit `Device::destroy` and does not fail the health by itself.
- **Three misses in a row** (`HangRule`: `TILE_DEADLINE` 2 s,
  `MISSES_BEFORE_FAILED` 3). A miss is a tile that takes longer than the
  deadline from when the worker takes it, or a tile that fails before it. A
  tile that succeeds in time resets the count. Failed tiles count because a
  destroyed Vulkan device under wgpu 30 makes every later tile fail at once,
  with no panic and no stall; counting only slow tiles left such a GPU
  "healthy" with every tile failing. One failure alone changes nothing.
  Three out-of-memory failures in a row (a host short of VRAM) also turn it
  Failed, and ISPC is then the better encoder there anyway.
- **A worker gone without a shutdown:** a send the channel refuses while the
  worker's shutdown token is not cancelled.

The first failure logs one WARN, `GPU encoder failed, encoding on CPU (ISPC)
for the rest of the session`, with the reason, and the dashboard's Status
pane shows `GPU failed, encoding on CPU (ISPC): <reason>` (the fallback before
the reason, so a narrow pane cuts only the reason).

From then on the encoder checks the health before each tile and encodes on
ISPC without sending. Images already queued are **handed back** to their
callers (`ChainOutcome::Returned`) and encoded on ISPC, not failed: a
GPU failure costs at most the tile in progress, and tiles keep arriving.

**The queue-age safety valve.** When the worker takes a request that has
already waited longer than the deadline in the queue (from the caller starting
its send), it hands the image back instead of processing it
(`HangRule::waited_out`). It is not a miss and never fails the GPU. A slow but
never-late GPU with a full queue would otherwise make the last caller wait
the queue's depth times the tile time, past the FUSE timeout; with the valve
no caller waits much longer than the deadline plus one tile. This is the one
place the threshold lives, for #464 to refine.

**Draining.** At shutdown and after a failure the worker closes its queue,
then reads it with `blocking_recv` until no sender holds a slot (`drain()`,
#464; reported and first fixed by @mmaechtel in #459). `try_recv` missed a
request whose sender had reserved its slot just before the close, and that
caller waited forever with a blocking thread and a CPU permit. Every request
is answered exactly once: processed, handed back, or, at shutdown, failed
with `GPU worker shut down`.

**A hung GPU is leaked, not dropped.** wgpu-core's `Queue::drop` waits for the
device to go idle with no timeout, and panics outside `catch_unwind` on an
error other than device loss. So when the GPU fails by a hang or a panic, the
worker leaks its processor (`std::mem::forget`) instead of dropping it, and
XEarthLayer can still exit. A clean shutdown, a lost device and tiles that
failed in time drop it normally.

### Stage Timing and Queue Depth (#419)

Each tile's `GpuTileTiming` comes back in its `GpuEncodeReply`:

| Stage | What it covers |
|---|---|
| `queue_wait` | From the caller starting its send, time blocked on a full channel included, to the worker taking the request. An idle worker blocks on its channels and wakes for a request (#423): there is no poll interval |
| `downsample` | CPU mip generation (`MipmapStream`, which works one level ahead) and the padding of the sub-block tail |
| `upload` | The tile's output and readback buffers, each level's texture and `queue.write_texture` |
| `gpu` | Command recording, the compute dispatch, submit, `map_async` and `poll(Wait)`: wall time, driver included (no timestamp queries) |
| `readback` | Receiving the map, copying out of the mapped range and growing the result `Vec` |
| `process` | The processor's whole call, on the worker |
| total | On the caller, from its send to the reply |

They appear in three places:

- **The profile trace** (`run --profile`): spans `gpu_encode` (caller), `gpu_tile` (worker), and per level `gpu_downsample` (`computes_level`), `gpu_buffers`, `gpu_upload`, `gpu_submit_poll` and `gpu_readback`, plus `dds_header_assembly` in `DdsEncoder`.
- **The debug log**: one `GPU encode: tile stages` line per tile. Since #423 the worker writes the levels into the DDS file in place, so there is no header assembly copy.
- **A `GPU encode sample` line every 60 s** at info, only when the window saw a tile or a failure. It gives p50, p95 and max per stage, tiles, `failed` (every request that came back without a tile: worker errors, a worker not running, a dropped reply), `fallback` (tiles handed back and encoded on ISPC: after a failure, or by the queue-age valve), `health` (`healthy` or `failed`) and the queue depth, now and the window's maximum. The service builder starts it, so a channel built in a test has no logger taking its samples. A window keeps at most 10,000 tiles (`WINDOW_CAP`) and counts the rest as `unsampled`.

Each lane has its own `GpuEncodeStats` and its own `GPU encode sample` line, with a `lane` field (`demand` or `prefetch`) and a `cancelled` count (#421, #422). The queue depth is raised before the send and lowered when the reply, the failure or the caller's cancel is back, by a guard that drops on every path. It counts callers, including those waiting on a full channel, which the channel's own capacity does not show; a caller that leaves on a cancel stops counting while its request may still be buffered.

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
- **A lone tile is only a little faster than ISPC on one thread:** about 25 ms against 32 ms. Of that, 5 to 6 ms was the idle worker's 10 ms poll (`try_recv` and sleep), removed by #423: an idle worker now blocks and wakes for a request.
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
- `"gpu"` -> `GpuEncoderChannel` via `create_gpu_resources()`; hard error on failure. The builder also starts the channel's 60 s `GPU encode sample` logger, which ends with the service's shutdown token or the channel

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
