//! Job request and response types for daemon communication.
//!
//! This module defines the message types used for communication between
//! job producers (FUSE, Prefetch) and the job executor daemon via channels.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────┐                        ┌──────────────────┐
//! │ FUSE Handler │──┐                     │                  │
//! └──────────────┘  │                     │                  │
//!                   ├─► JobRequest ─────► │ Executor Daemon  │
//! ┌──────────────┐  │                     │                  │
//! │ Prefetcher   │──┘                     │                  │
//! └──────────────┘                        └────────┬─────────┘
//!                                                  │
//!                          ◄──── DdsResponse ──────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::runtime::{JobRequest, DdsResponse, RequestOrigin};
//!
//! // FUSE request (needs response)
//! let (request, response_rx) = JobRequest::fuse(tile, cancellation);
//! sender.send(request).await?;
//! let dds_data = response_rx.await?.data;
//!
//! // Prefetch request (fire-and-forget)
//! let request = JobRequest::prefetch(tile);
//! sender.send(request).await?;
//! ```

use crate::coord::TileCoord;
use crate::executor::Priority;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// =============================================================================
// Request Origin
// =============================================================================

/// Origin of a job request for telemetry and priority decisions.
///
/// This enum helps the executor understand the context of requests,
/// enabling better scheduling decisions and telemetry reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestOrigin {
    /// X-Plane requested this tile via FUSE (highest priority).
    ///
    /// These requests block X-Plane's rendering pipeline and must
    /// complete as quickly as possible.
    Fuse,

    /// Predictive caching requested this tile.
    ///
    /// Lower priority than FUSE requests. Can be cancelled if
    /// resources are needed for on-demand requests.
    Prefetch,

    /// CLI-initiated cache warming.
    ///
    /// Bulk tile generation, typically run before a flight.
    /// Lowest priority among active work.
    Prewarm,

    /// Direct CLI command (e.g., `xearthlayer download`).
    ///
    /// Single tile download for testing/debugging.
    Manual,
}

impl RequestOrigin {
    /// Returns the recommended priority for this origin type.
    pub fn default_priority(self) -> Priority {
        match self {
            RequestOrigin::Fuse | RequestOrigin::Manual => Priority::ON_DEMAND,
            RequestOrigin::Prefetch | RequestOrigin::Prewarm => Priority::PREFETCH,
        }
    }

    /// Returns true if this request type requires a response.
    pub fn requires_response(self) -> bool {
        match self {
            RequestOrigin::Fuse | RequestOrigin::Manual | RequestOrigin::Prewarm => true,
            RequestOrigin::Prefetch => false,
        }
    }

    /// Returns true if this request originated from FUSE (X-Plane file access).
    ///
    /// Used for metrics to differentiate between on-demand FUSE requests
    /// and background prefetch/prewarm requests.
    pub fn is_fuse(self) -> bool {
        matches!(self, RequestOrigin::Fuse)
    }
}

impl std::fmt::Display for RequestOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestOrigin::Fuse => write!(f, "fuse"),
            RequestOrigin::Prefetch => write!(f, "prefetch"),
            RequestOrigin::Prewarm => write!(f, "prewarm"),
            RequestOrigin::Manual => write!(f, "manual"),
        }
    }
}

// =============================================================================
// Job Request
// =============================================================================

/// Request to generate a DDS tile.
///
/// This is the message sent from producers (FUSE, Prefetch) to the
/// executor daemon via a channel.
///
/// # Response Handling
///
/// The `response_tx` field is optional. FUSE requests always include
/// a response channel because X-Plane blocks waiting for the tile.
/// Prefetch requests are fire-and-forget and don't need responses.
pub struct JobRequest {
    /// The tile coordinates to generate.
    pub tile: TileCoord,

    /// Scheduling priority for the job.
    pub priority: Priority,

    /// Cancellation signal for timeout handling.
    ///
    /// Producers can cancel this token if the request times out,
    /// allowing the executor to skip or abort the work.
    pub cancellation: CancellationToken,

    /// Optional response channel.
    ///
    /// When present, the executor will send the result through this
    /// channel when the job completes.
    pub response_tx: Option<oneshot::Sender<DdsResponse>>,

    /// Origin of the request for telemetry.
    pub origin: RequestOrigin,
}

impl JobRequest {
    /// Creates a FUSE request with response channel.
    ///
    /// FUSE requests always need a response because X-Plane is waiting
    /// for the tile data.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    /// * `cancellation` - Token for timeout handling
    ///
    /// # Returns
    ///
    /// A tuple of the request and the response receiver.
    pub fn fuse(
        tile: TileCoord,
        cancellation: CancellationToken,
    ) -> (Self, oneshot::Receiver<DdsResponse>) {
        let (tx, rx) = oneshot::channel();
        let request = Self {
            tile,
            priority: Priority::ON_DEMAND,
            cancellation,
            response_tx: Some(tx),
            origin: RequestOrigin::Fuse,
        };
        (request, rx)
    }

    /// Creates a prefetch request (fire-and-forget).
    ///
    /// Prefetch requests don't need a response - the goal is just
    /// to warm the cache.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    pub fn prefetch(tile: TileCoord) -> Self {
        Self {
            tile,
            priority: Priority::PREFETCH,
            cancellation: CancellationToken::new(),
            response_tx: None,
            origin: RequestOrigin::Prefetch,
        }
    }

    /// Creates a prefetch request with a shared cancellation token.
    ///
    /// Useful when the prefetcher wants to cancel all pending requests
    /// during shutdown.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    /// * `cancellation` - Shared cancellation token
    pub fn prefetch_with_cancellation(tile: TileCoord, cancellation: CancellationToken) -> Self {
        Self {
            tile,
            priority: Priority::PREFETCH,
            cancellation,
            response_tx: None,
            origin: RequestOrigin::Prefetch,
        }
    }

    /// Creates a prewarm request with response channel.
    ///
    /// Prewarm requests run at low priority but track completion
    /// for progress reporting.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    pub fn prewarm(tile: TileCoord) -> (Self, oneshot::Receiver<DdsResponse>) {
        let (tx, rx) = oneshot::channel();
        let request = Self {
            tile,
            priority: Priority::PREFETCH,
            cancellation: CancellationToken::new(),
            response_tx: Some(tx),
            origin: RequestOrigin::Prewarm,
        };
        (request, rx)
    }

    /// Creates a manual request with response channel.
    ///
    /// Manual requests are for CLI downloads and debugging.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to generate
    pub fn manual(tile: TileCoord) -> (Self, oneshot::Receiver<DdsResponse>) {
        let (tx, rx) = oneshot::channel();
        let request = Self {
            tile,
            priority: Priority::ON_DEMAND,
            cancellation: CancellationToken::new(),
            response_tx: Some(tx),
            origin: RequestOrigin::Manual,
        };
        (request, rx)
    }

    /// Returns true if this request expects a response.
    pub fn expects_response(&self) -> bool {
        self.response_tx.is_some()
    }
}

impl std::fmt::Debug for JobRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRequest")
            .field("tile", &self.tile)
            .field("priority", &self.priority)
            .field("origin", &self.origin)
            .field("expects_response", &self.expects_response())
            .finish()
    }
}

// =============================================================================
// DDS Response
// =============================================================================

/// Response from job completion.
///
/// Contains the generated DDS data and metadata about the generation.
#[derive(Debug, Clone)]
pub struct DdsResponse {
    /// The generated DDS data.
    pub data: Vec<u8>,

    /// Whether this was a cache hit.
    ///
    /// Cache hits are much faster than full tile generation.
    pub cache_hit: bool,

    /// How long the request took (including queue time).
    pub duration: Duration,

    /// Whether the job succeeded.
    ///
    /// This is true if the job completed successfully, even if the data
    /// couldn't be read from cache (due to async timing issues).
    /// Callers should check this field for success, not just `has_data()`.
    pub job_succeeded: bool,
}

impl DdsResponse {
    /// Creates a new response with generated data.
    pub fn new(data: Vec<u8>, cache_hit: bool, duration: Duration, job_succeeded: bool) -> Self {
        Self {
            data,
            cache_hit,
            duration,
            job_succeeded,
        }
    }

    /// Converts from the fuse module's DdsResponse.
    pub fn from_fuse(fuse: crate::fuse::DdsResponse) -> Self {
        // FUSE responses are always "successful" since they come from actual data
        let job_succeeded = !fuse.data.is_empty();
        Self {
            data: fuse.data,
            cache_hit: fuse.cache_hit,
            duration: fuse.duration,
            job_succeeded,
        }
    }

    /// Converts to the fuse module's DdsResponse.
    pub fn to_fuse(self) -> crate::fuse::DdsResponse {
        crate::fuse::DdsResponse {
            data: self.data,
            cache_hit: self.cache_hit,
            duration: self.duration,
            job_succeeded: self.job_succeeded,
        }
    }

    /// Creates a cache hit response.
    pub fn cache_hit(data: Vec<u8>, duration: Duration) -> Self {
        Self::new(data, true, duration, true)
    }

    /// Creates a cache miss response (data was generated).
    pub fn cache_miss(data: Vec<u8>, duration: Duration) -> Self {
        Self::new(data, false, duration, true)
    }

    /// Creates a successful response where the job succeeded.
    ///
    /// Use this when the job completed successfully, regardless of whether
    /// the data could be read from cache.
    pub fn success(data: Vec<u8>, duration: Duration) -> Self {
        Self::new(data, false, duration, true)
    }

    /// Creates an empty response (for errors or timeouts).
    pub fn empty(duration: Duration) -> Self {
        Self::new(Vec::new(), false, duration, false)
    }

    /// Returns true if the response contains valid data.
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Returns true if the job succeeded.
    ///
    /// This is the preferred way to check for success when you don't need
    /// the actual data (e.g., for prewarm progress tracking).
    pub fn is_success(&self) -> bool {
        self.job_succeeded
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }
    }

    #[test]
    fn test_fuse_request_has_response_channel() {
        let (request, _rx) = JobRequest::fuse(test_tile(), CancellationToken::new());

        assert!(request.expects_response());
        assert_eq!(request.origin, RequestOrigin::Fuse);
        assert_eq!(request.priority, Priority::ON_DEMAND);
    }

    #[test]
    fn test_prefetch_request_no_response() {
        let request = JobRequest::prefetch(test_tile());

        assert!(!request.expects_response());
        assert_eq!(request.origin, RequestOrigin::Prefetch);
        assert_eq!(request.priority, Priority::PREFETCH);
    }

    #[test]
    fn test_prefetch_with_shared_cancellation() {
        let token = CancellationToken::new();
        let request = JobRequest::prefetch_with_cancellation(test_tile(), token.clone());

        // Cancelling the token should reflect in the request
        assert!(!request.cancellation.is_cancelled());
        token.cancel();
        assert!(request.cancellation.is_cancelled());
    }

    #[test]
    fn test_prewarm_request_has_response() {
        let (request, _rx) = JobRequest::prewarm(test_tile());

        assert!(request.expects_response());
        assert_eq!(request.origin, RequestOrigin::Prewarm);
        assert_eq!(request.priority, Priority::PREFETCH);
    }

    #[test]
    fn test_manual_request() {
        let (request, _rx) = JobRequest::manual(test_tile());

        assert!(request.expects_response());
        assert_eq!(request.origin, RequestOrigin::Manual);
        assert_eq!(request.priority, Priority::ON_DEMAND);
    }

    #[test]
    fn test_request_origin_default_priority() {
        assert_eq!(RequestOrigin::Fuse.default_priority(), Priority::ON_DEMAND);
        assert_eq!(
            RequestOrigin::Manual.default_priority(),
            Priority::ON_DEMAND
        );
        assert_eq!(
            RequestOrigin::Prefetch.default_priority(),
            Priority::PREFETCH
        );
        assert_eq!(
            RequestOrigin::Prewarm.default_priority(),
            Priority::PREFETCH
        );
    }

    #[test]
    fn test_request_origin_requires_response() {
        assert!(RequestOrigin::Fuse.requires_response());
        assert!(RequestOrigin::Manual.requires_response());
        assert!(RequestOrigin::Prewarm.requires_response());
        assert!(!RequestOrigin::Prefetch.requires_response());
    }

    #[test]
    fn test_request_origin_display() {
        assert_eq!(format!("{}", RequestOrigin::Fuse), "fuse");
        assert_eq!(format!("{}", RequestOrigin::Prefetch), "prefetch");
        assert_eq!(format!("{}", RequestOrigin::Prewarm), "prewarm");
        assert_eq!(format!("{}", RequestOrigin::Manual), "manual");
    }

    #[test]
    fn test_dds_response_creation() {
        let data = vec![1, 2, 3, 4];
        let duration = Duration::from_millis(100);

        let response = DdsResponse::new(data.clone(), true, duration, true);
        assert_eq!(response.data, data);
        assert!(response.cache_hit);
        assert_eq!(response.duration, duration);
        assert!(response.is_success());
    }

    #[test]
    fn test_dds_response_cache_hit() {
        let data = vec![1, 2, 3];
        let response = DdsResponse::cache_hit(data.clone(), Duration::from_millis(10));

        assert!(response.cache_hit);
        assert!(response.has_data());
        assert!(response.is_success());
    }

    #[test]
    fn test_dds_response_cache_miss() {
        let data = vec![1, 2, 3];
        let response = DdsResponse::cache_miss(data.clone(), Duration::from_secs(1));

        assert!(!response.cache_hit);
        assert!(response.has_data());
        assert!(response.is_success());
    }

    #[test]
    fn test_dds_response_empty() {
        let response = DdsResponse::empty(Duration::from_secs(10));

        assert!(!response.cache_hit);
        assert!(!response.has_data());
        assert!(!response.is_success());
    }

    #[test]
    fn test_dds_response_job_succeeded_no_data() {
        // Job succeeded but data couldn't be read from cache
        let response = DdsResponse::new(Vec::new(), false, Duration::from_secs(1), true);

        assert!(!response.has_data());
        assert!(response.is_success()); // Job still succeeded
    }

    #[tokio::test]
    async fn test_fuse_response_channel_works() {
        let (request, rx) = JobRequest::fuse(test_tile(), CancellationToken::new());

        // Simulate sending a response
        if let Some(tx) = request.response_tx {
            let response = DdsResponse::cache_hit(vec![1, 2, 3], Duration::from_millis(50));
            tx.send(response).unwrap();
        }

        // Receive the response
        let received = rx.await.unwrap();
        assert_eq!(received.data, vec![1, 2, 3]);
        assert!(received.cache_hit);
    }

    #[test]
    fn test_request_debug_format() {
        let request = JobRequest::prefetch(test_tile());
        let debug = format!("{:?}", request);

        assert!(debug.contains("JobRequest"));
        assert!(debug.contains("Prefetch"));
        assert!(debug.contains("expects_response: false"));
    }
}
