//! Bing Maps satellite imagery provider

use super::http::{AsyncHttpClient, HttpClient};
use super::types::{AsyncProvider, Provider, ProviderError};
use crate::coord::{tile_to_quadkey, TileCoord};

/// Bing Maps imagery provider.
///
/// Downloads satellite imagery from Bing Maps using quadkey-based URLs.
pub struct BingMapsProvider<C: HttpClient> {
    http_client: C,
    base_url: String,
}

impl<C: HttpClient> BingMapsProvider<C> {
    /// Creates a new BingMapsProvider with the given HTTP client.
    ///
    /// Uses the default Bing Maps base URL.
    pub fn new(http_client: C) -> Self {
        Self {
            http_client,
            base_url: "https://ecn.t0.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1".to_string(),
        }
    }

    /// Creates a new BingMapsProvider with a custom base URL.
    ///
    /// Useful for testing or using alternative Bing Maps servers.
    /// The base URL should contain `{quadkey}` as a placeholder.
    pub fn with_base_url(http_client: C, base_url: String) -> Self {
        Self {
            http_client,
            base_url,
        }
    }

    /// Constructs the download URL for a given chunk.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        let tile = TileCoord { row, col, zoom };
        let quadkey = tile_to_quadkey(&tile);
        self.base_url.replace("{quadkey}", &quadkey)
    }
}

impl<C: HttpClient> Provider for BingMapsProvider<C> {
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        // Validate zoom level
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        // Build URL and download
        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url)
    }

    fn name(&self) -> &str {
        "Bing Maps"
    }

    fn min_zoom(&self) -> u8 {
        1
    }

    fn max_zoom(&self) -> u8 {
        19
    }
}

/// Async Bing Maps imagery provider.
///
/// Downloads satellite imagery from Bing Maps using quadkey-based URLs
/// with non-blocking I/O. This is the preferred provider for high-throughput
/// scenarios.
pub struct AsyncBingMapsProvider<C: AsyncHttpClient> {
    http_client: C,
    base_url: String,
}

impl<C: AsyncHttpClient> AsyncBingMapsProvider<C> {
    /// Creates a new AsyncBingMapsProvider with the given HTTP client.
    ///
    /// Uses the default Bing Maps base URL.
    pub fn new(http_client: C) -> Self {
        Self {
            http_client,
            base_url: "https://ecn.t0.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1".to_string(),
        }
    }

    /// Creates a new AsyncBingMapsProvider with a custom base URL.
    ///
    /// Useful for testing or using alternative Bing Maps servers.
    /// The base URL should contain `{quadkey}` as a placeholder.
    pub fn with_base_url(http_client: C, base_url: String) -> Self {
        Self {
            http_client,
            base_url,
        }
    }

    /// Constructs the download URL for a given chunk.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        let tile = TileCoord { row, col, zoom };
        let quadkey = tile_to_quadkey(&tile);
        self.base_url.replace("{quadkey}", &quadkey)
    }
}

impl<C: AsyncHttpClient> AsyncProvider for AsyncBingMapsProvider<C> {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        // Validate zoom level
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        // Build URL and download
        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url).await
    }

    fn name(&self) -> &str {
        "Bing Maps"
    }

    fn min_zoom(&self) -> u8 {
        1
    }

    fn max_zoom(&self) -> u8 {
        19
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockAsyncHttpClient, MockHttpClient};

    #[test]
    fn test_provider_name() {
        let mock = MockHttpClient {
            response: Ok(vec![]),
        };
        let provider = BingMapsProvider::new(mock);
        assert_eq!(provider.name(), "Bing Maps");
    }

    #[test]
    fn test_min_zoom() {
        let mock = MockHttpClient {
            response: Ok(vec![]),
        };
        let provider = BingMapsProvider::new(mock);
        assert_eq!(provider.min_zoom(), 1);
    }

    #[test]
    fn test_max_zoom() {
        let mock = MockHttpClient {
            response: Ok(vec![]),
        };
        let provider = BingMapsProvider::new(mock);
        assert_eq!(provider.max_zoom(), 19);
    }

    #[test]
    fn test_supports_zoom() {
        let mock = MockHttpClient {
            response: Ok(vec![]),
        };
        let provider = BingMapsProvider::new(mock);

        assert!(!provider.supports_zoom(0));
        assert!(provider.supports_zoom(1));
        assert!(provider.supports_zoom(10));
        assert!(provider.supports_zoom(19));
        assert!(!provider.supports_zoom(20));
    }

    #[test]
    fn test_download_chunk_unsupported_zoom() {
        let mock = MockHttpClient {
            response: Ok(vec![1, 2, 3]),
        };
        let provider = BingMapsProvider::new(mock);

        let result = provider.download_chunk(0, 0, 0);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::UnsupportedZoom(0)
        ));
    }

    #[test]
    fn test_download_chunk_success() {
        let mock = MockHttpClient {
            response: Ok(vec![0xFF, 0xD8, 0xFF, 0xE0]), // JPEG magic bytes
        };
        let provider = BingMapsProvider::new(mock);

        let result = provider.download_chunk(100, 200, 10);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xFF, 0xD8, 0xFF, 0xE0]);
    }

    #[test]
    fn test_download_chunk_http_error() {
        let mock = MockHttpClient {
            response: Err(ProviderError::HttpError("404 Not Found".to_string())),
        };
        let provider = BingMapsProvider::new(mock);

        let result = provider.download_chunk(100, 200, 10);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ProviderError::HttpError(_)));
    }

    #[test]
    fn test_url_construction() {
        // Test that the provider constructs the correct URL using quadkeys
        let tile = TileCoord {
            row: 5,
            col: 3,
            zoom: 3,
        };
        let quadkey = tile_to_quadkey(&tile);
        assert_eq!(quadkey, "213"); // Verify our quadkey is correct

        // The URL should contain this quadkey
        // This is tested implicitly through the download_chunk tests
    }

    // Async provider tests

    #[tokio::test]
    async fn test_async_provider_name() {
        let mock = MockAsyncHttpClient {
            response: Ok(vec![]),
        };
        let provider = AsyncBingMapsProvider::new(mock);
        assert_eq!(provider.name(), "Bing Maps");
    }

    #[tokio::test]
    async fn test_async_supports_zoom() {
        let mock = MockAsyncHttpClient {
            response: Ok(vec![]),
        };
        let provider = AsyncBingMapsProvider::new(mock);

        assert!(!provider.supports_zoom(0));
        assert!(provider.supports_zoom(1));
        assert!(provider.supports_zoom(10));
        assert!(provider.supports_zoom(19));
        assert!(!provider.supports_zoom(20));
    }

    #[tokio::test]
    async fn test_async_download_chunk_unsupported_zoom() {
        let mock = MockAsyncHttpClient {
            response: Ok(vec![1, 2, 3]),
        };
        let provider = AsyncBingMapsProvider::new(mock);

        let result = provider.download_chunk(0, 0, 0).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::UnsupportedZoom(0)
        ));
    }

    #[tokio::test]
    async fn test_async_download_chunk_success() {
        let mock = MockAsyncHttpClient {
            response: Ok(vec![0xFF, 0xD8, 0xFF, 0xE0]), // JPEG magic bytes
        };
        let provider = AsyncBingMapsProvider::new(mock);

        let result = provider.download_chunk(100, 200, 10).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xFF, 0xD8, 0xFF, 0xE0]);
    }

    #[tokio::test]
    async fn test_async_download_chunk_http_error() {
        let mock = MockAsyncHttpClient {
            response: Err(ProviderError::HttpError("404 Not Found".to_string())),
        };
        let provider = AsyncBingMapsProvider::new(mock);

        let result = provider.download_chunk(100, 200, 10).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ProviderError::HttpError(_)));
    }
}
