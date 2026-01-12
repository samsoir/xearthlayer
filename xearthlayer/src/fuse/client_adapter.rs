//! Adapter for using DdsClient with the legacy DdsHandler interface.
//!
//! This module provides a bridge between the new `DdsClient`-based architecture
//! (job executor daemon) and the existing `DdsHandler` interface used by the
//! FUSE layer.
//!
//! # Design
//!
//! The adapter creates a `DdsHandler` closure that:
//! 1. Receives `DdsRequest` from the FUSE layer
//! 2. Spawns an async task to forward the request to `DdsClient`
//! 3. Waits for the response and sends it back via the oneshot channel
//!
//! This allows incremental migration from the pipeline-based architecture
//! to the job executor daemon without changing the FUSE layer.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::fuse::create_dds_handler_from_client;
//! use xearthlayer::executor::DdsClient;
//!
//! let client: Arc<dyn DdsClient> = runtime.dds_client();
//! let handler = create_dds_handler_from_client(client, runtime_handle);
//!
//! // Use handler with FuseMountConfig
//! let config = FuseMountConfig::new(handler, expected_size);
//! ```

use std::sync::Arc;

use tokio::runtime::Handle;
use tracing::{debug, trace};

use crate::executor::DdsClient;
use crate::fuse::{DdsHandler, DdsRequest};

/// Creates a `DdsHandler` that forwards requests to a `DdsClient`.
///
/// This adapter allows FUSE handlers to use the new `DdsClient`-based
/// architecture while maintaining compatibility with the existing
/// `DdsHandler` interface.
///
/// # Arguments
///
/// * `client` - The DDS client to forward requests to
/// * `runtime_handle` - Handle to the Tokio runtime for spawning tasks
///
/// # Returns
///
/// A `DdsHandler` that can be used with `FuseMountConfig`.
pub fn create_dds_handler_from_client(
    client: Arc<dyn DdsClient>,
    runtime_handle: Handle,
) -> DdsHandler {
    Arc::new(move |request: DdsRequest| {
        let client = Arc::clone(&client);
        let tile = request.tile;
        let cancellation = request.cancellation_token.clone();
        let result_tx = request.result_tx;
        let is_prefetch = request.is_prefetch;
        let job_id = request.job_id.clone();

        // Spawn async task to handle the request
        runtime_handle.spawn(async move {
            if is_prefetch {
                // Fire-and-forget for prefetch requests
                trace!(
                    job_id = %job_id,
                    tile_row = tile.row,
                    tile_col = tile.col,
                    "Forwarding prefetch request to DdsClient"
                );
                client.prefetch_with_cancellation(tile, cancellation);
                // Don't send response for prefetch - the receiver will be dropped
            } else {
                // Wait for response and forward to result_tx
                debug!(
                    job_id = %job_id,
                    tile_row = tile.row,
                    tile_col = tile.col,
                    "Forwarding DDS request to DdsClient"
                );

                let rx = client.request_dds(tile, cancellation);

                match rx.await {
                    Ok(response) => {
                        debug!(
                            job_id = %job_id,
                            data_len = response.data.len(),
                            cache_hit = response.cache_hit,
                            "Received response from DdsClient"
                        );
                        // Forward response to FUSE layer
                        let _ = result_tx.send(response);
                    }
                    Err(_) => {
                        debug!(
                            job_id = %job_id,
                            "DdsClient channel closed - request cancelled or executor shutdown"
                        );
                        // Channel closed - don't send anything, FUSE will timeout
                    }
                }
            }
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::{DdsClientError, Priority};
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    fn test_tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }
    }

    /// Mock DdsClient for testing
    struct MockDdsClient {
        request_count: AtomicUsize,
        prefetch_count: AtomicUsize,
        response: Option<DdsResponse>,
    }

    impl MockDdsClient {
        fn new() -> Self {
            Self {
                request_count: AtomicUsize::new(0),
                prefetch_count: AtomicUsize::new(0),
                response: Some(DdsResponse::cache_hit(
                    vec![1, 2, 3],
                    Duration::from_millis(10),
                )),
            }
        }
    }

    impl DdsClient for MockDdsClient {
        fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
            Ok(())
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.request_count.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            if let Some(ref response) = self.response {
                let _ = tx.send(response.clone());
            }
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            _priority: Priority,
            _origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.request_dds(tile, cancellation)
        }

        fn prefetch(&self, _tile: TileCoord) {
            self.prefetch_count.fetch_add(1, Ordering::Relaxed);
        }

        fn prefetch_with_cancellation(&self, tile: TileCoord, _cancellation: CancellationToken) {
            self.prefetch(tile);
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_adapter_forwards_request() {
        let client = Arc::new(MockDdsClient::new());
        let dyn_client: Arc<dyn DdsClient> = Arc::clone(&client) as Arc<dyn DdsClient>;
        let handler = create_dds_handler_from_client(dyn_client, tokio::runtime::Handle::current());

        // Create a DDS request
        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: crate::executor::JobId::auto(),
            tile: test_tile(),
            result_tx: tx,
            cancellation_token: CancellationToken::new(),
            is_prefetch: false,
        };

        // Call the handler
        handler(request);

        // Wait for response
        let response = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("Should receive response within timeout")
            .expect("Channel should not be closed");

        assert_eq!(response.data, vec![1, 2, 3]);
        assert!(response.cache_hit);
        assert_eq!(client.request_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_adapter_forwards_prefetch() {
        let client = Arc::new(MockDdsClient::new());
        let dyn_client: Arc<dyn DdsClient> = Arc::clone(&client) as Arc<dyn DdsClient>;
        let handler = create_dds_handler_from_client(dyn_client, tokio::runtime::Handle::current());

        // Create a prefetch request
        let (tx, _rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: crate::executor::JobId::auto(),
            tile: test_tile(),
            result_tx: tx,
            cancellation_token: CancellationToken::new(),
            is_prefetch: true,
        };

        // Call the handler
        handler(request);

        // Give it time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(client.prefetch_count.load(Ordering::Relaxed), 1);
        assert_eq!(client.request_count.load(Ordering::Relaxed), 0);
    }
}
