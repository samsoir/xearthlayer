//! TextureEncoder trait for abstracting texture encoding strategies.
//!
//! This module defines the `TextureEncoder` trait which allows different
//! texture encoding implementations to be used interchangeably, following
//! the Liskov Substitution Principle.
//!
//! # Example
//!
//! ```
//! use xearthlayer::texture::{TextureEncoder, DdsTextureEncoder};
//! use xearthlayer::dds::DdsFormat;
//! use std::sync::Arc;
//!
//! // Create a DDS encoder
//! let encoder: Arc<dyn TextureEncoder> = Arc::new(
//!     DdsTextureEncoder::new(DdsFormat::BC1)
//!         .with_mipmap_count(5)
//! );
//!
//! // Use through the trait interface
//! assert_eq!(encoder.extension(), "dds");
//! assert!(encoder.expected_size(4096, 4096) > 0);
//! ```

use crate::texture::TextureError;
use image::RgbaImage;
use std::sync::Arc;

/// Trait for texture encoding strategies.
///
/// This trait abstracts the encoding of RGBA image data into texture formats
/// suitable for X-Plane consumption. Implementations must be thread-safe
/// (`Send + Sync`) to support concurrent FUSE operations.
///
/// # Implementors
///
/// - [`DdsTextureEncoder`] - Encodes to DirectDraw Surface (DDS) format
///
/// # Future Implementors
///
/// - `Ktx2TextureEncoder` - For KTX2 format support if X-Plane adds it
/// - `MockTextureEncoder` - For testing without real encoding
pub trait TextureEncoder: Send + Sync {
    /// Encode an RGBA image into the target texture format.
    ///
    /// # Arguments
    ///
    /// * `image` - Source RGBA image to encode
    ///
    /// # Returns
    ///
    /// Complete texture file as bytes (including any headers).
    ///
    /// # Errors
    ///
    /// Returns `TextureError` if:
    /// - Image dimensions are invalid for the format
    /// - Encoding/compression fails
    fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError>;

    /// Return the expected file size for a given image dimension.
    ///
    /// This is used for FUSE file attribute reporting before the actual
    /// file is generated. The size should be accurate for the given
    /// dimensions and encoder configuration.
    ///
    /// # Arguments
    ///
    /// * `width` - Image width in pixels
    /// * `height` - Image height in pixels
    ///
    /// # Returns
    ///
    /// Expected file size in bytes.
    fn expected_size(&self, width: u32, height: u32) -> usize;

    /// Return the file extension for this texture format.
    ///
    /// # Returns
    ///
    /// File extension without the leading dot (e.g., "dds", "ktx2").
    fn extension(&self) -> &str;

    /// Return a human-readable name for this encoder.
    ///
    /// # Returns
    ///
    /// Encoder name (e.g., "DDS BC1", "DDS BC3").
    fn name(&self) -> &str;
}

/// Blanket implementation for Arc-wrapped encoders.
///
/// This allows sharing encoders across threads without changing the adapter pattern.
/// `Arc<DdsTextureEncoder>` automatically implements `TextureEncoder` by delegating
/// to the inner encoder.
impl<T: TextureEncoder + ?Sized> TextureEncoder for Arc<T> {
    fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
        (**self).encode(image)
    }

    fn expected_size(&self, width: u32, height: u32) -> usize {
        (**self).expected_size(width, height)
    }

    fn extension(&self) -> &str {
        (**self).extension()
    }

    fn name(&self) -> &str {
        (**self).name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Mock encoder for testing trait object behavior.
    struct MockTextureEncoder {
        name: String,
        extension: String,
        size: usize,
    }

    impl MockTextureEncoder {
        fn new() -> Self {
            Self {
                name: "Mock Encoder".to_string(),
                extension: "mock".to_string(),
                size: 1024,
            }
        }
    }

    impl TextureEncoder for MockTextureEncoder {
        fn encode(&self, _image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
            Ok(vec![0xDE, 0xAD, 0xBE, 0xEF])
        }

        fn expected_size(&self, _width: u32, _height: u32) -> usize {
            self.size
        }

        fn extension(&self) -> &str {
            &self.extension
        }

        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn test_trait_object_creation() {
        let encoder: Arc<dyn TextureEncoder> = Arc::new(MockTextureEncoder::new());
        assert_eq!(encoder.extension(), "mock");
        assert_eq!(encoder.name(), "Mock Encoder");
    }

    #[test]
    fn test_trait_object_encode() {
        let encoder: Arc<dyn TextureEncoder> = Arc::new(MockTextureEncoder::new());
        let image = RgbaImage::new(4, 4);

        let result = encoder.encode(&image);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_trait_object_expected_size() {
        let encoder: Arc<dyn TextureEncoder> = Arc::new(MockTextureEncoder::new());
        assert_eq!(encoder.expected_size(4096, 4096), 1024);
    }

    #[test]
    fn test_trait_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn TextureEncoder>();
    }

    #[test]
    fn test_multiple_trait_objects() {
        let encoders: Vec<Arc<dyn TextureEncoder>> = vec![
            Arc::new(MockTextureEncoder {
                name: "Encoder A".to_string(),
                extension: "a".to_string(),
                size: 100,
            }),
            Arc::new(MockTextureEncoder {
                name: "Encoder B".to_string(),
                extension: "b".to_string(),
                size: 200,
            }),
        ];

        assert_eq!(encoders[0].name(), "Encoder A");
        assert_eq!(encoders[1].name(), "Encoder B");
        assert_eq!(encoders[0].expected_size(0, 0), 100);
        assert_eq!(encoders[1].expected_size(0, 0), 200);
    }
}
