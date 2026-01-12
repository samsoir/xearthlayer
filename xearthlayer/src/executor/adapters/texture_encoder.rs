//! Texture encoder adapter for the executor framework.
//!
//! Adapts the `TextureEncoder` trait to the `TextureEncoderAsync` trait.

use crate::executor::{TextureEncodeError, TextureEncoderAsync};
use crate::texture::{TextureEncoder, TextureError};

/// Adapts a `TextureEncoder` to the `TextureEncoderAsync` trait.
///
/// The executor's `TextureEncoderAsync` trait is designed to be used with
/// `spawn_blocking`, so this adapter simply delegates to the underlying
/// encoder and maps error types.
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
    fn encode(&self, image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
        self.encoder.encode(image).map_err(map_texture_error)
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
        fn encode(&self, _image: &image::RgbaImage) -> Result<Vec<u8>, TextureError> {
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
        let result = TextureEncoderAsync::encode(&adapter, &image);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xDD, 0x53]);
    }

    #[test]
    fn test_texture_encoder_adapter_encode_failure() {
        let encoder = MockTextureEncoder::failing();
        let adapter = TextureEncoderAdapter::new(encoder);

        let image = image::RgbaImage::new(4, 4);
        let result = TextureEncoderAsync::encode(&adapter, &image);

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
}
