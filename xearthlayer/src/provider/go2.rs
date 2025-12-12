//! Google Maps GO2 satellite imagery provider.
//!
//! Uses Google's public tile servers without requiring an API key.
//! This is the same endpoint used by Ortho4XP's GO2 provider.
//!
//! # URL Pattern
//!
//! `http://mt{0,1,2,3}.google.com/vt/lyrs=s&x={x}&y={y}&z={zoom}`
//!
//! - `mt{0-3}` - Load balancing across 4 tile servers
//! - `lyrs=s` - Satellite imagery layer
//! - `x`, `y`, `z` - Standard XYZ tile coordinates
//!
//! # Coordinate System
//!
//! Google Maps uses standard Web Mercator XYZ tile coordinates:
//! - X: Column (0 to 2^zoom - 1, west to east)
//! - Y: Row (0 to 2^zoom - 1, north to south)
//! - Z: Zoom level (0 to 22)
//!
//! # Note
//!
//! This endpoint is not an official API and may have usage limits or
//! restrictions. For production use with high volume, consider using
//! the official Google Maps Platform API (GoogleMapsProvider).

use crate::provider::{AsyncHttpClient, AsyncProvider, HttpClient, Provider, ProviderError};
use std::sync::atomic::{AtomicU8, Ordering};

/// Google Maps GO2 satellite imagery provider.
///
/// Uses Google's public tile servers without requiring authentication.
/// Automatically rotates between mt0, mt1, mt2, mt3 servers for load balancing.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{Go2Provider, ReqwestClient};
///
/// let client = ReqwestClient::new().unwrap();
/// let provider = Go2Provider::new(client);
/// // Use provider with TileOrchestrator...
/// ```
pub struct Go2Provider<C: HttpClient> {
    http_client: C,
    /// Counter for round-robin server selection (0-3)
    server_counter: AtomicU8,
}

impl<C: HttpClient> Go2Provider<C> {
    /// Creates a new GO2 provider.
    ///
    /// No API key or authentication is required.
    ///
    /// # Arguments
    ///
    /// * `http_client` - HTTP client for making requests
    pub fn new(http_client: C) -> Self {
        Self {
            http_client,
            server_counter: AtomicU8::new(0),
        }
    }

    /// Gets the next server number (0-3) in round-robin fashion.
    fn next_server(&self) -> u8 {
        // Atomically increment and wrap around at 4
        let current = self.server_counter.fetch_add(1, Ordering::Relaxed);
        current % 4
    }

    /// Builds the tile URL for the given coordinates.
    ///
    /// Uses round-robin server selection across mt0-mt3.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        let server = self.next_server();
        format!(
            "http://mt{}.google.com/vt/lyrs=s&x={}&y={}&z={}",
            server, col, row, zoom
        )
    }
}

impl<C: HttpClient> Provider for Go2Provider<C> {
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url)
    }

    fn name(&self) -> &str {
        "Google GO2"
    }

    fn min_zoom(&self) -> u8 {
        0
    }

    fn max_zoom(&self) -> u8 {
        22
    }
}

/// Async Google Maps GO2 satellite imagery provider.
///
/// Uses Google's public tile servers without requiring authentication,
/// with non-blocking I/O. This is the preferred provider for high-throughput
/// scenarios.
pub struct AsyncGo2Provider<C: AsyncHttpClient> {
    http_client: C,
    /// Counter for round-robin server selection (0-3)
    server_counter: AtomicU8,
}

impl<C: AsyncHttpClient> AsyncGo2Provider<C> {
    /// Creates a new async GO2 provider.
    ///
    /// No API key or authentication is required.
    pub fn new(http_client: C) -> Self {
        Self {
            http_client,
            server_counter: AtomicU8::new(0),
        }
    }

    /// Gets the next server number (0-3) in round-robin fashion.
    fn next_server(&self) -> u8 {
        let current = self.server_counter.fetch_add(1, Ordering::Relaxed);
        current % 4
    }

    /// Builds the tile URL for the given coordinates.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        let server = self.next_server();
        format!(
            "http://mt{}.google.com/vt/lyrs=s&x={}&y={}&z={}",
            server, col, row, zoom
        )
    }
}

impl<C: AsyncHttpClient> AsyncProvider for AsyncGo2Provider<C> {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url).await
    }

    fn name(&self) -> &str {
        "Google GO2"
    }

    fn min_zoom(&self) -> u8 {
        0
    }

    fn max_zoom(&self) -> u8 {
        22
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockAsyncHttpClient, MockHttpClient};

    fn sample_jpeg_response() -> Vec<u8> {
        // Minimal valid JPEG header
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46]
    }

    #[test]
    fn test_provider_name() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);
        assert_eq!(provider.name(), "Google GO2");
    }

    #[test]
    fn test_zoom_range() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);
        assert_eq!(provider.min_zoom(), 0);
        assert_eq!(provider.max_zoom(), 22);
    }

    #[test]
    fn test_supports_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);
        assert!(provider.supports_zoom(0));
        assert!(provider.supports_zoom(15));
        assert!(provider.supports_zoom(22));
        assert!(!provider.supports_zoom(23));
    }

    #[test]
    fn test_url_construction() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);

        // First call should use server 0
        let url = provider.build_url(100, 200, 15);
        assert!(url.starts_with("http://mt"));
        assert!(url.contains(".google.com/vt/lyrs=s"));
        assert!(url.contains("x=200")); // col = x
        assert!(url.contains("y=100")); // row = y
        assert!(url.contains("z=15"));
    }

    #[test]
    fn test_server_rotation() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);

        // Server should rotate 0 -> 1 -> 2 -> 3 -> 0
        let servers: Vec<u8> = (0..8).map(|_| provider.next_server()).collect();
        assert_eq!(servers, vec![0, 1, 2, 3, 0, 1, 2, 3]);
    }

    #[test]
    fn test_download_chunk_unsupported_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 23); // Beyond max zoom
        assert!(result.is_err());
        match result {
            Err(ProviderError::UnsupportedZoom(zoom)) => assert_eq!(zoom, 23),
            _ => panic!("Expected UnsupportedZoom error"),
        }
    }

    #[test]
    fn test_download_chunk_success() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = Go2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 15);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), sample_jpeg_response());
    }

    #[test]
    fn test_download_chunk_network_error() {
        let mock_client = MockHttpClient {
            response: Err(ProviderError::HttpError("Connection refused".to_string())),
        };
        let provider = Go2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 15);
        assert!(result.is_err());
        match result {
            Err(ProviderError::HttpError(msg)) => {
                assert!(msg.contains("Connection refused"));
            }
            _ => panic!("Expected HttpError"),
        }
    }

    // Async provider tests

    #[tokio::test]
    async fn test_async_provider_name() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncGo2Provider::new(mock_client);
        assert_eq!(provider.name(), "Google GO2");
    }

    #[tokio::test]
    async fn test_async_supports_zoom() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncGo2Provider::new(mock_client);
        assert!(provider.supports_zoom(0));
        assert!(provider.supports_zoom(15));
        assert!(provider.supports_zoom(22));
        assert!(!provider.supports_zoom(23));
    }

    #[tokio::test]
    async fn test_async_download_chunk_unsupported_zoom() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncGo2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 23).await;
        assert!(result.is_err());
        match result {
            Err(ProviderError::UnsupportedZoom(zoom)) => assert_eq!(zoom, 23),
            _ => panic!("Expected UnsupportedZoom error"),
        }
    }

    #[tokio::test]
    async fn test_async_download_chunk_success() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncGo2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 15).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), sample_jpeg_response());
    }

    #[tokio::test]
    async fn test_async_download_chunk_network_error() {
        let mock_client = MockAsyncHttpClient {
            response: Err(ProviderError::HttpError("Connection refused".to_string())),
        };
        let provider = AsyncGo2Provider::new(mock_client);

        let result = provider.download_chunk(100, 200, 15).await;
        assert!(result.is_err());
        match result {
            Err(ProviderError::HttpError(msg)) => {
                assert!(msg.contains("Connection refused"));
            }
            _ => panic!("Expected HttpError"),
        }
    }
}
