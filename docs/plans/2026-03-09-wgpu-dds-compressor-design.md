# GPU-Accelerated DDS Compression via wgpu

**Date:** 2026-03-09
**Issue:** #67
**Status:** Design approved

## Problem

CPU saturation (100% across 32 logical cores) at DSF boundaries during DDS
texture encoding causes micro-stutters in X-Plane. The ISPC SIMD encoder
(intel_tex_2) improved throughput ~10x over the pure-Rust encoder, but CPU
contention between XEarthLayer and X-Plane remains the bottleneck.

Many systems have an idle integrated GPU (e.g., AMD Radeon on Ryzen processors)
that could handle DDS encoding, freeing CPU cores entirely.

## Solution

Add a `WgpuCompressor` implementation of the existing `BlockCompressor` trait
that offloads BC1/BC3 compression to a GPU via wgpu compute shaders.

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Trait API | Keep sync `compress()` | Already runs in `spawn_blocking`; async adds churn for no throughput gain |
| Shader source | `block_compression` crate | ISPC kernel ported to WGSL; MIT licensed; tested on Vulkan/Metal/DX12 |
| GPU device selection | By device type + name fallback | `integrated`/`discrete` covers common case; name substring for disambiguation |
| Mipmap generation | CPU-side (`MipmapGenerator`) | ~5ms; not a bottleneck; iGPU shared memory makes upload free |
| Initialization | Eager at startup | Fail-fast; shader compilation cost (~100-500ms) paid once, not during scene load |
| Feature gating | `gpu-encode` cargo feature | Avoids wgpu compile-time/binary-size cost for users who don't need it |
| Config errors | Hard error, exit non-zero | User explicitly requested GPU; silent fallback hides misconfiguration |

## Architecture

### Config

Two new keys in the `[texture]` section:

```ini
[texture]
format = bc1
compressor = ispc           # software | ispc | gpu
gpu_device = integrated     # integrated | discrete | <name substring>
```

- `compressor` (default: `ispc`) — Selects the `BlockCompressor` backend
- `gpu_device` (default: `integrated`) — GPU adapter selection, only used when `compressor = gpu`

Device type selection uses `wgpu::AdapterInfo::device_type`:
- `integrated` → `DeviceType::IntegratedGpu`
- `discrete` → `DeviceType::DiscreteGpu`
- Any other string → case-insensitive substring match against `AdapterInfo::name`

### Error Behavior

All errors are fatal at startup (exit non-zero with actionable message):

1. `compressor = gpu` without `gpu-encode` feature compiled:
   *"GPU compression requires the `gpu-encode` feature. Rebuild with `cargo build --features gpu-encode` or set `texture.compressor = ispc`."*

2. `gpu_device` matches no adapter:
   Lists all available adapters with name, type, and backend.

3. No Vulkan/Metal/DX12 runtime available:
   Guidance on installing GPU drivers.

### WgpuCompressor

New struct in `dds/compressor.rs`, behind `#[cfg(feature = "gpu-encode")]`:

```rust
pub struct WgpuCompressor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compressor: GpuBlockCompressor,  // from block_compression crate
}

impl BlockCompressor for WgpuCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;
    fn name(&self) -> &str; // "gpu"
}
```

Factory function for construction with adapter selection:

```rust
pub fn create_wgpu_compressor(gpu_device: &str) -> Result<WgpuCompressor, DdsError>
```

### Compression Flow (per mipmap level)

1. Create `wgpu::Texture` from `RgbaImage` RGBA data (write via `queue.write_texture`)
2. Create `TextureView` for compute shader input
3. Create output `wgpu::Buffer` sized for compressed blocks
4. `compressor.add_compression_task(variant, &view, w, h, &buffer, None, None)`
5. Create `CommandEncoder`, begin compute pass, `compressor.compress(&mut pass)`
6. Submit command buffer, poll device to completion
7. Map output buffer, copy compressed data to `Vec<u8>`

On integrated GPUs with shared memory, steps 1 and 7 are near-zero cost
(no PCIe transfer — just memory mapping).

### Service Wiring

`service/builder.rs::create_encoder()` reads `config.texture().compressor()`:

- `"software"` → `Box::new(SoftwareCompressor)`
- `"ispc"` → `Box::new(IspcCompressor)` (default)
- `"gpu"` → `create_wgpu_compressor(config.texture().gpu_device())`; hard error on failure

Result passed to `DdsEncoder::new(format).with_compressor(compressor)` as today.
No changes to task, executor, or job code.

### Feature Gate

```toml
[features]
gpu-encode = ["dep:wgpu", "dep:block_compression"]

[dependencies]
wgpu = { version = "24", optional = true }
block_compression = { version = "0.8", optional = true, default-features = false, features = ["bc15"] }
```

### Diagnostics

`xearthlayer diagnostics` output (when `gpu-encode` feature compiled):

```
GPU Adapters:
  [0] NVIDIA GeForce RTX 5090 (Discrete, Vulkan)
  [1] AMD Radeon Graphics (Integrated, Vulkan)
  Active compressor: gpu (AMD Radeon Graphics)
```

### Config Pipeline Changes

| File | Change |
|------|--------|
| `config/defaults.rs` | `DEFAULT_COMPRESSOR = "ispc"`, `DEFAULT_GPU_DEVICE = "integrated"` |
| `config/keys.rs` | Add `TextureCompressor`, `TextureGpuDevice` to `ConfigKey` enum |
| `config/settings.rs` | Add `compressor`, `gpu_device` to `TextureSettings` |
| `config/texture.rs` | Add `compressor()`, `gpu_device()` accessors to `TextureConfig` |
| `config/parser.rs` | Parse both keys from `[texture]` section |
| `config/writer.rs` | Write both keys with comments |
| `docs/configuration.md` | Document new settings |

## Data Flow

```
RgbaImage (CPU)
  ├─ MipmapGenerator::generate_chain() → [4096², 2048², 1024², 512², 256²]
  │
  └─ For each mip level:
       ├─ queue.write_texture(rgba_data)     ← near-free on iGPU (shared memory)
       ├─ add_compression_task(BC1/BC3)
       ├─ dispatch compute pass
       └─ read back compressed blocks        ← near-free on iGPU
  │
  DdsEncoder assembles header + compressed mip data → Vec<u8>
```

## Performance Expectations

| Metric | ISPC (current) | wgpu iGPU (expected) |
|--------|---------------|---------------------|
| 4096² BC1 encode | ~20-50ms | ~2-5ms |
| 4096² + 5 mipmaps | ~25-60ms | ~7-12ms (incl. CPU mipmap) |
| CPU utilization | 100% on encoding threads | ~0% (GPU-offloaded) |
| Memory overhead | Negligible | ~50-100MB (pipelines, buffers) |

Primary benefit is not raw speed but **CPU freedom** — encoding no longer competes
with X-Plane for CPU time at DSF boundaries.

## Future Considerations

- **GPU mipmap generation**: If CPU mipmap becomes a bottleneck, add a downsample
  compute shader. Currently ~5ms per tile, unlikely to matter.
- **Auto-selection**: `compressor = auto` could probe for GPU availability and
  fall back to ISPC. Deferred to a future iteration; explicit config preferred
  for initial release.
- **Mac Apple Silicon**: iGPU reports as `IntegratedGpu` but is the only GPU.
  Users should understand `integrated` on Mac means the shared GPU. No code
  changes needed — wgpu handles Metal backend transparently.
