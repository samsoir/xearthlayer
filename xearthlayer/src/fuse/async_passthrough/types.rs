//! Types for async DDS generation requests and responses.
//!
//! This module defines the communication types between the FUSE filesystem
//! and the async pipeline processor.

use crate::coord::TileCoord;
use crate::pipeline::{JobId, JobResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Request for DDS generation sent to the async pipeline.
#[derive(Debug)]
pub struct DdsRequest {
    /// Unique request ID for tracing
    pub job_id: JobId,
    /// Tile coordinates (tile-level, not chunk-level)
    pub tile: TileCoord,
    /// Channel to send result back
    pub result_tx: oneshot::Sender<DdsResponse>,
}

/// Response from the async pipeline.
#[derive(Debug, Clone)]
pub struct DdsResponse {
    /// The generated DDS data
    pub data: Vec<u8>,
    /// Whether this was a cache hit
    pub cache_hit: bool,
    /// Generation duration
    pub duration: Duration,
}

impl From<JobResult> for DdsResponse {
    fn from(result: JobResult) -> Self {
        Self {
            data: result.dds_data,
            cache_hit: result.cache_hit,
            duration: result.duration,
        }
    }
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

    #[test]
    fn test_dds_response_from_job_result() {
        let result = JobResult {
            job_id: JobId::new(),
            dds_data: vec![1, 2, 3],
            duration: Duration::from_secs(1),
            failed_chunks: 0,
            cache_hit: true,
        };

        let response: DdsResponse = result.into();

        assert_eq!(response.data, vec![1, 2, 3]);
        assert!(response.cache_hit);
        assert_eq!(response.duration, Duration::from_secs(1));
    }

    #[test]
    fn test_dds_response_from_job_result_no_cache_hit() {
        let result = JobResult {
            job_id: JobId::new(),
            dds_data: vec![0xDD, 0x53],
            duration: Duration::from_millis(500),
            failed_chunks: 2,
            cache_hit: false,
        };

        let response: DdsResponse = result.into();

        assert_eq!(response.data, vec![0xDD, 0x53]);
        assert!(!response.cache_hit);
        assert_eq!(response.duration, Duration::from_millis(500));
    }

    #[test]
    fn test_dds_request_creation() {
        let job_id = JobId::new();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let (tx, _rx) = oneshot::channel();

        let request = DdsRequest {
            job_id,
            tile,
            result_tx: tx,
        };

        assert_eq!(request.tile.row, 100);
        assert_eq!(request.tile.col, 200);
        assert_eq!(request.tile.zoom, 16);
    }
}
