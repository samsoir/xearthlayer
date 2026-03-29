# GPU-Accelerated DDS Encoding

This document describes XEarthLayer's GPU encoding pipeline for offloading BC1/BC3 DDS texture compression to a GPU via wgpu compute shaders.

## Motivation

CPU saturation (100% across all logical cores) at DSF boundaries during DDS texture encoding causes micro-stutters in X-Plane. The ISPC SIMD encoder improved throughput ~10x over the pure-Rust encoder, but CPU contention between XEarthLayer and X-Plane remains the bottleneck at DSF boundaries.

Many systems have an idle integrated GPU (e.g., AMD Radeon on Ryzen processors) that can handle DDS encoding, freeing CPU cores entirely. The primary benefit is **CPU freedom** -- encoding no longer competes with X-Plane for CPU time.

## Feature Gate

GPU encoding is opt-in via a cargo feature to avoid wgpu compile-time and binary-size costs:

```toml
[features]
gpu-encode = ["dep:wgpu", "dep:block_compression"]

[dependencies]
wgpu = { version = "24", optional = true }
block_compression = { version = "0.8", optional = true, default-features = false, features = ["bc15"] }
```

The `block_compression` crate provides ISPC compression kernels ported to WGSL compute shaders, tested on Vulkan/Metal/DX12.

## Configuration

Two keys in the `[texture]` section:

```ini
[texture]
format = bc1
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
1. `compressor = gpu` without `gpu-encode` feature compiled
2. `gpu_device` matches no adapter (lists all available adapters)
3. No Vulkan/Metal/DX12 runtime available

## Architecture

### Trait Hierarchy

The encoding pipeline uses two trait layers, reflecting different execution contracts:

```rust
/// Single-level compression (CPU backends)
pub trait ImageCompressor: Send + Sync {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;
}

/// Full-pipeline compression (GPU backend, feature-gated)
#[cfg(feature = "gpu-encode")]
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

`DdsEncoder` dispatches via a `CompressorBackend` enum:

```rust
pub enum CompressorBackend {
    Image(Arc<dyn ImageCompressor>),
    #[cfg(feature = "gpu-encode")]
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
    response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
}
```

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

VRAM budget: pipeline depth 2 with 4096x4096 tiles ~ 160MB (BC1) / 176MB (BC3) peak.

### Shared Resource Factory

`create_gpu_resources()` is a shared factory for `wgpu::Device`, `wgpu::Queue`, and `GpuBlockCompressor` creation, used by both the worker and diagnostics.

### Hardening

- `map_async` errors propagated via `std::sync::mpsc` (not silently ignored)
- Worker panic recovery via `catch_unwind` (logs error, sends failure, drains queue, breaks loop)
- `device.on_uncaptured_error()` registered for device loss logging
- Structured tracing on buffer lifecycle and error paths

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

| Metric | ISPC (CPU) | wgpu iGPU |
|--------|-----------|-----------|
| 4096x4096 BC1 encode | ~20-50ms | ~2-5ms |
| 4096x4096 + 5 mipmaps | ~25-60ms | ~7-12ms |
| CPU utilisation | 100% on encoding threads | ~0% (GPU-offloaded) |
| Memory overhead | Negligible | ~50-100MB (pipelines, buffers) |

On integrated GPUs with shared memory, texture upload and readback are near-free (no PCIe transfer).

## Diagnostics

`xearthlayer diagnostics` output (when `gpu-encode` feature compiled):

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
- `"gpu"` -> `GpuEncoderChannel` via `create_gpu_resources()`; hard error on failure

No changes to task, executor, or job code. The compressor is injected via the existing `DdsEncoder` API.

## Key Files

| File | Purpose |
|------|---------|
| `dds/compressor.rs` | `ImageCompressor` trait, `SoftwareCompressor`, `IspcCompressor`, `WgpuCompressor` |
| `dds/gpu_channel.rs` | `GpuEncoderChannel`, `MipmapCompressor` trait, GPU worker task |
| `dds/mipmap.rs` | `MipmapStream` iterator |
| `dds/encoder.rs` | `DdsEncoder`, `CompressorBackend` dispatch |
| `service/builder.rs` | Compressor construction and wiring |
| `config/keys.rs` | `TextureCompressor`, `TextureGpuDevice` config keys |

## Future Considerations

- **GPU mipmap generation**: Add a downsample compute shader if CPU mipmap becomes a bottleneck (currently ~5ms, unlikely)
- **Auto-selection**: `compressor = auto` could probe for GPU availability and fall back to ISPC
- **Mac Apple Silicon**: iGPU reports as `IntegratedGpu` but is the only GPU; works transparently via Metal backend
