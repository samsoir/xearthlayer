//! Google Maps satellite imagery provider.
//!
//! Uses Google Maps Platform API with proper authentication via API key and session tokens.
//! Requires users to have their own Google Cloud Platform account and
//! Maps API key with Map Tiles API enabled.
//!
//! # Authentication
//!
//! Google's Map Tiles API requires two-step authentication:
//! 1. Create a session token via POST to /v1/createSession
//! 2. Use the session token in all tile requests
//!
//! The session token is created automatically when the provider is initialized.
//!
//! # API Endpoint
//!
//! - Map Tiles API: `https://tile.googleapis.com/v1/2dtiles/{z}/{x}/{y}?session={SESSION}&key={API_KEY}`
//!
//! # Coordinate System
//!
//! Google Maps uses standard Web Mercator XYZ tile coordinates:
//! - X: Column (0 to 2^zoom - 1, west to east)
//! - Y: Row (0 to 2^zoom - 1, north to south)
//! - Z: Zoom level (0 to 22)
//!
//! This differs from Bing's quadkey system but maps directly to our
//! tile coordinates.

use serde::Deserialize;

use crate::provider::{AsyncHttpClient, AsyncProvider, HttpClient, Provider, ProviderError};

/// Subset of the `createSession` response we care about.
///
/// Google returns additional fields (e.g. `expiry`, `tileWidth`); we ignore
/// them and rely on serde to fail loudly if `session` is missing or non-string.
#[derive(Deserialize)]
struct SessionResponse {
    session: String,
}

/// JSON body sent to `createSession` (satellite map type, en-US, US region).
const SESSION_REQUEST_BODY: &str = r#"{
  "mapType": "satellite",
  "language": "en-US",
  "region": "US"
}"#;

/// Build the URL used to mint a session token for the given API key.
fn build_session_url(api_key: &str) -> String {
    format!(
        "https://tile.googleapis.com/v1/createSession?key={}",
        api_key
    )
}

/// Build the tile URL for the given coordinates and authenticated session.
fn build_tile_url(api_key: &str, session_token: &str, row: u32, col: u32, zoom: u8) -> String {
    format!(
        "https://tile.googleapis.com/v1/2dtiles/{}/{}/{}?session={}&key={}",
        zoom, col, row, session_token, api_key
    )
}

/// Parse a `createSession` response body and extract the session token.
///
/// Returns [`ProviderError::InvalidResponse`] if the body is not valid UTF-8
/// or if the JSON is missing/malformed (e.g. `session` is absent or non-string).
fn parse_session_response(body: &[u8]) -> Result<String, ProviderError> {
    let parsed: SessionResponse = serde_json::from_slice(body).map_err(|e| {
        let preview = String::from_utf8_lossy(body);
        ProviderError::InvalidResponse(format!(
            "Failed to parse session response ({}): {}",
            e, preview
        ))
    })?;
    Ok(parsed.session)
}

/// Google Maps satellite imagery provider.
///
/// Requires a valid Google Maps Platform API key. Users must:
/// 1. Create a Google Cloud Platform project
/// 2. Enable Map Tiles API
/// 3. Enable billing for the project
/// 4. Create an API key with appropriate restrictions
/// 5. Provide the API key to this provider
///
/// # Pricing
///
/// Google Maps Platform is a paid service. Check current pricing at:
/// https://cloud.google.com/maps-platform/pricing
///
/// # Example
///
/// ```no_run
/// use xearthlayer::provider::{GoogleMapsProvider, ReqwestClient};
///
/// let client = ReqwestClient::new().unwrap();
/// let provider = GoogleMapsProvider::new(client, "YOUR_API_KEY".to_string())
///     .expect("Failed to create session");
/// // Use provider with TileOrchestrator...
/// ```
pub struct GoogleMapsProvider<C: HttpClient> {
    http_client: C,
    api_key: String,
    session_token: String,
}

impl<C: HttpClient> GoogleMapsProvider<C> {
    /// Creates a new Google Maps provider with the given API key.
    ///
    /// This will immediately create a session token by making a POST request
    /// to Google's Map Tiles API. The session token is then used for all
    /// subsequent tile requests.
    ///
    /// # Arguments
    ///
    /// * `http_client` - HTTP client for making requests
    /// * `api_key` - Valid Google Maps Platform API key
    ///
    /// # Errors
    ///
    /// Returns an error if session creation fails (network error, invalid API key, etc.)
    pub fn new(http_client: C, api_key: String) -> Result<Self, ProviderError> {
        let session_token = Self::create_session(&http_client, &api_key)?;

        Ok(Self {
            http_client,
            api_key,
            session_token,
        })
    }

    /// Creates a session token for the Map Tiles API.
    ///
    /// Makes a POST request to https://tile.googleapis.com/v1/createSession
    /// with satellite map type configuration.
    fn create_session(http_client: &C, api_key: &str) -> Result<String, ProviderError> {
        let url = build_session_url(api_key);
        let response = http_client.post_json(&url, SESSION_REQUEST_BODY)?;
        parse_session_response(&response)
    }

    /// Builds the tile URL for the given coordinates.
    ///
    /// Google Maps uses standard XYZ coordinates (not quadkeys like Bing).
    /// The row/col from our coordinate system map directly to y/x in Google's API.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        build_tile_url(&self.api_key, &self.session_token, row, col, zoom)
    }
}

impl<C: HttpClient> Provider for GoogleMapsProvider<C> {
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url)
    }

    fn name(&self) -> &str {
        "Google Maps"
    }

    fn min_zoom(&self) -> u8 {
        0
    }

    fn max_zoom(&self) -> u8 {
        22
    }
}

/// Async Google Maps satellite imagery provider.
///
/// Requires a valid Google Maps Platform API key. Uses non-blocking I/O
/// for high-throughput scenarios.
///
/// # Note
///
/// Unlike the sync version, this provider must be created asynchronously
/// via [`AsyncGoogleMapsProvider::new()`] because session creation requires
/// an HTTP call.
pub struct AsyncGoogleMapsProvider<C: AsyncHttpClient> {
    http_client: C,
    api_key: String,
    session_token: String,
}

impl<C: AsyncHttpClient> AsyncGoogleMapsProvider<C> {
    /// Creates a new async Google Maps provider with the given API key.
    ///
    /// This will immediately create a session token by making an async POST request
    /// to Google's Map Tiles API.
    ///
    /// # Arguments
    ///
    /// * `http_client` - Async HTTP client for making requests
    /// * `api_key` - Valid Google Maps Platform API key
    ///
    /// # Errors
    ///
    /// Returns an error if session creation fails.
    pub async fn new(http_client: C, api_key: String) -> Result<Self, ProviderError> {
        let session_token = Self::create_session(&http_client, &api_key).await?;

        Ok(Self {
            http_client,
            api_key,
            session_token,
        })
    }

    /// Creates a session token for the Map Tiles API.
    async fn create_session(http_client: &C, api_key: &str) -> Result<String, ProviderError> {
        let url = build_session_url(api_key);
        let response = http_client.post_json(&url, SESSION_REQUEST_BODY).await?;
        parse_session_response(&response)
    }

    /// Builds the tile URL for the given coordinates.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> String {
        build_tile_url(&self.api_key, &self.session_token, row, col, zoom)
    }
}

impl<C: AsyncHttpClient> AsyncProvider for AsyncGoogleMapsProvider<C> {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom);
        self.http_client.get(&url).await
    }

    fn name(&self) -> &str {
        "Google Maps"
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

    fn mock_session_response() -> Vec<u8> {
        r#"{"session":"test_session_token_12345","expiry":"2025-01-01T00:00:00Z"}"#
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn test_provider_name() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_key".to_string())
            .expect("Failed to create provider");
        assert_eq!(provider.name(), "Google Maps");
    }

    #[test]
    fn test_zoom_range() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_key".to_string())
            .expect("Failed to create provider");
        assert_eq!(provider.min_zoom(), 0);
        assert_eq!(provider.max_zoom(), 22);
    }

    #[test]
    fn test_supports_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_key".to_string())
            .expect("Failed to create provider");
        assert!(provider.supports_zoom(0));
        assert!(provider.supports_zoom(15));
        assert!(provider.supports_zoom(22));
        assert!(!provider.supports_zoom(23));
    }

    #[test]
    fn test_url_construction() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_api_key".to_string())
            .expect("Failed to create provider");

        let url = provider.build_url(100, 200, 10);
        assert_eq!(
            url,
            "https://tile.googleapis.com/v1/2dtiles/10/200/100?session=test_session_token_12345&key=test_api_key"
        );
    }

    #[test]
    fn test_session_token_parsing() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_key".to_string())
            .expect("Failed to create provider");

        assert_eq!(provider.session_token, "test_session_token_12345");
    }

    #[test]
    fn test_download_chunk_unsupported_zoom() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "test_key".to_string())
            .expect("Failed to create provider");

        let result = provider.download_chunk(100, 200, 23); // Beyond max zoom
        assert!(result.is_err());
        match result {
            Err(ProviderError::UnsupportedZoom(zoom)) => assert_eq!(zoom, 23),
            _ => panic!("Expected UnsupportedZoom error"),
        }
    }

    #[test]
    fn test_session_token_included_in_url() {
        let mock_client = MockHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = GoogleMapsProvider::new(mock_client, "secret_key_123".to_string())
            .expect("Failed to create provider");

        let url = provider.build_url(10, 20, 5);
        assert!(url.contains("session=test_session_token_12345"));
        assert!(url.contains("key=secret_key_123"));
    }

    // Async provider tests

    #[tokio::test]
    async fn test_async_provider_name() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = AsyncGoogleMapsProvider::new(mock_client, "test_key".to_string())
            .await
            .expect("Failed to create provider");
        assert_eq!(provider.name(), "Google Maps");
    }

    #[tokio::test]
    async fn test_async_zoom_range() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = AsyncGoogleMapsProvider::new(mock_client, "test_key".to_string())
            .await
            .expect("Failed to create provider");
        assert_eq!(provider.min_zoom(), 0);
        assert_eq!(provider.max_zoom(), 22);
    }

    #[tokio::test]
    async fn test_async_supports_zoom() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = AsyncGoogleMapsProvider::new(mock_client, "test_key".to_string())
            .await
            .expect("Failed to create provider");
        assert!(provider.supports_zoom(0));
        assert!(provider.supports_zoom(15));
        assert!(provider.supports_zoom(22));
        assert!(!provider.supports_zoom(23));
    }

    #[tokio::test]
    async fn test_async_session_token_parsing() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = AsyncGoogleMapsProvider::new(mock_client, "test_key".to_string())
            .await
            .expect("Failed to create provider");

        assert_eq!(provider.session_token, "test_session_token_12345");
    }

    #[tokio::test]
    async fn test_async_download_chunk_unsupported_zoom() {
        let mock_client = MockAsyncHttpClient {
            response: Ok(mock_session_response()),
        };
        let provider = AsyncGoogleMapsProvider::new(mock_client, "test_key".to_string())
            .await
            .expect("Failed to create provider");

        let result = provider.download_chunk(100, 200, 23).await;
        assert!(result.is_err());
        match result {
            Err(ProviderError::UnsupportedZoom(zoom)) => assert_eq!(zoom, 23),
            _ => panic!("Expected UnsupportedZoom error"),
        }
    }

    // parse_session_response — pure helper tests

    #[test]
    fn parse_session_response_extracts_token() {
        let body = br#"{"session":"abc-123","expiry":"2025-01-01T00:00:00Z"}"#;
        let token = parse_session_response(body).expect("parse should succeed");
        assert_eq!(token, "abc-123");
    }

    #[test]
    fn parse_session_response_tolerates_field_order() {
        let body = br#"{"expiry":"2025-01-01T00:00:00Z","session":"reordered"}"#;
        let token = parse_session_response(body).expect("parse should succeed");
        assert_eq!(token, "reordered");
    }

    #[test]
    fn parse_session_response_rejects_non_string_session() {
        // The old split-based parser would silently misinterpret this and
        // pull the next quoted string ("expiry") as the session token.
        let body = br#"{"session": 12345, "expiry":"2025-01-01T00:00:00Z"}"#;
        let result = parse_session_response(body);
        assert!(result.is_err(), "non-string session must be rejected");
    }

    #[test]
    fn parse_session_response_rejects_missing_session_field() {
        let body = br#"{"expiry":"2025-01-01T00:00:00Z"}"#;
        let result = parse_session_response(body);
        assert!(result.is_err(), "missing session field must be rejected");
    }

    #[test]
    fn parse_session_response_rejects_invalid_json() {
        let body = b"not json at all";
        let result = parse_session_response(body);
        assert!(result.is_err(), "non-JSON input must be rejected");
    }

    #[test]
    fn parse_session_response_rejects_invalid_utf8() {
        let body: &[u8] = &[0xff, 0xfe, 0xfd];
        let result = parse_session_response(body);
        assert!(result.is_err(), "invalid UTF-8 must be rejected");
    }
}
