//! DDS encoder - main API for encoding images to DDS format.
//!
//! The encoder delegates compression to a backend via [`CompressorBackend`]:
//! - [`ImageCompressor`] backends (ISPC, Software) compress one level at a time
//!   through [`compress_level_into`], each straight into its place in one
//!   buffer of the file's exact size; the encoder manages mipmap iteration via
//!   [`MipmapStream`].
//! - [`MipmapCompressor`] backends (GPU channel) own the full mipmap pipeline;
//!   the encoder only handles DDS header assembly.

use crate::dds::compressor::{
    compress_level_into, default_compressor, ImageCompressor, MipmapCompressor,
};
use crate::dds::layout;
use crate::dds::mipmap::MipmapStream;
use crate::dds::types::{DdsError, DdsFormat, DdsHeader};
use image::RgbaImage;
use std::sync::Arc;
use tracing::debug;

/// Backend for compressing image data.
enum CompressorBackend {
    /// Single-level compressor — encoder manages mipmap iteration.
    Image(Arc<dyn ImageCompressor>),
    /// Full-pipeline compressor — backend owns mipmap iteration.
    Mipmap(Arc<dyn MipmapCompressor>),
}

/// DDS encoder configuration.
pub struct DdsEncoder {
    format: DdsFormat,
    generate_mipmaps: bool,
    mipmap_count: Option<usize>,
    backend: CompressorBackend,
}

impl DdsEncoder {
    /// Create a new DDS encoder with the specified format.
    ///
    /// Uses the default ISPC SIMD compressor for block compression.
    /// By default, generates full mipmap chain down to 1×1.
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            generate_mipmaps: true,
            mipmap_count: None,
            backend: CompressorBackend::Image(default_compressor()),
        }
    }

    /// Use a specific image compressor backend (ISPC, Software).
    ///
    /// The encoder manages mipmap iteration internally via [`MipmapStream`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::dds::{DdsEncoder, DdsFormat, SoftwareCompressor};
    /// use std::sync::Arc;
    ///
    /// let encoder = DdsEncoder::new(DdsFormat::BC1)
    ///     .with_compressor(Arc::new(SoftwareCompressor));
    /// ```
    pub fn with_compressor(mut self, compressor: Arc<dyn ImageCompressor>) -> Self {
        self.backend = CompressorBackend::Image(compressor);
        self
    }

    /// Use a mipmap compressor backend that owns the full pipeline.
    ///
    /// The backend receives the source image and returns compressed data
    /// for all mipmap levels. This avoids per-level channel round-trips
    /// for GPU-based compression.
    pub fn with_mipmap_compressor(mut self, compressor: Arc<dyn MipmapCompressor>) -> Self {
        self.backend = CompressorBackend::Mipmap(compressor);
        self
    }

    /// Disable mipmap generation.
    pub fn without_mipmaps(mut self) -> Self {
        self.generate_mipmaps = false;
        self
    }

    /// Set specific number of mipmap levels to generate.
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.generate_mipmaps = true;
        self.mipmap_count = Some(count);
        self
    }

    /// Encode RGBA image to DDS format.
    ///
    /// Takes ownership of the source image to avoid cloning. Mipmap levels
    /// are generated and compressed incrementally — only one uncompressed
    /// level is held in memory at a time.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image (ownership transferred)
    ///
    /// # Returns
    ///
    /// Complete DDS file as bytes (header + compressed data). From an
    /// [`ImageCompressor`] backend the `Vec` has no spare capacity, so
    /// `Bytes::from` adopts exactly the file (#393).
    ///
    /// # Errors
    ///
    /// Returns error if image dimensions are invalid or compression fails.
    pub fn encode(&self, image: RgbaImage) -> Result<Vec<u8>, DdsError> {
        // Before any downsampling or hand-off to a backend (#395).
        self.format.ensure_supported()?;
        let width = image.width();
        let height = image.height();

        if width == 0 || height == 0 {
            return Err(DdsError::InvalidDimensions(width, height));
        }

        let count = if self.generate_mipmaps {
            self.resolve_mipmap_count(width, height)
        } else {
            1
        };
        match &self.backend {
            CompressorBackend::Mipmap(compressor) => {
                self.encode_with_mipmap_compressor(compressor, image, width, height, count)
            }
            CompressorBackend::Image(compressor) => {
                self.encode_mipmap_stream(compressor.as_ref(), image, width, height, count)
            }
        }
    }

    /// Compress an image with its first `count` mipmap levels (1 without
    /// mipmaps) using a streaming pipeline, into one buffer.
    ///
    /// The file's size is known before the first level is compressed
    /// ([`layout::file_len`]), so the output is allocated once at exactly
    /// that size, the header written into its first [`layout::HEADER_LEN`]
    /// bytes and each level compressed straight into its own slice. The
    /// returned `Vec`'s capacity is its length: `Bytes::from` adopts the
    /// allocation whole, so spare capacity would be cached with the tile
    /// (#393: grown by appending, a 4096 tile held 16 MiB for 10.67 MiB,
    /// and every level was compressed to a buffer of its own and copied).
    ///
    /// Each mipmap level is generated, compressed, and dropped before the
    /// next is produced — only one uncompressed level lives in memory at a time.
    fn encode_mipmap_stream(
        &self,
        compressor: &dyn ImageCompressor,
        image: RgbaImage,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<u8>, DdsError> {
        // `count` is a level count `layout::chain_levels` resolved, a u32.
        let levels = count as u32;
        let mut output = vec![0; layout::file_len(width, height, levels, self.format)];
        let (header, payload) = output.split_at_mut(layout::HEADER_LEN);
        header.copy_from_slice(&DdsHeader::new(width, height, levels, self.format).to_bytes());

        debug!(
            width,
            height,
            mipmap_count = count,
            format = ?self.format,
            "Starting fused mipmap encode pipeline"
        );

        compress_chain_into(
            compressor,
            MipmapStream::new(image, count),
            self.format,
            payload,
        )?;

        debug!(
            total_dds_bytes = output.len(),
            "Fused mipmap encode complete"
        );

        Ok(output)
    }

    /// Resolve the number of mipmap levels to generate for an image of the
    /// given dimensions, respecting any explicit `mipmap_count` override.
    ///
    /// When no explicit count is set, counts levels from full size down to 1×1.
    ///
    /// An explicit override is clamped to the full chain length
    /// ([`layout::chain_levels`]). Without the clamp a caller asking for more
    /// levels than the dimensions allow would write that number into the DDS
    /// header while [`MipmapStream`] silently stopped at 1×1, leaving the
    /// header over-declaring what the payload contains.
    fn resolve_mipmap_count(&self, width: u32, height: u32) -> usize {
        let requested = self
            .mipmap_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX));
        layout::chain_levels(width, height, requested) as usize
    }

    /// Delegate compression to a MipmapCompressor backend.
    ///
    /// The backend owns mipmap generation and compression. This method
    /// only handles DDS header assembly.
    fn encode_with_mipmap_compressor(
        &self,
        compressor: &Arc<dyn MipmapCompressor>,
        image: RgbaImage,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<u8>, DdsError> {
        debug!(
            width,
            height,
            mipmap_count = count,
            format = ?self.format,
            backend = compressor.name(),
            "Delegating mipmap chain to MipmapCompressor"
        );

        let compressed = compressor.compress_mipmap_chain(image, self.format, count)?;

        // Timed for the GPU path's stages (#419): the copy behind the header
        // is the whole payload.
        let assembly = tracing::debug_span!(target: "profiling", "dds_header_assembly").entered();
        let started = std::time::Instant::now();
        let header = DdsHeader::new(width, height, count as u32, self.format);
        let mut output = header.to_bytes();
        output.extend_from_slice(&compressed);
        let assemble_us = started.elapsed().as_micros() as u64;
        drop(assembly);

        debug!(
            total_dds_bytes = output.len(),
            compressed_bytes = compressed.len(),
            assemble_us,
            "MipmapCompressor encode complete"
        );

        Ok(output)
    }
}

/// The compressed chain of `image` with `compressor`, without a header:
/// `mipmap_count` levels from full size, at most the full chain.
///
/// The same bytes [`DdsEncoder`] writes behind its header for that
/// compressor, so a tile the GPU channel overflows to ISPC is the tile
/// `texture.compressor = ispc` makes.
pub(crate) fn compress_chain(
    compressor: &dyn ImageCompressor,
    image: RgbaImage,
    format: DdsFormat,
    mipmap_count: usize,
) -> Result<Vec<u8>, DdsError> {
    let (width, height) = image.dimensions();
    let requested = u32::try_from(mipmap_count).unwrap_or(u32::MAX);
    let levels = layout::chain_levels(width, height, Some(requested));
    let mut payload = vec![0; layout::chain_len(width, height, levels, format)];
    compress_chain_into(
        compressor,
        MipmapStream::new(image, levels as usize),
        format,
        &mut payload,
    )?;
    Ok(payload)
}

/// Compress `levels`, largest first, back to back into `payload` with
/// [`compress_level_into`]; together they must fill it exactly.
///
/// The encoder sizes `payload` with [`layout`] and takes its levels from
/// [`MipmapStream`], which agree for any count [`layout::chain_levels`]
/// resolves. Should they ever disagree, this is an error: never a panic, and
/// never a file whose header declares a level its payload holds only as
/// zeros.
fn compress_chain_into(
    compressor: &dyn ImageCompressor,
    levels: impl Iterator<Item = RgbaImage>,
    format: DdsFormat,
    payload: &mut [u8],
) -> Result<(), DdsError> {
    let total = payload.len();
    let mut offset = 0;
    for (index, level) in levels.enumerate() {
        let (width, height) = level.dimensions();
        let len = layout::level_len(width, height, format);
        let slot = payload.get_mut(offset..offset + len).ok_or_else(|| {
            DdsError::CompressionFailed(format!(
                "level {index} ({width}x{height}) runs past the {total}-byte payload at {offset}"
            ))
        })?;
        compress_level_into(compressor, &level, format, slot)?;

        debug!(
            level = index,
            level_width = width,
            level_height = height,
            uncompressed_bytes = width as usize * height as usize * 4,
            compressed_bytes = len,
            "Compressed mipmap level"
        );

        offset += len;
        // `level` dropped here — memory reclaimed before next iteration
    }

    if offset != total {
        return Err(DdsError::CompressionFailed(format!(
            "the mipmap chain filled {offset} of the {total}-byte payload"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::compressor::tests::varied;
    use crate::dds::mipmap::MipmapGenerator;
    use crate::dds::{compress_level, IspcCompressor, SoftwareCompressor};

    #[test]
    fn test_encoder_new() {
        let encoder = DdsEncoder::new(DdsFormat::BC1);
        assert!(encoder.generate_mipmaps);
        assert_eq!(encoder.mipmap_count, None);
    }

    #[test]
    fn test_encoder_without_mipmaps() {
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();
        assert!(!encoder.generate_mipmaps);
    }

    #[test]
    fn test_encoder_with_mipmap_count() {
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        assert!(encoder.generate_mipmaps);
        assert_eq!(encoder.mipmap_count, Some(5));
    }

    #[test]
    fn test_encode_4x4_bc1() {
        let image = RgbaImage::new(4, 4);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Header (128) + 1 block (8) = 136 bytes
        assert_eq!(dds.len(), 128 + 8);

        // Check magic
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn encode_rejects_unsupported_format() {
        let result = DdsEncoder::new(DdsFormat::BC3).encode(RgbaImage::new(4, 4));
        assert!(matches!(result, Err(DdsError::UnsupportedFormat(_))));
    }

    #[test]
    fn encode_rejects_unsupported_format_before_the_mipmap_backend_runs() {
        /// A backend that records whether it was asked to do any work.
        struct Recording(std::sync::atomic::AtomicBool);
        impl MipmapCompressor for Recording {
            fn compress_mipmap_chain(
                &self,
                _image: RgbaImage,
                _format: DdsFormat,
                _mipmap_count: usize,
            ) -> Result<Vec<u8>, DdsError> {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Vec::new())
            }
            fn name(&self) -> &str {
                "recording"
            }
        }

        let backend = Arc::new(Recording(std::sync::atomic::AtomicBool::new(false)));
        let result = DdsEncoder::new(DdsFormat::BC3)
            .with_mipmap_compressor(Arc::clone(&backend) as Arc<dyn MipmapCompressor>)
            .encode(RgbaImage::new(256, 256));

        assert!(matches!(result, Err(DdsError::UnsupportedFormat(_))));
        assert!(
            !backend.0.load(std::sync::atomic::Ordering::SeqCst),
            "the backend must not be reached for an unsupported format"
        );
    }

    #[test]
    fn test_encode_256x256_bc1() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // 256×256 = 64×64 blocks, each 8 bytes
        // Header (128) + data (32768) = 32896
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_encode_with_mipmaps() {
        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // Should have 5 mipmap levels: 256, 128, 64, 32, 16
        // Sizes: 32768 + 8192 + 2048 + 512 + 128 = 43648
        // Total: 128 + 43648 = 43776
        assert_eq!(dds.len(), 43776);
    }

    #[test]
    fn test_encode_zero_dimensions() {
        let image = RgbaImage::new(0, 0);
        let encoder = DdsEncoder::new(DdsFormat::BC1);

        let result = encoder.encode(image);
        assert!(result.is_err());

        match result {
            Err(DdsError::InvalidDimensions(0, 0)) => {}
            _ => panic!("Expected InvalidDimensions error"),
        }
    }

    #[test]
    fn test_compress_with_software_compressor() {
        use crate::dds::SoftwareCompressor;
        use std::sync::Arc;

        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .without_mipmaps()
            .with_compressor(Arc::new(SoftwareCompressor));

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();
        // Header (128) + 64×64 blocks × 8 bytes = 32896
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_compress_with_ispc_compressor() {
        use crate::dds::IspcCompressor;
        use std::sync::Arc;

        let image = RgbaImage::new(256, 256);
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .without_mipmaps()
            .with_compressor(Arc::new(IspcCompressor));

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();
        assert_eq!(dds.len(), 128 + 32768);
    }

    #[test]
    fn test_encode_non_power_of_2() {
        // DDS can handle non-power-of-2 dimensions
        let image = RgbaImage::new(100, 100);
        let encoder = DdsEncoder::new(DdsFormat::BC1).without_mipmaps();

        let result = encoder.encode(image);
        assert!(result.is_ok());

        // 100×100 requires 25×25 blocks (round up)
        // 25 * 25 * 8 = 5000 bytes data
        let dds = result.unwrap();
        assert_eq!(dds.len(), 128 + 5000);
    }

    #[test]
    fn test_full_encode_4096x4096_bc1() {
        let image = RgbaImage::new(4096, 4096);
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_mipmap_count(5);

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let dds = result.unwrap();

        // 5 levels: 4096, 2048, 1024, 512, 256
        // 4096: 1024×1024 blocks * 8 = 8388608
        // 2048: 512×512 blocks * 8 = 2097152
        // 1024: 256×256 blocks * 8 = 524288
        // 512: 128×128 blocks * 8 = 131072
        // 256: 64×64 blocks * 8 = 32768
        // Total data: 11173888
        // Header: 128
        // Total: 11174016
        assert_eq!(dds.len(), 128 + 11_173_888);

        // Verify magic
        assert_eq!(&dds[0..4], b"DDS ");
    }

    #[test]
    fn test_fused_encode_matches_pregenerated() {
        let image = RgbaImage::from_fn(256, 256, |x, y| {
            image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
        });
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(crate::dds::IspcCompressor))
            .with_mipmap_count(5);

        // Expected: the header, then each level of a pre-generated chain.
        let mut expected = DdsHeader::new(256, 256, 5, DdsFormat::BC1).to_bytes();
        for level in MipmapGenerator::generate_chain_with_count(&image, 5) {
            let blocks =
                compress_level(&crate::dds::IspcCompressor, &level, DdsFormat::BC1).unwrap();
            expected.extend_from_slice(&blocks);
        }

        // Fused pipeline should produce identical bytes
        let result = encoder.encode(image).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_encode_dispatches_to_mipmap_compressor() {
        use crate::dds::compressor::MipmapCompressor;

        struct CapturingMipmapCompressor {
            expected_bc1_data: Vec<u8>,
        }

        impl MipmapCompressor for CapturingMipmapCompressor {
            fn compress_mipmap_chain(
                &self,
                image: RgbaImage,
                format: DdsFormat,
                mipmap_count: usize,
            ) -> Result<Vec<u8>, DdsError> {
                assert_eq!(image.dimensions(), (256, 256));
                assert_eq!(format, DdsFormat::BC1);
                assert_eq!(mipmap_count, 5);
                Ok(self.expected_bc1_data.clone())
            }

            fn name(&self) -> &str {
                "capturing-mock"
            }
        }

        // Compute expected compressed size: 5 levels of BC1 from 256×256
        // 256: 64×64×8=32768, 128: 32×32×8=8192, 64: 16×16×8=2048,
        // 32: 8×8×8=512, 16: 4×4×8=128
        let data_size = 32768 + 8192 + 2048 + 512 + 128;
        let mock_data = vec![0xAA; data_size];

        let compressor = CapturingMipmapCompressor {
            expected_bc1_data: mock_data.clone(),
        };

        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_mipmap_compressor(Arc::new(compressor));

        let image = RgbaImage::new(256, 256);
        let result = encoder.encode(image).unwrap();

        // Should be DDS header (128) + mock data
        assert_eq!(result.len(), 128 + data_size);
        // Verify header magic
        assert_eq!(&result[0..4], b"DDS ");
        // Verify our mock data follows the header
        assert_eq!(&result[128..], &mock_data[..]);
    }

    #[test]
    fn test_mipmap_compressor_output_matches_image_compressor() {
        use crate::dds::compressor::{ImageCompressor, MipmapCompressor, SoftwareCompressor};
        use crate::dds::mipmap::MipmapStream;

        /// MipmapCompressor that uses SoftwareCompressor internally,
        /// matching the ImageCompressor path's compression output.
        struct SoftwareMipmapCompressor;

        impl MipmapCompressor for SoftwareMipmapCompressor {
            fn compress_mipmap_chain(
                &self,
                image: RgbaImage,
                format: DdsFormat,
                mipmap_count: usize,
            ) -> Result<Vec<u8>, DdsError> {
                let sw = SoftwareCompressor;
                let mut output = Vec::new();
                for level in MipmapStream::new(image, mipmap_count) {
                    let compressed = sw.compress(&level, format)?;
                    output.extend_from_slice(&compressed);
                }
                Ok(output)
            }

            fn name(&self) -> &str {
                "software-mipmap"
            }
        }

        let image = RgbaImage::new(256, 256);

        // ImageCompressor path (fused MipmapStream loop)
        let image_encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_compressor(Arc::new(SoftwareCompressor));
        let image_result = image_encoder.encode(image.clone()).unwrap();

        // MipmapCompressor path (backend-owned pipeline)
        let mipmap_encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_count(5)
            .with_mipmap_compressor(Arc::new(SoftwareMipmapCompressor));
        let mipmap_result = mipmap_encoder.encode(image).unwrap();

        // Byte-for-byte parity
        assert_eq!(image_result, mipmap_result);
    }

    // =========================================================================
    // Full mipmap chain regression tests (issue #212)
    // =========================================================================
    //
    // A truncated chain makes X-Plane clamp sampling at the last declared
    // level, which undersamples sloped terrain at grazing angles and reads as
    // banding along contour lines. These tests pin the chain to the texture
    // dimensions and keep the DDS header honest about the payload.

    /// Walk a DDS payload level by level and report the levels found.
    ///
    /// Returns the per-level dimensions, asserting along the way that the
    /// payload holds exactly as many bytes as the levels require — no more,
    /// no less. Levels below 4x4 still occupy one whole block, which the
    /// naive `w * h / 2` under-counts.
    fn walk_payload(dds: &[u8], width: u32, height: u32, block_size: usize) -> Vec<(u32, u32)> {
        let declared = u32::from_le_bytes(dds[28..32].try_into().unwrap()) as usize;

        let mut levels = Vec::new();
        let mut offset = 128;
        let (mut w, mut h) = (width, height);

        for _ in 0..declared {
            let bytes = (w.div_ceil(4) as usize) * (h.div_ceil(4) as usize) * block_size;
            assert!(
                offset + bytes <= dds.len(),
                "payload ends early: level {}x{} needs {} bytes at offset {}, file is {}",
                w,
                h,
                bytes,
                offset,
                dds.len()
            );
            offset += bytes;
            levels.push((w, h));

            if w <= 1 || h <= 1 {
                break;
            }
            w /= 2;
            h /= 2;
        }

        assert_eq!(
            offset,
            dds.len(),
            "payload has {} trailing bytes after the declared levels",
            dds.len() - offset
        );
        assert_eq!(
            levels.len(),
            declared,
            "header declares {} levels but the payload holds {}",
            declared,
            levels.len()
        );

        levels
    }

    #[test]
    fn test_default_emits_full_chain_4096_bc1() {
        // The production default: no explicit count, so the chain follows the
        // texture size. This is the acceptance criterion from the work order.
        let image = RgbaImage::new(4096, 4096);
        let dds = DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(crate::dds::SoftwareCompressor))
            .encode(image)
            .unwrap();

        assert_eq!(dds.len(), 11_184_952, "full 13-level BC1 tile size");
        assert_eq!(
            u32::from_le_bytes(dds[28..32].try_into().unwrap()),
            13,
            "header must declare 13 levels"
        );

        let levels = walk_payload(&dds, 4096, 4096, 8);
        assert_eq!(levels.len(), 13);
        assert_eq!(*levels.last().unwrap(), (1, 1), "chain must end at 1x1");
    }

    #[test]
    fn test_header_count_matches_payload_for_odd_and_non_square() {
        // Non-square and non-power-of-two inputs must terminate cleanly
        // rather than panicking or leaving the header over-declaring.
        for (w, h) in [(64, 16), (96, 40), (33, 17), (7, 3), (5, 5), (1, 1)] {
            let dds = DdsEncoder::new(DdsFormat::BC1)
                .with_compressor(Arc::new(crate::dds::SoftwareCompressor))
                .encode(RgbaImage::new(w, h))
                .unwrap();

            let levels = walk_payload(&dds, w, h, 8);
            assert_eq!(
                levels.len(),
                MipmapGenerator::full_chain_count(w, h),
                "{}x{} chain length",
                w,
                h
            );
        }
    }

    #[test]
    fn test_explicit_count_is_clamped_to_full_chain() {
        // Asking for more levels than the dimensions allow must not make the
        // header over-declare what the payload contains.
        let dds = DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(crate::dds::SoftwareCompressor))
            .with_mipmap_count(99)
            .encode(RgbaImage::new(64, 64))
            .unwrap();

        assert_eq!(
            u32::from_le_bytes(dds[28..32].try_into().unwrap()),
            MipmapGenerator::full_chain_count(64, 64) as u32
        );
        walk_payload(&dds, 64, 64, 8);
    }

    #[test]
    fn test_explicit_count_still_truncates() {
        // The override remains usable for tests and special-purpose callers.
        let dds = DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(crate::dds::SoftwareCompressor))
            .with_mipmap_count(3)
            .encode(RgbaImage::new(4096, 4096))
            .unwrap();

        assert_eq!(u32::from_le_bytes(dds[28..32].try_into().unwrap()), 3);
        assert_eq!(walk_payload(&dds, 4096, 4096, 8).len(), 3);
    }

    // =========================================================================
    // One buffer of the file's exact size, each level compressed into its
    // place (#393)
    // =========================================================================

    /// The encoder as it was before #393, kept as the reference the in-place
    /// encode must match byte for byte: the header, then each level
    /// compressed to a buffer of its own and appended to a growing one.
    fn encode_by_appending(
        kernel: &dyn ImageCompressor,
        image: RgbaImage,
        requested: Option<u32>,
    ) -> Vec<u8> {
        let (width, height) = image.dimensions();
        let count = layout::chain_levels(width, height, requested);
        let mut output = DdsHeader::new(width, height, count, DdsFormat::BC1).to_bytes();
        for level in MipmapStream::new(image, count as usize) {
            output.extend_from_slice(&compress_level(kernel, &level, DdsFormat::BC1).unwrap());
        }
        output
    }

    /// An encoder on `kernel` asked for `requested` levels, `None` being the
    /// full chain.
    fn encoder_for(kernel: &Arc<dyn ImageCompressor>, requested: Option<u32>) -> DdsEncoder {
        let encoder = DdsEncoder::new(DdsFormat::BC1).with_compressor(Arc::clone(kernel));
        match requested {
            None => encoder,
            Some(count) => encoder.with_mipmap_count(count as usize),
        }
    }

    fn cpu_kernels() -> [Arc<dyn ImageCompressor>; 2] {
        [Arc::new(IspcCompressor), Arc::new(SoftwareCompressor)]
    }

    /// Tiny, odd, non-square and block-aligned sizes: every way a chain can
    /// end (at 1 x 1, at 4 x 1, below a block) and levels that need padding.
    const SHAPES: [(u32, u32); 11] = [
        (1, 1),
        (2, 2),
        (3, 5),
        (4, 4),
        (7, 3),
        (8, 2),
        (5, 5),
        (33, 17),
        (64, 16),
        (100, 100),
        (256, 256),
    ];

    /// Full chain, one level, a short chain and a request past the chain.
    const REQUESTS: [Option<u32>; 5] = [None, Some(1), Some(2), Some(3), Some(99)];

    #[test]
    fn encode_is_the_appended_levels_byte_for_byte() {
        for kernel in cpu_kernels() {
            for (w, h) in SHAPES {
                for requested in REQUESTS {
                    let image = varied(w, h);
                    let expected = encode_by_appending(kernel.as_ref(), image.clone(), requested);
                    let dds = encoder_for(&kernel, requested).encode(image).unwrap();
                    assert_eq!(dds, expected, "{} {w}x{h} {requested:?}", kernel.name());
                }
            }
        }
    }

    #[test]
    fn encode_without_mipmaps_is_the_first_level_byte_for_byte() {
        for kernel in cpu_kernels() {
            for (w, h) in SHAPES {
                let image = varied(w, h);
                let expected = encode_by_appending(kernel.as_ref(), image.clone(), Some(1));
                let dds = DdsEncoder::new(DdsFormat::BC1)
                    .with_compressor(Arc::clone(&kernel))
                    .without_mipmaps()
                    .encode(image)
                    .unwrap();
                assert_eq!(dds, expected, "{} {w}x{h}", kernel.name());
            }
        }
    }

    #[test]
    fn a_4096_tile_is_the_appended_levels_byte_for_byte() {
        // The production tile, full chain and an explicit count, through ISPC.
        let ispc: Arc<dyn ImageCompressor> = Arc::new(IspcCompressor);
        let image = varied(4096, 4096);
        for requested in [None, Some(5)] {
            let expected = encode_by_appending(ispc.as_ref(), image.clone(), requested);
            let dds = encoder_for(&ispc, requested).encode(image.clone()).unwrap();
            assert_eq!(dds.len(), expected.len(), "{requested:?}");
            assert!(dds == expected, "{requested:?}: the bytes differ");
        }
    }

    /// The levels of a `size` x `size` chain, largest first.
    fn chain(size: u32, count: usize) -> MipmapStream {
        MipmapStream::new(varied(size, size), count)
    }

    #[test]
    fn a_chain_that_ends_before_the_payload_is_an_error() {
        // Sized for three levels, given two: the header would declare a
        // level the payload holds only as zeros.
        let mut payload = vec![0; layout::chain_len(16, 16, 3, DdsFormat::BC1)];
        let result =
            compress_chain_into(&IspcCompressor, chain(16, 2), DdsFormat::BC1, &mut payload);
        assert!(
            matches!(result, Err(DdsError::CompressionFailed(_))),
            "{result:?}"
        );
    }

    #[test]
    fn a_chain_past_the_payload_is_an_error_not_a_panic() {
        for given in [2, 3] {
            let mut payload = vec![0; layout::chain_len(16, 16, 1, DdsFormat::BC1)];
            let result = compress_chain_into(
                &IspcCompressor,
                chain(16, given),
                DdsFormat::BC1,
                &mut payload,
            );
            assert!(
                matches!(result, Err(DdsError::CompressionFailed(_))),
                "{given} levels: {result:?}"
            );
        }
    }

    #[test]
    fn a_chain_that_fills_the_payload_is_its_levels_in_order() {
        let mut payload = vec![0; layout::chain_len(16, 16, 3, DdsFormat::BC1)];
        compress_chain_into(&IspcCompressor, chain(16, 3), DdsFormat::BC1, &mut payload).unwrap();
        let expected: Vec<u8> = chain(16, 3)
            .flat_map(|level| compress_level(&IspcCompressor, &level, DdsFormat::BC1).unwrap())
            .collect();
        assert_eq!(payload, expected);
    }

    #[test]
    fn an_encoded_tile_has_no_spare_capacity() {
        // `Bytes::from` adopts a Vec's whole allocation, so spare capacity
        // stays with the tile in the memory cache and on its way to the DDS
        // disk cache and FUSE, and counts in the `mmap_mb` telemetry: a
        // full 4096 tile grown by appending held 16,777,472 bytes for
        // 11,184,952.
        let ispc: Arc<dyn ImageCompressor> = Arc::new(IspcCompressor);
        let tile = encoder_for(&ispc, None).encode(varied(4096, 4096)).unwrap();
        assert_eq!(tile.len(), 11_184_952);
        assert_eq!(tile.capacity(), tile.len());

        for kernel in cpu_kernels() {
            for (w, h) in SHAPES {
                for requested in REQUESTS {
                    let dds = encoder_for(&kernel, requested)
                        .encode(varied(w, h))
                        .unwrap();
                    assert_eq!(
                        dds.capacity(),
                        dds.len(),
                        "{} {w}x{h} {requested:?}",
                        kernel.name()
                    );
                }
                let single = DdsEncoder::new(DdsFormat::BC1)
                    .with_compressor(Arc::clone(&kernel))
                    .without_mipmaps()
                    .encode(varied(w, h))
                    .unwrap();
                assert_eq!(single.capacity(), single.len(), "{} {w}x{h}", kernel.name());
            }
        }
    }

    /// Minor page faults this thread has taken so far; 0 off Linux.
    fn minor_faults() -> i64 {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: getrusage only writes the rusage it is given, and an
            // all-zero rusage is a valid value of that plain C struct.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) };
            usage.ru_minflt
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    /// Release-build timing of a 4096 x 4096 noise tile, full chain, through
    /// `DdsEncoder` with ISPC, against the encoder before #393
    /// (`encode_by_appending`, which takes the same page faults as that
    /// encoder did), single thread. The two alternate run by run so both see
    /// the same machine load; the source is cloned outside the timed region
    /// and the output dropped outside it. Page faults per encode are the
    /// deterministic part of the difference.
    ///
    /// Run it with glibc pinned as the service pins it (#227), or every
    /// buffer is recycled from an arena and the difference hides: `env
    /// MALLOC_MMAP_THRESHOLD_=1048576 MALLOC_ARENA_MAX=4 cargo test --release
    /// -p xearthlayer --lib time_a_4096_tile_encode -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn time_a_4096_tile_encode() {
        const RUNS: usize = 41;
        let source = varied(4096, 4096);
        let ispc: Arc<dyn ImageCompressor> = Arc::new(IspcCompressor);
        let encoder = encoder_for(&ispc, None);

        let run = |encode: &dyn Fn(RgbaImage) -> Vec<u8>| {
            let image = source.clone();
            let faults = minor_faults();
            let started = std::time::Instant::now();
            let dds = encode(image);
            let elapsed = started.elapsed();
            let faults = minor_faults() - faults;
            let capacity = dds.capacity();
            drop(std::hint::black_box(dds));
            (elapsed, faults, capacity)
        };
        let appending = |image: RgbaImage| encode_by_appending(ispc.as_ref(), image, None);
        let in_place = |image: RgbaImage| encoder.encode(image).unwrap();

        let mut before = Vec::with_capacity(RUNS);
        let mut after = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            before.push(run(&appending));
            after.push(run(&in_place));
        }
        let summary = |runs: &[(std::time::Duration, i64, usize)]| {
            let mut times: Vec<_> = runs.iter().map(|r| r.0).collect();
            let mut faults: Vec<_> = runs.iter().map(|r| r.1).collect();
            times.sort();
            faults.sort();
            format!(
                "median {:.2?} (min {:.2?}, max {:.2?}), {} page faults, capacity {} bytes",
                times[RUNS / 2],
                times[0],
                times[RUNS - 1],
                faults[RUNS / 2],
                runs[0].2
            )
        };
        println!(
            "4096 tile, ISPC, full chain, {RUNS} runs each: appending {}; DdsEncoder {}",
            summary(&before),
            summary(&after)
        );
    }
}
