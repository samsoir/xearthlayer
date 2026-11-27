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

- `Image`: Fused `MipmapStream` loop -- borrow each level, compress it, build the next over it
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

**One tile, one submission (#424).** The worker no longer builds mip levels on
the CPU or uploads them. For each tile it:
1. writes level 0 into a persistently mapped upload buffer kept with the
   tile's resources (`write_level0`: one copy of 64 MiB for a 4096 tile, no
   per-call staging), padded to whole blocks as the CPU pads it;
2. records, in one command encoder: the copy of level 0 into its texture, one
   mip pass (`dds/gpu_mips.rs`, `gpu_mips.wgsl`) that writes every lower level
   into `Rgba8Unorm` write-only storage textures, one BC1 pass
   (`block_compression::compress` once, every level at its offset in the
   payload), the copy into the readback buffer and the timer resolve;
3. submits once, polls once, maps once and copies the payload into its slice
   of the DDS file (#423's in-place output).

The mip shader is byte-identical to `downsample_box_2x` at every level,
padding included (#394's round half to even, odd sizes truncated as on the
CPU), checked on both GPUs for 4096 tiles and odd and sub-block sizes. Its
filter follows the CPU chain's (`MipmapGenerator::FILTER`, `MipFilter`), so
the two paths change together: linear light since #467, with the CPU chain's
own tables; the stored-value filter it replaced stays as a tested shader
variant nothing wires. The shader's uniform buffers are
written through the queue, never mapped at creation, so rebuilding resources
on a destroyed device fails tiles (and #420 fails the GPU) rather than
panicking.

**Two tiles in flight (#425).** The worker drives a `PipelinedProcessor`
(`begin`, `finish`, `abandon`, `DEPTH`); every older `TileProcessor` is depth
1. `WgpuTileProcessor` has two resource sets (`free_set`: the first free one,
so serial work only ever builds one). While the GPU runs tile A, the worker
begins tile B (writes its level 0, records and submits it), then finishes A.
`next_begin(depth, begun_lanes, demand, prefetch)` decides, as a pure
function: it begins another tile only if one is already queued (demand
first), never past the depth, and **never two prefetch tiles at once**, so a
demand tile still waits behind at most one prefetch tile (#421). With nothing
queued, A is finished at once: a finished tile is never held back. Prefetch-only
bursts therefore run at #424's serial rate; demand bursts gain the overlap.
Begun tiles keep their image (64 MiB each, at most two) until finished, so
#420's rules apply per tile in flight: each is judged by its own time,
from when it becomes the oldest tile in flight (`judged_from(taken,
ahead_finished)`, the later of when it was taken and when the tile ahead
finished), so the tile ahead's time never counts against it and a backlog of
in-time tiles stays in time, exactly as when tiles ran one at a time; a late tile hands back a demand tile begun behind it (and the
demand queue) but finishes a prefetch one; a GPU failure, a panic, or a device lost
from outside while tiles are begun hands back every tile not yet finishing,
with its image; a failed or abandoned tile discards only its own set
(`AfterTile`); a shutdown finishes every begun tile, then fails the queue.
`plan_tile` refuses a chain that does not fill its payload before any GPU
work.

**GPU resource reuse:** each resource set (level textures, mip storage
textures, the mapped upload buffer, output and readback buffers, the kernel
timer) is kept across tiles of the same size and format and dropped after a
failed tile (#423). For a 4096 tile a set is about 171 MiB (a 64 MiB upload
buffer, 85.3 MiB of level textures, two 10.67 MiB output and readback
buffers); both sets are about 341 MiB, kept for the session once created
(system memory on an integrated GPU), against about 107 MiB before #424. They
are deliberately not released when the worker goes idle: re-creating a set
costs about 11 ms, which cut bursts of two tiles from about 115 to about 71
tiles/s on both GPUs measured, and the memory is small beside a typical
session's.

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
| `upload` | Creating the tile's resources when needed, and writing level 0 into the mapped upload buffer (#424) |
| `gpu` | Recording, the submit and the poll for the whole tile: wall time, driver included. Under #425 the GPU work of a tile mostly runs while the worker uploads the next one, so `gpu` is near zero in bursts |
| `gpu_mips` | The mip pass alone, from GPU timestamp queries (#424). Under overlap `gpu_mips + gpu_kernel > gpu` is normal |
| `gpu_kernel` | The BC1 pass alone (one pass for every level since #424), from GPU timestamp queries (`dds/gpu_timestamps.rs`, #476). Present only where the adapter offers `TIMESTAMP_QUERY` with a usable timestamp period. The sample line gives `gpu_kernel_tiles` (how many of the window's tiles carried a kernel time) and `gpu_kernel = timestamps`, `unavailable` or `no tiles`; `unavailable` means no timestamp feature, an unusable period, the timer dropped after its buffer failed to map (warned once; it returns with the resources a failed tile rebuilds), or no usable ticks in the window |
| `readback` | Receiving the map, copying out of the mapped range and growing the result `Vec` |
| `process` | The tile's latency on the worker, from taken to finished. Under a backlog it spans about two tile periods (finishing the tile ahead, beginning the next), so it is not a per-tile cost; `upload`, `gpu_mips`, `gpu_kernel` and `readback` are. The deadline is judged by the tile's own time (see Two tiles in flight) |
| total | On the caller, from its send to the reply |

They appear in three places:

- **The profile trace** (`run --profile`): spans `gpu_encode` (caller), `gpu_tile` (worker, with `gpu_kernel_us` and `gpu_mips_us`), and `gpu_buffers`, `gpu_upload`, `gpu_submit`, `gpu_poll` and `gpu_readback`. The debug line `GPU kernel time` gives `bc1_us` and `mips_us` per tile.
- **The debug log**: one `GPU encode: tile stages` line per tile. Since #423 the worker writes the levels into the DDS file in place, so there is no header assembly copy.
- **A `GPU encode sample` line every 60 s** at info, only when the window saw a tile or a failure. It gives p50, p95 and max per stage, tiles, `failed` (every request that came back without a tile: worker errors, a worker not running, a dropped reply), `fallback` (tiles handed back and encoded on ISPC: after a failure, or by the queue-age valve), `health` (`healthy` or `failed`), `overlapped` (tiles begun while another was in flight, #425) and the queue depth, now and the window's maximum. The service builder starts it, so a channel built in a test has no logger taking its samples. A window keeps at most 10,000 tiles (`WINDOW_CAP`) and counts the rest as `unsampled`.

Each lane has its own `GpuEncodeStats` and its own `GPU encode sample` line, with a `lane` field (`demand` or `prefetch`) and a `cancelled` count (#421, #422). The queue depth is raised before the send and lowered when the reply, the failure or the caller's cancel is back, by a guard that drops on every path. It counts callers, including those waiting on a full channel, which the channel's own capacity does not show; a caller that leaves on a cancel stops counting while its request may still be buffered.

## Memory Optimization

### MipmapStream

`MipmapStream` lends one mipmap level at a time and builds each in the
buffer of the previous one, so no level after the first gets a buffer of
its own:

```rust
impl MipmapStream {
    pub fn new(image: RgbaImage, count: usize) -> Self;
    pub fn next_level(&mut self) -> Option<&RgbaImage>;
}
```

- Owns the source image; the first call lends it, each later call writes the
  next level over it (`MipmapGenerator::downsample_box_2x_in_place`)
- The level is valid until the next call; it is not an `Iterator`
- Memory footprint: the base image's buffer (~64 MB for a 4096 tile), held
  until the stream is dropped, plus a copy of two source rows per step

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

The GPU figure is wall time from submit to poll, driver included. Header assembly in `DdsEncoder` is not in the table.

### Where the time goes (#476)

Measured on 2026-11-26 on the same laptop, quiet, on AC (Radeon 890M on RADV, Mesa 26.3.1; RTX 5070 Laptop on NVIDIA 620.73.04; release build), with the ignored tests `gpu_calibration`, `gpu_bc1_kernels` and `gpu_mip_prototype` (#476) on a synthetic 4096 tile and a real Apple imagery tile of San Francisco. **Reference figures from one laptop.** Times are p50 per 4096 tile in ms, in bursts back to back.

| Stage | 890M | RTX 5070 Laptop |
|---|---|---|
| downsample (CPU chain, on the worker) | 6.4 | 6.2 |
| upload (`write_texture`, CPU copy into wgpu staging) | 4.2 | 8.0 |
| gpu (submit to poll) | 5.5 | 3.4 |
| **gpu_kernel (BC1 passes, timestamps)** | **1.41** | **0.26** |
| readback | 0.48 | 0.50 |
| worker total | 17.0 | 18.4 |
| throughput | 56–58 tiles/s | 51–54 tiles/s |

ISPC on the same tile: 31.4 ms on one pinned thread, 184–197 tiles/s on all 24 threads.

- **The worker is bound by its own CPU work, not the GPU.** The BC1 kernel is about 8% of the worker's time on the 890M and 1.4% on the 5070. Most of `gpu` beyond the kernel is wgpu's staging-to-texture copy (2.9 ms and 1.6 ms timed alone); an empty submit and wait costs 0.02 ms.
- **The kernel is not the problem.** `block_compression`'s BC1 runs at 1.1 G blocks/s on the 890M and 5.8 G blocks/s on the 5070 (ISPC: 55 M blocks/s per thread). It matches a minimal GPU-native kernel within 3%, and its quality matches ISPC: within 0.07 dB PSNR on RADV, and 99.9% of blocks byte-identical on NVIDIA. A native kernel of equal quality (principal-axis fit plus one least-squares refit) was 10 to 25% slower. Mixing GPU and ISPC tiles in one area would not be visible numerically.
- **Mips on the GPU.** A compute shader averaging stored values with #394's rounding is byte-identical to `downsample_box_2x` at every level on both GPUs, for 4096 tiles and odd sizes (37×23, 1001×777, 4095×4097). It builds the chain in 1.4 ms (890M) and 0.26 ms (5070), against about 6 ms on one CPU thread. The linear-light filter of #467 costs almost nothing more on the GPU (+0 to 7%). Uploading level 0 only (64 MiB) instead of every level (85.33 MiB) saves about 1.5 ms (890M) and 2.8 ms (5070); the CPU copy into wgpu's staging runs at about 11 GB/s on NVIDIA.
- **Encoding on the GPU slows the CPU.** One ISPC thread ran 45 to 58% slower while either GPU encoded a steady stream (12% with the worker's CPU work alone and no GPU). It is the same on the discrete GPU, so not shared-memory contention on the iGPU; likely candidates are wgpu allocating, mapping and zeroing about 85 MiB of staging per tile, driver threads and memory bandwidth. The figure is sensitive to setup (+23% unpinned).
- **NVIDIA power states.** With 1 s idle gaps the 5070 sat in P4 for most of the run; throughput fell from 51 to 44 to 47 tiles/s, and `gpu` p95 rose to 10.3 ms while the kernel moved only from 0.26 to 0.33 ms: the cost is transfer and synchronisation, not compute.

### After #424 and #425

Measured on 2026-11-26 with `tests/gpu_calibration.rs` on the same laptop (release, synthetic 4096 tile, two runs each; Firefox at about 50% CPU throughout). **Reference figures from one laptop.**

| | 890M before | 890M #424 | 890M #425 | 5070 before | 5070 #424 | 5070 #425 |
|---|---|---|---|---|---|---|
| Demand bursts, tiles/s | 55.6–58.5 | 109.5–112.9 | 116.2–119.7 | 51.1–52.2 | 104.2–107.4 | 135.3–138.0 |
| Prefetch-lane bursts, tiles/s | same | same | 111.8–113.3 | same | same | 101.9–107.0 |
| Lone tile p50, ms | 16.9–17.3 | 8.9–9.1 | 8.6–9.1 | 18.4–19.3 | 9.2–9.3 | 9.3–9.6 |
| Upload p50, ms | 4.3 | 3.1 | 5.7 | 8.1 | 5.9 | 5.95 |
| Mip pass (timestamps), ms | (CPU 6.5) | 1.30 | 1.7 | (CPU 6.4) | 0.23 | 0.25 |
| BC1 kernel (timestamps), ms | 1.43 | 1.23 | 1.6 | 0.26 | 0.26 | 0.28 |

ISPC on the same tile: about 32 ms on one thread, about 190 tiles/s on all 24.

- **A demand tile is now served about 3.5 times faster on either GPU than by an ISPC thread**, and a GPU sustains 60 to 70% of all-core ISPC throughput without using the cores.
- **The CPU still pays for the GPU running.** At an equal GPU rate (30 tiles/s), a concurrent ISPC thread ran 30 to 40% slower before and after, although the worker's own CPU time per tile fell about four times (about 17 ms to 3.5 ms); a control doing the old worker's CPU work without a GPU costs 12 to 15%. The rest is the GPU running at all: shared memory bandwidth on the iGPU and the laptop's power and thermal budget. Uncapped, the slowdown rises with the GPU's doubled rate. Frame time decides whether the scheduler caps the GPU's rate (#475).
- **On the RTX 5070 the level-0 upload is now the limit** (see Future Considerations).

**What followed for phase 2 (#475), as planned before #424 and #425:**
- **#424:** generate the mip chain on the GPU, upload level 0 only, one submission per tile. Estimates: about 110 tiles/s on the 890M and 125 on the 5070.
- **A persistently mapped upload buffer** (or assembling chunks straight into it) joins or follows #424, since the staging copy then dominates on NVIDIA.
- **#425:** two tiles in flight bound a tile at the larger of its CPU side (about 3.5 to 6.5 ms) and GPU side. Estimates: about 180 tiles/s on the 890M and 160 on the 5070.
- **No kernel work** is warranted.

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
| `dds/mipmap.rs` | `MipmapStream` (lends levels, built in place) |
| `dds/encoder.rs` | `DdsEncoder`, `CompressorBackend` dispatch |
| `service/builder.rs` | Compressor construction and wiring |
| `config/keys.rs` | `TextureCompressor`, `TextureGpuDevice` config keys |

## Future Considerations

- **Scheduling across the CPU and GPUs**: `compressor = auto`, calibrated per machine, is the plan in #475 (GPU-first, ISPC as overflow)
- **Upload placement on discrete GPUs**: wgpu places the mapped upload buffer in device-local (resizable BAR) memory, so the 64 MiB write crosses PCIe at about 11 GB/s on the laptop RTX 5070, about 6 ms per tile, which caps it near 140 tiles/s; wgpu offers no control of that placement
- **Mac Apple Silicon**: iGPU reports as `IntegratedGpu` but is the only GPU; works transparently via Metal backend
