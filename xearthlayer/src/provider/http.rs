//! HTTP client abstraction for testability

use super::types::ProviderError;
use std::future::Future;
use tracing::{debug, trace, warn};

/// Trait for synchronous HTTP client operations.
///
/// This abstraction allows for dependency injection and easier testing
/// by enabling mock HTTP clients in tests.
///
/// **Note**: This is the legacy synchronous trait. For new code, prefer
/// [`AsyncHttpClient`] which uses non-blocking I/O.
pub trait HttpClient: Send + Sync {
    /// Performs an HTTP GET request.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn get(&self, url: &str) -> Result<Vec<u8>, ProviderError>;

    /// Performs an HTTP GET request with Bearer token authentication.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    /// * `bearer_token` - The bearer token for Authorization header
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn get_with_bearer(&self, url: &str, bearer_token: &str) -> Result<Vec<u8>, ProviderError>;

    /// Performs an HTTP POST request with JSON body.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    /// * `json_body` - JSON body as a string
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn post_json(&self, url: &str, json_body: &str) -> Result<Vec<u8>, ProviderError>;
}

/// Trait for asynchronous HTTP client operations.
///
/// This is the preferred HTTP client trait for new code. It uses non-blocking
/// I/O via async/await, avoiding thread pool exhaustion under high load.
pub trait AsyncHttpClient: Send + Sync {
    /// Performs an async HTTP GET request.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;

    /// Performs an async HTTP GET request with custom headers.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    /// * `headers` - Slice of (header_name, header_value) tuples
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;

    /// Performs an async HTTP GET request with Bearer token authentication.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    /// * `bearer_token` - The bearer token for Authorization header
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn get_with_bearer(
        &self,
        url: &str,
        bearer_token: &str,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;

    /// Performs an async HTTP POST request with JSON body.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to request
    /// * `json_body` - JSON body as a string
    ///
    /// # Returns
    ///
    /// The response body as bytes or an error.
    fn post_json(
        &self,
        url: &str,
        json_body: &str,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;
}

/// Real HTTP client implementation using reqwest.
#[derive(Clone)]
pub struct ReqwestClient {
    client: reqwest::blocking::Client,
}

/// Default User-Agent string for HTTP requests.
/// Required by some tile servers (e.g., Google) that reject requests without a User-Agent.
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

impl ReqwestClient {
    /// Creates a new ReqwestClient with default configuration.
    pub fn new() -> Result<Self, ProviderError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .map_err(|e| {
                ProviderError::HttpError(format!("Failed to create HTTP client: {}", e))
            })?;

        Ok(Self { client })
    }

    /// Creates a new ReqwestClient with custom timeout.
    pub fn with_timeout(timeout_secs: u64) -> Result<Self, ProviderError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .map_err(|e| {
                ProviderError::HttpError(format!("Failed to create HTTP client: {}", e))
            })?;

        Ok(Self { client })
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new().expect("Failed to create default HTTP client")
    }
}

impl HttpClient for ReqwestClient {
    fn get(&self, url: &str) -> Result<Vec<u8>, ProviderError> {
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|e| ProviderError::HttpError(format!("Request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }

    fn get_with_bearer(&self, url: &str, bearer_token: &str) -> Result<Vec<u8>, ProviderError> {
        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", bearer_token))
            .send()
            .map_err(|e| ProviderError::HttpError(format!("Request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }

    fn post_json(&self, url: &str, json_body: &str) -> Result<Vec<u8>, ProviderError> {
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(json_body.to_string())
            .send()
            .map_err(|e| ProviderError::HttpError(format!("POST request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from POST {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }
}

/// Async HTTP client implementation using reqwest.
///
/// This client uses non-blocking I/O and is the preferred choice for
/// high-throughput scenarios. Unlike the blocking `ReqwestClient`, this
/// does not consume threads from Tokio's blocking pool.
#[derive(Clone)]
pub struct AsyncReqwestClient {
    client: reqwest::Client,
}

impl AsyncReqwestClient {
    /// Creates a new AsyncReqwestClient with default configuration.
    ///
    /// Optimized for high-throughput satellite imagery download:
    /// - Large connection pool with high idle limits
    /// - TCP keepalive to maintain warm connections
    /// - TCP nodelay for reduced latency
    pub fn new() -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(DEFAULT_USER_AGENT)
            // Connection pooling - keep many connections alive for parallel requests
            .pool_max_idle_per_host(128)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            // TCP optimizations
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .tcp_nodelay(true)
            .build()
            .map_err(|e| {
                ProviderError::HttpError(format!("Failed to create async HTTP client: {}", e))
            })?;

        Ok(Self { client })
    }

    /// Creates a new AsyncReqwestClient with custom timeout.
    pub fn with_timeout(timeout_secs: u64) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .user_agent(DEFAULT_USER_AGENT)
            // Connection pooling - keep many connections alive for parallel requests
            .pool_max_idle_per_host(128)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            // TCP optimizations
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .tcp_nodelay(true)
            .build()
            .map_err(|e| {
                ProviderError::HttpError(format!("Failed to create async HTTP client: {}", e))
            })?;

        Ok(Self { client })
    }
}

impl Default for AsyncReqwestClient {
    fn default() -> Self {
        Self::new().expect("Failed to create default async HTTP client")
    }
}

impl AsyncHttpClient for AsyncReqwestClient {
    async fn get(&self, url: &str) -> Result<Vec<u8>, ProviderError> {
        trace!(url = url, "HTTP GET request starting");

        let response = match self.client.get(url).send().await {
            Ok(resp) => {
                debug!(
                    url = url,
                    status = resp.status().as_u16(),
                    "HTTP response received"
                );
                resp
            }
            Err(e) => {
                warn!(
                    url = url,
                    error = %e,
                    is_connect = e.is_connect(),
                    is_timeout = e.is_timeout(),
                    is_request = e.is_request(),
                    "HTTP request failed"
                );
                return Err(ProviderError::HttpError(format!("Request failed: {}", e)));
            }
        };

        // Check HTTP status
        if !response.status().is_success() {
            warn!(
                url = url,
                status = response.status().as_u16(),
                "HTTP error status"
            );
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from {}",
                response.status(),
                url
            )));
        }

        // Read response body
        match response.bytes().await {
            Ok(bytes) => {
                trace!(url = url, bytes = bytes.len(), "HTTP response body read");
                Ok(bytes.to_vec())
            }
            Err(e) => {
                warn!(url = url, error = %e, "Failed to read response body");
                Err(ProviderError::HttpError(format!(
                    "Failed to read response: {}",
                    e
                )))
            }
        }
    }

    async fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, ProviderError> {
        let mut request = self.client.get(url);

        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        let response = request
            .send()
            .await
            .map_err(|e| ProviderError::HttpError(format!("Request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }

    async fn get_with_bearer(
        &self,
        url: &str,
        bearer_token: &str,
    ) -> Result<Vec<u8>, ProviderError> {
        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", bearer_token))
            .send()
            .await
            .map_err(|e| ProviderError::HttpError(format!("Request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }

    async fn post_json(&self, url: &str, json_body: &str) -> Result<Vec<u8>, ProviderError> {
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(json_body.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::HttpError(format!("POST request failed: {}", e)))?;

        // Check HTTP status
        if !response.status().is_success() {
            return Err(ProviderError::HttpError(format!(
                "HTTP {} from POST {}",
                response.status(),
                url
            )));
        }

        // Read response body
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ProviderError::HttpError(format!("Failed to read response: {}", e)))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Mock HTTP client for testing (synchronous)
    #[derive(Clone)]
    pub struct MockHttpClient {
        pub response: Result<Vec<u8>, ProviderError>,
    }

    impl HttpClient for MockHttpClient {
        fn get(&self, _url: &str) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        fn get_with_bearer(
            &self,
            _url: &str,
            _bearer_token: &str,
        ) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        fn post_json(&self, _url: &str, _json_body: &str) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }
    }

    /// Mock async HTTP client for testing
    #[derive(Clone)]
    pub struct MockAsyncHttpClient {
        pub response: Result<Vec<u8>, ProviderError>,
    }

    impl AsyncHttpClient for MockAsyncHttpClient {
        async fn get(&self, _url: &str) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        async fn get_with_headers(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        async fn get_with_bearer(
            &self,
            _url: &str,
            _bearer_token: &str,
        ) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        async fn post_json(&self, _url: &str, _json_body: &str) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }
    }

    #[test]
    fn test_mock_client_success() {
        let mock = MockHttpClient {
            response: Ok(vec![1, 2, 3, 4]),
        };

        let result = mock.get("http://example.com");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_mock_client_error() {
        let mock = MockHttpClient {
            response: Err(ProviderError::HttpError("Test error".to_string())),
        };

        let result = mock.get("http://example.com");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_async_client_success() {
        let mock = MockAsyncHttpClient {
            response: Ok(vec![1, 2, 3, 4]),
        };

        let result = mock.get("http://example.com").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_mock_async_client_error() {
        let mock = MockAsyncHttpClient {
            response: Err(ProviderError::HttpError("Test error".to_string())),
        };

        let result = mock.get("http://example.com").await;
        assert!(result.is_err());
    }
}
