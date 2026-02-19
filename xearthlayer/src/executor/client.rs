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
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Timeout for background send when the channel is full for ON_DEMAND requests.
const ON_DEMAND_SEND_TIMEOUT: Duration = Duration::from_millis(500);

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

    /// Returns the current channel pressure as a value between 0.0 and 1.0.
    ///
    /// - 0.0: Channel is empty (no backpressure)
    /// - 1.0: Channel is full (maximum backpressure)
    ///
    /// Producers can use this to throttle submission rate. For example,
    /// the prefetch coordinator defers cycles when pressure exceeds 0.8.
    ///
    /// Default implementation returns 0.0 (no pressure) for mock clients.
    fn channel_pressure(&self) -> f64 {
        0.0
    }

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

        // Try non-blocking send first (fast path)
        if let Err(e) = self.tx.try_send(request) {
            let req = match e {
                mpsc::error::TrySendError::Full(req) => req,
                mpsc::error::TrySendError::Closed(req) => {
                    // Channel closed — immediate empty response
                    if let Some(response_tx) = req.response_tx {
                        let _ = response_tx.send(DdsResponse::empty(Duration::ZERO));
                    }
                    return rx;
                }
            };

            if priority >= Priority::ON_DEMAND {
                // ON_DEMAND: spawn background task to wait for channel capacity.
                // The FUSE caller is already waiting on the oneshot rx, so this
                // is transparent. The 500ms timeout prevents indefinite blocking.
                let sender = self.tx.clone();
                let tile_debug = format!("{:?}", req.tile);
                tokio::spawn(async move {
                    match tokio::time::timeout(ON_DEMAND_SEND_TIMEOUT, sender.send(req)).await {
                        Ok(Ok(())) => {} // Successfully queued
                        Ok(Err(send_err)) => {
                            // Channel closed during wait
                            if let Some(response_tx) = send_err.0.response_tx {
                                let _ = response_tx.send(DdsResponse::empty(Duration::ZERO));
                            }
                        }
                        Err(_timeout) => {
                            // Timed out — req was consumed by send() and dropped with
                            // the future. The response_tx drop causes RecvError on the
                            // oneshot receiver; FUSE handles this as a timeout.
                            tracing::warn!(
                                tile = %tile_debug,
                                "ON_DEMAND request timed out waiting for executor channel"
                            );
                        }
                    }
                });
            } else {
                // PREFETCH / HOUSEKEEPING: fire-and-forget, immediate empty response
                if let Some(response_tx) = req.response_tx {
                    let _ = response_tx.send(DdsResponse::empty(Duration::ZERO));
                }
            }
        }

        rx
    }

    fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }

    fn channel_pressure(&self) -> f64 {
        let max = self.tx.max_capacity();
        if max == 0 {
            return 1.0;
        }
        let available = self.tx.capacity();
        1.0 - (available as f64 / max as f64)
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
    async fn test_channel_client_request_dds_channel_full_on_demand_waits() {
        let (tx, _rx) = mpsc::channel(1);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel with a prefetch request
        client.prefetch(test_tile());

        // ON_DEMAND request on full channel spawns background task that waits
        // up to 500ms. Since we never drain the channel, the background task
        // times out and drops the request (including response_tx). The oneshot
        // receiver then gets RecvError.
        let response_rx = client.request_dds(test_tile(), CancellationToken::new());

        // Wait for the background send to time out (500ms + margin)
        let result = tokio::time::timeout(Duration::from_secs(2), response_rx).await;
        match result {
            Ok(Err(_recv_error)) => {} // Expected: response_tx dropped on timeout
            Ok(Ok(response)) => {
                // Also acceptable if somehow an empty response arrives
                assert!(!response.has_data());
            }
            Err(_timeout) => panic!("Test timed out waiting for background send to complete"),
        }
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

    #[tokio::test]
    async fn test_fuse_request_retries_on_channel_full() {
        // Channel capacity of 1
        let (tx, mut rx) = mpsc::channel(1);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel
        client.prefetch(test_tile());

        // ON_DEMAND request with full channel — should NOT immediately fail.
        // Instead it spawns a background task that waits for capacity.
        let response_rx = client.request_dds(test_tile(), CancellationToken::new());

        // Drain the channel to make room for the background send
        let _first = rx.recv().await.unwrap();

        // The background send should now succeed
        let received = rx.recv().await.unwrap();
        assert_eq!(received.priority, Priority::ON_DEMAND);
        assert!(received.response_tx.is_some());

        // Send a response back
        let response = DdsResponse::cache_hit(vec![42], Duration::from_millis(5));
        received.response_tx.unwrap().send(response).unwrap();

        // FUSE caller receives the response
        let result = response_rx.await.unwrap();
        assert_eq!(result.data, vec![42]);
    }

    #[tokio::test]
    async fn test_prefetch_still_uses_try_send() {
        // Channel capacity of 1
        let (tx, _rx) = mpsc::channel(1);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel
        client.prefetch(test_tile());

        // Prefetch request should fail silently (fire-and-forget)
        let result = client.submit(JobRequest::prefetch(test_tile()));
        assert_eq!(result, Err(DdsClientError::ChannelFull));
    }

    #[test]
    fn test_channel_pressure_empty() {
        let (tx, _rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx);

        let pressure = client.channel_pressure();
        assert!(
            (pressure - 0.0).abs() < f64::EPSILON,
            "Empty channel should have 0.0 pressure, got {}",
            pressure
        );
    }

    #[test]
    fn test_channel_pressure_full() {
        let (tx, _rx) = mpsc::channel::<JobRequest>(2);
        let client = ChannelDdsClient::new(tx);

        // Fill the channel
        let req1 = JobRequest::prefetch(test_tile());
        let req2 = JobRequest::prefetch(test_tile());
        client.submit(req1).unwrap();
        client.submit(req2).unwrap();

        let pressure = client.channel_pressure();
        assert!(
            (pressure - 1.0).abs() < f64::EPSILON,
            "Full channel should have 1.0 pressure, got {}",
            pressure
        );
    }

    #[test]
    fn test_channel_pressure_partial() {
        let (tx, _rx) = mpsc::channel::<JobRequest>(4);
        let client = ChannelDdsClient::new(tx);

        // Fill half the channel
        client.submit(JobRequest::prefetch(test_tile())).unwrap();
        client.submit(JobRequest::prefetch(test_tile())).unwrap();

        let pressure = client.channel_pressure();
        assert!(
            (pressure - 0.5).abs() < f64::EPSILON,
            "Half-full channel should have 0.5 pressure, got {}",
            pressure
        );
    }

    #[test]
    fn test_channel_pressure_default_trait_impl() {
        // Mock client using default trait impl should return 0.0
        struct MockClient;
        impl DdsClient for MockClient {
            fn submit(&self, _: JobRequest) -> Result<(), DdsClientError> {
                Ok(())
            }
            fn request_dds(
                &self,
                _: TileCoord,
                _: CancellationToken,
            ) -> oneshot::Receiver<DdsResponse> {
                let (_, rx) = oneshot::channel();
                rx
            }
            fn request_dds_with_options(
                &self,
                _: TileCoord,
                _: Priority,
                _: RequestOrigin,
                _: CancellationToken,
            ) -> oneshot::Receiver<DdsResponse> {
                let (_, rx) = oneshot::channel();
                rx
            }
            fn is_connected(&self) -> bool {
                true
            }
        }

        let client = MockClient;
        assert!(
            (client.channel_pressure() - 0.0).abs() < f64::EPSILON,
            "Default trait impl should return 0.0"
        );
    }
}
