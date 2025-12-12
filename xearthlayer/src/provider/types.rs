//! Provider types and traits

use std::fmt;
use std::future::Future;

/// Errors that can occur during provider operations.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderError {
    /// HTTP request failed
    HttpError(String),
    /// Coordinates outside provider's supported range
    UnsupportedCoordinates { row: u32, col: u32, zoom: u8 },
    /// Zoom level not supported by this provider
    UnsupportedZoom(u8),
    /// Invalid response data from provider
    InvalidResponse(String),
    /// Provider-specific error
    ProviderSpecific(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::HttpError(msg) => write!(f, "HTTP error: {}", msg),
            ProviderError::UnsupportedCoordinates { row, col, zoom } => {
                write!(
                    f,
                    "Coordinates ({}, {}) at zoom {} not supported by provider",
                    row, col, zoom
                )
            }
            ProviderError::UnsupportedZoom(zoom) => {
                write!(f, "Zoom level {} not supported by provider", zoom)
            }
            ProviderError::InvalidResponse(msg) => write!(f, "Invalid response: {}", msg),
            ProviderError::ProviderSpecific(msg) => write!(f, "Provider error: {}", msg),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Trait for satellite imagery providers.
///
/// Implementors provide satellite imagery chunks from various sources
/// (Bing Maps, Google, NAIP, etc.).
pub trait Provider: Send + Sync {
    /// Downloads a 256×256 pixel imagery chunk.
    ///
    /// # Arguments
    ///
    /// * `row` - Tile row coordinate at chunk resolution
    /// * `col` - Tile column coordinate at chunk resolution
    /// * `zoom` - Zoom level
    ///
    /// # Returns
    ///
    /// Raw image data (typically JPEG format) or an error.
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError>;

    /// Returns the provider's name for logging and identification.
    fn name(&self) -> &str;

    /// Returns the minimum supported zoom level.
    fn min_zoom(&self) -> u8;

    /// Returns the maximum supported zoom level.
    fn max_zoom(&self) -> u8;

    /// Checks if this provider supports the given zoom level.
    fn supports_zoom(&self, zoom: u8) -> bool {
        zoom >= self.min_zoom() && zoom <= self.max_zoom()
    }
}

/// Async trait for satellite imagery providers.
///
/// This is the preferred provider trait for new code. It uses non-blocking
/// I/O via async/await, avoiding thread pool exhaustion under high load.
///
/// Implementors provide satellite imagery chunks from various sources
/// (Bing Maps, Google, NAIP, etc.) using async HTTP clients.
pub trait AsyncProvider: Send + Sync {
    /// Downloads a 256×256 pixel imagery chunk asynchronously.
    ///
    /// # Arguments
    ///
    /// * `row` - Tile row coordinate at chunk resolution
    /// * `col` - Tile column coordinate at chunk resolution
    /// * `zoom` - Zoom level
    ///
    /// # Returns
    ///
    /// Raw image data (typically JPEG format) or an error.
    fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;

    /// Returns the provider's name for logging and identification.
    fn name(&self) -> &str;

    /// Returns the minimum supported zoom level.
    fn min_zoom(&self) -> u8;

    /// Returns the maximum supported zoom level.
    fn max_zoom(&self) -> u8;

    /// Checks if this provider supports the given zoom level.
    fn supports_zoom(&self, zoom: u8) -> bool {
        zoom >= self.min_zoom() && zoom <= self.max_zoom()
    }
}
