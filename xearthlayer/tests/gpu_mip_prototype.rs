//! GPU mip generation prototype (#476, for #424 option (d) and #467): a
//! compute shader (`support/mip_box.wgsl`) that builds each level from the
//! one above with a 2x2 box filter, in two variants:
//!
//! - **stored values**, rounded half to even as #394 rounds: the CPU
//!   chain's filter before #467, timed only here, since the production
//!   CPU chain no longer averages stored values (`dds::gpu_mips` tests it
//!   against a reference of its own);
//! - **linear light**, the CPU chain's filter since #467 (16-bit linear
//!   sums, a 256-entry decode table, half-to-even rounding, a 65,536-entry
//!   encode table): checked byte for byte against
//!   `MipmapGenerator::downsample_box_2x` (through the public
//!   `generate_chain_with_count`) and against a CPU reference of the same
//!   filter written here, on the 4096 test images and on odd sizes.
//!
//! It reports the time per tile of the whole chain on each GPU (kernel time
//! from timestamp queries, and wall time from submit to the end of the
//! wait) against the CPU chain (`MipmapStream`, one thread), and the upload
//! the GPU chain would save: level 0 alone (64 MiB) against every level as
//! production uploads them today (85.33 MiB).
//!
//! Requires a GPU with timestamp queries; ignored. Run, in release:
//!
//! ```text
//! cargo test --release -p xearthlayer --test gpu_mip_prototype -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `XEL_BENCH_GPUS`, `XEL_BENCH_CHUNKS` and `XEL_BENCH_TILE`: see
//! `support/gpu_bench.rs`.

#[path = "support/gpu_bench.rs"]
#[allow(dead_code)]
mod gpu_bench;

use std::time::{Duration, Instant};

use gpu_bench::*;
use image::RgbaImage;
use xearthlayer::dds::{MipmapGenerator, MipmapStream};

// =============================================================================
// The linear-light filter's tables and its CPU reference
// =============================================================================

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB byte to 16-bit linear, and 16-bit linear to the nearest byte.
struct Tables {
    decode: Vec<u32>,
    encode: Vec<u8>,
}

impl Tables {
    fn new() -> Self {
        let decode = (0..256)
            .map(|b| (srgb_to_linear(f64::from(b) / 255.0) * 65535.0).round() as u32)
            .collect();
        let encode = (0..65536)
            .map(|v| (linear_to_srgb(f64::from(v) / 65535.0) * 255.0).round() as u8)
            .collect();
        Self { decode, encode }
    }

    /// The encode table packed four bytes to a little-endian word.
    fn encode_words(&self) -> Vec<u32> {
        self.encode
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }
}

/// The CPU's `quarter_half_to_even`: four values' average from their sum, half to
/// even.
fn average4(sum: u32) -> u32 {
    (sum + 1 + ((sum >> 2) & 1)) >> 2
}

/// One level of the linear-light filter on the CPU, the reference for the
/// GPU's `mip_linear`.
fn downsample_linear(source: &RgbaImage, t: &Tables) -> RgbaImage {
    let (w, h) = (source.width() / 2, source.height() / 2);
    RgbaImage::from_fn(w, h, |x, y| {
        let p = [
            source.get_pixel(2 * x, 2 * y),
            source.get_pixel(2 * x + 1, 2 * y),
            source.get_pixel(2 * x, 2 * y + 1),
            source.get_pixel(2 * x + 1, 2 * y + 1),
        ];
        image::Rgba(std::array::from_fn(|c| {
            if c == 3 {
                average4(p.iter().map(|q| u32::from(q[3])).sum()) as u8
            } else {
                let sum = p.iter().map(|q| t.decode[q[c] as usize]).sum();
                t.encode[average4(sum) as usize]
            }
        }))
    })
}

fn linear_chain(image: &RgbaImage, levels: usize, t: &Tables) -> Vec<RgbaImage> {
    let mut chain = vec![image.clone()];
    while chain.len() < levels {
        let last = chain.last().unwrap();
        if last.width() <= 1 || last.height() <= 1 {
            break;
        }
        chain.push(downsample_linear(last, t));
    }
    chain
}

// =============================================================================
// The GPU chain
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    Stored,
    Linear,
}

/// The level buffers, pipelines and bind groups of one chain shape.
struct GpuChain {
    dims: Vec<(u32, u32)>,
    buffers: Vec<wgpu::Buffer>,
    stored: wgpu::ComputePipeline,
    linear: wgpu::ComputePipeline,
    stored_binds: Vec<wgpu::BindGroup>,
    linear_binds: Vec<wgpu::BindGroup>,
}

impl GpuChain {
    fn new(gpu: &Gpu, width: u32, height: u32, tables: &Tables) -> Self {
        let levels = MipmapGenerator::full_chain_count(width, height);
        let mut dims = vec![(width, height)];
        while dims.len() < levels {
            let (w, h) = *dims.last().unwrap();
            dims.push((w / 2, h / 2));
        }
        let buffers: Vec<wgpu::Buffer> = dims
            .iter()
            .map(|&(w, h)| gpu.storage("mip-level", u64::from(w * h * 4).max(4)))
            .collect();
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("mip-box"),
                source: wgpu::ShaderSource::Wgsl(include_str!("support/mip_box.wgsl").into()),
            });
        let stored = pipeline(gpu, &module, "mip_stored");
        let linear = pipeline(gpu, &module, "mip_linear");
        let words = |v: &[u32]| -> Vec<u8> { v.iter().flat_map(|w| w.to_le_bytes()).collect() };
        let decode = gpu.storage("srgb-decode", 256 * 4);
        gpu.queue.write_buffer(&decode, 0, &words(&tables.decode));
        let encode = gpu.storage("srgb-encode", 65536);
        gpu.queue
            .write_buffer(&encode, 0, &words(&tables.encode_words()));

        let mut stored_binds = Vec::new();
        let mut linear_binds = Vec::new();
        for i in 0..dims.len() - 1 {
            let ((sw, sh), (dw, dh)) = (dims[i], dims[i + 1]);
            let params = gpu.uniform(&[sw, sh, dw, dh]);
            let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffers[i].as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffers[i + 1].as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params.as_entire_binding(),
                },
            ];
            stored_binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mip-stored"),
                layout: &stored.get_bind_group_layout(0),
                entries: &entries,
            }));
            entries.push(wgpu::BindGroupEntry {
                binding: 3,
                resource: decode.as_entire_binding(),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: 4,
                resource: encode.as_entire_binding(),
            });
            linear_binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mip-linear"),
                layout: &linear.get_bind_group_layout(0),
                entries: &entries,
            }));
        }
        Self {
            dims,
            buffers,
            stored,
            linear,
            stored_binds,
            linear_binds,
        }
    }

    /// Queues level 0's upload.
    fn upload(&self, gpu: &Gpu, image: &RgbaImage) {
        gpu.queue.write_buffer(&self.buffers[0], 0, image.as_raw());
    }

    /// Builds every level below level 0 with `filter`, one timed pass per
    /// level: each level's kernel time, and the wall time from submit to
    /// the end of the wait; `None` when the timestamps were unusable (the
    /// levels are built all the same).
    fn build(&self, gpu: &Gpu, filter: Filter) -> Option<(Vec<Duration>, Duration)> {
        let (pipe, binds) = match filter {
            Filter::Stored => (&self.stored, &self.stored_binds),
            Filter::Linear => (&self.linear, &self.linear_binds),
        };
        gpu.timed_passes(binds.len() as u32, |pass, i| {
            let (dw, dh) = self.dims[i as usize + 1];
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, &binds[i as usize], &[]);
            pass.dispatch_workgroups(dw.div_ceil(8), dh.div_ceil(8), 1);
        })
    }

    fn read(&self, gpu: &Gpu) -> Vec<RgbaImage> {
        self.dims
            .iter()
            .zip(&self.buffers)
            .map(|(&(w, h), b)| {
                let bytes = gpu.read(b, u64::from(w * h * 4).max(4));
                RgbaImage::from_raw(w, h, bytes[..(w * h * 4) as usize].to_vec()).unwrap()
            })
            .collect()
    }
}

/// `None` when `gpu` equals `cpu` byte for byte, else where they first
/// differ.
fn first_difference(gpu: &[RgbaImage], cpu: &[RgbaImage]) -> Option<String> {
    if gpu.len() != cpu.len() {
        return Some(format!("{} levels against {}", gpu.len(), cpu.len()));
    }
    for (level, (g, c)) in gpu.iter().zip(cpu).enumerate() {
        if g.dimensions() != c.dimensions() {
            return Some(format!(
                "level {level}: {:?} against {:?}",
                g.dimensions(),
                c.dimensions()
            ));
        }
        if let Some(i) = g.as_raw().iter().zip(c.as_raw()).position(|(a, b)| a != b) {
            let differing = g
                .as_raw()
                .iter()
                .zip(c.as_raw())
                .filter(|(a, b)| a != b)
                .count();
            let px = (i / 4) as u32;
            return Some(format!(
                "level {level} ({}x{}): first at x={} y={} channel {}: GPU {} CPU {}; {differing} bytes differ",
                g.width(),
                g.height(),
                px % g.width(),
                px / g.width(),
                i % 4,
                g.as_raw()[i],
                c.as_raw()[i]
            ));
        }
    }
    None
}

fn odd_image(w: u32, h: u32) -> RgbaImage {
    // Deterministic noise over every byte value.
    let mut state = 0x476u64;
    RgbaImage::from_fn(w, h, |_, _| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        image::Rgba((state >> 32).to_le_bytes()[..4].try_into().unwrap())
    })
}

#[test]
#[ignore]
fn gpu_mip_chain_against_the_cpu_chain() {
    eprintln!("Machine: {}", machine_label());
    let tables = Tables::new();
    let round_trip = (0..256usize)
        .filter(|&b| tables.encode[tables.decode[b] as usize] as usize != b)
        .count();
    assert_eq!(round_trip, 0, "every byte survives decode and encode");

    let images = test_images();

    // The CPU chain on one thread, as the GPU worker runs it today.
    let pin = pin_cpu();
    eprintln!("\n== CPU chain, one thread (pinned to CPU {pin:?}) ==");
    for (name, img) in &images {
        let (stored, linear) = on_cpu(pin, || {
            let mut samples = Vec::new();
            for _ in 0..11 {
                let source = img.clone();
                let t = Instant::now();
                let mut stream = MipmapStream::new(source, 13);
                let mut levels = 0usize;
                while let Some(level) = stream.next_level() {
                    levels += level.width() as usize;
                }
                samples.push(t.elapsed());
                std::hint::black_box(levels);
            }
            let stored = median(&mut samples[1..].to_vec());
            let linear = time_median(2, || {
                std::hint::black_box(linear_chain(img, 13, &tables));
            });
            (stored, linear)
        });
        eprintln!(
            "  {name}: linear-light chain (MipmapStream) {:.2} ms; linear-light reference \
             (unoptimised, per pixel) {:.2} ms",
            ms(stored),
            ms(linear)
        );
    }

    for selector in gpu_selectors() {
        let Some(gpu) = Gpu::open(&selector) else {
            eprintln!("\nSkipping `{selector}`: no such adapter");
            continue;
        };
        eprintln!("\n== GPU `{selector}`: {} ==", adapter_label(&gpu.info));
        if !gpu.has_timestamps() {
            eprintln!("  no timestamp queries: kernel time cannot be measured");
            continue;
        }
        eprintln!(
            "  NVIDIA state: {}",
            nvidia_state().unwrap_or_else(|| "-".into())
        );

        // Byte identity on odd sizes first.
        for (w, h) in [(37, 23), (1001, 777), (4095, 4097), (64, 1)] {
            let img = odd_image(w, h);
            let chain = GpuChain::new(&gpu, w, h, &tables);
            let levels = chain.dims.len();
            chain.upload(&gpu, &img);
            // The production CPU chain averages in linear light (#467), so
            // it is the linear shader it is held against; the stored-value
            // shader is only timed here.
            if levels > 1 {
                chain.build(&gpu, Filter::Linear);
            }
            let cpu = MipmapGenerator::generate_chain_with_count(&img, levels);
            match first_difference(&chain.read(&gpu), &cpu) {
                None => eprintln!("  {w}x{h} ({levels} levels): linear GPU chain identical"),
                Some(d) => eprintln!("  {w}x{h}: linear GPU chain DIFFERS: {d}"),
            }
            match first_difference(&chain.read(&gpu), &linear_chain(&img, levels, &tables)) {
                None => eprintln!("  {w}x{h}: linear GPU chain identical to the CPU reference"),
                Some(d) => eprintln!("  {w}x{h}: linear GPU chain DIFFERS: {d}"),
            }
        }

        let chain = GpuChain::new(&gpu, 4096, 4096, &tables);
        for (name, img) in &images {
            eprintln!("  -- {name}, 4096 x 4096, 13 levels --");
            chain.upload(&gpu, img);
            chain.build(&gpu, Filter::Linear);
            let cpu = MipmapGenerator::generate_chain_with_count(img, 13);
            match first_difference(&chain.read(&gpu), &cpu) {
                None => eprintln!("  linear: identical to downsample_box_2x at every level"),
                Some(d) => eprintln!("  linear: DIFFERS: {d}"),
            }
            match first_difference(&chain.read(&gpu), &linear_chain(img, 13, &tables)) {
                None => eprintln!("  linear: identical to the CPU reference at every level"),
                Some(d) => eprintln!("  linear: DIFFERS: {d}"),
            }

            for filter in [Filter::Stored, Filter::Linear] {
                let Some(runs): Option<Vec<(Vec<Duration>, Duration)>> =
                    (0..21).map(|_| chain.build(&gpu, filter)).collect()
                else {
                    eprintln!("  {filter:?}: not timed, its timestamps were unusable");
                    continue;
                };
                let runs = &runs[1..];
                let mut kernel: Vec<Duration> = runs.iter().map(|r| r.0.iter().sum()).collect();
                let mut wall: Vec<Duration> = runs.iter().map(|r| r.1).collect();
                let per_level: Vec<String> = (0..12)
                    .map(|l| {
                        let mut v: Vec<Duration> = runs.iter().map(|r| r.0[l]).collect();
                        format!("{:.1}", median(&mut v).as_secs_f64() * 1e6)
                    })
                    .collect();
                eprintln!(
                    "  {filter:?}: chain kernel {:.3} ms (median of 20), submit to wait {:.3} ms; \
                     per level (us): {}",
                    ms(median(&mut kernel)),
                    ms(median(&mut wall)),
                    per_level.join(" ")
                );
            }
        }

        // The upload: level 0 alone, against every level into textures as
        // production does now. Each split in two: the queued write, which
        // copies into a wgpu staging buffer on the CPU (production's
        // `upload` stage), and the submit and wait that follow, in which
        // the GPU copies the staging buffer into place (inside production's
        // `gpu` stage). An empty submit and wait is the round trip's floor.
        let img = &images[0].1;
        let cpu = MipmapGenerator::generate_chain_with_count(img, 13);
        let padded: Vec<RgbaImage> = cpu.iter().map(pad_to_blocks).collect();
        let textures: Vec<(wgpu::Texture, wgpu::TextureView)> =
            padded.iter().map(|l| gpu.texture(l)).collect();
        let split = |write: &dyn Fn()| -> (Duration, Duration) {
            let (mut writes, mut waits) = (Vec::new(), Vec::new());
            for i in 0..11 {
                let t = Instant::now();
                write();
                let w = t.elapsed();
                let s = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()));
                if i > 0 {
                    writes.push(w);
                    waits.push(s);
                }
            }
            (median(&mut writes), median(&mut waits))
        };
        let empty = time_median(20, || {
            gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()));
        });
        let buffer0 = split(&|| chain.upload(&gpu, img));
        let texture0 = split(&|| gpu.write_texture(&textures[0].0, &padded[0]));
        let every = split(&|| {
            for (l, (t, _)) in padded.iter().zip(&textures) {
                gpu.write_texture(t, l);
            }
        });
        let mib =
            |v: &[RgbaImage]| v.iter().map(|l| l.as_raw().len()).sum::<usize>() as f64 / 1048576.0;
        eprintln!(
            "  upload (queued write on the CPU + submit and wait; empty submit and wait {:.3} ms):",
            ms(empty)
        );
        for (what, size, (w, s)) in [
            ("level 0 into a storage buffer", mib(&padded[..1]), buffer0),
            ("level 0 into a texture", mib(&padded[..1]), texture0),
            (
                "every level into textures (production)",
                mib(&padded),
                every,
            ),
        ] {
            eprintln!(
                "    {what:<40} {size:>6.2} MiB: write {:.2} ms + submit/wait {:.2} ms = {:.2} ms",
                ms(w),
                ms(s),
                ms(w + s)
            );
        }
        eprintln!(
            "  NVIDIA state after: {}",
            nvidia_state().unwrap_or_else(|| "-".into())
        );
    }
}
