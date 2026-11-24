//! Error types for texture encoding operations.

use std::fmt;

/// Errors that can occur during texture encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureError {
    /// Image dimensions are invalid for encoding.
    InvalidDimensions {
        width: u32,
        height: u32,
        reason: String,
    },
    /// Encoding operation failed.
    EncodingFailed(String),
    /// Unsupported texture format or feature.
    UnsupportedFormat(String),
    /// Invalid configuration.
    InvalidConfig(String),
}

impl fmt::Display for TextureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextureError::InvalidDimensions {
                width,
                height,
                reason,
            } => {
                write!(f, "Invalid dimensions {}×{}: {}", width, height, reason)
            }
            TextureError::EncodingFailed(msg) => write!(f, "Encoding failed: {}", msg),
            TextureError::UnsupportedFormat(msg) => write!(f, "Unsupported format: {}", msg),
            TextureError::InvalidConfig(msg) => write!(f, "Invalid configuration: {}", msg),
        }
    }
}

impl std::error::Error for TextureError {}

/// Convert from DdsError to TextureError.
impl From<crate::dds::DdsError> for TextureError {
    fn from(err: crate::dds::DdsError) -> Self {
        match err {
            crate::dds::DdsError::InvalidDimensions(w, h) => TextureError::InvalidDimensions {
                width: w,
                height: h,
                reason: "Invalid for DDS encoding".to_string(),
            },
            crate::dds::DdsError::UnsupportedFormat(msg) => TextureError::UnsupportedFormat(msg),
            crate::dds::DdsError::CompressionFailed(msg) => TextureError::EncodingFailed(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_texture_error_display_invalid_dimensions() {
        let err = TextureError::InvalidDimensions {
            width: 100,
            height: 200,
            reason: "must be power of 2".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Invalid dimensions 100×200: must be power of 2"
        );
    }

    #[test]
    fn test_texture_error_display_encoding_failed() {
        let err = TextureError::EncodingFailed("compression error".to_string());
        assert_eq!(err.to_string(), "Encoding failed: compression error");
    }

    #[test]
    fn test_texture_error_display_unsupported_format() {
        let err = TextureError::UnsupportedFormat("KTX2".to_string());
        assert_eq!(err.to_string(), "Unsupported format: KTX2");
    }

    #[test]
    fn test_texture_error_display_invalid_config() {
        let err = TextureError::InvalidConfig("mipmap count too high".to_string());
        assert_eq!(
            err.to_string(),
            "Invalid configuration: mipmap count too high"
        );
    }

    #[test]
    fn test_texture_error_from_dds_error() {
        use crate::dds::DdsError;

        let dds_err = DdsError::InvalidDimensions(0, 0);
        let tex_err: TextureError = dds_err.into();
        assert!(matches!(tex_err, TextureError::InvalidDimensions { .. }));

        let dds_err = DdsError::CompressionFailed("test".to_string());
        let tex_err: TextureError = dds_err.into();
        assert!(matches!(tex_err, TextureError::EncodingFailed(_)));
    }
}
