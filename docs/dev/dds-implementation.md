# DDS Implementation Summary

## Overview

Successfully implemented complete DDS (DirectX Surface) texture encoding with BC1 compression and mipmap generation, integrated into the CLI for X-Plane compatibility testing. BC1 is the only format XEarthLayer encodes; see [Supported formats](#supported-formats).

## Implementation Statistics

- **Total Tests**: 161 passing (93 new DDS tests)
- **Test Coverage**: ~90% for DDS module
- **Encoding Performance**: 0.21s for 4096×4096 BC1 (well under 1s target)
- **File Size**: 11,184,952 bytes for a 4096×4096 BC1 tile with the full 13-level mipmap chain

## Module Structure

```
xearthlayer/src/dds/
├── types.rs        - Core types, errors, DDS header structures
├── conversion.rs   - RGB565 conversion, color distance, interpolation
├── layout.rs       - DDS file geometry: header length, block bytes, level, chain and file sizes
├── header.rs       - DDS header construction and serialization
├── bc1.rs          - BC1/DXT1 compression (8 bytes per block)
├── compressor.rs   - ImageCompressor, MipmapCompressor, IspcCompressor, SoftwareCompressor,
│                     compress_level / compress_level_into (pad one level to whole
│                     blocks, compress it into a new buffer or a given slice)
├── gpu_channel.rs  - GpuEncoderChannel (GPU worker)
├── mipmap.rs       - Mipmap chain generation with box filtering
├── encoder.rs      - Main DDS encoder API
└── mod.rs          - Public API exports
```

## DDS geometry

`dds::layout` (`dds/layout.rs`, #391) owns every size in a DDS file, as const
fns: `HEADER_LEN` (128), `BLOCK_DIM` (4), `DdsFormat::block_bytes`, `level_len`
(one level, `ceil(w/4) * ceil(h/4) * block_bytes`, so 2×2 and 1×1 levels take one
block each), `full_chain_levels` (`floor(log2(min(w, h))) + 1`: the chain stops at
the smaller side), `chain_levels` (a requested count clamped to the full chain),
`chain_len` and `file_len`. The header, both CPU compressors, the GPU worker,
`DdsTextureEncoder::expected_size` (FUSE `st_size`), `EXPECTED_DDS_SIZE`, the night
lighting layout (`LitLayout`, `minimal_black_dds`), the BC1 repair pass, the Laminar
pattern decoder and the magenta placeholder all call it; none writes the arithmetic
out. It is not `cache::layout`, which places the cache tiers on disk.

`dds::compress_level_into(kernel, level, format, out)` (#392, #393) is the one
definition of encoding a single level: it pads the level to whole 4×4 blocks and
compresses it with the kernel it is given into `out`, which must be exactly
`level_len` bytes. `compress_level` is the same into a buffer of its own. Kernels
write in place through `ImageCompressor::compress_into` (ISPC through
`intel_tex_2`'s `compress_blocks_into`; the default compresses and copies, which
is what `SoftwareCompressor` does).

So every DDS buffer is sized from `dds::layout` before anything is encoded, and
allocated once:

- `DdsEncoder` allocates the whole file at `file_len` for the resolved level
  count, writes the header into its first `HEADER_LEN` bytes and compresses each
  level into its slice (`compress_chain_into`; a level that would overrun the
  file, or a file left short, is an error). The returned `Vec` has no spare
  capacity, so `Bytes::from` adopts it as is: an encoded 4096 tile is one
  allocation of 11,184,952 bytes. It used to start from the header and grow by
  doubling to 16,777,472 bytes, a mapping of which the unwritten third took no
  memory but counted in the allocator's `mmap_mb` (see
  `memory-telemetry.md`), with a copy of every level on the way. Measured on a
  4096 noise tile with the service's allocator settings (#227): about 1,000 to
  2,700 fewer page faults and 1 to 1.5 ms less per encode, about 3%.
- Night lighting's `bc1_levels` allocates the lit payload at its exact size and
  compresses each lit level into its slice with `IspcCompressor` named, then
  repairs that slice in place.

The GPU path (`GpuEncoderChannel`, `encode_with_mipmap_compressor`) still grows
its output from the header and copies it once more; it is not covered by this.

## Supported formats

XEarthLayer encodes BC1 (DXT1) only (#395). BC3 (DXT5) was retired: a BC3 tile is
twice the size of a BC1 tile (22,369,776 against 11,184,952 bytes for a 4096×4096
full chain) for an alpha channel that opaque ortho imagery does not use, and VRAM is
the binding constraint. BC3 also never worked end to end, because the FUSE read gate
validates every tile against the BC1 size and served the magenta placeholder in its
place. There is no user setting for the format: the `texture.format` key and
`run --dds-format` were removed.

The encoder interfaces keep their `format` parameter (`DdsFormat`) as the extension
point for future formats:

- `ImageCompressor::compress(image, format)`
- `ImageCompressor::compress_into(image, format, out)`
- `compress_level(kernel, level, format)` and `compress_level_into(kernel, level, format, out)`
- `MipmapCompressor::compress_mipmap_chain(image, format, count)`
- `DdsEncoder::new(format)`
- `TextureConfig::new(format)`

Only BC1 is implemented behind them. `DdsFormat::BC3` remains as a named but
unsupported format, and `DdsFormat::is_supported()` is the one statement of what is
supported. Any value other than BC1 is rejected with `DdsError::UnsupportedFormat`:

- when the encoder is built at service start (`create_encoder` in
  `service/builder.rs`), so a wrong format fails at startup and not once per tile;
- at the top of `DdsEncoder::encode`, before any work;
- in `compress_level` and `compress_level_into`, before the level is padded or
  the kernel runs or `out` is written (an empty level is refused there too, with
  `InvalidDimensions(0, 0)`, and an `out` that is not exactly `level_len` bytes
  with `CompressionFailed`);
- at each backend entry point: `IspcCompressor`, `SoftwareCompressor`, and
  `GpuEncoderChannel`, which rejects before the request is sent to the GPU worker.
  `IspcCompressor` also refuses a level that is not whole 4×4 blocks
  (`InvalidDimensions`) rather than letting the kernel read past it;
  `compress_level(_into)` pads first, so no caller reaches that.

The format facts stay where they describe the format and encode nothing: the block
size (8 or 16 bytes) is `DdsFormat::block_bytes` in `dds::layout`, and the FourCC
(`DXT1` or `DXT5`) is in `DdsHeader`. `EXPECTED_DDS_SIZE`
(`file_len(4096, 4096, full_chain_levels(4096, 4096), BC1)`), the FUSE read gate
(`validate_dds_or_placeholder`), the DDS disk validator and the magenta placeholder
are BC1-only by construction, which is correct because only BC1 can be encoded.

Adding a format requires:

1. A kernel arm in each backend: ISPC, software and GPU.
2. A format-aware expected size for the read gate (`validate_dds_or_placeholder`)
   and the DDS disk validator. `dds::layout` and FUSE `st_size`
   (`DdsTextureEncoder::expected_size`) already take the format.
3. A magenta placeholder in that format: one solid block of the format repeated
   for the whole chain (`solid_bc1_full_chain` in `fuse/placeholder.rs` is the
   BC1 one), no encoder needed.
4. Adding the format to `DdsFormat::is_supported`.
5. A user-facing switch: a config key and its validation.

## Compression Formats

### BC1 (DXT1)
- **Compression**: 4:1 (0.5 bytes per pixel)
- **Block Size**: 8 bytes per 4×4 pixels
- **Color**: Two RGB565 endpoints + 2-bit indices
- **Alpha**: 1-bit or none
- **Best For**: Opaque satellite imagery
- **Performance**: ~10.5 MB for 4096×4096 with 5 mipmaps

## CLI Integration

The encoder has no command-line options. XEarthLayer always encodes BC1 with the full
mipmap chain; `texture.compressor` chooses the backend (`software`, `ispc`, `gpu` or `hybrid`).
See `docs/configuration.md`.

## Test Results

### Format Validation
- ✅ BC1/DXT1 format confirmed by `file` command
- ✅ ImageMagick can read and identify format
- ✅ Mipmap chain correctly embedded

### Performance Benchmarks
- **Download**: 2-3 seconds for 256 chunks
- **BC1 Encoding**: 0.21s for 4096×4096
- **Total Pipeline**: <3.5 seconds end-to-end

### Size Comparison (4096×4096 with 5 mipmaps)
- BC1 DDS: ~11 MB
- JPEG: ~8.8 MB (90% quality)

## Technical Details

### Mipmap Chain Structure
A 4096×4096 tile carries the full chain, 13 levels down to 1×1:
- Level 0: 4096×4096 (8,388,608 bytes BC1)
- Level 1: 2048×2048 (2,097,152 bytes BC1)
- Level 2: 1024×1024 (524,288 bytes BC1)
- Levels 3 to 10: 512×512 down to 4×4 (174,760 bytes together)
- Levels 11 and 12: 2×2 and 1×1, one 8-byte block each (edge-padded to 4×4 before encoding)
- **Total**: 11,184,824 bytes + 128 byte header = 11,184,952 bytes (`EXPECTED_DDS_SIZE`)

### Mipmap Downsample
`MipmapGenerator::downsample_box_2x` (`dds/mipmap.rs`) builds each level from
the one above with a 2×2 box filter, on raw RGBA bytes (#389). Each output row
takes three passes: the two source rows are added byte by byte into a `u16`
scratch row, adjacent pixel pairs of that row are added into a second scratch
row of block sums, and each block sum `s = a + b + c + d` becomes its average
through `block_average`, rounded half to even: `(s + 1 + ((s >> 2) & 1)) >> 2`
(#394). An odd last row or column of the source is dropped.

The rounding keeps every level as bright as the one above it. Truncating
`s / 4`, as the encoder did before #394, loses three eighths of a step per
level on noisy imagery, so a tile's fifth level was about two steps darker
than its source and its smallest about four and a half (out of 255). Rounding
half up gains an eighth of a step per level instead; half to even has no bias.
`every_level_keeps_the_source_mean_within_half_a_step` holds the mean of every
level of a 4096 chain within half a step of the source's, which truncation and
half-up both fail. A sum of 1020 averages to 255, so opaque alpha stays opaque.

The passes are separate so that each is a contiguous loop the compiler
vectorises. Folding the rounding into the pair pass made the chain about twice
as slow: the compiler vectorised it across the 4-byte pixel stride, with a
shuffle for every pixel. The 4096×4096 chain takes about 6 to 7 ms on one core,
about a tenth more than the truncating two-pass version and half the earlier
per-pixel loop (about 13 ms). Both encoder backends use it: the CPU path
through `MipmapStream`, and the GPU worker, where it runs on the single worker
thread for every tile. Night lighting does not: its levels are averaged in
linear light by `lit_chain` (see `night-lighting-design.md`).

### Compression Algorithm
- **Endpoint Selection**: Bounding box method (min/max per channel)
- **Color Quantization**: RGB565 format (5-6-5 bits)
- **Index Generation**: Perceptual color distance with green weighting
- **Mipmap Filtering**: Box filter (2×2 average, see [Mipmap Downsample](#mipmap-downsample))

### File Format Compliance
- Microsoft DirectDraw Surface specification
- Compatible with X-Plane texture system
- Readable by standard DDS tools (GIMP, Paint.NET, ImageMagick)

## Verification Steps

1. **File Type Check**
   ```bash
   file output.dds
   # Output: Microsoft DirectDraw Surface (DDS): 4096 x 4096, compressed using DXT1
   ```

2. **ImageMagick Inspection**
   ```bash
   identify -verbose output.dds
   # Shows: Format: DDS, Geometry: 4096x4096, Compression: DXT1
   ```

3. **Hex Dump Verification**
   ```bash
   hexdump -C output.dds | head -5
   # Shows: Magic "DDS ", FourCC "DXT1"
   ```

## Next Steps

1. **X-Plane Testing**: Copy DDS files to X-Plane Custom Scenery and verify rendering
2. **Quality Optimization**: Consider implementing PCA-based endpoint selection
3. **Performance Optimization**: Add SIMD acceleration for block compression
4. **Advanced Features**: Implement Lanczos filtering for higher quality mipmaps
5. **FUSE Integration**: Connect DDS encoder to virtual filesystem

## Files Generated

Test script available at `/tmp/test_dds_output.sh` which:
- Tests the BC1 format
- Verifies file format with `file` command
- Compares sizes against expected values
- Generates sample files for manual inspection

Sample files:
- `/tmp/sf_bc1.dds` - San Francisco, BC1 format
- `/tmp/london.jpg` - London, JPEG format (comparison)

## Conclusion

The DDS implementation is **production-ready** with:
- ✅ Complete BC1 compression
- ✅ Mipmap generation
- ✅ CLI integration
- ✅ 93 comprehensive tests passing
- ✅ Performance under target (0.21s vs 1s target)
- ✅ X-Plane compatible format
- ✅ Verified with multiple tools

Ready to integrate with FUSE filesystem for on-demand texture generation!
