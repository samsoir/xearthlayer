# GPU Encoder Memory Optimization

**Issue:** #120
**Date:** 2026-03-27
**Status:** Draft
**Depends on:** PR #121 (MipmapStream, owned RgbaImage signatures)

## Problem

The GPU encoding path (`gpu-encode` feature) forces a 64 MB clone of each mipmap
level at the `BlockCompressor` trait boundary. `GpuEncoderChannel::compress()` takes
`&RgbaImage` (per the trait) and must clone to send ownership over the mpsc channel
to the GPU worker thread.

Heaptrack profiling (2 GB memory cache, PR #121 branch) shows:
- `GpuEncoderChannel::compress` clone: **1.88 GB** peak (#2 consumer)
- Appears twice in peak consumers across different call paths (~3.76 GB total)
- 469 MB in per-submission readback buffer allocations

Additionally, the current architecture creates 5 channel round-trips per tile
(one per mipmap level), each involving `blocking_send`, `blocking_recv`, oneshot
channel construction, and GPU resource allocation. This CPU overhead contributes
to the FPS impact observed during GPU encoding bursts.

## Design

### Trait Separation: ImageCompressor and MipmapCompressor

Rename `BlockCompressor` to `ImageCompressor` — reflects its actual contract
(compresses a single image level):

```rust
pub trait ImageCompressor: Send + Sync {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;
}
```

Add a new `MipmapCompressor` trait for backends that own the full mipmap pipeline:

```rust
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

- `IspcCompressor` and `SoftwareCompressor` implement `ImageCompressor` (renamed)
- `GpuEncoderChannel` implements `MipmapCompressor` (new)
- The traits model different execution contracts: caller-managed iteration vs.
  callee-owned pipeline

### DdsEncoder Dispatch

`DdsEncoder` holds a `CompressorBackend` enum:

```rust
pub enum CompressorBackend {
    Image(Arc<dyn ImageCompressor>),
    #[cfg(feature = "gpu-encode")]
    Mipmap(Arc<dyn MipmapCompressor>),
}
```

In `encode()`:
- `Image`: Existing fused `MipmapStream` loop (unchanged from PR #121)
- `Mipmap`: Capture dimensions and mipmap count, call `compress_mipmap_chain(image,
  format, count)`, prepend DDS header to returned bytes

Builder methods:
- `with_compressor(Arc<dyn ImageCompressor>)` — existing, renamed
- `with_mipmap_compressor(Arc<dyn MipmapCompressor>)` — new, `#[cfg(feature = "gpu-encode")]`

`encode_with_mipmaps(&[RgbaImage])` continues to use `ImageCompressor` only.

### GpuEncoderChannel — Worker-Side Streaming

The channel message changes from single-level to full-tile:

```rust
struct GpuEncodeRequest {
    image: RgbaImage,       // source image, moved (not cloned)
    format: DdsFormat,
    mipmap_count: usize,
    response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
}
```

`GpuEncoderChannel` implements `MipmapCompressor`:
- Moves the source image into `GpuEncodeRequest` — zero clones
- One channel round-trip per tile (was 5)
- One oneshot pair per tile (was 5)

The GPU worker loop changes to process full tiles:

1. Receive `GpuEncodeRequest` with owned source image
2. Create `MipmapStream::new(image, mipmap_count)` — worker owns the iteration
3. For each level: upload → dispatch compute → readback → append to output
4. While GPU compresses level N, CPU downsamples to produce level N+1 (natural
   pipeline overlap within a single tile)
5. Return concatenated compressed bytes on the oneshot

### GPU Buffer Reuse

Pre-allocate GPU resources for the largest level (level 0) and reuse for
smaller levels:

- **Input texture**: Created per mipmap level (wgpu textures are immutable in size).
  This is unavoidable but the cost is minimal compared to the eliminated clones.
- **Output buffer**: Allocate for level 0 compressed size, reuse for smaller levels.
- **Readback buffer**: Allocate once at level 0 size, reuse with unmap/remap cycle.

Output and readback buffers are reused across all mipmap levels within a tile,
reducing GPU allocation overhead from 5 buffer pairs to 1. Input textures are
created per level due to wgpu's immutable texture sizing.

## Memory Impact

**Per-tile CPU memory (GPU path):**

| Buffer | Current | After |
|--------|---------|-------|
| `image.clone()` in channel send | 64 MB | 0 (moved) |
| Channel round-trips per tile | 5 | 1 |
| Oneshot channels per tile | 5 pairs | 1 pair |

**Per-tile GPU memory:**

| Resource | Current | After |
|----------|---------|-------|
| Textures allocated per tile | 5 | 1 (reused) |
| Output buffers per tile | 5 | 1 (reused) |
| Readback buffers per tile | 5 | 1 (reused) |
| Peak GPU memory | ~85 MB | ~83 MB (no churn) |

**CPU contention reduction per tile:**
- 4 fewer `blocking_send`/`blocking_recv` round-trips
- 4 fewer oneshot channel constructions
- 4 fewer wgpu texture/buffer create+destroy cycles
- At 40 concurrent tiles: 160 fewer cross-thread synchronization points per burst

## Scope Boundaries

**In scope:**
- `BlockCompressor` → `ImageCompressor` rename (mechanical)
- `MipmapCompressor` trait (feature-gated)
- `CompressorBackend` enum in `DdsEncoder`
- `GpuEncoderChannel` reimplemented as `MipmapCompressor`
- GPU worker refactored for full-tile processing with buffer reuse
- Channel message type changed to full-tile request
- All affected tests updated

**Out of scope:**
- GPU-side downsampling (compute shader for mipmap generation) — follow-up if
  profiling shows CPU downsample is a worker thread bottleneck
- Inter-tile pipeline overlap (multiple tiles in flight on GPU simultaneously)
- CPU compressor path changes (already optimized in PR #121)

## Testing

Implementation follows strict TDD methodology (red-green-refactor).

Test plan:

- `BlockCompressor` → `ImageCompressor` rename: existing tests pass with updated names
- `MipmapCompressor` mock: verifies `DdsEncoder` dispatches correctly and prepends
  DDS header to returned bytes
- `GpuEncoderChannel` as `MipmapCompressor`: one message in, one result out, correct
  concatenated byte count for all levels
- Buffer reuse: multiple tiles processed sequentially without GPU resource leaks
  (adapted from existing stress test)
- Byte-for-byte parity: `MipmapCompressor` output matches `ImageCompressor` output
  for the same input image
- Heaptrack verification: `GpuEncoderChannel::compress` clone eliminated from peak
  consumers
- FPS impact: qualitative comparison against ISPC baseline from PR #121
