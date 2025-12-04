//! HTTP client implementation for fetching package libraries and metadata.

use std::time::Duration;

use reqwest::blocking::Client;

use crate::package::{
    parse_package_library, parse_package_metadata, PackageLibrary, PackageMetadata,
};

use super::traits::LibraryClient;
use super::{ManagerError, ManagerResult};

/// Default HTTP request timeout (30 seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// HTTP-based implementation of [`LibraryClient`].
///
/// Fetches package library indexes and metadata files from remote URLs.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::manager::HttpLibraryClient;
///
/// let client = HttpLibraryClient::new();
/// let library = client.fetch_library("https://example.com/library.txt")?;
/// ```
#[derive(Clone)]
pub struct HttpLibraryClient {
    client: Client,
    timeout: Duration,
}

impl std::fmt::Debug for HttpLibraryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpLibraryClient")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Default for HttpLibraryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpLibraryClient {
    /// Create a new HTTP library client with default settings.
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
    }

    /// Create a new HTTP library client with a custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("XEarthLayer-Manager/1.0")
            .build()
            .expect("failed to create HTTP client");

        Self { client, timeout }
    }

    /// Fetch text content from a URL.
    fn fetch_text(&self, url: &str) -> ManagerResult<String> {
        let response = self.client.get(url).send().map_err(|e| {
            if e.is_timeout() {
                ManagerError::Timeout {
                    url: url.to_string(),
                    timeout_secs: self.timeout.as_secs(),
                }
            } else {
                ManagerError::HttpError(e.to_string())
            }
        })?;

        if !response.status().is_success() {
            return Err(ManagerError::HttpError(format!(
                "HTTP {} for {}",
                response.status(),
                url
            )));
        }

        response
            .text()
            .map_err(|e| ManagerError::HttpError(e.to_string()))
    }
}

impl LibraryClient for HttpLibraryClient {
    fn fetch_library(&self, url: &str) -> ManagerResult<PackageLibrary> {
        let content = self
            .fetch_text(url)
            .map_err(|e| ManagerError::LibraryFetchFailed {
                url: url.to_string(),
                reason: e.to_string(),
            })?;

        parse_package_library(&content).map_err(|e| ManagerError::LibraryParseFailed {
            url: url.to_string(),
            reason: e.to_string(),
        })
    }

    fn fetch_metadata(&self, url: &str) -> ManagerResult<PackageMetadata> {
        let content = self
            .fetch_text(url)
            .map_err(|e| ManagerError::MetadataFetchFailed {
                url: url.to_string(),
                reason: e.to_string(),
            })?;

        parse_package_metadata(&content).map_err(|e| ManagerError::MetadataParseFailed {
            url: url.to_string(),
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = HttpLibraryClient::new();
        assert_eq!(client.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn test_client_with_timeout() {
        let client = HttpLibraryClient::with_timeout(Duration::from_secs(60));
        assert_eq!(client.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_default_impl() {
        let client = HttpLibraryClient::default();
        assert_eq!(client.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    // Note: Network-dependent tests are in integration tests.
    // These unit tests verify client construction only.
}
