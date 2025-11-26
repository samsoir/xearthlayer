//! Error types for tile generation.
//!
//! Provides a unified error type for the tile generation abstraction,
//! wrapping underlying errors from orchestrator, texture encoding, and
//! coordinate conversion.

use std::fmt;

/// Errors that can occur during tile generation.
#[derive(Debug, Clone)]
pub enum TileGeneratorError {
    /// Invalid coordinates (latitude/longitude out of range)
    InvalidCoordinates {
        /// Latitude value
        lat: f64,
        /// Longitude value
        lon: f64,
        /// Reason for invalidity
        reason: String,
    },
    /// Download failed
    DownloadFailed(String),
    /// Texture encoding failed
    EncodingFailed(String),
    /// Internal error
    Internal(String),
}

impl fmt::Display for TileGeneratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TileGeneratorError::InvalidCoordinates { lat, lon, reason } => {
                write!(
                    f,
                    "Invalid coordinates (lat={}, lon={}): {}",
                    lat, lon, reason
                )
            }
            TileGeneratorError::DownloadFailed(msg) => {
                write!(f, "Tile download failed: {}", msg)
            }
            TileGeneratorError::EncodingFailed(msg) => {
                write!(f, "Texture encoding failed: {}", msg)
            }
            TileGeneratorError::Internal(msg) => {
                write!(f, "Internal error: {}", msg)
            }
        }
    }
}

impl std::error::Error for TileGeneratorError {}

impl From<crate::orchestrator::OrchestratorError> for TileGeneratorError {
    fn from(err: crate::orchestrator::OrchestratorError) -> Self {
        TileGeneratorError::DownloadFailed(err.to_string())
    }
}

impl From<crate::texture::TextureError> for TileGeneratorError {
    fn from(err: crate::texture::TextureError) -> Self {
        TileGeneratorError::EncodingFailed(err.to_string())
    }
}

impl From<crate::coord::CoordError> for TileGeneratorError {
    fn from(err: crate::coord::CoordError) -> Self {
        TileGeneratorError::InvalidCoordinates {
            lat: 0.0,
            lon: 0.0,
            reason: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_coordinates_display() {
        let err = TileGeneratorError::InvalidCoordinates {
            lat: 91.0,
            lon: -123.0,
            reason: "latitude out of range".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("91"));
        assert!(msg.contains("-123"));
        assert!(msg.contains("latitude out of range"));
    }

    #[test]
    fn test_download_failed_display() {
        let err = TileGeneratorError::DownloadFailed("connection timeout".to_string());
        assert_eq!(err.to_string(), "Tile download failed: connection timeout");
    }

    #[test]
    fn test_encoding_failed_display() {
        let err = TileGeneratorError::EncodingFailed("invalid dimensions".to_string());
        assert_eq!(
            err.to_string(),
            "Texture encoding failed: invalid dimensions"
        );
    }

    #[test]
    fn test_internal_display() {
        let err = TileGeneratorError::Internal("unexpected state".to_string());
        assert_eq!(err.to_string(), "Internal error: unexpected state");
    }

    #[test]
    fn test_error_trait() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<TileGeneratorError>();
    }

    #[test]
    fn test_debug_trait() {
        let err = TileGeneratorError::DownloadFailed("test".to_string());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("DownloadFailed"));
        assert!(debug_str.contains("test"));
    }
}
