//! Service error types.

use crate::provider::ProviderError;
use std::fmt;
use std::io;

/// Errors that can occur during service operations.
#[derive(Debug)]
pub enum ServiceError {
    /// Failed to create HTTP client
    HttpClientError(String),
    /// Failed to create provider
    ProviderError(ProviderError),
    /// Failed to create cache
    CacheError(String),
    /// Invalid configuration
    ConfigError(String),
    /// I/O error (file operations, FUSE mount, etc.)
    IoError(io::Error),
    /// Invalid coordinates
    InvalidCoordinates { lat: f64, lon: f64, reason: String },
    /// Zoom level out of range
    InvalidZoom { zoom: u8, min: u8, max: u8 },
    /// Failed to create Tokio runtime
    RuntimeError(String),
    /// FUSE mount or operation error
    FuseError(String),
    /// Service not started or required component not available
    NotStarted(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpClientError(msg) => write!(f, "HTTP client error: {}", msg),
            Self::ProviderError(e) => write!(f, "Provider error: {}", e),
            Self::CacheError(msg) => write!(f, "Cache error: {}", msg),
            Self::ConfigError(msg) => write!(f, "Configuration error: {}", msg),
            Self::IoError(e) => write!(f, "I/O error: {}", e),
            Self::InvalidCoordinates { lat, lon, reason } => {
                write!(f, "Invalid coordinates ({}, {}): {}", lat, lon, reason)
            }
            Self::InvalidZoom { zoom, min, max } => {
                write!(
                    f,
                    "Invalid zoom level {}: must be between {} and {}",
                    zoom, min, max
                )
            }
            Self::RuntimeError(msg) => write!(f, "Runtime error: {}", msg),
            Self::FuseError(msg) => write!(f, "FUSE error: {}", msg),
            Self::NotStarted(msg) => write!(f, "Service not started: {}", msg),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProviderError(e) => Some(e),
            Self::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ProviderError> for ServiceError {
    fn from(e: ProviderError) -> Self {
        Self::ProviderError(e)
    }
}

impl From<io::Error> for ServiceError {
    fn from(e: io::Error) -> Self {
        Self::IoError(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_http_client_error() {
        let err = ServiceError::HttpClientError("connection refused".to_string());
        assert!(err.to_string().contains("HTTP client error"));
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn test_display_config_error() {
        let err = ServiceError::ConfigError("invalid timeout".to_string());
        assert!(err.to_string().contains("Configuration error"));
    }

    #[test]
    fn test_display_invalid_coordinates() {
        let err = ServiceError::InvalidCoordinates {
            lat: 91.0,
            lon: 0.0,
            reason: "latitude out of range".to_string(),
        };
        assert!(err.to_string().contains("91"));
        assert!(err.to_string().contains("latitude out of range"));
    }

    #[test]
    fn test_display_invalid_zoom() {
        let err = ServiceError::InvalidZoom {
            zoom: 25,
            min: 1,
            max: 19,
        };
        assert!(err.to_string().contains("25"));
        assert!(err.to_string().contains("1"));
        assert!(err.to_string().contains("19"));
    }

    #[test]
    fn test_from_provider_error() {
        let provider_err = ProviderError::UnsupportedZoom(25);
        let service_err: ServiceError = provider_err.into();
        assert!(matches!(service_err, ServiceError::ProviderError(_)));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let service_err: ServiceError = io_err.into();
        assert!(matches!(service_err, ServiceError::IoError(_)));
    }

    #[test]
    fn test_error_trait() {
        let err = ServiceError::ConfigError("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn test_display_runtime_error() {
        let err = ServiceError::RuntimeError("failed to spawn runtime".to_string());
        assert!(err.to_string().contains("Runtime error"));
        assert!(err.to_string().contains("failed to spawn runtime"));
    }
}
