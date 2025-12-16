//! MapBox Satellite imagery provider.
//!
//! Provides access to MapBox's satellite imagery via their Raster Tiles API.
//! Requires a MapBox access token (free tier available with usage limits).
//!
//! # URL Pattern
//!
//! `https://api.mapbox.com/v4/mapbox.satellite/{z}/{x}/{y}.jpg?access_token={token}`
//!
//! - Uses standard XYZ tile coordinates
//! - Requires access token as query parameter
//! - Satellite tiles are always delivered as JPEG
//!
//! # Getting an Access Token
//!
//! 1. Create a free account at <https://www.mapbox.com/>
//! 2. Navigate to your account's access tokens page
//! 3. Use the default public token or create a new one
//!
//! # Usage Limits
//!
//! Free tier includes 200,000 tile requests per month.
//! See <https://www.mapbox.com/pricing/> for details.
//!
//! # Coordinate System
//!
//! Uses standard Web Mercator XYZ tile coordinates:
//! - X: Column (0 to 2^zoom - 1, west to east)
//! - Y: Row (0 to 2^zoom - 1, north to south)
//! - Z: Zoom level (0 to 22)

use crate::provider::{AsyncHttpClient, AsyncProvider, HttpClient, Provider, ProviderError};

/// Base URL for MapBox satellite tiles.
const MAPBOX_BASE_URL: &str = "https://api.mapbox.com/v4/mapbox.satellite";

/// Minimum zoom level supported by MapBox.
const MIN_ZOOM: u8 = 0;

/// Maximum zoom level supported by MapBox satellite imagery.
/// While the API supports up to zoom 30, satellite imagery is typically
/// available up to zoom 22.
const MAX_ZOOM: u8 = 22;

/// MapBox satellite imagery provider.
///
/// Provides access to MapBox's high-quality satellite imagery.
/// Requires a valid access token.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{MapBoxProvider, ReqwestClient};
///
/// let client = ReqwestClient::new().unwrap();
/// let provider = MapBoxProvider::new(client, "your_access_token");
/// // Use provider with TileOrchestrator...
/// ```
pub struct MapBoxProvider<C: HttpClient> {
    http_client: C,
    access_token: String,
}

impl<C: HttpClient> MapBoxProvider<C> {
    /// Creates a new MapBox provider with the given access token.
    ///
    /// # Arguments
    ///
    /// * `http_client` - HTTP client for making requests
    /// * `access_token` - MapBox access token
    pub fn new(http_client: C, access_token: impl Into<String>) -> Self {
        Self {
            http_client,
            access_token: access_token.into(),
        }
    }

    /// Builds the tile URL for the given coordinates.
    ///
    /// MapBox uses the pattern: `{base}/{z}/{x}/{y}.jpg?access_token={token}`
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        format!(
            "{}/{}/{}/{}.jpg?access_token={}",
            MAPBOX_BASE_URL, zoom, col, row, self.access_token
        )
    }
}

impl<C: HttpClient> Provider for MapBoxProvider<C> {
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url)
    }

    fn name(&self) -> &str {
        "MapBox"
    }

    fn min_zoom(&self) -> u8 {
        MIN_ZOOM
    }

    fn max_zoom(&self) -> u8 {
        MAX_ZOOM
    }
}

/// Async MapBox satellite imagery provider.
///
/// Provides access to MapBox's satellite imagery with non-blocking I/O.
/// This is the preferred provider for high-throughput scenarios.
pub struct AsyncMapBoxProvider<C: AsyncHttpClient> {
    http_client: C,
    access_token: String,
}

impl<C: AsyncHttpClient> AsyncMapBoxProvider<C> {
    /// Creates a new async MapBox provider with the given access token.
    pub fn new(http_client: C, access_token: impl Into<String>) -> Self {
        Self {
            http_client,
            access_token: access_token.into(),
        }
    }

    /// Builds the tile URL for the given coordinates.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        format!(
            "{}/{}/{}/{}.jpg?access_token={}",
            MAPBOX_BASE_URL, zoom, col, row, self.access_token
        )
    }
}

impl<C: AsyncHttpClient> AsyncProvider for AsyncMapBoxProvider<C> {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url).await
    }

    fn name(&self) -> &str {
        "MapBox"
    }

    fn min_zoom(&self) -> u8 {
        MIN_ZOOM
    }

    fn max_zoom(&self) -> u8 {
        MAX_ZOOM
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
        let provider = MapBoxProvider::new(mock_client, "test_token");
        assert_eq!(provider.name(), "MapBox");
    }

    #[test]
    fn test_zoom_range() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = MapBoxProvider::new(mock_client, "test_token");
        assert_eq!(provider.min_zoom(), 0);
        assert_eq!(provider.max_zoom(), 22);
    }

    #[test]
    fn test_supports_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = MapBoxProvider::new(mock_client, "test_token");
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
        let provider = MapBoxProvider::new(mock_client, "pk.test123");

        let url = provider.build_url(100, 200, 15);
        assert_eq!(
            url,
            "https://api.mapbox.com/v4/mapbox.satellite/15/200/100.jpg?access_token=pk.test123"
        );
    }

    #[test]
    fn test_url_construction_zoom_0() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = MapBoxProvider::new(mock_client, "pk.test123");

        let url = provider.build_url(0, 0, 0);
        assert_eq!(
            url,
            "https://api.mapbox.com/v4/mapbox.satellite/0/0/0.jpg?access_token=pk.test123"
        );
    }

    #[test]
    fn test_download_chunk_unsupported_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = MapBoxProvider::new(mock_client, "test_token");

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
        let provider = MapBoxProvider::new(mock_client, "test_token");

        let result = provider.download_chunk(100, 200, 15);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), sample_jpeg_response());
    }

    #[test]
    fn test_download_chunk_network_error() {
        let mock_client = MockHttpClient {
            response: Err(ProviderError::HttpError("Connection refused".to_string())),
        };
        let provider = MapBoxProvider::new(mock_client, "test_token");

        let result = provider.download_chunk(100, 200, 15);
        assert!(result.is_err());
        match result {
            Err(ProviderError::HttpError(msg)) => {
                assert!(msg.contains("Connection refused"));
            }
            _ => panic!("Expected HttpError"),
        }
    }

    #[test]
    fn test_download_chunk_auth_error() {
        let mock_client = MockHttpClient {
            response: Err(ProviderError::HttpError("401 Unauthorized".to_string())),
        };
        let provider = MapBoxProvider::new(mock_client, "invalid_token");

        let result = provider.download_chunk(100, 200, 15);
        assert!(result.is_err());
    }

    // Async provider tests

    #[tokio::test]
    async fn test_async_provider_name() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");
        assert_eq!(provider.name(), "MapBox");
    }

    #[tokio::test]
    async fn test_async_zoom_range() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");
        assert_eq!(provider.min_zoom(), 0);
        assert_eq!(provider.max_zoom(), 22);
    }

    #[tokio::test]
    async fn test_async_supports_zoom() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(sample_jpeg_response()),
        };
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");
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
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");

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
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");

        let result = provider.download_chunk(100, 200, 15).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), sample_jpeg_response());
    }

    #[tokio::test]
    async fn test_async_download_chunk_network_error() {
        let mock_client = MockAsyncHttpClient {
            response: Err(ProviderError::HttpError("Connection refused".to_string())),
        };
        let provider = AsyncMapBoxProvider::new(mock_client, "test_token");

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
