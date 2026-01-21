//! DDS client abstraction for requesting tile generation.
//!
//! This module provides the [`DdsClient`] trait - the abstraction that producers
//! (FUSE, Prefetch) use to request DDS tile generation. Producers don't need to
//! know about the job executor internals, channels, or job scheduling.
//!
//! # Design
//!
//! The `DdsClient` trait follows the **Dependency Inversion Principle**:
//! - High-level modules (FUSE, Prefetch) depend on the abstraction (trait)
//! - Low-level modules (channel, executor) implement the abstraction
//! - Changes to the executor don't affect producers
//!
//! # Implementations
//!
//! - [`ChannelDdsClient`] - Production implementation using mpsc channels
//! - Mock implementations can be created for testing
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{DdsClient, DdsClientError};
//! use xearthlayer::runtime::{JobRequest, DdsResponse};
//!
//! async fn request_tile(client: &dyn DdsClient, tile: TileCoord) -> Result<Vec<u8>, DdsClientError> {
//!     let cancellation = CancellationToken::new();
//!     let rx = client.request_dds(tile, cancellation);
//!
//!     match rx.await {
//!         Ok(response) => Ok(response.data),
//!         Err(_) => Err(DdsClientError::ChannelClosed),
//!     }
//! }
//! ```

use crate::coord::TileCoord;
use crate::executor::Priority;
use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
use std::fmt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// =============================================================================
// Error Types
// =============================================================================

/// Error when submitting a job request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdsClientError {
    /// Channel is full (backpressure).
    ///
    /// The executor is overwhelmed. Producers should either retry with
    /// backoff or return an error to callers.
    ChannelFull,

    /// Channel is closed (executor shutdown).
    ///
    /// The executor has been stopped. No more requests can be submitted.
    ChannelClosed,
}

impl fmt::Display for DdsClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdsClientError::ChannelFull => write!(f, "request channel is full"),
            DdsClientError::ChannelClosed => write!(f, "executor has shut down"),
        }
    }
}

impl std::error::Error for DdsClientError {}

// =============================================================================
// DdsClient Trait
// =============================================================================

/// Trait for submitting DDS generation requests to the executor.
///
/// This is the only interface producers (FUSE, Prefetch) need to know about.
/// They don't know how jobs are scheduled, executed, or which resources are used.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow sharing across async tasks.
/// The trait is also `'static` to allow storage in long-lived structs.
///
/// # Example
///
/// ```ignore
/// struct FuseHandler {
///     client: Arc<dyn DdsClient>,
///     timeout: Duration,
/// }
///
/// impl FuseHandler {
///     async fn read_dds(&self, tile: TileCoord) -> Vec<u8> {
///         let cancellation = CancellationToken::new();
///         let rx = self.client.request_dds(tile, cancellation.clone());
///
///         match tokio::time::timeout(self.timeout, rx).await {
///             Ok(Ok(response)) => response.data,
///             _ => {
///                 cancellation.cancel();
///                 self.generate_placeholder()
///             }
///         }
///     }
/// }
/// ```
pub trait DdsClient: Send + Sync + 'static {
    /// Submits a job request (fire-and-forget).
    ///
    /// This is used for prefetch requests where no response is needed.
    /// The request is queued and will be processed when resources allow.
    ///
    /// # Arguments
    ///
    /// * `request` - The job request to submit
    ///
    /// # Returns
    ///
    /// `Ok(())` if the request was queued, `Err` if the channel is full or closed.
    fn submit(&self, request: JobRequest) -> Result<(), DdsClientError>;

    /// Requests DDS generation and returns a response channel.
    ///
    /// This is the primary method for FUSE requests. It creates a request
    /// with a response channel and submits it, returning the receiver.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    /// * `cancellation` - Token for timeout handling
    ///
    /// # Returns
    ///
    /// A oneshot receiver that will receive the DDS response when ready.
    /// If the channel is full/closed, an empty response will be sent.
    fn request_dds(
        &self,
        tile: TileCoord,
        cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse>;

    /// Requests DDS generation with custom priority and origin.
    ///
    /// This method allows full control over the request parameters.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    /// * `priority` - Scheduling priority
    /// * `origin` - Request origin for telemetry
    /// * `cancellation` - Token for timeout handling
    ///
    /// # Returns
    ///
    /// A oneshot receiver that will receive the DDS response when ready.
    fn request_dds_with_options(
        &self,
        tile: TileCoord,
        priority: Priority,
        origin: RequestOrigin,
        cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse>;

    /// Submits a prefetch request (fire-and-forget).
    ///
    /// Convenience method for prefetch requests. Lower priority than
    /// `request_dds` and doesn't block on response.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to prefetch
    fn prefetch(&self, tile: TileCoord) {
        let request = JobRequest::prefetch(tile);
        let _ = self.submit(request);
    }

    /// Submits a prefetch request with shared cancellation.
    ///
    /// Allows batch cancellation of prefetch requests during shutdown.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to prefetch
    /// * `cancellation` - Shared cancellation token
    fn prefetch_with_cancellation(&self, tile: TileCoord, cancellation: CancellationToken) {
        let request = JobRequest::prefetch_with_cancellation(tile, cancellation);
        let _ = self.submit(request);
    }

    /// Checks if the client is still connected to the executor.
    ///
    /// Returns `false` if the executor has been shut down.
    fn is_connected(&self) -> bool;

    /// Returns a clone of the underlying sender for async operations.
    ///
    /// This allows callers to use `send().await` for backpressure-aware
    /// submission, which is useful for bulk operations like prewarm.
    ///
    /// # Returns
    ///
    /// `Some(sender)` if the client has an underlying channel sender,
    /// `None` for mock implementations.
    fn sender(&self) -> Option<mpsc::Sender<JobRequest>> {
        None
    }
}

// =============================================================================
// Channel Implementation
// =============================================================================

/// DdsClient implementation using mpsc channel.
///
/// This is the production implementation that sends requests to the
/// executor daemon via a bounded mpsc channel.
///
/// # Backpressure
///
/// When the channel is full, `submit` returns `DdsClientError::ChannelFull`.
/// For `request_dds`, a placeholder response is sent immediately.
///
/// # Example
///
/// ```ignore
/// let (tx, rx) = mpsc::channel(1000);
/// let client = ChannelDdsClient::new(tx);
///
/// // Use with FUSE handler
/// let fuse = FuseHandler::new(Arc::new(client));
/// ```
pub struct ChannelDdsClient {
    /// Channel sender for requests.
    tx: mpsc::Sender<JobRequest>,
}

impl ChannelDdsClient {
    /// Creates a new channel client.
    ///
    /// # Arguments
    ///
    /// * `tx` - The channel sender for submitting requests
    pub fn new(tx: mpsc::Sender<JobRequest>) -> Self {
        Self { tx }
    }
}

impl DdsClient for ChannelDdsClient {
    fn submit(&self, request: JobRequest) -> Result<(), DdsClientError> {
        self.tx.try_send(request).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => DdsClientError::ChannelFull,
            mpsc::error::TrySendError::Closed(_) => DdsClientError::ChannelClosed,
        })
    }

    fn request_dds(
        &self,
        tile: TileCoord,
        cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        self.request_dds_with_options(tile, Priority::ON_DEMAND, RequestOrigin::Fuse, cancellation)
    }

    fn request_dds_with_options(
        &self,
        tile: TileCoord,
        priority: Priority,
        origin: RequestOrigin,
        cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        let (tx, rx) = oneshot::channel();

        let request = JobRequest {
            tile,
            priority,
            cancellation,
            response_tx: Some(tx),
            origin,
        };

        // Best effort send - if channel is full, response will never arrive
        // The receiver will get an error when the sender is dropped
        if let Err(e) = self.tx.try_send(request) {
            // Channel full or closed - need to send something on the response channel
            // The tx was moved into the request, so we need to extract and respond
            let req = match e {
                mpsc::error::TrySendError::Full(req) => req,
                mpsc::error::TrySendError::Closed(req) => req,
            };
            if let Some(response_tx) = req.response_tx {
                let _ = response_tx.send(DdsResponse::empty(std::time::Duration::ZERO));
            }
        }

        rx
    }

    fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }

    fn sender(&self) -> Option<mpsc::Sender<JobRequest>> {
        Some(self.tx.clone())
    }
}

impl fmt::Debug for ChannelDdsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelDdsClient")
            .field("is_connected", &self.is_connected())
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }
    }

    #[test]
    fn test_error_display() {
        assert_eq!(
            format!("{}", DdsClientError::ChannelFull),
            "request channel is full"
        );
        assert_eq!(
            format!("{}", DdsClientError::ChannelClosed),
            "executor has shut down"
        );
    }

    #[tokio::test]
    async fn test_channel_client_submit_success() {
        let (tx, mut rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        let request = JobRequest::prefetch(test_tile());
        assert!(client.submit(request).is_ok());

        // Verify request was received
        let received = rx.recv().await.unwrap();
        assert_eq!(received.tile, test_tile());
        assert_eq!(received.origin, RequestOrigin::Prefetch);
    }

    #[tokio::test]
    async fn test_channel_client_submit_full() {
        let (tx, _rx) = mpsc::channel(1);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel
        let request1 = JobRequest::prefetch(test_tile());
        assert!(client.submit(request1).is_ok());

        // Next should fail with ChannelFull
        let request2 = JobRequest::prefetch(test_tile());
        assert_eq!(client.submit(request2), Err(DdsClientError::ChannelFull));
    }

    #[tokio::test]
    async fn test_channel_client_submit_closed() {
        let (tx, rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        // Drop receiver to close channel
        drop(rx);

        let request = JobRequest::prefetch(test_tile());
        assert_eq!(client.submit(request), Err(DdsClientError::ChannelClosed));
    }

    #[tokio::test]
    async fn test_channel_client_request_dds() {
        let (tx, mut rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        let response_rx = client.request_dds(test_tile(), CancellationToken::new());

        // Verify request was received
        let received = rx.recv().await.unwrap();
        assert_eq!(received.tile, test_tile());
        assert_eq!(received.origin, RequestOrigin::Fuse);
        assert_eq!(received.priority, Priority::ON_DEMAND);
        assert!(received.response_tx.is_some());

        // Send a response
        let response = DdsResponse::cache_hit(vec![1, 2, 3], Duration::from_millis(10));
        received.response_tx.unwrap().send(response).unwrap();

        // Verify response was received
        let received_response = response_rx.await.unwrap();
        assert_eq!(received_response.data, vec![1, 2, 3]);
        assert!(received_response.cache_hit);
    }

    #[tokio::test]
    async fn test_channel_client_request_dds_with_options() {
        let (tx, mut rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        let _response_rx = client.request_dds_with_options(
            test_tile(),
            Priority::PREFETCH,
            RequestOrigin::Prewarm,
            CancellationToken::new(),
        );

        let received = rx.recv().await.unwrap();
        assert_eq!(received.priority, Priority::PREFETCH);
        assert_eq!(received.origin, RequestOrigin::Prewarm);
    }

    #[tokio::test]
    async fn test_channel_client_request_dds_channel_full() {
        let (tx, _rx) = mpsc::channel(1);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel with a prefetch request
        client.prefetch(test_tile());

        // Request with response should get empty response
        let response_rx = client.request_dds(test_tile(), CancellationToken::new());

        // Should receive an empty response (not hang)
        let response = response_rx.await.unwrap();
        assert!(!response.has_data());
    }

    #[tokio::test]
    async fn test_channel_client_prefetch() {
        let (tx, mut rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        client.prefetch(test_tile());

        let received = rx.recv().await.unwrap();
        assert_eq!(received.tile, test_tile());
        assert_eq!(received.origin, RequestOrigin::Prefetch);
        assert!(!received.expects_response());
    }

    #[tokio::test]
    async fn test_channel_client_prefetch_with_cancellation() {
        let (tx, mut rx) = mpsc::channel(10);
        let client = ChannelDdsClient::new(tx);

        let token = CancellationToken::new();
        client.prefetch_with_cancellation(test_tile(), token.clone());

        let received = rx.recv().await.unwrap();
        assert!(!received.cancellation.is_cancelled());

        // Cancelling the original should affect the request
        token.cancel();
        assert!(received.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn test_channel_client_is_connected() {
        let (tx, rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx);

        assert!(client.is_connected());

        drop(rx);

        assert!(!client.is_connected());
    }

    #[test]
    fn test_channel_client_debug() {
        let (tx, _rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx);

        let debug = format!("{:?}", client);
        assert!(debug.contains("ChannelDdsClient"));
        assert!(debug.contains("is_connected"));
    }
}
