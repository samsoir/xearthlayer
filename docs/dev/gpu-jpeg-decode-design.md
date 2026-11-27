# GPU JPEG Decode (speclet)

**Status:** proposed (2026-11-26). Part of the compute-pipeline plan,
[#475](https://github.com/samsoir/xearthlayer/issues/475). It is built on a
spike measured on one laptop, and is not implemented; tracked in
[#489](https://github.com/samsoir/xearthlayer/issues/489).

**Companion:** [Prefetch V4: streaming prefetch](prefetch-v4-streaming-design.md).
GPU decode only pays off on a steady stream of tiles; V4 is what supplies
that stream. Each design is half of the same goal: tiles streamed through the
GPU, end to end, with the CPU left for X-Plane.

## Why

With the GPU encoder doing one submission per tile (#424) and keeping two tiles
in flight (#425), encoding a tile costs the CPU about 3.5 ms. Assembly is now
XEL's largest CPU cost per tile: decoding 256 JPEG chunks into the 4096 × 4096
RGBA canvas (`chunk_image::decode_into`, #390).

- **CPU cost:** about 41 ms of one core per Bing tile. At the GPU encoder's rate
  of about 118 tiles/s (Radeon 890M), decoding alone keeps about five cores busy.
- **Upload:** the 64 MiB canvas has to be copied to the GPU for every tile. On
  NVIDIA that copy is now the encoder's bottleneck
  ([GPU encoding](gpu-encoding-design.md), "After #424 and #425").

Decoding on the GPU removes both. The upload becomes the compressed JPEG data
(about 4 MB per tile), and the canvas never leaves the GPU: it feeds #424's mip
pass directly.

## What the spike found

All figures come from one laptop (Ryzen AI 9 HX 370; Radeon 890M on Mesa 26.3.1
RADV; RTX 5070 Laptop on NVIDIA 620.73.04; release builds). **They are
reference only.**

### Where CPU decode time goes

Profiled with perf on zune-jpeg 0.5.13 (the decoder XEL uses), for one San
Francisco tile from Bing (256 chunks, 3.84 MB):

| Stage | Share | Fit for a GPU |
|---|---|---|
| Huffman entropy decoding | ~59% | Serial within a chunk |
| Inverse DCT | ~25% | Yes |
| Chroma upsampling, colour conversion, post-processing | ~14% | Yes |

### Options rejected

- **Hardware JPEG decoders through VA-API.** Both GPUs offer
  `VAProfileJPEGBaseline` decode: AMD through VCN, and NVIDIA through
  `nvidia-vaapi-driver`. Measured with ffmpeg:
  - AMD's engine stays flat at about 22 tiles/s from 1 to 8 streams, because its
    fixed cost per frame dominates on 256 × 256 frames.
  - Copying frames back to system memory halves that rate and erases the CPU
    saving.
  - The NVIDIA path costs more CPU than software decoding.
  - Using either from wgpu would need DMA-BUF interop.
- **Hybrid decode** (Huffman on the CPU, the rest on the GPU). This saves at most
  about 40% of decode CPU, and the upload only shrinks to 48 MiB of i16
  coefficients.

### Formats in use

Chunks surveyed over San Francisco at zoom 16. Every provider serves baseline
JPEG (SOF0), 256 × 256, with no restart markers and nothing progressive.

| Provider | Chroma | Tables | Quality | Use |
|---|---|---|---|---|
| Apple Maps | 4:2:0 | one set for every chunk (2,000 sampled) | ~80 | Common |
| Bing | 4:2:0 | one set for every chunk | ~79 | Common |
| Google GO2 | 4:2:0 | one set for every chunk | ~75 | Common |
| ArcGIS | 4:2:0 | a set per chunk (optimised Huffman) | ~85 | Rare |
| USGS | 4:4:4 | one set for every chunk | ~85 | Rare |

The three providers in real use share one profile: baseline 4:2:0, with one
table set per provider.

That holds beyond San Francisco. A contributor checked 1,000 Apple Maps chunks
from 40 European regions in their own cache. All were baseline, 256 × 256,
4:2:0, with no restart markers.

### Measured decode on the GPU

The prototype is described under Design below.

| Per 4096 tile | 890M | RTX 5070 Laptop |
|---|---|---|
| Bing, one tile | ~54 ms | ~33 ms |
| Bing, 16 tiles batched | **8.2 ms (121 tiles/s)** | **3.7 ms (268 tiles/s)** |
| Google GO2, 16 batched | 7.8 ms (128 tiles/s) | 3.9 ms (258 tiles/s) |
| ArcGIS, 16 batched | 23–35 ms (noisy) | 4.3 ms (235 tiles/s) |
| USGS (4:4:4), 16 batched | 12.7 ms (79 tiles/s) | 6.3 ms (159 tiles/s) |
| Apple Maps, one tile | ~56 ms | ~33 ms |
| Apple Maps, 16 batched | **7.6 ms (131 tiles/s)** | **3.4 ms (299 tiles/s)** |
| CPU (zune), Bing, one core | 41 ms | |
| CPU (zune), Apple Maps, one core | 48 ms | |

The Apple tile is larger than the Bing one: 4.8 MB of chunks against 3.8 MB. As
a result, zune takes longer on it, but the batched GPU decode does not.

**Correctness.** Each chunk was compared with zune's RGBA output. Mean PSNR is
51–55 dB on the 4:2:0 providers and 61 dB on USGS; the worst chunk is 45.9 dB.
Apple's mean is 54.1 dB. The output is not byte-identical, because of a float
inverse DCT and differences in edge handling.

- **Batched:** the decode is fast, with the Huffman pass at about 4 ms per tile
  on the 890M and 2.3 ms on the 5070.
- **A lone tile:** it is slower than one CPU core, because each chunk's
  bitstream decodes serially on one GPU lane.

That is why this design depends on [V4](prefetch-v4-streaming-design.md)
keeping batches full.

## Design

### Three passes per batch of chunks

1. **`huffman`: one invocation per chunk.**
   - It reads the chunk's entropy-coded data and removes the `0xFF00` byte
     stuffing itself.
   - It decodes every MCU (4:2:0: 256 MCUs of 6 blocks; 4:4:4: 1024 MCUs of 3),
     and writes the quantised coefficients as packed i16, in natural order.
   - Huffman tables use a 9-bit fast lookup with a slow path for longer codes.
   - When a batch shares one table set (Apple, Bing, Google, USGS), the
     workgroup copies it into workgroup memory first. That halved the pass's
     time on the 890M.
2. **`idct`: one workgroup of 64 threads per 8 × 8 block.**
   - It dequantises the block, then runs a separable inverse DCT in workgroup
     memory, rows then columns.
   - It clamps and rounds to bytes, as libjpeg does, and writes into per-chunk
     Y, Cb and Cr sample planes.
3. **`colour`: one invocation per pixel.**
   - 4:2:0 chroma uses libjpeg's triangle ("fancy" h2v2) filter, with edges
     replicated at chunk borders.
   - YCbCr is converted to RGBA and written at the chunk's place in the tile
     canvas, which stays on the GPU.

The CPU's share of the work:
- **Header parsing:** reads DQT, DHT, SOF0 and SOS. Measured at about 1.3 ms per
  tile with tables deduplicated.
- **Packing:** each chunk's scan data and a per-chunk record (scan offset and
  length, table and quantisation offsets, sampling) are packed into buffers.
- **Upload:** the packed buffers are uploaded once per batch.

### Supported profile and fallback

The GPU path takes baseline (SOF0) 8-bit, three-component chunks, 256 × 256,
4:2:0 or 4:4:4, with no restart markers. Anything else goes to the CPU decoder
under #390's rules; `chunk_image::decode` remains the reference for what a
usable chunk is. That covers progressive, restart markers, grayscale,
arithmetic coding, CMYK and other sizes.

Header checks stay on the CPU, so a chunk is routed before any GPU work.

Within the GPU decode, a chunk whose bitstream fails sets a per-chunk error flag
and is redecoded on the CPU, or painted magenta and counted as rejected.
Failures include an invalid code, a run past 64 coefficients, or data ending
early. Either outcome keeps #388's rule: no tile with a rejected chunk is
cached.

### Into the encoder

On the GPU path the decoded canvas is #424's level 0, already in GPU memory. The
mip pass reads it directly, so there is no level-0 upload or mapped upload
buffer for those tiles. Only the compressed data goes up, and only the encoded
DDS comes back.

### Memory

Per 4096 tile, the prototype held:
- 48 MiB of packed coefficients (4:2:0; 96 MiB at 4:4:4);
- 48 MiB of sample planes;
- the 64 MiB canvas.

A production version should fuse passes where it can. The `idct` and `colour`
passes can share workgroup memory for 4:4:4, and for 4:2:0 with an MCU halo. It
should also size batches to the device's limits.

### Quality

GPU-decoded tiles are not byte-identical to CPU-decoded ones. Under #475's rule,
mixing devices must be imperceptible. The acceptance threshold should be stated
as decoded PSNR against zune; the spike's worst chunk is 45.9 dB. Visual
side-by-side review backs it up. Cache keys are unaffected, because both paths
write a valid tile for the same coordinates.

## Who decodes where

This is decided by the scheduler (#475 phase 4), using calibration.

- **Streams of tiles** (prefetch, and the 20 or more DSF areas of a scene load):
  decode and encode on the GPU, in batches that V4 keeps full.
- **Tiles X-Plane is waiting for:** as fast as possible. On a busy GPU they
  join the next batch, ahead of prefetch (#421). With no GPU, or a GPU slower
  than ISPC, they decode on the CPU. Decoding a tile's 256 chunks across cores
  is a cheap CPU-side improvement on its own.
- **No GPU:** the CPU decoder, as today.

## Open items

- ArcGIS's per-chunk tables are slow on RADV (global-memory lookups). Options:
  u16 table entries, or grouping chunks by table set. The provider is rarely
  used, so this is low priority.
- Detecting bitstream errors on the GPU and falling back per chunk (see
  Supported profile and fallback).
- Fusing the `idct` and `colour` passes, and the memory each batch holds.
- Validating on Metal (Apple Silicon) and Vulkan with naga in CI, as the mip
  shader is today.
- Lone-tile latency: tables are already in workgroup memory; a faster serial
  bit reader is the next lever.

## References

- [#475](https://github.com/samsoir/xearthlayer/issues/475): the compute-pipeline
  plan (scheduler, calibration, phases).
- [Prefetch V4: streaming prefetch](prefetch-v4-streaming-design.md): the stream
  this decoder needs.
- [GPU encoding](gpu-encoding-design.md): the encoder this feeds (#424, #425).
- #388, #390: what a usable chunk is, and today's CPU assembly.
