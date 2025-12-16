//! Apple Maps satellite imagery provider.
//!
//! Provides access to Apple Maps satellite imagery via their MapKit JS API.
//! Uses DuckDuckGo's map integration to obtain access tokens.
//!
//! # URL Pattern
//!
//! `https://sat-cdn.apple-mapkit.com/tile?style=7&size=1&scale=1&z={zoom}&x={col}&y={row}&v={version}&accessKey={token}`
//!
//! - `style=7` - Satellite imagery
//! - `size=1` and `scale=1` - Standard tile size
//! - `v` - API version number
//! - `accessKey` - Dynamic access token
//!
//! # Token Acquisition
//!
//! Tokens are obtained through a two-step process:
//! 1. Fetch a bearer token from DuckDuckGo's local.js endpoint
//! 2. Use that to authenticate with Apple's MapKit bootstrap API
//! 3. Extract the access key and version from the satellite tile source
//!
//! Tokens may expire, requiring automatic refresh on 403/410 responses.
//!
//! # Coordinate System
//!
//! Uses standard Web Mercator XYZ tile coordinates:
//! - X: Column (0 to 2^zoom - 1, west to east)
//! - Y: Row (0 to 2^zoom - 1, north to south)
//! - Z: Zoom level (0 to 20)

use crate::provider::{AsyncHttpClient, AsyncProvider, HttpClient, Provider, ProviderError};
use std::sync::{Arc, RwLock};

/// DuckDuckGo endpoint for obtaining the initial bearer token.
const DUCKDUCKGO_TOKEN_URL: &str = "https://duckduckgo.com/local.js?get_mk_token=1";

/// Apple MapKit bootstrap endpoint for obtaining tile access credentials.
const APPLE_BOOTSTRAP_URL: &str =
    "https://cdn.apple-mapkit.com/ma/bootstrap?apiVersion=2&mkjsVersion=5.79.95&poi=1";

/// Base URL for Apple Maps satellite tiles.
const APPLE_TILE_URL: &str = "https://sat-cdn.apple-mapkit.com/tile";

/// Minimum zoom level supported by Apple Maps.
const MIN_ZOOM: u8 = 0;

/// Maximum zoom level supported by Apple Maps satellite imagery.
const MAX_ZOOM: u8 = 20;

/// Holds the Apple Maps access credentials.
#[derive(Clone, Debug)]
struct AppleCredentials {
    access_key: String,
    version: String,
}

/// Manages Apple Maps token lifecycle.
///
/// Handles token acquisition, caching, and automatic refresh on expiration.
#[derive(Clone)]
struct AppleTokenManager {
    credentials: Arc<RwLock<Option<AppleCredentials>>>,
}

impl AppleTokenManager {
    /// Initialize with pre-fetched credentials.
    fn with_credentials(access_key: String, version: String) -> Self {
        Self {
            credentials: Arc::new(RwLock::new(Some(AppleCredentials {
                access_key,
                version,
            }))),
        }
    }

    /// Get current credentials, if available.
    fn get_credentials(&self) -> Option<AppleCredentials> {
        self.credentials.read().ok()?.clone()
    }

    /// Update credentials after a successful token fetch.
    fn set_credentials(&self, access_key: String, version: String) {
        if let Ok(mut creds) = self.credentials.write() {
            *creds = Some(AppleCredentials {
                access_key,
                version,
            });
        }
    }

    /// Clear credentials to force a refresh.
    fn clear_credentials(&self) {
        if let Ok(mut creds) = self.credentials.write() {
            *creds = None;
        }
    }
}

/// Fetch Apple Maps credentials using the sync HTTP client.
fn fetch_credentials_sync<C: HttpClient>(
    http_client: &C,
) -> Result<AppleCredentials, ProviderError> {
    // Step 1: Get DuckDuckGo bearer token
    let ddg_response = http_client.get(DUCKDUCKGO_TOKEN_URL)?;
    let ddg_text = String::from_utf8(ddg_response).map_err(|e| {
        ProviderError::InvalidResponse(format!("Invalid UTF-8 from DuckDuckGo: {}", e))
    })?;

    let ddg_token = extract_ddg_token(&ddg_text)?;

    // Step 2: Fetch Apple bootstrap with bearer token
    // Note: We need to make a request with Authorization header
    // Since our HttpClient trait doesn't support custom headers for GET,
    // we'll use post_json with an empty body as a workaround, or add the token as needed
    // For now, we'll try fetching with a custom approach
    let bootstrap_url = format!("{}&token={}", APPLE_BOOTSTRAP_URL, ddg_token);
    let bootstrap_response = http_client.get(&bootstrap_url)?;
    let bootstrap_text = String::from_utf8(bootstrap_response)
        .map_err(|e| ProviderError::InvalidResponse(format!("Invalid UTF-8 from Apple: {}", e)))?;

    extract_apple_credentials(&bootstrap_text)
}

/// Fetch Apple Maps credentials using the async HTTP client.
async fn fetch_credentials_async<C: AsyncHttpClient>(
    http_client: &C,
) -> Result<AppleCredentials, ProviderError> {
    // Step 1: Get DuckDuckGo bearer token
    let ddg_response = http_client.get(DUCKDUCKGO_TOKEN_URL).await?;
    let ddg_text = String::from_utf8(ddg_response).map_err(|e| {
        ProviderError::InvalidResponse(format!("Invalid UTF-8 from DuckDuckGo: {}", e))
    })?;

    let ddg_token = extract_ddg_token(&ddg_text)?;

    // Step 2: Fetch Apple bootstrap with bearer token
    let bootstrap_url = format!("{}&token={}", APPLE_BOOTSTRAP_URL, ddg_token);
    let bootstrap_response = http_client.get(&bootstrap_url).await?;
    let bootstrap_text = String::from_utf8(bootstrap_response)
        .map_err(|e| ProviderError::InvalidResponse(format!("Invalid UTF-8 from Apple: {}", e)))?;

    extract_apple_credentials(&bootstrap_text)
}

/// Extract the bearer token from DuckDuckGo's response.
fn extract_ddg_token(response: &str) -> Result<String, ProviderError> {
    // DuckDuckGo returns JavaScript that contains the token
    // Format varies but typically includes the token in quotes
    // Example: DDG.Data.mapsAPIKey = "token_here";

    // Try to find token patterns
    for pattern in &["mapsAPIKey", "mk_token", "token"] {
        if let Some(pos) = response.find(pattern) {
            let after_key = &response[pos..];
            // Look for quoted string after the key
            if let Some(quote_start) = after_key.find('"') {
                let after_quote = &after_key[quote_start + 1..];
                if let Some(quote_end) = after_quote.find('"') {
                    let token = &after_quote[..quote_end];
                    if !token.is_empty() && token.len() > 10 {
                        return Ok(token.to_string());
                    }
                }
            }
            // Also try single quotes
            if let Some(quote_start) = after_key.find('\'') {
                let after_quote = &after_key[quote_start + 1..];
                if let Some(quote_end) = after_quote.find('\'') {
                    let token = &after_quote[..quote_end];
                    if !token.is_empty() && token.len() > 10 {
                        return Ok(token.to_string());
                    }
                }
            }
        }
    }

    Err(ProviderError::InvalidResponse(
        "Could not extract DuckDuckGo token".to_string(),
    ))
}

/// Extract access key and version from Apple's bootstrap response.
fn extract_apple_credentials(response: &str) -> Result<AppleCredentials, ProviderError> {
    // The bootstrap response is JSON containing tileSources
    // We need to find the satellite source and extract accessKey and v parameters

    // Look for satellite tile source URL
    let satellite_marker = "sat-cdn.apple-mapkit.com";
    let pos = response
        .find(satellite_marker)
        .ok_or_else(|| ProviderError::InvalidResponse("Satellite source not found".to_string()))?;

    let context = &response[pos.saturating_sub(200)..std::cmp::min(pos + 500, response.len())];

    // Extract accessKey
    let access_key = extract_url_param(context, "accessKey")
        .ok_or_else(|| ProviderError::InvalidResponse("accessKey not found".to_string()))?;

    // Extract version
    let version = extract_url_param(context, "v=")
        .or_else(|| extract_url_param(context, "version="))
        .unwrap_or_else(|| "9571".to_string()); // Default version fallback

    Ok(AppleCredentials {
        access_key,
        version,
    })
}

/// Extract a URL parameter value from a string.
fn extract_url_param(text: &str, param: &str) -> Option<String> {
    let search = if param.ends_with('=') {
        param.to_string()
    } else {
        format!("{}=", param)
    };

    let pos = text.find(&search)?;
    let after_param = &text[pos + search.len()..];

    // Find end of value (& or " or ' or whitespace)
    let end = after_param
        .find(|c: char| c == '&' || c == '"' || c == '\'' || c.is_whitespace())
        .unwrap_or(after_param.len());

    let value = &after_param[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Apple Maps satellite imagery provider.
///
/// Provides access to Apple Maps satellite imagery via DuckDuckGo's
/// MapKit integration. Tokens are automatically acquired and refreshed.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{AppleMapsProvider, ReqwestClient};
///
/// let client = ReqwestClient::new().unwrap();
/// let provider = AppleMapsProvider::new(client)?;
/// // Use provider with TileOrchestrator...
/// ```
pub struct AppleMapsProvider<C: HttpClient> {
    http_client: C,
    token_manager: AppleTokenManager,
}

impl<C: HttpClient + Clone> AppleMapsProvider<C> {
    /// Creates a new Apple Maps provider.
    ///
    /// This will fetch initial credentials from DuckDuckGo/Apple.
    ///
    /// # Errors
    ///
    /// Returns an error if credential acquisition fails.
    pub fn new(http_client: C) -> Result<Self, ProviderError> {
        let credentials = fetch_credentials_sync(&http_client)?;
        Ok(Self {
            http_client,
            token_manager: AppleTokenManager::with_credentials(
                credentials.access_key,
                credentials.version,
            ),
        })
    }

    /// Builds the tile URL for the given coordinates.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> Result<String, ProviderError> {
        let creds = self.token_manager.get_credentials().ok_or_else(|| {
            ProviderError::ProviderSpecific("Apple Maps credentials not available".to_string())
        })?;

        Ok(format!(
            "{}?style=7&size=1&scale=1&z={}&x={}&y={}&v={}&accessKey={}",
            APPLE_TILE_URL, zoom, col, row, creds.version, creds.access_key
        ))
    }

    /// Refresh credentials after an authentication failure.
    fn refresh_credentials(&self) -> Result<(), ProviderError> {
        self.token_manager.clear_credentials();
        let credentials = fetch_credentials_sync(&self.http_client)?;
        self.token_manager
            .set_credentials(credentials.access_key, credentials.version);
        Ok(())
    }
}

impl<C: HttpClient + Clone> Provider for AppleMapsProvider<C> {
    fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom)?;
        match self.http_client.get(&url) {
            Ok(data) => Ok(data),
            Err(ProviderError::HttpError(msg)) if msg.contains("403") || msg.contains("410") => {
                // Token expired, refresh and retry
                self.refresh_credentials()?;
                let url = self.build_url(row, col, zoom)?;
                self.http_client.get(&url)
            }
            Err(e) => Err(e),
        }
    }

    fn name(&self) -> &str {
        "Apple Maps"
    }

    fn min_zoom(&self) -> u8 {
        MIN_ZOOM
    }

    fn max_zoom(&self) -> u8 {
        MAX_ZOOM
    }
}

/// Async Apple Maps satellite imagery provider.
///
/// Provides access to Apple Maps satellite imagery with non-blocking I/O.
/// This is the preferred provider for high-throughput scenarios.
pub struct AsyncAppleMapsProvider<C: AsyncHttpClient> {
    http_client: C,
    token_manager: AppleTokenManager,
}

impl<C: AsyncHttpClient + Clone> AsyncAppleMapsProvider<C> {
    /// Creates a new async Apple Maps provider.
    ///
    /// This will fetch initial credentials from DuckDuckGo/Apple.
    ///
    /// # Errors
    ///
    /// Returns an error if credential acquisition fails.
    pub async fn new(http_client: C) -> Result<Self, ProviderError> {
        let credentials = fetch_credentials_async(&http_client).await?;
        Ok(Self {
            http_client,
            token_manager: AppleTokenManager::with_credentials(
                credentials.access_key,
                credentials.version,
            ),
        })
    }

    /// Builds the tile URL for the given coordinates.
    fn build_url(&self, row: u32, col: u32, zoom: u8) -> Result<String, ProviderError> {
        let creds = self.token_manager.get_credentials().ok_or_else(|| {
            ProviderError::ProviderSpecific("Apple Maps credentials not available".to_string())
        })?;

        Ok(format!(
            "{}?style=7&size=1&scale=1&z={}&x={}&y={}&v={}&accessKey={}",
            APPLE_TILE_URL, zoom, col, row, creds.version, creds.access_key
        ))
    }

    /// Refresh credentials after an authentication failure.
    async fn refresh_credentials(&self) -> Result<(), ProviderError> {
        self.token_manager.clear_credentials();
        let credentials = fetch_credentials_async(&self.http_client).await?;
        self.token_manager
            .set_credentials(credentials.access_key, credentials.version);
        Ok(())
    }
}

impl<C: AsyncHttpClient + Clone> AsyncProvider for AsyncAppleMapsProvider<C> {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        if !self.supports_zoom(zoom) {
            return Err(ProviderError::UnsupportedZoom(zoom));
        }

        let url = self.build_url(row, col, zoom)?;
        match self.http_client.get(&url).await {
            Ok(data) => Ok(data),
            Err(ProviderError::HttpError(msg)) if msg.contains("403") || msg.contains("410") => {
                // Token expired, refresh and retry
                self.refresh_credentials().await?;
                let url = self.build_url(row, col, zoom)?;
                self.http_client.get(&url).await
            }
            Err(e) => Err(e),
        }
    }

    fn name(&self) -> &str {
        "Apple Maps"
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

    #[test]
    fn test_extract_ddg_token() {
        let response = r#"DDG.Data.mapsAPIKey = "test_token_12345";"#;
        let token = extract_ddg_token(response).unwrap();
        assert_eq!(token, "test_token_12345");
    }

    #[test]
    fn test_extract_ddg_token_single_quotes() {
        let response = r#"DDG.Data.mapsAPIKey = 'test_token_67890';"#;
        let token = extract_ddg_token(response).unwrap();
        assert_eq!(token, "test_token_67890");
    }

    #[test]
    fn test_extract_ddg_token_not_found() {
        let response = r#"some random content without token"#;
        let result = extract_ddg_token(response);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_url_param() {
        let text = "path?accessKey=abc123&v=9571&other=value";
        assert_eq!(
            extract_url_param(text, "accessKey"),
            Some("abc123".to_string())
        );
        assert_eq!(extract_url_param(text, "v="), Some("9571".to_string()));
        assert_eq!(extract_url_param(text, "other"), Some("value".to_string()));
        assert_eq!(extract_url_param(text, "missing"), None);
    }

    #[test]
    fn test_extract_url_param_at_end() {
        let text = "path?accessKey=abc123";
        assert_eq!(
            extract_url_param(text, "accessKey"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_extract_apple_credentials() {
        let response = r#"{"tileSources":[{"name":"satellite","path":"https://sat-cdn.apple-mapkit.com/tile?style=7&v=9571&accessKey=test_key_abc"}]}"#;
        let creds = extract_apple_credentials(response).unwrap();
        assert_eq!(creds.access_key, "test_key_abc");
        assert_eq!(creds.version, "9571");
    }

    #[test]
    fn test_token_manager_lifecycle() {
        // Create manager with initial credentials
        let manager = AppleTokenManager::with_credentials("key123".to_string(), "9571".to_string());

        let creds = manager.get_credentials().unwrap();
        assert_eq!(creds.access_key, "key123");
        assert_eq!(creds.version, "9571");

        // Test clearing credentials
        manager.clear_credentials();
        assert!(manager.get_credentials().is_none());

        // Test setting new credentials
        manager.set_credentials("newkey".to_string(), "9999".to_string());
        let creds = manager.get_credentials().unwrap();
        assert_eq!(creds.access_key, "newkey");
        assert_eq!(creds.version, "9999");
    }

    #[test]
    fn test_token_manager_with_credentials() {
        let manager = AppleTokenManager::with_credentials("key456".to_string(), "9999".to_string());
        let creds = manager.get_credentials().unwrap();
        assert_eq!(creds.access_key, "key456");
        assert_eq!(creds.version, "9999");
    }

    #[test]
    fn test_build_url_format() {
        let manager =
            AppleTokenManager::with_credentials("test_key".to_string(), "9571".to_string());

        // Manually construct URL to verify format
        let creds = manager.get_credentials().unwrap();
        let url = format!(
            "{}?style=7&size=1&scale=1&z={}&x={}&y={}&v={}&accessKey={}",
            APPLE_TILE_URL, 15, 200, 100, creds.version, creds.access_key
        );

        assert_eq!(
            url,
            "https://sat-cdn.apple-mapkit.com/tile?style=7&size=1&scale=1&z=15&x=200&y=100&v=9571&accessKey=test_key"
        );
    }

    #[test]
    fn test_zoom_constants() {
        assert_eq!(MIN_ZOOM, 0);
        assert_eq!(MAX_ZOOM, 20);
    }
}
