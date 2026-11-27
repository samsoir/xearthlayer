//! The GPU worker's mip chain (#424): level 0 is uploaded, and every level
//! below it is built on the GPU by a compute shader (`gpu_mips.wgsl`), into
//! the textures the BC1 kernel reads, in the tile's one submission.
//!
//! Before #424 the worker built the chain on the CPU (`MipmapStream`, about
//! 6 ms a 4096 tile) and copied every level into wgpu's staging (85.33 MiB,
//! 4 to 8 ms), which bound it to its own CPU work (#476). The shader builds
//! the chain in about the BC1 kernel's time, and only level 0 (64 MiB) is
//! uploaded.
//!
//! The chain is byte for byte the CPU's: [`gpu_filter`] is
//! [`MipmapGenerator::FILTER`], and the shader's linear-light filter (#467)
//! is [`MipmapGenerator::downsample_box_2x`] (the CPU's own tables, round
//! half to even, #394; an odd last row or column dropped), with each level
//! padded to whole blocks by repeating its edge, as `pad_to_block_multiple`
//! pads a level for the CPU's kernels. The stored-value filter, the one
//! before #467, is here as a tested option, used by no chain.
//!
//! The pure parts, tested by value: the chain's geometry ([`chain_plan`]),
//! the upload's layout ([`upload_row_bytes`], [`write_level0`]), the filter
//! ([`gpu_filter`], [`MipFilter::entry_point`]) and the linear-light tables
//! ([`SrgbTables`]). The shader itself is checked on hardware (ignored
//! tests here and in `gpu_channel`).

use image::RgbaImage;

use crate::dds::layout::{self, BLOCK_DIM};
use crate::dds::mipmap::{self, MipFilter, MipmapGenerator};
use crate::dds::DdsFormat;

/// The shader: `mip_stored` and `mip_linear`.
const SHADER: &str = include_str!("gpu_mips.wgsl");

/// The unit tests read every level back; production never does.
const READ_BACK: wgpu::TextureUsages = if cfg!(test) {
    wgpu::TextureUsages::COPY_SRC
} else {
    wgpu::TextureUsages::empty()
};

/// Edge of the shader's workgroup, in texels (`@workgroup_size(8, 8)`).
const WORKGROUP: u32 = 8;

/// The filter of the GPU worker's chain: the CPU chain's, always, so a tile
/// is the same bytes on either path and a switch of filter moves both at
/// once. The one place the GPU's filter is decided.
pub(crate) const fn gpu_filter() -> MipFilter {
    MipmapGenerator::FILTER
}

impl MipFilter {
    /// The shader's entry point for this filter.
    pub(crate) const fn entry_point(self) -> &'static str {
        match self {
            MipFilter::StoredValues => "mip_stored",
            MipFilter::LinearLight => "mip_linear",
        }
    }
}

/// One level of a tile's chain as the GPU holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LevelPlan {
    /// The level's own size, as the CPU chain has it: the level above
    /// halved, an odd texel dropped.
    pub width: u32,
    pub height: u32,
    /// Its texture's size: whole 4 x 4 blocks, as the BC1 kernel requires.
    pub padded_width: u32,
    pub padded_height: u32,
    /// Where its blocks start in the DDS payload, and how many bytes they
    /// take ([`layout::level_len`]).
    pub offset: usize,
    pub len: usize,
}

/// The first `levels` levels of a `width` x `height` chain, at most the
/// full chain ([`layout::full_chain_levels`]), as `MipmapStream` yields
/// them and `LevelSlots` places them: each level the one above halved, its
/// blocks after the ones before it. Empty for a zero dimension or no
/// levels.
pub(crate) fn chain_plan(
    width: u32,
    height: u32,
    levels: usize,
    format: DdsFormat,
) -> Vec<LevelPlan> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let levels = levels.min(layout::full_chain_levels(width, height) as usize);
    let mut plan = Vec::with_capacity(levels);
    let (mut w, mut h, mut offset) = (width, height, 0);
    for _ in 0..levels {
        let len = layout::level_len(w, h, format);
        plan.push(LevelPlan {
            width: w,
            height: h,
            padded_width: w.next_multiple_of(BLOCK_DIM),
            padded_height: h.next_multiple_of(BLOCK_DIM),
            offset,
            len,
        });
        offset += len;
        (w, h) = (w / 2, h / 2);
    }
    plan
}

/// The payload bytes of `plan`: where its last level ends.
pub(crate) fn chain_bytes(plan: &[LevelPlan]) -> usize {
    plan.last().map_or(0, |l| l.offset + l.len)
}

/// Bytes of one row of the upload buffer for a level 0 `padded_width`
/// texels wide: its texels, rounded up to the 256 bytes a buffer-to-texture
/// copy requires between rows (`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`). A
/// 4096 tile's rows (16 KiB) need no padding.
pub(crate) const fn upload_row_bytes(padded_width: u32) -> u32 {
    (padded_width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// Writes `image` as level 0 of the upload, `padded_width` x
/// `padded_height` texels (whole blocks), `row_bytes` apart
/// ([`upload_row_bytes`]): each row's texels, then copies of its last
/// texel up to the padded width, and copies of the last row below it, as
/// `pad_to_block_multiple` pads. `write(offset, bytes)` puts `bytes` at
/// `offset` in the upload: the mapped buffer in production (its memory is
/// written, never read), a `Vec` in tests. A tile whose rows need no
/// padding, every 4096 tile, is one write of the whole image: its one copy
/// on the CPU.
///
/// `image` must not be empty, and the padded size must not be smaller than
/// its own.
pub(crate) fn write_level0(
    image: &RgbaImage,
    padded_width: u32,
    padded_height: u32,
    row_bytes: u32,
    mut write: impl FnMut(usize, &[u8]),
) {
    let (width, height) = image.dimensions();
    debug_assert!(width > 0 && height > 0);
    debug_assert!(padded_width >= width && padded_height >= height);
    let raw = image.as_raw();
    let texels = width as usize * 4;
    let row_bytes = row_bytes as usize;
    if (padded_width, padded_height) == (width, height) && row_bytes == texels {
        write(0, raw);
        return;
    }
    let tail = (padded_width - width) as usize;
    let mut padding = Vec::with_capacity(tail * 4);
    for y in 0..padded_height as usize {
        let source = y.min(height as usize - 1);
        let row = &raw[source * texels..(source + 1) * texels];
        write(y * row_bytes, row);
        if tail > 0 {
            padding.clear();
            for _ in 0..tail {
                padding.extend_from_slice(&row[texels - 4..]);
            }
            write(y * row_bytes + texels, &padding);
        }
    }
}

/// The linear-light filter's tables (#467): an sRGB byte to 16-bit linear,
/// and 16-bit linear to the nearest byte. They are the CPU chain's tables
/// (`dds::mipmap`), so the shader averages with the values the CPU does.
pub(crate) struct SrgbTables {
    pub decode: Vec<u32>,
    pub encode: Vec<u8>,
}

impl SrgbTables {
    pub(crate) fn new() -> Self {
        let decode = mipmap::srgb_to_linear_table()
            .iter()
            .map(|&v| u32::from(v))
            .collect();
        let encode = mipmap::linear_to_srgb_table().to_vec();
        Self { decode, encode }
    }

    /// The encode table four bytes to a little-endian word, as the shader
    /// reads it.
    fn encode_words(&self) -> Vec<u32> {
        self.encode
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }
}

/// Little-endian bytes of `words`.
fn bytes_of(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A buffer of `usage` holding `bytes`, written through the queue before
/// its next submission. Never mapped: on a lost or destroyed device the
/// buffer is invalid and the tile using it fails, where a mapping would
/// have to be refused here (#420).
fn buffer_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    bytes: &[u8],
) -> wgpu::Buffer {
    // A uniform binding is at least 16 bytes; a write is whole words.
    let size = (bytes.len() as u64).max(16).next_multiple_of(4);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytes);
    buffer
}

/// The mip shader's pipeline for one filter, made once per device, with
/// the linear-light tables when that is the filter.
pub(crate) struct MipPipeline {
    pipeline: wgpu::ComputePipeline,
    /// The decode and encode tables, for [`MipFilter::LinearLight`].
    tables: Option<(wgpu::Buffer, wgpu::Buffer)>,
}

impl MipPipeline {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue, filter: MipFilter) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tile-mips"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("tile-mips"),
            layout: None,
            module: &module,
            entry_point: Some(filter.entry_point()),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let tables = (filter == MipFilter::LinearLight).then(|| {
            let t = SrgbTables::new();
            let usage = wgpu::BufferUsages::STORAGE;
            (
                buffer_with(device, queue, "srgb-decode", usage, &bytes_of(&t.decode)),
                buffer_with(
                    device,
                    queue,
                    "srgb-encode",
                    usage,
                    &bytes_of(&t.encode_words()),
                ),
            )
        });
        Self { pipeline, tables }
    }
}

/// One level texture of a chain: what the mip shader writes (below level
/// 0) and the BC1 kernel reads.
pub(crate) struct LevelTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
}

/// The textures of a whole chain of one tile size, a texture per level at
/// its padded size, and the mip shader's bind group for each level below
/// level 0 (its source, its texture, its size). Made once per size and kept
/// with the worker's other resources (#423).
pub(crate) struct MipChain {
    plan: Vec<LevelPlan>,
    levels: Vec<LevelTexture>,
    /// `binds[i]` builds level `i + 1` from level `i`.
    binds: Vec<wgpu::BindGroup>,
}

impl MipChain {
    /// Every level of a `width` x `height` chain, BC1 sized.
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &MipPipeline,
        width: u32,
        height: u32,
        format: DdsFormat,
    ) -> Self {
        let full = layout::full_chain_levels(width, height) as usize;
        let plan = chain_plan(width, height, full, format);
        let levels: Vec<LevelTexture> = plan
            .iter()
            .enumerate()
            .map(|(i, l)| {
                // Level 0 is copied in; every other level is the shader's.
                let written = if i == 0 {
                    wgpu::TextureUsages::COPY_DST
                } else {
                    wgpu::TextureUsages::STORAGE_BINDING
                };
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tile-level"),
                    size: wgpu::Extent3d {
                        width: l.padded_width,
                        height: l.padded_height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | written | READ_BACK,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                LevelTexture { texture, view }
            })
            .collect();
        let layout = pipeline.pipeline.get_bind_group_layout(0);
        let binds = plan
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, l)| {
                let params = buffer_with(
                    device,
                    queue,
                    "tile-mip-params",
                    wgpu::BufferUsages::UNIFORM,
                    &bytes_of(&[l.width, l.height]),
                );
                let mut entries = vec![
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&levels[i - 1].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&levels[i].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params.as_entire_binding(),
                    },
                ];
                if let Some((decode, encode)) = &pipeline.tables {
                    entries.push(wgpu::BindGroupEntry {
                        binding: 3,
                        resource: decode.as_entire_binding(),
                    });
                    entries.push(wgpu::BindGroupEntry {
                        binding: 4,
                        resource: encode.as_entire_binding(),
                    });
                }
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tile-mip"),
                    layout: &layout,
                    entries: &entries,
                })
            })
            .collect();
        Self {
            plan,
            levels,
            binds,
        }
    }

    /// The full chain's levels.
    pub(crate) fn plan(&self) -> &[LevelPlan] {
        &self.plan
    }

    /// Level `index`'s texture.
    pub(crate) fn level(&self, index: usize) -> &LevelTexture {
        &self.levels[index]
    }

    /// Records the dispatches that build levels `1..levels` into `pass`,
    /// each from the one above it: the pass orders them, as each dispatch
    /// reads the texture the one before it wrote.
    pub(crate) fn record(
        &self,
        pipeline: &MipPipeline,
        pass: &mut wgpu::ComputePass,
        levels: usize,
    ) {
        pass.set_pipeline(&pipeline.pipeline);
        for (bind, l) in self
            .binds
            .iter()
            .zip(&self.plan[1..])
            .take(levels.saturating_sub(1))
        {
            pass.set_bind_group(0, bind, &[]);
            pass.dispatch_workgroups(
                l.padded_width.div_ceil(WORKGROUP),
                l.padded_height.div_ceil(WORKGROUP),
                1,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::compressor::pad_to_block_multiple;
    use crate::dds::compressor::tests::varied;

    #[test]
    fn the_gpu_chain_uses_the_cpu_chains_filter() {
        assert_eq!(gpu_filter(), MipmapGenerator::FILTER);
        // The CPU averages in linear light (#467), so the GPU does.
        assert_eq!(gpu_filter(), MipFilter::LinearLight);
        assert_eq!(gpu_filter().entry_point(), "mip_linear");
        assert_eq!(MipFilter::StoredValues.entry_point(), "mip_stored");
    }

    /// The shader compiles, with no GPU: naga, the compiler wgpu runs on
    /// it, parses and validates it, and writes each filter's entry point as
    /// Metal (MSL, at the versions macOS 15 and 26 give wgpu) and as
    /// Vulkan's SPIR-V. A shader error fails here on every CI host, Linux
    /// included, rather than at the worker's start on a host with that GPU.
    #[test]
    fn the_shader_compiles_for_metal_and_vulkan_with_an_entry_point_per_filter() {
        use naga::back::{msl, spv};
        use naga::valid::{Capabilities, ValidationFlags, Validator};

        let module = naga::front::wgsl::parse_str(SHADER)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(SHADER)));
        let info = Validator::new(ValidationFlags::all(), Capabilities::default())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(SHADER)));
        for filter in [MipFilter::StoredValues, MipFilter::LinearLight] {
            let entry = filter.entry_point();
            assert!(
                module
                    .entry_points
                    .iter()
                    .any(|ep| ep.name == entry && ep.stage == naga::ShaderStage::Compute),
                "{entry} is a compute entry point"
            );
            for lang_version in [(3, 2), (4, 0)] {
                let options = msl::Options {
                    lang_version,
                    ..msl::Options::default()
                };
                let pipeline = msl::PipelineOptions {
                    entry_point: Some((naga::ShaderStage::Compute, entry.to_string())),
                    ..msl::PipelineOptions::default()
                };
                let (source, _) = msl::write_string(&module, &info, &options, &pipeline)
                    .unwrap_or_else(|e| panic!("{entry} in MSL {lang_version:?}: {e}"));
                assert!(source.contains(entry), "{entry} in MSL {lang_version:?}");
            }
            let pipeline = spv::PipelineOptions {
                shader_stage: naga::ShaderStage::Compute,
                entry_point: entry.to_string(),
            };
            let words = spv::write_vec(&module, &info, &spv::Options::default(), Some(&pipeline))
                .unwrap_or_else(|e| panic!("{entry} in SPIR-V: {e}"));
            // SPIR-V's magic number opens every module.
            assert_eq!(words.first(), Some(&0x0723_0203), "{entry} in SPIR-V");
        }
    }

    /// The plan is the CPU chain's levels, padded, placed as `LevelSlots`
    /// places them, for square, odd, non-square, truncated and too-long
    /// requests.
    #[test]
    fn the_plan_is_the_cpu_chain_placed_in_the_payload() {
        for (w, h, levels) in [
            (4096, 4096, 13),
            (4096, 4096, 5),
            (4096, 4096, 20),
            (64, 64, 7),
            (37, 23, 9),
            (1001, 777, 10),
            (64, 1, 3),
            (6, 6, 2),
        ] {
            let cpu = MipmapGenerator::generate_chain_with_count(&varied(w, h), levels);
            let plan = chain_plan(w, h, levels, DdsFormat::BC1);
            assert_eq!(plan.len(), cpu.len(), "{w}x{h} {levels}");
            let mut offset = 0;
            for (l, image) in plan.iter().zip(&cpu) {
                let padded = pad_to_block_multiple(image);
                assert_eq!((l.width, l.height), image.dimensions(), "{w}x{h}");
                assert_eq!((l.padded_width, l.padded_height), padded.dimensions());
                assert_eq!(l.offset, offset);
                assert_eq!(l.len, layout::level_len(l.width, l.height, DdsFormat::BC1));
                offset += l.len;
            }
            assert_eq!(chain_bytes(&plan), offset);
            assert_eq!(
                chain_bytes(&plan),
                layout::chain_len(w, h, plan.len() as u32, DdsFormat::BC1)
            );
        }
    }

    #[test]
    fn no_dimension_or_no_level_plans_nothing() {
        assert!(chain_plan(0, 64, 7, DdsFormat::BC1).is_empty());
        assert!(chain_plan(64, 0, 7, DdsFormat::BC1).is_empty());
        assert!(chain_plan(64, 64, 0, DdsFormat::BC1).is_empty());
        assert_eq!(chain_bytes(&[]), 0);
    }

    #[test]
    fn upload_rows_are_whole_256_byte_steps() {
        for (width, row) in [(4096, 16384), (64, 256), (4, 256), (68, 512), (1004, 4096)] {
            assert_eq!(upload_row_bytes(width), row, "{width}");
        }
    }

    /// The upload as `write_level0` lays it out, in a `Vec`.
    fn uploaded(image: &RgbaImage) -> (Vec<u8>, u32, u32, u32) {
        let (w, h) = image.dimensions();
        let (pw, ph) = (w.next_multiple_of(4), h.next_multiple_of(4));
        let row = upload_row_bytes(pw);
        let mut upload = vec![0xEE; (row * ph) as usize];
        let mut writes = 0;
        write_level0(image, pw, ph, row, |at, bytes| {
            upload[at..at + bytes.len()].copy_from_slice(bytes);
            writes += 1;
        });
        if (pw, ph) == (w, h) && row == w * 4 {
            assert_eq!(writes, 1, "a tile whose rows need no padding is one write");
        }
        (upload, pw, ph, row)
    }

    /// Level 0 in the upload is the CPU's padded level 0, row by row, for
    /// sizes that need no padding, padding on either axis, and rows
    /// narrower than the copy's 256 bytes.
    #[test]
    fn level_0_is_uploaded_as_the_cpu_pads_it() {
        for (w, h) in [
            (64, 64),
            (128, 4),
            (37, 23),
            (6, 6),
            (1, 1),
            (64, 1),
            (1001, 3),
        ] {
            let image = varied(w, h);
            let (upload, pw, ph, row) = uploaded(&image);
            let padded = pad_to_block_multiple(&image);
            for y in 0..ph as usize {
                let texels = (pw * 4) as usize;
                assert!(
                    upload[y * row as usize..][..texels] == padded.as_raw()[y * texels..][..texels],
                    "{w}x{h}: row {y}"
                );
                // Past the texels, the row's padding is never written.
                assert!(
                    upload[y * row as usize + texels..(y + 1) * row as usize]
                        .iter()
                        .all(|&b| b == 0xEE),
                    "{w}x{h}: row {y} past its texels"
                );
            }
        }
    }

    #[test]
    fn a_4096_tile_is_uploaded_as_it_is() {
        let image = varied(4096, 4096);
        let (upload, ..) = uploaded(&image);
        assert!(upload == *image.as_raw());
    }

    // =========================================================================
    // The shader on hardware (ignored)
    // =========================================================================

    /// A device on `XEL_TEST_GPU_DEVICE` (`integrated` by default), or
    /// `None` when the host has no such GPU.
    fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let selector =
            std::env::var("XEL_TEST_GPU_DEVICE").unwrap_or_else(|_| "integrated".to_string());
        let health = crate::dds::gpu_health::SharedGpuHealth::new();
        crate::dds::create_gpu_resources(&selector, &health)
            .ok()
            .map(|(device, queue, _, _)| (device, queue))
    }

    /// The chain the shader builds from `image` with `filter`: every level
    /// read back at its padded size.
    fn gpu_chain(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &RgbaImage,
        filter: MipFilter,
    ) -> Vec<RgbaImage> {
        let pipeline = MipPipeline::new(device, queue, filter);
        let (w, h) = image.dimensions();
        let chain = MipChain::new(device, queue, &pipeline, w, h, DdsFormat::BC1);
        let level0 = pad_to_block_multiple(image);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &chain.level(0).texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            level0.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(level0.width() * 4),
                rows_per_image: Some(level0.height()),
            },
            wgpu::Extent3d {
                width: level0.width(),
                height: level0.height(),
                depth_or_array_layers: 1,
            },
        );
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            chain.record(&pipeline, &mut pass, chain.plan().len());
        }
        let reads: Vec<(wgpu::Buffer, u32)> = chain
            .plan()
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let row = upload_row_bytes(l.padded_width);
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("test-level"),
                    size: u64::from(row * l.padded_height),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &chain.level(i).texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row),
                            rows_per_image: Some(l.padded_height),
                        },
                    },
                    wgpu::Extent3d {
                        width: l.padded_width,
                        height: l.padded_height,
                        depth_or_array_layers: 1,
                    },
                );
                (buffer, row)
            })
            .collect();
        queue.submit(std::iter::once(encoder.finish()));
        for (buffer, _) in &reads {
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the GPU finishes");
        reads
            .iter()
            .zip(chain.plan())
            .map(|((buffer, row), l)| {
                let data = buffer.slice(..).get_mapped_range().unwrap();
                let texels = (l.padded_width * 4) as usize;
                let bytes = (0..l.padded_height as usize)
                    .flat_map(|y| data[y * *row as usize..][..texels].to_vec())
                    .collect();
                RgbaImage::from_raw(l.padded_width, l.padded_height, bytes).unwrap()
            })
            .collect()
    }

    /// The stored-value filter on the CPU, the reference for the shader's
    /// unused option: every channel's stored bytes, half to even (#394).
    fn downsample_stored(source: &RgbaImage) -> RgbaImage {
        let average4 = |sum: u32| (sum + 1 + ((sum >> 2) & 1)) >> 2;
        let (w, h) = (source.width() / 2, source.height() / 2);
        RgbaImage::from_fn(w, h, |x, y| {
            let p = [
                source.get_pixel(2 * x, 2 * y),
                source.get_pixel(2 * x + 1, 2 * y),
                source.get_pixel(2 * x, 2 * y + 1),
                source.get_pixel(2 * x + 1, 2 * y + 1),
            ];
            image::Rgba(std::array::from_fn(|c| {
                average4(p.iter().map(|q| u32::from(q[c])).sum()) as u8
            }))
        })
    }

    /// The CPU's chain with `filter`, each level padded as the CPU pads
    /// it for a kernel.
    fn cpu_chain(image: &RgbaImage, filter: MipFilter) -> Vec<RgbaImage> {
        let levels = layout::full_chain_levels(image.width(), image.height()) as usize;
        let chain = match filter {
            MipFilter::LinearLight => MipmapGenerator::generate_chain_with_count(image, levels),
            MipFilter::StoredValues => {
                let mut chain = vec![image.clone()];
                while chain.len() < levels {
                    let next = downsample_stored(chain.last().unwrap());
                    chain.push(next);
                }
                chain
            }
        };
        chain
            .iter()
            .map(|l| pad_to_block_multiple(l).into_owned())
            .collect()
    }

    /// The shader's chain, padding included, is the CPU's byte for byte,
    /// with either filter: the linear-light filter against
    /// `downsample_box_2x`, the stored-value one against its reference
    /// above, on the 4096 tile, odd sizes, non-square ones and a single
    /// row.
    #[test]
    #[ignore] // Requires GPU hardware
    fn the_gpu_chain_is_the_cpu_chain_with_either_filter() {
        let Some((device, queue)) = device() else {
            eprintln!("Skipping: no usable GPU");
            return;
        };
        for (w, h) in [
            (4096, 4096),
            (37, 23),
            (1001, 777),
            (4095, 4097),
            (64, 1),
            (6, 6),
        ] {
            let image = varied(w, h);
            for filter in [MipFilter::StoredValues, MipFilter::LinearLight] {
                let gpu = gpu_chain(&device, &queue, &image, filter);
                let cpu = cpu_chain(&image, filter);
                assert_eq!(gpu.len(), cpu.len(), "{w}x{h} {filter:?}");
                for (level, (g, c)) in gpu.iter().zip(&cpu).enumerate() {
                    assert_eq!(g.dimensions(), c.dimensions(), "{w}x{h} {filter:?} {level}");
                    let differ = g
                        .as_raw()
                        .iter()
                        .zip(c.as_raw())
                        .filter(|(a, b)| a != b)
                        .count();
                    assert_eq!(differ, 0, "{w}x{h} {filter:?}: level {level}, bytes differ");
                }
            }
        }
    }

    #[test]
    fn every_byte_survives_the_linear_tables() {
        let t = SrgbTables::new();
        assert_eq!(t.decode.len(), 256);
        assert_eq!(t.encode.len(), 65536);
        assert_eq!((t.decode[0], t.decode[255]), (0, 65535));
        for b in 0..256 {
            assert_eq!(usize::from(t.encode[t.decode[b] as usize]), b, "byte {b}");
        }
        let words = t.encode_words();
        assert_eq!(words.len(), 16384);
        assert_eq!(bytes_of(&words), t.encode);
    }
}
