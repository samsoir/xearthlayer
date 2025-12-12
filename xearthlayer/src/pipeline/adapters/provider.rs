//! Provider adapter for the async pipeline.
//!
//! Provides adapters to bridge provider traits to the pipeline's `ChunkProvider` trait:
//!
//! - [`ProviderAdapter`] - Adapts sync `Provider` using `spawn_blocking` (legacy)
//! - [`AsyncProviderAdapter`] - Adapts `AsyncProvider` directly (preferred)
//!
//! # Choosing an Adapter
//!
//! For new code, prefer `AsyncProviderAdapter` with `AsyncProvider` implementations.
//! This avoids thread pool exhaustion under high load by using non-blocking I/O.

use crate::pipeline::{ChunkDownloadError, ChunkProvider};
use crate::provider::{AsyncProvider, Provider, ProviderError};
use std::sync::Arc;

/// Adapts a synchronous `Provider` to the async `ChunkProvider` trait.
///
/// The existing providers use blocking HTTP calls. This adapter wraps them
/// in `tokio::task::spawn_blocking` to make them async-compatible.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{ProviderFactory, ProviderConfig, ReqwestClient};
/// use xearthlayer::pipeline::adapters::ProviderAdapter;
///
/// let http = ReqwestClient::new()?;
/// let factory = ProviderFactory::new(http);
/// let (provider, _, _) = factory.create(&ProviderConfig::Bing)?;
///
/// let adapter = ProviderAdapter::new(provider);
/// // adapter implements ChunkProvider
/// ```
pub struct ProviderAdapter {
    provider: Arc<dyn Provider>,
}

impl ProviderAdapter {
    /// Creates a new provider adapter.
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

impl ChunkProvider for ProviderAdapter {
    async fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> Result<Vec<u8>, ChunkDownloadError> {
        let provider = Arc::clone(&self.provider);

        // Run blocking HTTP call on the blocking thread pool
        tokio::task::spawn_blocking(move || provider.download_chunk(row, col, zoom))
            .await
            .map_err(|e| ChunkDownloadError::permanent(format!("task panicked: {}", e)))?
            .map_err(map_provider_error)
    }

    fn name(&self) -> &str {
        self.provider.name()
    }
}

/// Maps provider errors to chunk download errors with retry semantics.
fn map_provider_error(err: ProviderError) -> ChunkDownloadError {
    match err {
        // HTTP errors are typically transient and should be retried
        ProviderError::HttpError(msg) => ChunkDownloadError::retryable(msg),
        // These are permanent failures - retrying won't help
        ProviderError::UnsupportedCoordinates { row, col, zoom } => {
            ChunkDownloadError::permanent(format!(
                "unsupported coordinates: ({}, {}) at zoom {}",
                row, col, zoom
            ))
        }
        ProviderError::UnsupportedZoom(zoom) => {
            ChunkDownloadError::permanent(format!("unsupported zoom level: {}", zoom))
        }
        ProviderError::InvalidResponse(msg) => {
            ChunkDownloadError::permanent(format!("invalid response: {}", msg))
        }
        ProviderError::ProviderSpecific(msg) => {
            // Provider-specific errors could be either - assume permanent to be safe
            ChunkDownloadError::permanent(format!("provider error: {}", msg))
        }
    }
}

/// Adapts an async `AsyncProvider` to the `ChunkProvider` trait.
///
/// This is the preferred adapter for production use. Unlike `ProviderAdapter`,
/// this adapter uses non-blocking I/O throughout, avoiding thread pool exhaustion
/// under high concurrent load.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{AsyncBingMapsProvider, AsyncReqwestClient};
/// use xearthlayer::pipeline::adapters::AsyncProviderAdapter;
/// use std::sync::Arc;
///
/// let http = AsyncReqwestClient::new()?;
/// let provider = Arc::new(AsyncBingMapsProvider::new(http));
/// let adapter = AsyncProviderAdapter::new(provider);
/// // adapter implements ChunkProvider with true async I/O
/// ```
pub struct AsyncProviderAdapter<P: AsyncProvider> {
    provider: Arc<P>,
}

impl<P: AsyncProvider> AsyncProviderAdapter<P> {
    /// Creates a new async provider adapter from an owned provider.
    pub fn new(provider: P) -> Self {
        Self {
            provider: Arc::new(provider),
        }
    }

    /// Creates a new async provider adapter from an Arc-wrapped provider.
    ///
    /// Use this when you need to share the provider across multiple adapters.
    pub fn from_arc(provider: Arc<P>) -> Self {
        Self { provider }
    }
}

impl<P: AsyncProvider + 'static> ChunkProvider for AsyncProviderAdapter<P> {
    async fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> Result<Vec<u8>, ChunkDownloadError> {
        self.provider
            .download_chunk(row, col, zoom)
            .await
            .map_err(map_provider_error)
    }

    fn name(&self) -> &str {
        self.provider.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock for sync Provider
    struct MockProvider {
        name: String,
        response: Result<Vec<u8>, ProviderError>,
    }

    impl MockProvider {
        fn success(data: Vec<u8>) -> Self {
            Self {
                name: "mock".to_string(),
                response: Ok(data),
            }
        }

        fn failing(err: ProviderError) -> Self {
            Self {
                name: "mock".to_string(),
                response: Err(err),
            }
        }
    }

    impl Provider for MockProvider {
        fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn min_zoom(&self) -> u8 {
            0
        }

        fn max_zoom(&self) -> u8 {
            22
        }
    }

    // Mock for async AsyncProvider
    struct MockAsyncProvider {
        name: String,
        response: Result<Vec<u8>, ProviderError>,
    }

    impl MockAsyncProvider {
        fn success(data: Vec<u8>) -> Self {
            Self {
                name: "mock_async".to_string(),
                response: Ok(data),
            }
        }

        fn failing(err: ProviderError) -> Self {
            Self {
                name: "mock_async".to_string(),
                response: Err(err),
            }
        }
    }

    impl AsyncProvider for MockAsyncProvider {
        async fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ProviderError> {
            self.response.clone()
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn min_zoom(&self) -> u8 {
            0
        }

        fn max_zoom(&self) -> u8 {
            22
        }
    }

    // Sync ProviderAdapter tests

    #[tokio::test]
    async fn test_provider_adapter_success() {
        let provider = Arc::new(MockProvider::success(vec![1, 2, 3]));
        let adapter = ProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_provider_adapter_http_error_is_retryable() {
        let provider = Arc::new(MockProvider::failing(ProviderError::HttpError(
            "timeout".to_string(),
        )));
        let adapter = ProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_retryable);
    }

    #[tokio::test]
    async fn test_provider_adapter_unsupported_zoom_is_permanent() {
        let provider = Arc::new(MockProvider::failing(ProviderError::UnsupportedZoom(25)));
        let adapter = ProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 25).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[tokio::test]
    async fn test_provider_adapter_unsupported_coords_is_permanent() {
        let provider = Arc::new(MockProvider::failing(
            ProviderError::UnsupportedCoordinates {
                row: 999999,
                col: 999999,
                zoom: 16,
            },
        ));
        let adapter = ProviderAdapter::new(provider);

        let result = adapter.download_chunk(999999, 999999, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[tokio::test]
    async fn test_provider_adapter_invalid_response_is_permanent() {
        let provider = Arc::new(MockProvider::failing(ProviderError::InvalidResponse(
            "bad data".to_string(),
        )));
        let adapter = ProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[test]
    fn test_provider_adapter_name() {
        let provider = Arc::new(MockProvider::success(vec![]));
        let adapter = ProviderAdapter::new(provider);
        assert_eq!(adapter.name(), "mock");
    }

    // AsyncProviderAdapter tests

    #[tokio::test]
    async fn test_async_provider_adapter_success() {
        let provider = MockAsyncProvider::success(vec![1, 2, 3]);
        let adapter = AsyncProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_async_provider_adapter_http_error_is_retryable() {
        let provider = MockAsyncProvider::failing(ProviderError::HttpError("timeout".to_string()));
        let adapter = AsyncProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_retryable);
    }

    #[tokio::test]
    async fn test_async_provider_adapter_unsupported_zoom_is_permanent() {
        let provider = MockAsyncProvider::failing(ProviderError::UnsupportedZoom(25));
        let adapter = AsyncProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 25).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[tokio::test]
    async fn test_async_provider_adapter_unsupported_coords_is_permanent() {
        let provider = MockAsyncProvider::failing(ProviderError::UnsupportedCoordinates {
            row: 999999,
            col: 999999,
            zoom: 16,
        });
        let adapter = AsyncProviderAdapter::new(provider);

        let result = adapter.download_chunk(999999, 999999, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[tokio::test]
    async fn test_async_provider_adapter_invalid_response_is_permanent() {
        let provider =
            MockAsyncProvider::failing(ProviderError::InvalidResponse("bad data".to_string()));
        let adapter = AsyncProviderAdapter::new(provider);

        let result = adapter.download_chunk(100, 200, 16).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_retryable);
    }

    #[test]
    fn test_async_provider_adapter_name() {
        let provider = MockAsyncProvider::success(vec![]);
        let adapter = AsyncProviderAdapter::new(provider);
        assert_eq!(adapter.name(), "mock_async");
    }
}
