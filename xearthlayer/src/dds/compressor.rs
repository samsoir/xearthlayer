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
//! GPU encoding does not implement [`ImageCompressor`]: it owns the whole mip
//! chain through [`MipmapCompressor`], implemented by
//! [`GpuEncoderChannel`](super::gpu_channel::GpuEncoderChannel), whose worker
//! gets its device from [`create_gpu_resources`].

use super::layout::{self, BLOCK_DIM};
use super::types::{DdsError, DdsFormat};
use image::RgbaImage;
use std::borrow::Cow;
use std::sync::Arc;

/// Pad an image up to a whole number of 4×4 compression blocks.
///
/// The tail of a mipmap chain reaches 2×2 and 1×1, which are smaller than a
/// single BCn block. Two backends cannot take such a level:
///
/// - The GPU kernel asserts both dimensions are multiples of 4 and panics
///   otherwise, taking the whole encoder worker down with it.
/// - The ISPC kernel reads a full 4×4 footprint at `stride = width * 4`, so on
///   a tightly packed 2×2 (16-byte) or 1×1 (4-byte) surface it would read out
///   of bounds, and it sizes its output from the pixel area (`ceil(w*h/16)`)
///   rather than `blocks_wide * blocks_high`, one block where an 8×2 level
///   takes two. [`IspcCompressor`] therefore refuses a level that is not whole
///   blocks ([`DdsError::InvalidDimensions`]).
///
/// `SoftwareCompressor` already handles sub-block levels correctly — padding
/// only changes its pad-region pixels from black to edge-replicated.
///
/// Padding once, here, gives every backend a surface it can handle and makes
/// the emitted byte count match the DDS header's `blocks_wide * blocks_high`
/// on every path.
///
/// Padding replicates the edge pixels rather than filling with black, so the
/// compressed block keeps the level's actual color.
///
/// Returns [`Cow::Borrowed`] when the image is already block-aligned, which is
/// every level down to 4×4 — the hot path allocates nothing.
pub(crate) fn pad_to_block_multiple(image: &RgbaImage) -> Cow<'_, RgbaImage> {
    let width = image.width();
    let height = image.height();
    let padded_width = width.next_multiple_of(BLOCK_DIM);
    let padded_height = height.next_multiple_of(BLOCK_DIM);

    if padded_width == width && padded_height == height {
        return Cow::Borrowed(image);
    }

    let mut padded = RgbaImage::new(padded_width, padded_height);
    for y in 0..padded_height {
        // Clamp to the source extent so the padding replicates the edge.
        let src_y = y.min(height - 1);
        for x in 0..padded_width {
            let src_x = x.min(width - 1);
            padded.put_pixel(x, y, *image.get_pixel(src_x, src_y));
        }
    }

    Cow::Owned(padded)
}

/// Block-compress one level of any size with `kernel` into `out`: pad it to
/// whole blocks by replicating its edge (`pad_to_block_multiple`, which says
/// why every kernel needs it), then compress straight into `out`, which must
/// be exactly [`layout::level_len`]`(width, height, format)` bytes, the
/// level's place in a DDS file (the 2 x 2 and 1 x 1 levels at the tail of a
/// chain take one block each).
///
/// This is the one definition of "pad then compress", [`compress_level`]
/// being this into a buffer of its own. [`DdsEncoder`] calls it for every
/// level, each into its slice of one buffer allocated at the file's exact
/// size (#393), and so does the night lighting generator, which encodes only
/// its visible levels and repairs each one after the kernel, naming its
/// kernel rather than taking a default.
///
/// # Errors
///
/// [`DdsError::UnsupportedFormat`] for any format but BC1,
/// [`DdsError::InvalidDimensions`] (with the level's own size) for an empty
/// level and [`DdsError::CompressionFailed`] for `out` of any other length,
/// all before the kernel runs or `out` is written; otherwise whatever the
/// kernel returns, `out` then holding no defined bytes.
///
/// [`DdsEncoder`]: super::DdsEncoder
pub fn compress_level_into(
    kernel: &dyn ImageCompressor,
    level: &RgbaImage,
    format: DdsFormat,
    out: &mut [u8],
) -> Result<(), DdsError> {
    check_level_into(level, format, out)?;
    kernel.compress_into(&pad_to_block_multiple(level), format, out)
}

/// [`compress_level_into`] a buffer of the level's exact size, returned.
///
/// # Errors
///
/// Those of [`compress_level_into`]: an unsupported format or an empty level
/// before the kernel runs, otherwise whatever the kernel returns.
pub fn compress_level(
    kernel: &dyn ImageCompressor,
    level: &RgbaImage,
    format: DdsFormat,
) -> Result<Vec<u8>, DdsError> {
    let (width, height) = level.dimensions();
    let mut blocks = vec![0; layout::level_len(width, height, format)];
    compress_level_into(kernel, level, format, &mut blocks)?;
    Ok(blocks)
}

/// What every entry point that compresses a level into a caller's slice
/// checks before a kernel runs or `out` is written: the format is supported,
/// the level is not empty, and `out` is exactly the level's
/// [`layout::level_len`] bytes.
fn check_level_into(level: &RgbaImage, format: DdsFormat, out: &[u8]) -> Result<(), DdsError> {
    format.ensure_supported()?;
    let (width, height) = level.dimensions();
    if width == 0 || height == 0 {
        return Err(DdsError::InvalidDimensions(width, height));
    }
    let len = layout::level_len(width, height, format);
    if out.len() != len {
        return Err(DdsError::CompressionFailed(format!(
            "a {width}x{height} {format} level takes {len} bytes, the output has {}",
            out.len()
        )));
    }
    Ok(())
}

// =============================================================================
// Trait
// =============================================================================

/// Trait for block-compressing an RGBA image to BCn format.
///
/// Implementations handle the full-surface compression in a single call.
/// The DDS encoder handles mipmap generation and header assembly; this trait
/// is only responsible for compressing one image level. Callers go through
/// [`compress_level_into`] or [`compress_level`], which pad a level to whole
/// blocks first.
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
    /// * `image` - Source RGBA image (dimensions should be multiples of 4;
    ///   a backend may refuse others, as [`IspcCompressor`] does)
    /// * `format` - Target compression format. Only BC1 is supported; any
    ///   other format must be rejected with [`DdsError::UnsupportedFormat`]
    ///   (see [`DdsFormat`]).
    ///
    /// # Returns
    ///
    /// Compressed block data (without DDS header).
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError>;

    /// Compress an RGBA image into `out`, which must be exactly
    /// [`layout::level_len`]`(width, height, format)` bytes: the image's
    /// place as a level of a DDS file, so a whole file can be written into
    /// one buffer allocated once at its size (#393).
    ///
    /// The default calls [`compress`](Self::compress) and copies its blocks,
    /// so a backend that only implements `compress` works unchanged
    /// ([`SoftwareCompressor`]); a backend that can write into a slice
    /// overrides it ([`IspcCompressor`]).
    ///
    /// # Errors
    ///
    /// [`DdsError::UnsupportedFormat`] for any format but BC1,
    /// [`DdsError::InvalidDimensions`] for an empty image and
    /// [`DdsError::CompressionFailed`] for `out` of any other length, all
    /// before `out` is written: an implementation must never panic on a
    /// wrong length. Otherwise whatever `compress` returns, or
    /// [`DdsError::CompressionFailed`] when it returns another number of
    /// bytes than `out` holds.
    fn compress_into(
        &self,
        image: &RgbaImage,
        format: DdsFormat,
        out: &mut [u8],
    ) -> Result<(), DdsError> {
        check_level_into(image, format, out)?;
        let blocks = self.compress(image, format)?;
        if blocks.len() != out.len() {
            return Err(DdsError::CompressionFailed(format!(
                "{} compressed a {}x{} level to {} bytes, not {}",
                self.name(),
                image.width(),
                image.height(),
                blocks.len(),
                out.len()
            )));
        }
        out.copy_from_slice(&blocks);
        Ok(())
    }

    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;
}

// =============================================================================
// Mipmap compressor trait (GPU pipeline)
// =============================================================================

/// Trait for compressing an entire mipmap chain from a source image.
///
/// Unlike [`ImageCompressor`] which compresses a single level (caller manages
/// mipmap iteration), `MipmapCompressor` owns the full pipeline: it receives
/// the source image, generates mipmap levels internally, and returns all
/// compressed levels concatenated.
///
/// This is designed for backends like GPU compute where owning the iteration
/// reduces cross-thread round-trips and enables buffer reuse.
///
/// # Returns
///
/// Concatenated compressed data for all mipmap levels (without DDS header).
/// Level 0 (full resolution) first, then each successively halved level.
pub trait MipmapCompressor: Send + Sync {
    /// Compress a full mipmap chain from the source image.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image (ownership transferred, no clones)
    /// * `format` - Target compression format. Only BC1 is supported; any
    ///   other format must be rejected with [`DdsError::UnsupportedFormat`]
    ///   before any work is done (see [`DdsFormat`]).
    /// * `mipmap_count` - Number of mipmap levels to generate (including original)
    fn compress_mipmap_chain(
        &self,
        image: RgbaImage,
        format: DdsFormat,
        mipmap_count: usize,
    ) -> Result<Vec<u8>, DdsError>;

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
    /// [`compress_into`](Self::compress_into) a buffer of the level's size.
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        let mut blocks = vec![0; layout::level_len(image.width(), image.height(), format)];
        self.compress_into(image, format, &mut blocks)?;
        Ok(blocks)
    }

    /// Compresses straight into `out` with
    /// `intel_tex_2::bc1::compress_blocks_into`, which asserts that `out` is
    /// the kernel's own size for the surface (`calc_output_size`, from the
    /// pixel count: `ceil(w * h / 16) * 8`). Everything is checked first, so
    /// no input reaches that assertion: `out` must be
    /// [`layout::level_len`] bytes and the image whole 4 x 4 blocks, for
    /// which the two sizes are equal. A level that is not whole blocks is
    /// refused ([`DdsError::InvalidDimensions`]) rather than compressed: the
    /// kernel reads a whole block's footprint and would read past the image
    /// (`pad_to_block_multiple`); [`compress_level_into`] pads it first.
    fn compress_into(
        &self,
        image: &RgbaImage,
        format: DdsFormat,
        out: &mut [u8],
    ) -> Result<(), DdsError> {
        check_level_into(image, format, out)?;
        let (width, height) = image.dimensions();
        if !width.is_multiple_of(BLOCK_DIM) || !height.is_multiple_of(BLOCK_DIM) {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let surface = intel_tex_2::RgbaSurface {
            data: image.as_raw(),
            width,
            height,
            stride: width * 4, // tightly packed RGBA
        };

        // BC1 is the only supported format (checked above).
        intel_tex_2::bc1::compress_blocks_into(&surface, out);
        Ok(())
    }

    fn name(&self) -> &str {
        "ispc"
    }
}

// =============================================================================
// Software compressor (pure Rust fallback)
// =============================================================================

/// Pure-Rust block compressor using hand-rolled BC1 encoding.
///
/// This compressor processes blocks one at a time without SIMD acceleration.
/// It exists as a fallback for platforms where ISPC prebuilt kernels are not
/// available, and for testing/comparison purposes.
pub struct SoftwareCompressor;

impl ImageCompressor for SoftwareCompressor {
    fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
        format.ensure_supported()?;
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let blocks_wide = width.div_ceil(BLOCK_DIM);
        let blocks_high = height.div_ceil(BLOCK_DIM);

        // BC1 is the only supported format (checked above).
        let mut output = Vec::with_capacity(layout::level_len(width, height, format));

        for block_y in 0..blocks_high {
            for block_x in 0..blocks_wide {
                let block = extract_block(image, block_x, block_y);
                let compressed = super::bc1::Bc1Encoder::compress_block(&block).to_vec();
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
// GPU resources (wgpu device for the GPU pipeline worker)
// =============================================================================

mod gpu {
    use super::*;
    use block_compression::GpuBlockCompressor;

    /// Create GPU resources for block compression.
    ///
    /// Initializes a wgpu device, queue, and `GpuBlockCompressor` for the
    /// selected GPU adapter, for the GPU pipeline worker
    /// ([`crate::dds::gpu_channel`]).
    ///
    /// # Arguments
    ///
    /// * `gpu_device` - Device selector: "integrated", "discrete", or adapter name substring
    ///
    /// # Returns
    ///
    /// A tuple of `(Device, Queue, GpuBlockCompressor, selection)`, where
    /// `selection` names the GPU that was selected and carries every GPU
    /// the host offered, for the encoder policy (#203).
    pub fn create_gpu_resources(
        gpu_device: &str,
    ) -> Result<
        (
            wgpu::Device,
            wgpu::Queue,
            GpuBlockCompressor,
            crate::system::GpuSelection,
        ),
        DdsError,
    > {
        // Enumeration + selection live in `system::gpu` so the wizard's
        // adapter→config-string mapping (`GpuAdapter::config_value`) stays
        // in lockstep with this module's config-string→adapter mapping.
        let adapters = crate::system::enumerate_gpus_raw();
        let adapter = crate::system::find_gpu(&adapters, gpu_device)
            .map_err(|e| DdsError::CompressionFailed(e.to_string()))?;
        let selected = crate::system::GpuSelection {
            adapter: crate::system::GpuAdapter::from_wgpu(adapter),
            host: crate::system::HardwareProfile::new(
                adapters
                    .iter()
                    .map(crate::system::GpuAdapter::from_wgpu)
                    .collect(),
            ),
        };

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xearthlayer-dds"),
            ..Default::default()
        }))
        .map_err(|e| DdsError::CompressionFailed(format!("Failed to create wgpu device: {e}")))?;

        device.on_uncaptured_error(std::sync::Arc::new(|error| {
            tracing::error!(error = %error, "wgpu uncaptured error (possible device loss)");
        }));

        let compressor = GpuBlockCompressor::new(device.clone(), queue.clone());

        tracing::info!(adapter = %selected.adapter, "GPU resources initialized");

        Ok((device, queue, compressor, selected))
    }
}

pub use gpu::create_gpu_resources;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
pub(crate) mod tests {
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
    fn ispc_rejects_unsupported_format() {
        let result = IspcCompressor.compress(&RgbaImage::new(4, 4), DdsFormat::BC3);
        assert!(matches!(result, Err(DdsError::UnsupportedFormat(_))));
    }

    #[test]
    fn software_rejects_unsupported_format() {
        let result = SoftwareCompressor.compress(&RgbaImage::new(4, 4), DdsFormat::BC3);
        assert!(matches!(result, Err(DdsError::UnsupportedFormat(_))));
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

    // =========================================================================
    // compress_level (#392)
    // =========================================================================

    /// A level whose texels differ from one another: a hash of the position.
    pub(crate) fn varied(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            let h = (u64::from(y) << 32 | u64::from(x)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            image::Rgba([(h >> 40) as u8, (h >> 48) as u8, (h >> 56) as u8, 255])
        })
    }

    #[test]
    fn compress_level_emits_the_layouts_bytes_for_every_level_shape() {
        // The 2 x 2 and 1 x 1 tail of a chain, non-square and odd levels:
        // ISPC alone reads past a sub-block surface and sizes non-square
        // output from the area, so the padding is what makes these whole.
        let kernels: [&dyn ImageCompressor; 2] = [&IspcCompressor, &SoftwareCompressor];
        for kernel in kernels {
            for (w, h) in [
                (1, 1),
                (2, 2),
                (3, 3),
                (4, 4),
                (8, 2),
                (2, 8),
                (5, 7),
                (64, 16),
                (100, 100),
            ] {
                let blocks = compress_level(kernel, &varied(w, h), DdsFormat::BC1).unwrap();
                assert_eq!(
                    blocks.len(),
                    layout::level_len(w, h, DdsFormat::BC1),
                    "{} {w}x{h}",
                    kernel.name()
                );
            }
        }
    }

    #[test]
    fn compress_level_is_the_kernel_on_a_block_aligned_level() {
        let level = varied(64, 32);
        let kernels: [&dyn ImageCompressor; 2] = [&IspcCompressor, &SoftwareCompressor];
        for kernel in kernels {
            assert_eq!(
                compress_level(kernel, &level, DdsFormat::BC1).unwrap(),
                kernel.compress(&level, DdsFormat::BC1).unwrap(),
                "{}",
                kernel.name()
            );
        }
    }

    #[test]
    fn compress_level_pads_by_replicating_the_edge() {
        // A 2 x 3 level compresses as the 4 x 4 level its edges extend to.
        let level = varied(2, 3);
        let extended = RgbaImage::from_fn(4, 4, |x, y| *level.get_pixel(x.min(1), y.min(2)));
        let kernels: [&dyn ImageCompressor; 2] = [&IspcCompressor, &SoftwareCompressor];
        for kernel in kernels {
            assert_eq!(
                compress_level(kernel, &level, DdsFormat::BC1).unwrap(),
                kernel.compress(&extended, DdsFormat::BC1).unwrap(),
                "{}",
                kernel.name()
            );
        }
    }

    /// A kernel that records whether it was called.
    #[derive(Default)]
    struct Recording(std::sync::atomic::AtomicBool);

    impl ImageCompressor for Recording {
        fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            SoftwareCompressor.compress(image, format)
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    impl Recording {
        fn called(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn compress_level_rejects_an_empty_level_naming_its_own_size() {
        for (w, h) in [(0, 0), (0, 5), (5, 0)] {
            let kernel = Recording::default();
            let result = compress_level(&kernel, &RgbaImage::new(w, h), DdsFormat::BC1);
            assert!(
                matches!(result, Err(DdsError::InvalidDimensions(rw, rh)) if (rw, rh) == (w, h)),
                "{w}x{h}: {result:?}"
            );
            assert!(!kernel.called(), "{w}x{h} reached the kernel");
        }
    }

    #[test]
    fn compress_level_rejects_an_unsupported_format_before_the_kernel_runs() {
        let kernel = Recording::default();
        let result = compress_level(&kernel, &RgbaImage::new(2, 2), DdsFormat::BC3);
        assert!(matches!(result, Err(DdsError::UnsupportedFormat(_))));
        assert!(!kernel.called());
    }

    // =========================================================================
    // compress_into and compress_level_into (#393)
    // =========================================================================

    /// What `out` holds before a call, so a test can see whether it was
    /// written.
    const UNTOUCHED: u8 = 0xEE;

    /// Block-aligned levels: square, non-square and not a power of two.
    const WHOLE_BLOCKS: [(u32, u32); 6] =
        [(4, 4), (8, 4), (64, 16), (16, 64), (100, 100), (256, 256)];

    /// Levels of every shape a chain or a caller can hand over, block-aligned
    /// or not.
    const EVERY_SHAPE: [(u32, u32); 11] = [
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        (8, 2),
        (2, 8),
        (5, 7),
        (64, 16),
        (33, 17),
        (100, 100),
        (256, 256),
    ];

    fn untouched(len: usize) -> Vec<u8> {
        vec![UNTOUCHED; len]
    }

    #[test]
    fn compress_into_writes_what_compress_returns() {
        let kernels: [&dyn ImageCompressor; 2] = [&IspcCompressor, &SoftwareCompressor];
        for kernel in kernels {
            for (w, h) in WHOLE_BLOCKS {
                let level = varied(w, h);
                let mut out = untouched(layout::level_len(w, h, DdsFormat::BC1));
                kernel
                    .compress_into(&level, DdsFormat::BC1, &mut out)
                    .unwrap();
                assert_eq!(
                    out,
                    kernel.compress(&level, DdsFormat::BC1).unwrap(),
                    "{} {w}x{h}",
                    kernel.name()
                );
            }
        }
    }

    #[test]
    fn ispc_writes_the_bytes_its_allocating_kernel_returns() {
        // `compress` and `compress_into` both reach the kernel through
        // `compress_blocks_into` now: pin them to `compress_blocks`, the call
        // `compress` made before #393, which allocates its own buffer.
        for (w, h) in WHOLE_BLOCKS {
            let level = varied(w, h);
            let expected = intel_tex_2::bc1::compress_blocks(&intel_tex_2::RgbaSurface {
                data: level.as_raw(),
                width: w,
                height: h,
                stride: w * 4,
            });
            let mut out = untouched(layout::level_len(w, h, DdsFormat::BC1));
            IspcCompressor
                .compress_into(&level, DdsFormat::BC1, &mut out)
                .unwrap();
            assert_eq!(out, expected, "compress_into {w}x{h}");
            assert_eq!(
                IspcCompressor.compress(&level, DdsFormat::BC1).unwrap(),
                expected,
                "compress {w}x{h}"
            );
        }
    }

    #[test]
    fn compress_into_rejects_an_unsupported_format_before_touching_out() {
        let recording = Recording::default();
        let kernels: [&dyn ImageCompressor; 3] = [&IspcCompressor, &SoftwareCompressor, &recording];
        for kernel in kernels {
            // Sized for either format, so only the format is wrong.
            for len in [
                layout::level_len(4, 4, DdsFormat::BC1),
                layout::level_len(4, 4, DdsFormat::BC3),
            ] {
                let mut out = untouched(len);
                let result = kernel.compress_into(&varied(4, 4), DdsFormat::BC3, &mut out);
                assert!(
                    matches!(result, Err(DdsError::UnsupportedFormat(_))),
                    "{}: {result:?}",
                    kernel.name()
                );
                assert_eq!(out, untouched(len), "{} wrote out", kernel.name());
            }
        }
        assert!(!recording.called());
    }

    #[test]
    fn compress_into_refuses_an_out_of_the_wrong_length_without_panicking() {
        // intel_tex_2's compress_blocks_into asserts on the length: a wrong
        // one must be an error before it is reached, not a panic.
        let recording = Recording::default();
        let kernels: [&dyn ImageCompressor; 3] = [&IspcCompressor, &SoftwareCompressor, &recording];
        let len = layout::level_len(16, 16, DdsFormat::BC1);
        for kernel in kernels {
            for wrong in [0, 8, len - 8, len - 1, len + 1, len + 8] {
                let mut out = untouched(wrong);
                let result = kernel.compress_into(&varied(16, 16), DdsFormat::BC1, &mut out);
                assert!(
                    matches!(result, Err(DdsError::CompressionFailed(_))),
                    "{} {wrong} bytes: {result:?}",
                    kernel.name()
                );
                assert_eq!(out, untouched(wrong), "{} wrote out", kernel.name());
            }
        }
        assert!(!recording.called(), "the kernel ran for a wrong length");
    }

    #[test]
    fn ispc_refuses_a_level_that_is_not_whole_blocks() {
        // The kernel reads a whole 4 x 4 footprint, so on a level that is not
        // whole blocks it would read past the image; compress_level_into pads
        // first and never hands it one.
        for (w, h) in [(1, 1), (2, 2), (3, 3), (8, 2), (2, 8), (5, 7), (33, 17)] {
            let mut out = untouched(layout::level_len(w, h, DdsFormat::BC1));
            let result = IspcCompressor.compress_into(&varied(w, h), DdsFormat::BC1, &mut out);
            assert!(
                matches!(result, Err(DdsError::InvalidDimensions(rw, rh)) if (rw, rh) == (w, h)),
                "{w}x{h}: {result:?}"
            );
            assert_eq!(out, untouched(out.len()), "{w}x{h} wrote out");
            let result = IspcCompressor.compress(&varied(w, h), DdsFormat::BC1);
            assert!(
                matches!(result, Err(DdsError::InvalidDimensions(rw, rh)) if (rw, rh) == (w, h)),
                "compress {w}x{h}: {result:?}"
            );
        }
    }

    /// A kernel whose `compress` returns one block too many.
    struct Overlong;

    impl ImageCompressor for Overlong {
        fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
            let mut blocks = SoftwareCompressor.compress(image, format)?;
            blocks.extend_from_slice(&[0; 8]);
            Ok(blocks)
        }

        fn name(&self) -> &str {
            "overlong"
        }
    }

    #[test]
    fn the_default_compress_into_refuses_a_kernel_that_returns_the_wrong_length() {
        let len = layout::level_len(8, 8, DdsFormat::BC1);
        let mut out = untouched(len);
        let result = Overlong.compress_into(&varied(8, 8), DdsFormat::BC1, &mut out);
        assert!(
            matches!(result, Err(DdsError::CompressionFailed(_))),
            "{result:?}"
        );
        assert_eq!(out, untouched(len));
    }

    #[test]
    fn compress_level_into_is_the_kernel_on_the_padded_level() {
        // The reference is compress_level as #392 wrote it: pad, then the
        // kernel's own compress.
        let kernels: [&dyn ImageCompressor; 2] = [&IspcCompressor, &SoftwareCompressor];
        for kernel in kernels {
            for (w, h) in EVERY_SHAPE {
                let level = varied(w, h);
                let expected = kernel
                    .compress(&pad_to_block_multiple(&level), DdsFormat::BC1)
                    .unwrap();
                let mut out = untouched(layout::level_len(w, h, DdsFormat::BC1));
                compress_level_into(kernel, &level, DdsFormat::BC1, &mut out).unwrap();
                assert_eq!(out, expected, "{} {w}x{h}", kernel.name());
                assert_eq!(
                    compress_level(kernel, &level, DdsFormat::BC1).unwrap(),
                    expected,
                    "compress_level {} {w}x{h}",
                    kernel.name()
                );
            }
        }
    }

    #[test]
    fn compress_level_into_rejects_before_the_kernel_runs_or_out_is_touched() {
        let len = layout::level_len(4, 4, DdsFormat::BC1);
        let cases: [(RgbaImage, DdsFormat, usize); 6] = [
            (varied(4, 4), DdsFormat::BC3, len),
            (RgbaImage::new(0, 0), DdsFormat::BC1, 0),
            (RgbaImage::new(0, 5), DdsFormat::BC1, len),
            (varied(4, 4), DdsFormat::BC1, len - 1),
            (varied(4, 4), DdsFormat::BC1, len + 1),
            (varied(2, 2), DdsFormat::BC1, 0),
        ];
        for (n, (level, format, out_len)) in cases.into_iter().enumerate() {
            let kernel = Recording::default();
            let mut out = untouched(out_len);
            let result = compress_level_into(&kernel, &level, format, &mut out);
            assert!(result.is_err(), "case {n}: {result:?}");
            assert!(!kernel.called(), "case {n} reached the kernel");
            assert_eq!(out, untouched(out_len), "case {n} wrote out");
        }
    }

    #[test]
    fn compress_level_into_names_what_it_rejects() {
        let mut out = untouched(8);
        assert!(matches!(
            compress_level_into(&IspcCompressor, &varied(4, 4), DdsFormat::BC3, &mut out),
            Err(DdsError::UnsupportedFormat(_))
        ));
        assert!(matches!(
            compress_level_into(
                &IspcCompressor,
                &RgbaImage::new(0, 5),
                DdsFormat::BC1,
                &mut out
            ),
            Err(DdsError::InvalidDimensions(0, 5))
        ));
        let mut short = untouched(7);
        assert!(matches!(
            compress_level_into(&IspcCompressor, &varied(2, 2), DdsFormat::BC1, &mut short),
            Err(DdsError::CompressionFailed(_))
        ));
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    #[test]
    fn test_create_gpu_resources_invalid_device() {
        let result = create_gpu_resources("nonexistent_device_12345");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_gpu_resources_returns_adapter_name() {
        // Skip if no GPU available
        let result = create_gpu_resources("integrated");
        if let Ok((_, _, _, selection)) = result {
            assert!(!selection.adapter.name.is_empty());
            assert!(
                selection.host.adapters.contains(&selection.adapter),
                "the host profile includes the selected adapter"
            );
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
            Ok((_, _, _, selection)) => assert!(!selection.adapter.name.is_empty()),
            Err(_) => {} // No GPU available, that's fine
        }
    }

    // =========================================================================
    // MipmapCompressor trait tests
    // =========================================================================

    struct MockMipmapCompressor;

    impl MipmapCompressor for MockMipmapCompressor {
        fn compress_mipmap_chain(
            &self,
            _image: RgbaImage,
            _format: DdsFormat,
            _mipmap_count: usize,
        ) -> Result<Vec<u8>, DdsError> {
            Ok(vec![0xAA, 0xBB])
        }

        fn name(&self) -> &str {
            "mock-mipmap"
        }
    }

    #[test]
    fn test_mipmap_compressor_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockMipmapCompressor>();
    }

    #[test]
    fn test_mipmap_compressor_returns_data() {
        let compressor = MockMipmapCompressor;
        let image = RgbaImage::new(256, 256);
        let result = compressor.compress_mipmap_chain(image, DdsFormat::BC1, 5);
        assert_eq!(result.unwrap(), vec![0xAA, 0xBB]);
    }

    #[test]
    fn test_mipmap_compressor_name() {
        let compressor = MockMipmapCompressor;
        assert_eq!(compressor.name(), "mock-mipmap");
    }
}
