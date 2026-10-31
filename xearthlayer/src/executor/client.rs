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
use crate::pinned_budget::PinnedTileBudget;
use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

    /// Checks if the client is still connected to the executor.
    ///
    /// Returns `false` if the executor has been shut down.
    fn is_connected(&self) -> bool;

    /// The pinned-tile budget the mount accounts against, if any.
    ///
    /// The filesystem pins and releases against the same budget the prefetch
    /// coordinator reads, and this client is the one object the mount already
    /// holds, so it carries the shared handle. `None` for clients with no mount.
    fn pinned_budget(&self) -> Option<Arc<PinnedTileBudget>> {
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

    /// Pinned-tile budget measuring FUSE demand (#246).
    ///
    /// Shared with the FUSE filesystem, which pins and releases against it.
    /// Absent for a client with no mount (tests).
    pinned_budget: Option<Arc<PinnedTileBudget>>,
}

impl ChannelDdsClient {
    /// Creates a new channel client.
    ///
    /// # Arguments
    ///
    /// * `tx` - The channel sender for submitting requests
    pub fn new(tx: mpsc::Sender<JobRequest>) -> Self {
        Self {
            tx,
            pinned_budget: None,
        }
    }

    /// Attaches the pinned-tile budget shared with the FUSE filesystem (#246).
    pub fn with_pinned_budget(mut self, budget: Arc<PinnedTileBudget>) -> Self {
        self.pinned_budget = Some(budget);
        self
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
            created: Instant::now(),
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

    fn pinned_budget(&self) -> Option<Arc<PinnedTileBudget>> {
        self.pinned_budget.as_ref().map(Arc::clone)
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
        let response = DdsResponse::cache_hit(vec![1, 2, 3].into(), Duration::from_millis(10));
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
        client.submit(JobRequest::prefetch(test_tile())).unwrap();

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
        client.submit(JobRequest::prefetch(test_tile())).unwrap();

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
        let response = DdsResponse::cache_hit(vec![42].into(), Duration::from_millis(5));
        received.response_tx.unwrap().send(response).unwrap();

        // FUSE caller receives the response
        let result = response_rx.await.unwrap();
        assert_eq!(result.data, vec![42]);
    }

    /// #246: the filesystem pins against the budget and prefetch reads it. If
    /// the two ever hold separate budgets, both halves still compile and both
    /// report plausible numbers, so nothing fails except the throttle. This is
    /// what says they are the same object.
    #[test]
    fn pinning_is_visible_through_the_clients_budget() {
        let budget = Arc::new(PinnedTileBudget::with_ceiling(1000));
        let (tx, _rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx).with_pinned_budget(Arc::clone(&budget));

        let held = client.pinned_budget().expect("budget attached");
        assert_eq!(held.fraction(), 0.0, "idle mount, no pressure");

        // Stands in for the filesystem materialising tiles for open handles.
        budget.pin(500);

        assert_eq!(held.fraction(), 0.5);
    }

    /// The mount recovers the budget from the client, because the client is
    /// created first. A `pinned_budget()` that cloned the value instead of the
    /// handle would leave the filesystem pinning into an orphan.
    #[test]
    fn the_budget_recovered_from_the_client_is_the_same_object() {
        let budget = Arc::new(PinnedTileBudget::with_ceiling(1000));
        let (tx, _rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx).with_pinned_budget(Arc::clone(&budget));

        let recovered = client
            .pinned_budget()
            .expect("a client given a budget must hand it back");
        recovered.pin(1000);

        assert_eq!(
            budget.fraction(),
            1.0,
            "pinning through the recovered handle must be visible to the client"
        );
    }

    #[test]
    fn a_client_with_no_mount_has_no_budget() {
        let (tx, _rx) = mpsc::channel::<JobRequest>(10);
        let client = ChannelDdsClient::new(tx);

        assert!(client.pinned_budget().is_none());
    }
}
