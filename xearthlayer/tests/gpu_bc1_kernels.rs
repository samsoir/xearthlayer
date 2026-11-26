//! BC1 kernel microbenchmark (#476, phase 1 of #475): how fast and how good
//! `block_compression`'s BC1 kernel is, against a minimal GPU-native BC1
//! kernel (`support/bc1_native.wgsl`, prototype only) and against ISPC on
//! one thread and on every thread, on the same images.
//!
//! GPU figures are kernel time only, from timestamp queries around each
//! dispatch's compute pass: no upload, readback or synchronisation. Each
//! level of a 4096 chain (the CPU chain, small levels padded to a block as
//! production pads them) is dispatched many times; the median per dispatch
//! gives M blocks/s. Quality is the decoded RGB error against the level
//! the kernel was given (RMSE and PSNR), for level 0 and over the whole
//! chain.
//!
//! Requires a GPU with timestamp queries; ignored. Run, in release, one
//! measurement at a time on a quiet machine:
//!
//! ```text
//! cargo test --release -p xearthlayer --test gpu_bc1_kernels -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `XEL_BENCH_GPUS`, `XEL_BENCH_CHUNKS` and `XEL_BENCH_TILE`: see
//! `support/gpu_bench.rs`.

#[path = "support/gpu_bench.rs"]
#[allow(dead_code)]
mod gpu_bench;

use std::time::Duration;

use block_compression::{CompressionVariant, GpuBlockCompressor};
use gpu_bench::*;
use image::RgbaImage;
use xearthlayer::dds::MipmapGenerator;

/// The BC1 kernels measured on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kernel {
    /// `block_compression` 0.10's BC1, as production runs it.
    BlockCompression,
    /// The native prototype, principal-axis fit and nearest-of-four indices.
    NativeFit,
    /// The native prototype with one least-squares refit.
    NativeFitRefine,
}

impl Kernel {
    const ALL: [Kernel; 3] = [
        Kernel::BlockCompression,
        Kernel::NativeFit,
        Kernel::NativeFitRefine,
    ];

    fn name(self) -> &'static str {
        match self {
            Kernel::BlockCompression => "block_compression BC1",
            Kernel::NativeFit => "native fit",
            Kernel::NativeFitRefine => "native fit+refine",
        }
    }
}

/// The 13 levels of a 4096 chain, each padded to whole blocks.
fn chain(image: &RgbaImage) -> Vec<RgbaImage> {
    let levels = MipmapGenerator::full_chain_count(image.width(), image.height());
    MipmapGenerator::generate_chain_with_count(image, levels)
        .iter()
        .map(pad_to_blocks)
        .collect()
}

fn blocks_of(level: &RgbaImage) -> u64 {
    u64::from(level.width() / 4) * u64::from(level.height() / 4)
}

/// Dispatches per level: enough for about 8 M blocks, between 5 and 200.
fn reps_for(blocks: u64) -> u32 {
    (8_000_000u64.div_ceil(blocks)).clamp(5, 200) as u32
}

/// Sum of squared RGB errors and the channel count, to pool over a chain.
fn sq_error(source: &RgbaImage, decoded: &RgbaImage) -> (f64, f64) {
    let (rmse, _) = rgb_error(source, decoded);
    let n = f64::from(source.width()) * f64::from(source.height()) * 3.0;
    (rmse * rmse * n, n)
}

fn psnr_of(sum: f64, n: f64) -> (f64, f64) {
    let mse = sum / n;
    (mse.sqrt(), 10.0 * (255.0 * 255.0 / mse).log10())
}

/// One kernel's figures on one image's chain.
#[derive(Default)]
struct KernelRun {
    /// Per level: size, blocks, median kernel time.
    levels: Vec<(u32, u64, Duration)>,
    level0: (f64, f64),
    /// Blocks of the whole chain byte for byte ISPC's, and all blocks.
    same_as_ispc: (u64, u64),
    chain: (f64, f64),
}

impl KernelRun {
    fn chain_time(&self) -> Duration {
        self.levels.iter().map(|l| l.2).sum()
    }

    fn print(&self, title: &str) {
        let l0 = self.levels[0];
        let total_blocks: u64 = self.levels.iter().map(|l| l.1).sum();
        eprintln!(
            "  {title:<24} level 0: {:>8.3} ms {:>8.1} M blocks/s | chain: {:>8.3} ms {:>8.1} M blocks/s | \
             PSNR level 0 {:.2} dB (RMSE {:.3}), chain {:.2} dB (RMSE {:.3})",
            ms(l0.2),
            l0.1 as f64 / l0.2.as_secs_f64() / 1e6,
            ms(self.chain_time()),
            total_blocks as f64 / self.chain_time().as_secs_f64() / 1e6,
            self.level0.1,
            self.level0.0,
            self.chain.1,
            self.chain.0,
        );
        eprintln!(
            "      blocks identical to ISPC's: {:.2}% of {}",
            100.0 * self.same_as_ispc.0 as f64 / self.same_as_ispc.1 as f64,
            self.same_as_ispc.1
        );
    }

    fn print_levels(&self) {
        let cells: Vec<String> = self
            .levels
            .iter()
            .map(|(size, blocks, t)| {
                format!(
                    "{size}:{:.1}us/{:.0}M",
                    t.as_secs_f64() * 1e6,
                    *blocks as f64 / t.as_secs_f64() / 1e6
                )
            })
            .collect();
        eprintln!(
            "      per level (size:kernel/M blocks/s): {}",
            cells.join(" ")
        );
    }
}

struct Native {
    fit: wgpu::ComputePipeline,
    refine: wgpu::ComputePipeline,
}

impl Native {
    fn new(gpu: &Gpu) -> Self {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("bc1-native"),
                source: wgpu::ShaderSource::Wgsl(include_str!("support/bc1_native.wgsl").into()),
            });
        let fit = pipeline(gpu, &module, "bc1_fit");
        let refine = pipeline(gpu, &module, "bc1_fit_refine");
        Self { fit, refine }
    }
}

/// Runs `kernel` on every level of `levels`, timing it, and returns its
/// figures and its decoded error; `None` when a level's timestamps were
/// unusable (`Gpu::timed_passes` says why).
fn run_kernel(
    gpu: &Gpu,
    kernel: Kernel,
    levels: &[RgbaImage],
    bc: &mut GpuBlockCompressor,
    native: &Native,
) -> Option<KernelRun> {
    let mut run = KernelRun::default();
    let (mut sum, mut n) = (0.0, 0.0);
    for (i, level) in levels.iter().enumerate() {
        let (w, h) = level.dimensions();
        let blocks = blocks_of(level);
        let (_texture, view) = gpu.texture(level);
        let out = gpu.storage("bench-blocks", blocks * 8);
        let reps = reps_for(blocks);
        let timed = match kernel {
            Kernel::BlockCompression => gpu.timed_passes(reps, |pass, _| {
                bc.add_compression_task(CompressionVariant::BC1, &view, w, h, &out, None, None);
                bc.compress(pass);
            }),
            Kernel::NativeFit | Kernel::NativeFitRefine => {
                let pipe = if kernel == Kernel::NativeFit {
                    &native.fit
                } else {
                    &native.refine
                };
                let params = gpu.uniform(&[w, h, 0, 0]);
                let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("bc1-native"),
                    layout: &pipe.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: out.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: params.as_entire_binding(),
                        },
                    ],
                });
                gpu.timed_passes(reps, |pass, _| {
                    pass.set_pipeline(pipe);
                    pass.set_bind_group(0, &bind, &[]);
                    pass.dispatch_workgroups((w / 4).div_ceil(8), (h / 4).div_ceil(8), 1);
                })
            }
        };
        let (mut times, _wall) = timed?;
        run.levels.push((w, blocks, median(&mut times)));
        let bytes = gpu.read(&out, blocks * 8);
        let ispc = ispc_bc1(level);
        run.same_as_ispc.0 += bytes
            .chunks_exact(8)
            .zip(ispc.chunks_exact(8))
            .filter(|(a, b)| a == b)
            .count() as u64;
        run.same_as_ispc.1 += blocks;
        let decoded = decode_bc1(&bytes, w, h);
        let (s, c) = sq_error(level, &decoded);
        if i == 0 {
            run.level0 = psnr_of(s, c);
        }
        sum += s;
        n += c;
    }
    run.chain = psnr_of(sum, n);
    Some(run)
}

#[test]
#[ignore]
fn bc1_kernels_throughput_and_quality() {
    eprintln!("Machine: {}", machine_label());
    let images: Vec<(String, Vec<RgbaImage>)> = test_images()
        .into_iter()
        .map(|(name, img)| (name, chain(&img)))
        .collect();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();

    // ISPC first, on an otherwise idle machine.
    for (name, levels) in &images {
        eprintln!("\n== ISPC BC1 on {name} ==");
        let level0 = &levels[0];
        let b0 = blocks_of(level0) as f64;
        let pin = pin_cpu();
        let one = on_cpu(pin, || {
            time_median(5, || {
                std::hint::black_box(ispc_bc1(level0));
            })
        });
        let all = time_median(10, || {
            std::hint::black_box(ispc_bc1_parallel(level0, &pool));
        });
        assert_eq!(
            ispc_bc1(level0),
            ispc_bc1_parallel(level0, &pool),
            "strips encode the same blocks"
        );
        let chain_one = on_cpu(pin, || {
            time_median(5, || {
                for l in levels {
                    std::hint::black_box(ispc_bc1(l));
                }
            })
        });
        let total_blocks: u64 = levels.iter().map(blocks_of).sum();
        let (mut sum, mut n) = (0.0, 0.0);
        let mut l0 = (0.0, 0.0);
        for (i, l) in levels.iter().enumerate() {
            let decoded = decode_bc1(&ispc_bc1(l), l.width(), l.height());
            let (s, c) = sq_error(l, &decoded);
            if i == 0 {
                l0 = psnr_of(s, c);
            }
            sum += s;
            n += c;
        }
        let chain_err = psnr_of(sum, n);
        eprintln!(
            "  ISPC 1 thread (CPU {pin:?})  level 0: {:>8.3} ms {:>8.1} M blocks/s | chain: {:>8.3} ms {:>8.1} M blocks/s",
            ms(one),
            b0 / one.as_secs_f64() / 1e6,
            ms(chain_one),
            total_blocks as f64 / chain_one.as_secs_f64() / 1e6,
        );
        eprintln!(
            "  ISPC {threads} threads           level 0: {:>8.3} ms {:>8.1} M blocks/s",
            ms(all),
            b0 / all.as_secs_f64() / 1e6,
        );
        eprintln!(
            "  ISPC quality             PSNR level 0 {:.2} dB (RMSE {:.3}), chain {:.2} dB (RMSE {:.3})",
            l0.1, l0.0, chain_err.1, chain_err.0
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
            "  timestamp period {} ns; NVIDIA state: {}",
            gpu.queue.get_timestamp_period(),
            nvidia_state().unwrap_or_else(|| "-".into())
        );
        let mut bc = GpuBlockCompressor::new(gpu.device.clone(), gpu.queue.clone());
        let native = Native::new(&gpu);
        for (name, levels) in &images {
            eprintln!("  -- {name} --");
            // Warm-up: shader compilation, first allocations.
            for kernel in Kernel::ALL {
                run_kernel(&gpu, kernel, &levels[..1], &mut bc, &native);
            }
            for kernel in Kernel::ALL {
                let Some(run) = run_kernel(&gpu, kernel, levels, &mut bc, &native) else {
                    eprintln!("  {}: skipped, its timestamps were unusable", kernel.name());
                    continue;
                };
                run.print(kernel.name());
                run.print_levels();
            }
        }
        eprintln!(
            "  NVIDIA state after: {}",
            nvidia_state().unwrap_or_else(|| "-".into())
        );
    }
}
