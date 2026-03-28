//! Block compressor trait and implementations for DDS texture encoding.
//!
//! This module provides the [`ImageCompressor`] trait that abstracts BCn block
//! compression, allowing different backends to be swapped in without changing
//! the DDS encoding pipeline.
//!
//! # Available Implementations
//!
//! - [`IspcCompressor`] — SIMD-optimized via Intel ISPC (default, 5-10× faster)
//! - [`SoftwareCompressor`] — Pure-Rust fallback (no external dependencies)
//!
//! # Future Implementations
//!
//! - GPU compressor via `wgpu` compute shaders (planned)

use super::types::{DdsError, DdsFormat};
use image::RgbaImage;
use std::sync::Arc;

// =============================================================================
// Trait
// =============================================================================

/// Trait for block-compressing an RGBA image to BCn format.
///
/// Implementations handle the full-surface compression in a single call.
/// The DDS encoder handles mipmap generation and header assembly; this trait
/// is only responsible for compressing one image level.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` for use in the concurrent tile
/// generation pipeline.
pub trait ImageCompressor: Send + Sync {
    /// Compress an RGBA image to BCn block format.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image (dimensions should be multiples of 4)
    /// * `format` - Target compression format (BC1 or BC3)
    ///
    /// # Returns
    ///
    /// Compressed block data (without DDS header).
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;

    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;
}

// =============================================================================
// ISPC compressor (intel_tex_2)
// =============================================================================

/// SIMD-optimized block compressor using Intel's ISPC texture compression.
///
/// This is the recommended compressor for production use. It processes
/// multiple 4×4 blocks simultaneously using CPU SIMD instructions (SSE4/AVX2),
/// providing 5-10× speedup over pure-Rust compression.
///
/// Prebuilt ISPC kernels are included for Linux (x86_64), macOS (x86_64,
/// aarch64), and Windows (x86_64) — no ISPC compiler needed at build time.
pub struct IspcCompressor;

impl ImageCompressor for IspcCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let rgba_data = image.as_raw();
        let stride = width * 4; // tightly packed RGBA

        let surface = intel_tex_2::RgbaSurface {
            data: rgba_data,
            width,
            height,
            stride,
        };

        let compressed = match format {
            DdsFormat::BC1 => intel_tex_2::bc1::compress_blocks(&surface),
            DdsFormat::BC3 => intel_tex_2::bc3::compress_blocks(&surface),
        };

        Ok(compressed)
    }

    fn name(&self) -> &str {
        "ispc"
    }
}

// =============================================================================
// Software compressor (pure Rust fallback)
// =============================================================================

/// Pure-Rust block compressor using hand-rolled BC1/BC3 encoding.
///
/// This compressor processes blocks one at a time without SIMD acceleration.
/// It exists as a fallback for platforms where ISPC prebuilt kernels are not
/// available, and for testing/comparison purposes.
pub struct SoftwareCompressor;

impl ImageCompressor for SoftwareCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);

        let block_size: u32 = match format {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        };

        let mut output = Vec::with_capacity((blocks_wide * blocks_high * block_size) as usize);

        for block_y in 0..blocks_high {
            for block_x in 0..blocks_wide {
                let block = extract_block(image, block_x, block_y);
                let compressed = match format {
                    DdsFormat::BC1 => super::bc1::Bc1Encoder::compress_block(&block).to_vec(),
                    DdsFormat::BC3 => super::bc3::Bc3Encoder::compress_block(&block).to_vec(),
                };
                output.extend_from_slice(&compressed);
            }
        }

        Ok(output)
    }

    fn name(&self) -> &str {
        "software"
    }
}

/// Extract a 4×4 pixel block from the image, padding with black if needed.
fn extract_block(image: &RgbaImage, block_x: u32, block_y: u32) -> [[u8; 4]; 16] {
    let mut block = [[0u8; 4]; 16];

    for y in 0..4 {
        for x in 0..4 {
            let pixel_x = block_x * 4 + x;
            let pixel_y = block_y * 4 + y;

            if pixel_x < image.width() && pixel_y < image.height() {
                let p = image.get_pixel(pixel_x, pixel_y);
                block[(y * 4 + x) as usize] = [p[0], p[1], p[2], p[3]];
            }
        }
    }

    block
}

// =============================================================================
// Default compressor selection
// =============================================================================

/// Create the default block compressor for this platform.
///
/// Returns the ISPC compressor, which uses SIMD-optimized encoding.
/// In the future, this may auto-detect GPU availability and return
/// a wgpu-based compressor when a suitable GPU is present.
pub fn default_compressor() -> Arc<dyn ImageCompressor> {
    Arc::new(IspcCompressor)
}

// =============================================================================
// GPU compressor (wgpu compute shaders)
// =============================================================================

#[cfg(feature = "gpu-encode")]
mod gpu {
    use super::*;
    use block_compression::{CompressionVariant, GpuBlockCompressor};

    /// GPU-accelerated block compressor using wgpu compute shaders.
    ///
    /// Uses the `block_compression` crate to dispatch BC1/BC3 compression
    /// on a GPU via wgpu. The `GpuBlockCompressor` is wrapped in a `Mutex`
    /// because it holds internal mutable state and is not `Send + Sync`.
    ///
    /// # Device Selection
    ///
    /// The constructor accepts a device selector string:
    /// - `"integrated"` — selects an integrated GPU (e.g., AMD iGPU)
    /// - `"discrete"` — selects a discrete GPU (e.g., NVIDIA)
    /// - Any other string — case-insensitive substring match on adapter name
    pub struct WgpuCompressor {
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: std::sync::Mutex<GpuBlockCompressor>,
        adapter_name: String,
    }

    // Safety: wgpu::Device and wgpu::Queue are internally Arc'd and thread-safe.
    // GpuBlockCompressor is protected by Mutex for exclusive access.
    unsafe impl Send for WgpuCompressor {}
    unsafe impl Sync for WgpuCompressor {}

    impl WgpuCompressor {
        /// Returns the name of the selected GPU adapter.
        pub fn adapter_name(&self) -> &str {
            &self.adapter_name
        }

        /// Create a new GPU compressor, selecting an adapter by device type or name.
        ///
        /// # Arguments
        ///
        /// * `gpu_device` - Device selector: "integrated", "discrete", or adapter name substring
        ///
        /// # Errors
        ///
        /// Returns `DdsError::CompressionFailed` if no matching adapter is found or
        /// device creation fails.
        pub fn try_new(gpu_device: &str) -> Result<Self, DdsError> {
            let (device, queue, compressor, adapter_name) = create_gpu_resources(gpu_device)?;

            Ok(Self {
                device,
                queue,
                compressor: std::sync::Mutex::new(compressor),
                adapter_name,
            })
        }
    }

    /// Create GPU resources for block compression.
    ///
    /// Initializes a wgpu device, queue, and `GpuBlockCompressor` for the
    /// selected GPU adapter. This is the shared factory used by both
    /// `WgpuCompressor` (standalone) and the pipeline worker.
    ///
    /// # Arguments
    ///
    /// * `gpu_device` - Device selector: "integrated", "discrete", or adapter name substring
    ///
    /// # Returns
    ///
    /// A tuple of `(Device, Queue, GpuBlockCompressor, adapter_name)`.
    pub fn create_gpu_resources(
        gpu_device: &str,
    ) -> Result<(wgpu::Device, wgpu::Queue, GpuBlockCompressor, String), DdsError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapters: Vec<wgpu::Adapter> =
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        if adapters.is_empty() {
            return Err(DdsError::CompressionFailed(
                "No GPU adapters available".to_string(),
            ));
        }

        let adapter = select_adapter(&adapters, gpu_device)?;
        let info = adapter.get_info();
        let adapter_name = format!("{} ({:?}, {:?})", info.name, info.device_type, info.backend);

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xearthlayer-dds"),
            ..Default::default()
        }))
        .map_err(|e| DdsError::CompressionFailed(format!("Failed to create wgpu device: {e}")))?;

        device.on_uncaptured_error(std::sync::Arc::new(|error| {
            tracing::error!(error = %error, "wgpu uncaptured error (possible device loss)");
        }));

        let compressor = GpuBlockCompressor::new(device.clone(), queue.clone());

        tracing::info!(adapter = %adapter_name, "GPU resources initialized");

        Ok((device, queue, compressor, adapter_name))
    }

    fn select_adapter<'a>(
        adapters: &'a [wgpu::Adapter],
        gpu_device: &str,
    ) -> Result<&'a wgpu::Adapter, DdsError> {
        // Try device type match first
        let target_type = match gpu_device.to_lowercase().as_str() {
            "integrated" => Some(wgpu::DeviceType::IntegratedGpu),
            "discrete" => Some(wgpu::DeviceType::DiscreteGpu),
            _ => None,
        };

        if let Some(device_type) = target_type {
            if let Some(adapter) = adapters
                .iter()
                .find(|a| a.get_info().device_type == device_type)
            {
                return Ok(adapter);
            }
        } else {
            // Name substring match (case-insensitive)
            let needle = gpu_device.to_lowercase();
            if let Some(adapter) = adapters
                .iter()
                .find(|a| a.get_info().name.to_lowercase().contains(&needle))
            {
                return Ok(adapter);
            }
        }

        // Build error with available adapters list
        let available: Vec<String> = adapters
            .iter()
            .map(|a| {
                let info = a.get_info();
                format!(
                    "  - {} ({:?}, {:?})",
                    info.name, info.device_type, info.backend
                )
            })
            .collect();

        Err(DdsError::CompressionFailed(format!(
            "No GPU adapter matching '{}'. Available adapters:\n{}",
            gpu_device,
            available.join("\n"),
        )))
    }

    impl ImageCompressor for WgpuCompressor {
        fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
            let width = image.width();
            let height = image.height();

            if width == 0 || height == 0 {
                return Err(DdsError::InvalidDimensions(width, height));
            }

            let variant = match format {
                DdsFormat::BC1 => CompressionVariant::BC1,
                DdsFormat::BC3 => CompressionVariant::BC3,
            };

            let block_size: u32 = match format {
                DdsFormat::BC1 => 8,
                DdsFormat::BC3 => 16,
            };

            let blocks_wide = width.div_ceil(4);
            let blocks_high = height.div_ceil(4);
            let output_size = (blocks_wide * blocks_high * block_size) as u64;

            // Create GPU texture
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("dds-input"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Upload pixel data
            let rgba_data = image.as_raw();
            let bytes_per_row = width * 4;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );

            let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            // Create output buffer for compressed data
            let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dds-output"),
                size: output_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            // Create readback buffer
            let readback_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dds-readback"),
                size: output_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });

            // Run compression
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dds-compress"),
                });

            {
                let mut compressor = self.compressor.lock().map_err(|e| {
                    DdsError::CompressionFailed(format!("GPU compressor lock poisoned: {e}"))
                })?;

                compressor.add_compression_task(
                    variant,
                    &texture_view,
                    width,
                    height,
                    &output_buffer,
                    None,
                    None,
                );

                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("dds-compute"),
                    timestamp_writes: None,
                });

                compressor.compress(&mut pass);
            }

            // Copy output to readback buffer
            encoder.copy_buffer_to_buffer(&output_buffer, 0, &readback_buffer, 0, output_size);

            self.queue.submit(std::iter::once(encoder.finish()));

            // Map and read back
            let buffer_slice = readback_buffer.slice(..);
            let (sender, receiver) = std::sync::mpsc::channel();
            buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap();
            });
            let _ = self.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            });
            receiver
                .recv()
                .map_err(|e| {
                    DdsError::CompressionFailed(format!("GPU readback channel error: {e}"))
                })?
                .map_err(|e| {
                    DdsError::CompressionFailed(format!("GPU buffer mapping failed: {e}"))
                })?;

            let data = buffer_slice.get_mapped_range();
            let result = data.to_vec();
            drop(data);
            readback_buffer.unmap();

            Ok(result)
        }

        fn name(&self) -> &str {
            "gpu"
        }
    }
}

#[cfg(feature = "gpu-encode")]
pub use gpu::{create_gpu_resources, WgpuCompressor};

/// Create a GPU-accelerated block compressor using wgpu compute shaders.
///
/// # Arguments
///
/// * `gpu_device` - Device selector: "integrated", "discrete", or adapter name substring
///
/// # Errors
///
/// Returns `DdsError::CompressionFailed` if no matching adapter is found.
#[cfg(feature = "gpu-encode")]
pub fn create_wgpu_compressor(gpu_device: &str) -> Result<WgpuCompressor, DdsError> {
    WgpuCompressor::try_new(gpu_device)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ispc_compress_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        // 1 block × 8 bytes
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_ispc_compress_4x4_bc3() {
        let image = RgbaImage::new(4, 4);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        // 1 block × 16 bytes
        assert_eq!(result.len(), 16);
    }

    #[test]
    fn test_ispc_compress_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        // 64×64 blocks × 8 bytes
        assert_eq!(result.len(), 32768);
    }

    #[test]
    fn test_ispc_compress_256x256_bc3() {
        let image = RgbaImage::new(256, 256);
        let compressor = IspcCompressor;
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        // 64×64 blocks × 16 bytes
        assert_eq!(result.len(), 65536);
    }

    #[test]
    fn test_software_compress_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let compressor = SoftwareCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_software_compress_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let compressor = SoftwareCompressor;
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 32768);
    }

    #[test]
    fn test_ispc_and_software_same_output_size() {
        let image = RgbaImage::new(256, 256);
        let ispc = IspcCompressor;
        let software = SoftwareCompressor;

        let ispc_result = ispc.compress(&image, DdsFormat::BC1).unwrap();
        let sw_result = software.compress(&image, DdsFormat::BC1).unwrap();

        assert_eq!(ispc_result.len(), sw_result.len());
    }

    #[test]
    fn test_compressor_zero_dimensions() {
        let image = RgbaImage::new(0, 0);
        let compressor = IspcCompressor;
        assert!(compressor.compress(&image, DdsFormat::BC1).is_err());
    }

    #[test]
    fn test_default_compressor_is_ispc() {
        let compressor = default_compressor();
        assert_eq!(compressor.name(), "ispc");
    }

    #[test]
    fn test_compressor_names() {
        assert_eq!(IspcCompressor.name(), "ispc");
        assert_eq!(SoftwareCompressor.name(), "software");
    }
}

#[cfg(test)]
#[cfg(feature = "gpu-encode")]
mod gpu_tests {
    use super::*;

    #[test]
    fn test_wgpu_compressor_name() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return, // Skip if no GPU
        };
        assert_eq!(compressor.name(), "gpu");
    }

    #[test]
    fn test_wgpu_compress_4x4_bc1() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(4, 4);
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_wgpu_compress_256x256_bc1() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(256, 256);
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 32768);
    }

    #[test]
    fn test_wgpu_compress_256x256_bc3() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(256, 256);
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        assert_eq!(result.len(), 65536);
    }

    #[test]
    fn test_wgpu_and_ispc_same_output_size() {
        let gpu = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let ispc = IspcCompressor;
        let image = RgbaImage::new(256, 256);
        let gpu_result = gpu.compress(&image, DdsFormat::BC1).unwrap();
        let ispc_result = ispc.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(gpu_result.len(), ispc_result.len());
    }

    #[test]
    fn test_wgpu_compress_zero_dimensions() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(0, 0);
        assert!(compressor.compress(&image, DdsFormat::BC1).is_err());
    }

    #[test]
    fn test_wgpu_invalid_device_returns_error() {
        // "nonexistent_device_12345" should never match any adapter
        let result = WgpuCompressor::try_new("nonexistent_device_12345");
        // This should either fail with no matching adapter, or on CI with no GPU at all
        assert!(result.is_err());
    }

    #[test]
    fn test_create_gpu_resources_invalid_device() {
        let result = create_gpu_resources("nonexistent_device_12345");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_gpu_resources_returns_adapter_name() {
        // Skip if no GPU available
        let result = create_gpu_resources("integrated");
        if let Ok((_, _, _, adapter_name)) = result {
            assert!(!adapter_name.is_empty());
        }
    }

    #[test]
    fn test_create_gpu_resources_registers_error_handler() {
        // This test verifies create_gpu_resources still works after
        // adding the on_uncaptured_error handler. Skip if no GPU.
        let result = create_gpu_resources("integrated");
        // Either succeeds (GPU available) or fails with adapter error (no GPU)
        // — neither should panic from the error handler registration.
        match result {
            Ok((_, _, _, name)) => assert!(!name.is_empty()),
            Err(_) => {} // No GPU available, that's fine
        }
    }
}
