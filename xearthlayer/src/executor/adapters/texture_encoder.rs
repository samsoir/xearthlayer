//! Texture encoder adapter for the executor framework.
//!
//! Adapts the `TextureEncoder` trait to the `TextureEncoderAsync` trait.

use crate::dds::AwaitedEncode;
use crate::executor::{BlockingExecutor, ExecutorError, TextureEncodeError, TextureEncoderAsync};
use crate::texture::{TextureEncoder, TextureError};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Adapts a `TextureEncoder` to the `TextureEncoderAsync` trait.
///
/// A job's encode goes in two steps (#422): the encoder's
/// [`encode_awaiting`](TextureEncoder::encode_awaiting) is awaited, so a
/// GPU encoder's wait for its worker parks no thread, and whatever CPU work
/// is left ([`AwaitedEncode::Cpu`]: the whole encode with a CPU encoder, or
/// an image the GPU handed back) runs on the task's blocking executor, under
/// its CPU permit, as before.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::texture::DdsTextureEncoder;
/// use xearthlayer::dds::DdsFormat;
/// use xearthlayer::executor::adapters::TextureEncoderAdapter;
///
/// let encoder = DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5);
/// let adapter = TextureEncoderAdapter::new(encoder);
/// // adapter implements TextureEncoderAsync
/// ```
pub struct TextureEncoderAdapter<E> {
    encoder: E,
}

impl<E> TextureEncoderAdapter<E> {
    /// Creates a new texture encoder adapter.
    pub fn new(encoder: E) -> Self {
        Self { encoder }
    }
}

impl<E: TextureEncoder + 'static> TextureEncoderAsync for TextureEncoderAdapter<E> {
    fn encode_blocking(&self, image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
        self.encoder.encode(image).map_err(map_texture_error)
    }

    async fn encode<X: BlockingExecutor>(
        self: &Arc<Self>,
        image: image::RgbaImage,
        cancel: &CancellationToken,
        blocking: &X,
    ) -> Result<Result<Vec<u8>, TextureEncodeError>, ExecutorError> {
        let image = match self.encoder.encode_awaiting(image, cancel).await {
            Ok(AwaitedEncode::Cpu(image)) => image,
            Ok(AwaitedEncode::Encoded(data)) => return Ok(Ok(data)),
            Ok(AwaitedEncode::Cancelled) => {
                return Ok(Err(TextureEncodeError::new(
                    "encode skipped: the job was cancelled before it started",
                )))
            }
            Err(e) => return Ok(Err(map_texture_error(e))),
        };
        let adapter = Arc::clone(self);
        blocking
            .execute_blocking(move || {
                adapter
                    .encoder
                    .encode_on_cpu(image)
                    .map_err(map_texture_error)
            })
            .await
    }

    fn expected_size(&self, width: u32, height: u32) -> usize {
        self.encoder.expected_size(width, height)
    }
}

/// Maps texture errors to executor encode errors.
fn map_texture_error(err: TextureError) -> TextureEncodeError {
    TextureEncodeError::new(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTextureEncoder {
        size: usize,
        should_fail: bool,
    }

    impl MockTextureEncoder {
        fn new(size: usize) -> Self {
            Self {
                size,
                should_fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                size: 0,
                should_fail: true,
            }
        }
    }

    impl TextureEncoder for MockTextureEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureError> {
            if self.should_fail {
                Err(TextureError::EncodingFailed("mock failure".to_string()))
            } else {
                Ok(vec![0xDD, 0x53])
            }
        }

        fn expected_size(&self, _width: u32, _height: u32) -> usize {
            self.size
        }

        fn extension(&self) -> &str {
            "dds"
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn test_texture_encoder_adapter_encode_success() {
        let encoder = MockTextureEncoder::new(1024);
        let adapter = TextureEncoderAdapter::new(encoder);

        let image = image::RgbaImage::new(4, 4);
        let result = TextureEncoderAsync::encode_blocking(&adapter, image);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xDD, 0x53]);
    }

    #[test]
    fn test_texture_encoder_adapter_encode_failure() {
        let encoder = MockTextureEncoder::failing();
        let adapter = TextureEncoderAdapter::new(encoder);

        let image = image::RgbaImage::new(4, 4);
        let result = TextureEncoderAsync::encode_blocking(&adapter, image);

        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("mock failure"));
    }

    #[test]
    fn test_texture_encoder_adapter_expected_size() {
        let encoder = MockTextureEncoder::new(1024);
        let adapter = TextureEncoderAdapter::new(encoder);

        assert_eq!(
            TextureEncoderAsync::expected_size(&adapter, 4096, 4096),
            1024
        );
    }

    #[test]
    fn test_texture_encoder_adapter_expected_size_varies() {
        let adapter = TextureEncoderAdapter::new(MockTextureEncoder::new(2048));
        assert_eq!(
            TextureEncoderAsync::expected_size(&adapter, 4096, 4096),
            2048
        );

        let adapter = TextureEncoderAdapter::new(MockTextureEncoder::new(512));
        assert_eq!(
            TextureEncoderAsync::expected_size(&adapter, 4096, 4096),
            512
        );
    }

    // ------------------------------------------------------------------
    // The awaited encode (#422)
    // ------------------------------------------------------------------

    use crate::dds::{ChainOutcome, DdsFormat, IspcCompressor, MipmapCompressor};
    use crate::executor::traits::BlockingExecutor;
    use crate::texture::DdsTextureEncoder;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Runs each closure inline and counts them: the blocking threads an
    /// encode would take.
    #[derive(Default)]
    struct CountingExecutor(AtomicUsize);

    impl BlockingExecutor for CountingExecutor {
        fn execute_blocking<F, R>(
            &self,
            f: F,
        ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static,
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(f()) })
        }
    }

    /// A mipmap backend that hands every image back, as a failed GPU does.
    struct HandsBack;

    impl MipmapCompressor for HandsBack {
        fn compress_mipmap_chain(
            &self,
            image: image::RgbaImage,
            _format: DdsFormat,
            _mipmap_count: usize,
            _output: Vec<u8>,
        ) -> Result<ChainOutcome, crate::dds::DdsError> {
            Ok(ChainOutcome::Returned {
                image,
                reason: "GPU failed: test".to_string(),
            })
        }

        fn name(&self) -> &str {
            "hands-back"
        }
    }

    /// An image whose texels differ from one another.
    fn varied(width: u32, height: u32) -> image::RgbaImage {
        image::RgbaImage::from_fn(width, height, |x, y| {
            let h = (u64::from(y) << 32 | u64::from(x)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            image::Rgba([(h >> 40) as u8, (h >> 48) as u8, (h >> 56) as u8, 255])
        })
    }

    fn ispc() -> DdsTextureEncoder {
        DdsTextureEncoder::new(DdsFormat::BC1).with_compressor(Arc::new(IspcCompressor))
    }

    /// A CPU encoder's job encode is its synchronous encode, byte for byte,
    /// on one blocking-executor call: the same thread use as before #422.
    #[tokio::test]
    async fn a_cpu_encode_is_the_same_bytes_on_one_blocking_call() {
        let adapter = Arc::new(TextureEncoderAdapter::new(ispc()));
        let blocking = CountingExecutor::default();
        let dds = adapter
            .encode(varied(64, 64), &CancellationToken::new(), &blocking)
            .await
            .unwrap()
            .unwrap();
        assert!(
            dds == ispc().encode(varied(64, 64)).unwrap(),
            "the bytes differ"
        );
        assert_eq!(blocking.0.load(Ordering::SeqCst), 1);
    }

    /// An image the GPU hands back is encoded by the fallback on the
    /// blocking executor, under the task's CPU permit, byte for byte what
    /// ISPC makes.
    #[tokio::test]
    async fn a_handed_back_image_is_the_ispc_encode_on_one_blocking_call() {
        let gpu = ispc().with_mipmap_compressor(Arc::new(HandsBack));
        let adapter = Arc::new(TextureEncoderAdapter::new(gpu));
        let blocking = CountingExecutor::default();
        let dds = adapter
            .encode(varied(64, 64), &CancellationToken::new(), &blocking)
            .await
            .unwrap()
            .unwrap();
        assert!(
            dds == ispc().encode(varied(64, 64)).unwrap(),
            "the bytes differ"
        );
        assert_eq!(blocking.0.load(Ordering::SeqCst), 1);
    }

    /// An image handed back after its job was cancelled is not encoded on
    /// the CPU either: that encode is not paid for yet. The error ends the
    /// job as cancelled.
    #[tokio::test]
    async fn a_handed_back_image_of_a_cancelled_job_takes_no_blocking_call() {
        let gpu = ispc().with_mipmap_compressor(Arc::new(HandsBack));
        let adapter = Arc::new(TextureEncoderAdapter::new(gpu));
        let blocking = CountingExecutor::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = adapter
            .encode(varied(64, 64), &cancel, &blocking)
            .await
            .unwrap();
        assert!(result.unwrap_err().message.contains("cancelled"));
        assert_eq!(blocking.0.load(Ordering::SeqCst), 0);
    }
}
