//! DDS texture encoder implementation.
//!
//! Provides a `TextureEncoder` implementation that encodes RGBA images
//! to DirectDraw Surface (DDS) format with BC1 compression.

use crate::dds::{
    default_compressor, layout, AwaitedEncode, DdsEncoder, DdsFormat, ImageCompressor,
};
use crate::texture::{TextureEncoder, TextureError};
use futures::future::BoxFuture;
use image::RgbaImage;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// DDS texture encoder.
///
/// Encodes RGBA images to DDS format with configurable compression
/// and mipmap generation. The compressor backend is shared via `Arc`
/// across clones, allowing efficient reuse in the concurrent pipeline.
///
/// # Example
///
/// ```
/// use xearthlayer::texture::{DdsTextureEncoder, TextureEncoder};
/// use xearthlayer::dds::DdsFormat;
///
/// let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
///
/// assert_eq!(encoder.extension(), "dds");
/// assert_eq!(encoder.name(), "DDS BC1");
/// ```
pub struct DdsTextureEncoder {
    format: DdsFormat,
    /// Mipmap levels to emit; `None` means the full chain for the texture size.
    mipmap_count: Option<usize>,
    /// Image compressor for per-level encoding (ISPC, Software). Always
    /// initialized to the default compressor (ISPC); when
    /// `mipmap_compressor` is set it is that backend's fallback, encoding
    /// the images it hands back (#420).
    compressor: Arc<dyn ImageCompressor>,
    /// When set, `encode()` delegates the full mipmap chain to this backend
    /// instead of iterating levels via `MipmapStream` + `compressor`, which
    /// encodes only what the backend hands back.
    mipmap_compressor: Option<Arc<dyn crate::dds::MipmapCompressor>>,
}

impl Clone for DdsTextureEncoder {
    fn clone(&self) -> Self {
        Self {
            format: self.format,
            mipmap_count: self.mipmap_count,
            compressor: Arc::clone(&self.compressor),
            mipmap_compressor: self.mipmap_compressor.as_ref().map(Arc::clone),
        }
    }
}

impl std::fmt::Debug for DdsTextureEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DdsTextureEncoder");
        s.field("format", &self.format)
            .field("mipmap_count", &self.mipmap_count);

        if let Some(ref mc) = self.mipmap_compressor {
            s.field("compressor", &mc.name())
                .field("fallback", &self.compressor.name());
        } else {
            s.field("compressor", &self.compressor.name());
        }

        s.finish()
    }
}

impl DdsTextureEncoder {
    /// Create a new DDS encoder with the specified compression format.
    ///
    /// By default, generates the full mipmap chain for whatever texture size
    /// is encoded — 13 levels for 4096×4096, down to 1×1.
    ///
    /// # Arguments
    ///
    /// * `format` - DDS compression format (only BC1 is supported, #395)
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: None,
            compressor: default_compressor(),
            mipmap_compressor: None,
        }
    }

    /// Truncate the mipmap chain to a fixed number of levels.
    ///
    /// Intended for tests and special-purpose callers. Production code should
    /// leave this unset so the chain follows the texture dimensions. The count
    /// is clamped to the full chain length at encode time.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of mipmap levels (including base level)
    ///
    /// # Example
    ///
    /// ```
    /// use xearthlayer::texture::DdsTextureEncoder;
    /// use xearthlayer::dds::DdsFormat;
    ///
    /// // Generate 3 mipmap levels: 4096, 2048, 1024
    /// let encoder = DdsTextureEncoder::new(DdsFormat::BC1)
    ///     .with_mipmap_count(3);
    /// ```
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = Some(count);
        self
    }

    /// Set the image compressor backend (ISPC, Software).
    ///
    /// # Arguments
    ///
    /// * `compressor` - A shared image compressor implementation
    pub fn with_compressor(mut self, compressor: Arc<dyn ImageCompressor>) -> Self {
        self.compressor = compressor;
        self
    }

    /// Use a mipmap compressor backend (GPU pipeline).
    ///
    /// When set, the encoder delegates the full mipmap chain to this backend
    /// instead of using the image compressor per level; the image compressor
    /// encodes the images the backend hands back (a failed GPU, #420).
    pub fn with_mipmap_compressor(
        mut self,
        compressor: Arc<dyn crate::dds::MipmapCompressor>,
    ) -> Self {
        self.mipmap_compressor = Some(compressor);
        self
    }

    /// Get the compression format.
    pub fn format(&self) -> DdsFormat {
        self.format
    }

    /// Get the mipmap level override, or `None` for the full chain.
    pub fn mipmap_count(&self) -> Option<usize> {
        self.mipmap_count
    }

    /// Resolve the mipmap chain length for the given dimensions.
    ///
    /// Resolved by [`layout::chain_levels`], as `DdsEncoder` resolves it, so
    /// that `expected_size()` — which becomes the `st_size` FUSE reports to
    /// X-Plane — can never disagree with the number of levels actually
    /// written.
    fn resolve_mipmap_count(&self, width: u32, height: u32) -> u32 {
        let requested = self
            .mipmap_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX));
        layout::chain_levels(width, height, requested)
    }
}

impl DdsTextureEncoder {
    /// The DDS encoder for one encode, on this encoder's backends.
    fn dds_encoder(&self) -> DdsEncoder {
        let mut encoder = DdsEncoder::new(self.format);

        if let Some(count) = self.mipmap_count {
            encoder = encoder.with_mipmap_count(count);
        }

        if let Some(ref mc) = self.mipmap_compressor {
            encoder.with_mipmap_compressor(Arc::clone(mc), Arc::clone(&self.compressor))
        } else {
            encoder.with_compressor(Arc::clone(&self.compressor))
        }
    }
}

impl TextureEncoder for DdsTextureEncoder {
    fn encode(&self, image: RgbaImage) -> Result<Vec<u8>, TextureError> {
        self.dds_encoder().encode(image).map_err(TextureError::from)
    }

    /// With a mipmap backend (the GPU channel), its chain, awaited; with an
    /// image backend, the whole encode left to the CPU (#422).
    fn encode_awaiting<'a>(
        &'a self,
        image: RgbaImage,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AwaitedEncode, TextureError>> {
        Box::pin(async move {
            self.dds_encoder()
                .encode_awaiting(image, cancel)
                .await
                .map_err(TextureError::from)
        })
    }

    /// The image backend's encode, or the mipmap backend's fallback's for
    /// an image it handed back (#422).
    fn encode_on_cpu(&self, image: RgbaImage) -> Result<Vec<u8>, TextureError> {
        self.dds_encoder()
            .encode_on_cpu(image)
            .map_err(TextureError::from)
    }

    fn expected_size(&self, width: u32, height: u32) -> usize {
        let levels = self.resolve_mipmap_count(width, height);
        layout::file_len(width, height, levels, self.format)
    }

    fn extension(&self) -> &str {
        "dds"
    }

    fn name(&self) -> &str {
        match self.format {
            DdsFormat::BC1 => "DDS BC1",
            DdsFormat::BC3 => "DDS BC3",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_new_default_mipmap_count() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.mipmap_count(), None);
        assert_eq!(encoder.format(), DdsFormat::BC1);
    }

    #[test]
    fn test_with_mipmap_count() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3).with_mipmap_count(3);
        assert_eq!(encoder.mipmap_count(), Some(3));
        assert_eq!(encoder.format(), DdsFormat::BC3);
    }

    #[test]
    fn test_extension() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.extension(), "dds");
    }

    #[test]
    fn test_name_bc1() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        assert_eq!(encoder.name(), "DDS BC1");
    }

    #[test]
    fn test_name_bc3() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3);
        assert_eq!(encoder.name(), "DDS BC3");
    }

    #[test]
    fn test_expected_size_4096_bc1_5_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        let size = encoder.expected_size(4096, 4096);

        // 5 levels: 4096, 2048, 1024, 512, 256
        // 4096: 1024×1024 blocks * 8 = 8388608
        // 2048: 512×512 blocks * 8 = 2097152
        // 1024: 256×256 blocks * 8 = 524288
        // 512: 128×128 blocks * 8 = 131072
        // 256: 64×64 blocks * 8 = 32768
        // Total data: 11173888
        // Header: 128
        // Total: 11174016
        assert_eq!(size, 11_174_016);
    }

    #[test]
    fn test_expected_size_4096_bc3_5_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC3).with_mipmap_count(5);
        let size = encoder.expected_size(4096, 4096);

        // BC3 is double BC1 (16 bytes per block vs 8)
        // Data: 11173888 * 2 = 22347776
        // Header: 128
        // Total: 22347904
        assert_eq!(size, 22_347_904);
    }

    #[test]
    fn test_expected_size_256_bc1_no_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let size = encoder.expected_size(256, 256);

        // 256×256 = 64×64 blocks * 8 bytes = 32768 bytes data
        // Header: 128
        // Total: 32896
        assert_eq!(size, 32_896);
    }

    #[test]
    fn test_expected_size_small_image() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let size = encoder.expected_size(4, 4);

        // 4×4 = 1×1 blocks * 8 bytes = 8 bytes data
        // Header: 128
        // Total: 136
        assert_eq!(size, 136);
    }

    #[test]
    fn test_encode_small_image() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let image = RgbaImage::new(4, 4);

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Header (128) + 1 block (8) = 136 bytes
        assert_eq!(data.len(), 136);

        // Check magic
        assert_eq!(&data[0..4], b"DDS ");
    }

    #[test]
    fn test_encode_256x256_bc1() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1);
        let image = RgbaImage::new(256, 256);

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), encoder.expected_size(256, 256));
    }

    #[test]
    fn test_encode_with_mipmaps() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
        let image = RgbaImage::new(256, 256);

        let result = encoder.encode(image);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should have 5 mipmap levels
        assert_eq!(data.len(), encoder.expected_size(256, 256));
    }

    #[test]
    fn test_encode_zero_dimensions() {
        let encoder = DdsTextureEncoder::new(DdsFormat::BC1);
        let image = RgbaImage::new(0, 0);

        let result = encoder.encode(image);
        assert!(result.is_err());

        match result {
            Err(TextureError::InvalidDimensions { width, height, .. }) => {
                assert_eq!(width, 0);
                assert_eq!(height, 0);
            }
            _ => panic!("Expected InvalidDimensions error"),
        }
    }

    #[test]
    fn test_as_trait_object() {
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        assert_eq!(encoder.extension(), "dds");
        assert_eq!(encoder.name(), "DDS BC1");
        assert_eq!(encoder.expected_size(4096, 4096), 11_174_016);
    }

    #[test]
    fn test_clone() {
        let encoder1 = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(3);
        let encoder2 = encoder1.clone();

        assert_eq!(encoder1.format(), encoder2.format());
        assert_eq!(encoder1.mipmap_count(), encoder2.mipmap_count());
    }

    #[test]
    fn test_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DdsTextureEncoder>();
    }

    #[test]
    fn test_with_compressor_software() {
        use crate::dds::SoftwareCompressor;

        let encoder =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));

        // Should encode successfully with software compressor
        let image = RgbaImage::new(4, 4);
        let result = encoder.encode(image);
        assert!(result.is_ok());
    }

    #[test]
    fn test_with_compressor_clone() {
        use crate::dds::SoftwareCompressor;

        let encoder1 =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));
        let encoder2 = encoder1.clone();

        // Both clones should encode successfully
        let image = RgbaImage::new(4, 4);
        assert!(encoder1.encode(image.clone()).is_ok());
        assert!(encoder2.encode(image).is_ok());
    }

    /// With a mipmap backend, the default compressor (ISPC) is its fallback:
    /// an image the backend hands back is the ISPC encode, byte for byte
    /// (#420).
    #[test]
    fn an_image_the_mipmap_backend_hands_back_is_encoded_with_ispc() {
        use crate::dds::{ChainOutcome, DdsError, MipmapCompressor};

        struct HandsBack;
        impl MipmapCompressor for HandsBack {
            fn compress_mipmap_chain(
                &self,
                image: RgbaImage,
                _format: DdsFormat,
                _mipmap_count: usize,
                _output: Vec<u8>,
            ) -> Result<ChainOutcome, DdsError> {
                Ok(ChainOutcome::Returned {
                    image,
                    reason: "GPU device lost".to_string(),
                })
            }
            fn name(&self) -> &str {
                "hands-back"
            }
        }

        let image = RgbaImage::from_fn(256, 256, |x, y| {
            image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
        });
        let gpu =
            DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_compressor(Arc::new(HandsBack));
        let ispc = DdsTextureEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(crate::dds::IspcCompressor));
        let dds = gpu.encode(image.clone()).unwrap();
        assert_eq!(dds.len(), gpu.expected_size(256, 256));
        assert_eq!(dds, ispc.encode(image).unwrap());
    }

    #[test]
    fn test_with_compressor_debug() {
        use crate::dds::SoftwareCompressor;

        let encoder =
            DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(SoftwareCompressor));

        let debug_str = format!("{:?}", encoder);
        assert!(debug_str.contains("DdsTextureEncoder"));
        assert!(debug_str.contains("software"));
    }

    /// With a mipmap backend, the debug output names its fallback too.
    #[test]
    fn debug_names_the_fallback_of_a_mipmap_backend() {
        use crate::dds::{ChainOutcome, DdsError, MipmapCompressor, SoftwareCompressor};

        struct Backend;
        impl MipmapCompressor for Backend {
            fn compress_mipmap_chain(
                &self,
                _image: RgbaImage,
                _format: DdsFormat,
                _mipmap_count: usize,
                output: Vec<u8>,
            ) -> Result<ChainOutcome, DdsError> {
                Ok(ChainOutcome::Compressed(output))
            }
            fn name(&self) -> &str {
                "backend"
            }
        }

        let encoder = DdsTextureEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(SoftwareCompressor))
            .with_mipmap_compressor(Arc::new(Backend));
        let debug_str = format!("{encoder:?}");
        assert!(
            debug_str.contains(r#"compressor: "backend""#),
            "{debug_str}"
        );
        assert!(debug_str.contains(r#"fallback: "software""#), "{debug_str}");
    }
}
