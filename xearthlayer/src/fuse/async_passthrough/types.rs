//! Types for async DDS generation requests and responses.
//!
//! This module defines the communication types between the FUSE filesystem
//! and the async tile generation system.

use crate::coord::TileCoord;
use crate::executor::JobId;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// Re-export DdsResponse from runtime for consistency
pub use crate::runtime::DdsResponse;

/// Request for DDS generation sent to the tile generation system.
#[derive(Debug)]
pub struct DdsRequest {
    /// Unique request ID for tracing
    pub job_id: JobId,
    /// Tile coordinates (tile-level, not chunk-level)
    pub tile: TileCoord,
    /// Channel to send result back
    pub result_tx: oneshot::Sender<DdsResponse>,
    /// Cancellation token for aborting the request when FUSE times out.
    /// When cancelled, the system should stop processing and release resources.
    pub cancellation_token: CancellationToken,
    /// Whether this is a prefetch request (background caching).
    /// Prefetch requests use non-blocking resource acquisition and are
    /// lower priority than on-demand FUSE requests.
    pub is_prefetch: bool,
}

/// Handler function type for processing DDS requests.
///
/// This is called by the filesystem when a virtual DDS file is read.
/// The handler should process the request asynchronously and send the
/// result back via the oneshot channel in the request.
pub type DdsHandler = Arc<dyn Fn(DdsRequest) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_dds_response_creation() {
        let response = DdsResponse::new(vec![1, 2, 3], true, Duration::from_secs(1), true);

        assert_eq!(response.data, vec![1, 2, 3]);
        assert!(response.cache_hit);
        assert_eq!(response.duration, Duration::from_secs(1));
        assert!(response.is_success());
    }

    #[test]
    fn test_dds_response_cache_miss() {
        let response = DdsResponse::new(vec![0xDD, 0x53], false, Duration::from_millis(500), true);

        assert_eq!(response.data, vec![0xDD, 0x53]);
        assert!(!response.cache_hit);
        assert_eq!(response.duration, Duration::from_millis(500));
        assert!(response.is_success());
    }

    #[test]
    fn test_dds_request_creation() {
        let job_id = JobId::auto();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let (tx, _rx) = oneshot::channel();
        let cancellation_token = CancellationToken::new();

        let request = DdsRequest {
            job_id,
            tile,
            result_tx: tx,
            cancellation_token,
            is_prefetch: false,
        };

        assert_eq!(request.tile.row, 100);
        assert_eq!(request.tile.col, 200);
        assert_eq!(request.tile.zoom, 16);
        assert!(!request.is_prefetch);
    }

    #[test]
    fn test_dds_request_prefetch() {
        let job_id = JobId::auto();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let (tx, _rx) = oneshot::channel();
        let cancellation_token = CancellationToken::new();

        let request = DdsRequest {
            job_id,
            tile,
            result_tx: tx,
            cancellation_token,
            is_prefetch: true,
        };

        assert!(request.is_prefetch);
    }

    #[test]
    fn test_cancellation_token() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());

        token.cancel();
        assert!(token.is_cancelled());
    }
}
