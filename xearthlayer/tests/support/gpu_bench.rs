//! Shared helpers of the GPU measurement tests (#476): `gpu_calibration`,
//! `gpu_bc1_kernels` and `gpu_mip_prototype`. Every test using them is
//! `#[ignore]`d: they need a GPU, take seconds to minutes, and report
//! figures rather than assert speeds.
//!
//! Environment variables, read by these tests only, never by the library:
//!
//! - `XEL_BENCH_GPUS`: the adapters to measure, comma-separated, each as
//!   `texture.gpu_device` takes it (`integrated`, `discrete`, or part of the
//!   name). Default `integrated,discrete`; an adapter the host lacks is
//!   skipped with a note.
//! - `XEL_BENCH_CHUNKS` and `XEL_BENCH_TILE`: a real ortho tile to measure
//!   on besides the synthetic one, from a chunk cache directory (a region
//!   directory such as `.../Apple Maps/chunks/+37-123`) and the tile's
//!   name in it (`12_1583_655`). The 256 chunk files are only read.
//!
//! Some helpers are used by one test binary only, hence the `dead_code`
//! allowance on the module where it is included.

use std::time::{Duration, Instant};

use image::RgbaImage;
use xearthlayer::dds::gpu_timestamps::KernelTimer;

/// The adapters to measure (`XEL_BENCH_GPUS`).
pub fn gpu_selectors() -> Vec<String> {
    std::env::var("XEL_BENCH_GPUS")
        .unwrap_or_else(|_| "integrated,discrete".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// An integer from the environment, or `default`.
pub fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// =============================================================================
// Labels: every figure is printed with the machine, adapter, driver and build
// =============================================================================

/// The CPU model, its threads, the kernel and the build profile.
pub fn machine_label() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| std::env::consts::OS.to_string());
    let profile = if cfg!(debug_assertions) {
        "DEBUG build (figures meaningless)"
    } else {
        "release build"
    };
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "?".to_string());
    format!(
        "{cpu}, {threads} threads, Linux {kernel}, xearthlayer {} at {commit}, {profile}",
        env!("CARGO_PKG_VERSION")
    )
}

/// The adapter's name, kind, backend and driver.
pub fn adapter_label(info: &wgpu::AdapterInfo) -> String {
    format!(
        "{} ({:?}, {:?}, driver {} {})",
        info.name, info.device_type, info.backend, info.driver, info.driver_info
    )
}

/// The adapter `selector` names, as `texture.gpu_device` would pick it.
pub fn adapter_info(selector: &str) -> Option<wgpu::AdapterInfo> {
    let adapters = xearthlayer::system::enumerate_gpus_raw();
    xearthlayer::system::find_gpu(&adapters, selector)
        .ok()
        .map(wgpu::Adapter::get_info)
}

/// The NVIDIA GPU's performance state, memory clock, temperature and
/// power, when `nvidia-smi` is installed: the laptop's driver drops the
/// discrete GPU to low power states between bursts (#419), which changes
/// every figure, so the harnesses print it around each phase.
pub fn nvidia_state() -> Option<String> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=pstate,clocks.mem,clocks.sm,temperature.gpu,power.draw",
            "--format=csv,noheader",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// =============================================================================
// Test images
// =============================================================================

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash2(x: i64, y: i64, seed: u64) -> u64 {
    splitmix(seed ^ splitmix((x as u64).wrapping_mul(0x1F1F_1F1F) ^ (y as u64) << 32))
}

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Smooth value noise in `0..1` at `scale` pixels per cell.
fn value_noise(x: f32, y: f32, scale: f32, seed: u64) -> f32 {
    let (fx, fy) = (x / scale, y / scale);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let (ix, iy) = (x0 as i64, y0 as i64);
    let v = |dx, dy| unit(hash2(ix + dx, iy + dy, seed));
    let top = v(0, 0) + (v(1, 0) - v(0, 0)) * sx;
    let bottom = v(0, 1) + (v(1, 1) - v(0, 1)) * sx;
    top + (bottom - top) * sy
}

fn fbm(x: f32, y: f32, scale: f32, octaves: u32, seed: u64) -> f32 {
    let (mut sum, mut amp, mut norm, mut s) = (0.0, 1.0, 0.0, scale);
    for o in 0..octaves {
        sum += amp * value_noise(x, y, s, seed.wrapping_add(u64::from(o) * 7919));
        norm += amp;
        amp *= 0.5;
        s *= 0.5;
    }
    sum / norm
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// A synthetic ortho tile that looks like imagery to a block encoder:
/// water with a sandy shore, fields of flat colour with furrows, forest
/// canopy, and a town of streets and roofs with shadows, under sensor
/// noise. Deterministic. Unlike a gradient under noise it has the flat
/// blocks, hard edges and fine texture that decide BC1's cost and error.
pub fn synthetic_ortho(size: u32) -> RgbaImage {
    const SEED: u64 = 476;
    let crops: [[f32; 3]; 6] = [
        [112.0, 128.0, 62.0],
        [146.0, 140.0, 82.0],
        [92.0, 112.0, 58.0],
        [170.0, 152.0, 104.0],
        [128.0, 118.0, 84.0],
        [78.0, 98.0, 52.0],
    ];
    let roofs: [[f32; 3]; 5] = [
        [150.0, 150.0, 155.0],
        [178.0, 96.0, 72.0],
        [208.0, 204.0, 198.0],
        [96.0, 96.0, 104.0],
        [132.0, 118.0, 104.0],
    ];
    let mut img = RgbaImage::new(size, size);
    let rows: Vec<(u32, Vec<u8>)> = {
        use rayon::prelude::*;
        (0..size)
            .into_par_iter()
            .map(|y| {
                let mut row = Vec::with_capacity(size as usize * 4);
                for x in 0..size {
                    let (fx, fy) = (x as f32, y as f32);
                    let land = fbm(fx, fy, 900.0, 5, SEED);
                    let urban = fbm(fx, fy, 700.0, 3, SEED + 1);
                    let wood = fbm(fx, fy, 400.0, 4, SEED + 2);
                    let fine = value_noise(fx, fy, 3.0, SEED + 3);
                    let mut c = if land < 0.36 {
                        // Water, darker offshore.
                        let depth = ((0.36 - land) / 0.36).clamp(0.0, 1.0);
                        mix([70.0, 104.0, 112.0], [28.0, 52.0, 72.0], depth)
                            .map(|v| v + (fine - 0.5) * 4.0)
                    } else if land < 0.385 {
                        // Shore.
                        [196.0, 184.0, 150.0].map(|v| v + (fine - 0.5) * 12.0)
                    } else if urban > 0.6 {
                        // Town: 96 px blocks, 12 px streets, roofs in 24 px lots.
                        let (bx, by) = (x % 96, y % 96);
                        if bx < 12 || by < 12 {
                            [88.0, 88.0, 90.0].map(|v| v + (fine - 0.5) * 10.0)
                        } else {
                            let (lx, ly) = (x / 24, y / 24);
                            let h = hash2(i64::from(lx), i64::from(ly), SEED + 4);
                            let roof = roofs[(h % roofs.len() as u64) as usize];
                            let (ix, iy) = (x % 24, y % 24);
                            let edge = ix < 2 || iy < 2;
                            // A shadow on the lot's south-east side.
                            let shadow = ix > 19 || iy > 19;
                            let k = if edge {
                                0.8
                            } else if shadow {
                                0.45
                            } else {
                                1.0
                            };
                            roof.map(|v| v * k + (fine - 0.5) * 14.0)
                        }
                    } else if wood > 0.58 {
                        // Canopy: blobs of light and shade.
                        let crown = value_noise(fx, fy, 6.0, SEED + 5);
                        mix([34.0, 52.0, 30.0], [72.0, 96.0, 52.0], crown)
                            .map(|v| v + (fine - 0.5) * 16.0)
                    } else {
                        // Fields: 160 px squares of one crop each, furrowed.
                        let (cx, cy) = ((fx / 160.0) as i64, (fy / 160.0) as i64);
                        let h = hash2(cx, cy, SEED + 6);
                        let crop = crops[(h % crops.len() as u64) as usize];
                        let angle = unit(h) * std::f32::consts::PI;
                        let furrow = ((fx * angle.cos() + fy * angle.sin()) * 0.9).sin();
                        crop.map(|v| v + furrow * 5.0 + (fine - 0.5) * 10.0)
                    };
                    // Two straight roads across the land.
                    let on_road = |a: f32, b: f32, w: f32| (a * fx + b * fy).rem_euclid(1500.0) < w;
                    if land >= 0.385 && (on_road(0.8, 0.6, 7.0) || on_road(-0.3, 0.95, 5.0)) {
                        c = [118.0, 116.0, 112.0];
                    }
                    // Sensor noise.
                    let n = (unit(hash2(i64::from(x), i64::from(y), SEED + 7)) - 0.5) * 6.0;
                    for v in c {
                        row.push((v + n).round().clamp(0.0, 255.0) as u8);
                    }
                    row.push(255);
                }
                (y, row)
            })
            .collect()
    };
    for (y, row) in rows {
        let start = y as usize * size as usize * 4;
        img.as_mut()[start..start + row.len()].copy_from_slice(&row);
    }
    img
}

/// The real tile `XEL_BENCH_TILE` from the chunk directory
/// `XEL_BENCH_CHUNKS`, assembled from its 256 JPEG chunks, which are only
/// read; `None` when either is unset or a chunk is missing or not 256 x 256.
pub fn real_tile() -> Option<(String, RgbaImage)> {
    let dir = std::path::PathBuf::from(std::env::var("XEL_BENCH_CHUNKS").ok()?);
    let tile = std::env::var("XEL_BENCH_TILE").ok()?;
    let mut img = RgbaImage::new(4096, 4096);
    for row in 0..16u32 {
        for col in 0..16u32 {
            let path = dir.join(format!("chunk_{tile}_{row}_{col}.cache"));
            let bytes = std::fs::read(&path)
                .map_err(|e| eprintln!("real tile: {}: {e}", path.display()))
                .ok()?;
            let chunk = image::load_from_memory(&bytes).ok()?.to_rgba8();
            if chunk.dimensions() != (256, 256) {
                eprintln!("real tile: {} is not 256x256", path.display());
                return None;
            }
            image::imageops::replace(&mut img, &chunk, i64::from(col * 256), i64::from(row * 256));
        }
    }
    Some((format!("real tile {tile}"), img))
}

/// The images to measure on: the synthetic ortho tile, and the real tile
/// when one is named.
pub fn test_images() -> Vec<(String, RgbaImage)> {
    let mut images = vec![("synthetic ortho".to_string(), synthetic_ortho(4096))];
    images.extend(real_tile());
    images
}

/// `image` padded to whole 4 x 4 blocks by repeating its last column and
/// row, as the production GPU path pads the 2 x 2 and 1 x 1 levels.
pub fn pad_to_blocks(image: &RgbaImage) -> RgbaImage {
    let (w, h) = image.dimensions();
    let (pw, ph) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    if (pw, ph) == (w, h) {
        return image.clone();
    }
    RgbaImage::from_fn(pw, ph, |x, y| *image.get_pixel(x.min(w - 1), y.min(h - 1)))
}

// =============================================================================
// BC1: decode, quality, ISPC
// =============================================================================

/// The BC1 blocks of a `width` x `height` image (whole blocks), decoded.
pub fn decode_bc1(blocks: &[u8], width: u32, height: u32) -> RgbaImage {
    let bw = width.div_ceil(4) as usize;
    let mut out = RgbaImage::new(width, height);
    let mut rgba = [0u8; 64];
    for (i, block) in blocks.chunks_exact(8).enumerate() {
        bcdec_rs::bc1(block, &mut rgba, 16);
        let (bx, by) = ((i % bw) as u32 * 4, (i / bw) as u32 * 4);
        for py in 0..4 {
            for px in 0..4 {
                let (x, y) = (bx + px, by + py);
                if x < width && y < height {
                    let at = (py * 16 + px * 4) as usize;
                    out.put_pixel(x, y, image::Rgba(rgba[at..at + 4].try_into().unwrap()));
                }
            }
        }
    }
    out
}

/// Root-mean-square error and PSNR (dB) of `decoded` against `source`, over
/// R, G and B.
pub fn rgb_error(source: &RgbaImage, decoded: &RgbaImage) -> (f64, f64) {
    assert_eq!(source.dimensions(), decoded.dimensions());
    let mut sum = 0u64;
    for (a, b) in source
        .as_raw()
        .chunks_exact(4)
        .zip(decoded.as_raw().chunks_exact(4))
    {
        for c in 0..3 {
            let d = i64::from(a[c]) - i64::from(b[c]);
            sum += (d * d) as u64;
        }
    }
    let mse = sum as f64 / (f64::from(source.width()) * f64::from(source.height()) * 3.0);
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    (mse.sqrt(), psnr)
}

/// ISPC's BC1 of `image` (whole blocks) on the calling thread.
pub fn ispc_bc1(image: &RgbaImage) -> Vec<u8> {
    intel_tex_2::bc1::compress_blocks(&intel_tex_2::RgbaSurface {
        data: image.as_raw(),
        width: image.width(),
        height: image.height(),
        stride: image.width() * 4,
    })
}

/// ISPC's BC1 of `image` on `pool`, in strips of 64 rows: the same bytes as
/// [`ispc_bc1`], since BC1 blocks are independent.
pub fn ispc_bc1_parallel(image: &RgbaImage, pool: &rayon::ThreadPool) -> Vec<u8> {
    use rayon::prelude::*;
    const STRIP: u32 = 64;
    let (w, h) = image.dimensions();
    let mut out = vec![0u8; (w / 4 * (h / 4) * 8) as usize];
    let strip_out = (w / 4 * (STRIP / 4) * 8) as usize;
    let strip_in = (w * 4 * STRIP) as usize;
    pool.install(|| {
        out.par_chunks_mut(strip_out)
            .zip(image.as_raw().par_chunks(strip_in))
            .for_each(|(o, src)| {
                intel_tex_2::bc1::compress_blocks_into(
                    &intel_tex_2::RgbaSurface {
                        data: src,
                        width: w,
                        height: (src.len() / (w as usize * 4)) as u32,
                        stride: w * 4,
                    },
                    o,
                );
            });
    });
    out
}

/// The median of `samples` (sorted in place).
pub fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Milliseconds, for printing.
pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The median wall time of `runs` calls of `f`, after one call not counted.
pub fn time_median(runs: usize, mut f: impl FnMut()) -> Duration {
    f();
    let mut samples: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    median(&mut samples)
}

// =============================================================================
// GPU
// =============================================================================

/// A device on one adapter, with timestamp queries where it has them.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: wgpu::AdapterInfo,
}

impl Gpu {
    /// The adapter `selector` names (as `texture.gpu_device`), its device
    /// requested with the features production requests: timestamp queries
    /// when the adapter has them. `None` when the host has no such adapter.
    pub fn open(selector: &str) -> Option<Self> {
        let adapters = xearthlayer::system::enumerate_gpus_raw();
        let adapter = xearthlayer::system::find_gpu(&adapters, selector).ok()?;
        let info = adapter.get_info();
        let required_features =
            xearthlayer::dds::gpu_timestamps::requested_features(adapter.features());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xel-bench"),
            required_features,
            ..Default::default()
        }))
        .ok()?;
        Some(Self {
            device,
            queue,
            info,
        })
    }

    /// Whether the device has timestamp queries.
    pub fn has_timestamps(&self) -> bool {
        self.device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
    }

    /// `image` as a sampled RGBA8 texture, as the production path uploads
    /// a level.
    pub fn texture(&self, image: &RgbaImage) -> (wgpu::Texture, wgpu::TextureView) {
        let (w, h) = image.dimensions();
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bench-input"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.write_texture(&texture, image);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// Queues `image` into `texture` (`queue.write_texture`).
    pub fn write_texture(&self, texture: &wgpu::Texture, image: &RgbaImage) {
        let (w, h) = image.dimensions();
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// A storage buffer of `size` bytes that can be copied from.
    pub fn storage(&self, label: &str, size: u64) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// A uniform buffer holding `words`.
    pub fn uniform(&self, words: &[u32]) -> wgpu::Buffer {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bench-params"),
            size: bytes.len().max(16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, &bytes);
        buffer
    }

    /// Submits the queued work and waits for it: the wall time from the
    /// submit to the end of the wait.
    pub fn submit_wait(&self, encoder: wgpu::CommandEncoder) -> Duration {
        let t = Instant::now();
        let index = self.queue.submit(std::iter::once(encoder.finish()));
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(30)),
            })
            .expect("the GPU finishes within 30 s");
        t.elapsed()
    }

    /// The first `size` bytes of `buffer`, read back.
    pub fn read(&self, buffer: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bench-readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, size);
        let (tx, rx) = std::sync::mpsc::channel();
        let index = self.queue.submit(std::iter::once(encoder.finish()));
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(30)),
            })
            .expect("the readback finishes");
        rx.recv().unwrap().expect("the readback maps");
        let data = staging.slice(..).get_mapped_range().unwrap().to_vec();
        staging.unmap();
        data
    }

    /// Records `passes` compute passes, each timed by timestamps, with
    /// `record` filling pass `i`; submits and waits. Returns each pass's
    /// kernel time and the wall time from submit to the end of the wait.
    /// Panics on a device without timestamp queries: check
    /// [`has_timestamps`](Self::has_timestamps) first.
    pub fn timed_passes(
        &self,
        passes: u32,
        mut record: impl FnMut(&mut wgpu::ComputePass<'_>, u32),
    ) -> (Vec<Duration>, Duration) {
        let timer = KernelTimer::new(&self.device, &self.queue, passes).expect("timestamp queries");
        let mut encoder = self.device.create_command_encoder(&Default::default());
        for i in 0..passes {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("bench-pass"),
                timestamp_writes: Some(timer.pass_writes(i)),
            });
            record(&mut pass, i);
        }
        timer.resolve(&mut encoder, passes);
        let t = Instant::now();
        let index = self.queue.submit(std::iter::once(encoder.finish()));
        let mapped = timer.map(passes);
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(60)),
            })
            .expect("the GPU finishes within 60 s");
        let wall = t.elapsed();
        let kernels = timer.read(&mapped, passes).expect("readable timestamps");
        (kernels, wall)
    }
}

/// A compute pipeline for `entry` of `module`, its layout derived from the
/// shader.
pub fn pipeline(gpu: &Gpu, module: &wgpu::ShaderModule, entry: &str) -> wgpu::ComputePipeline {
    gpu.device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: None,
            module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        })
}
