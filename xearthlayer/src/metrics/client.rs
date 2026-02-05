//! Metrics emission layer.
//!
//! The [`MetricsClient`] provides a fire-and-forget interface for emitting
//! metric events. It's designed to be:
//!
//! - **Cheap to clone**: Backed by a channel sender
//! - **Fire-and-forget**: Never blocks, silently drops if channel is full
//! - **Type-safe**: Convenience methods for each event type
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::metrics::MetricsClient;
//!
//! let client: MetricsClient = ...;
//!
//! // Record a download completion
//! client.download_completed(30_000, 5_000);
//!
//! // Record a cache hit
//! client.disk_cache_hit(30_000);
//! ```

use super::event::MetricEvent;
use tokio::sync::mpsc;

/// Client for emitting metric events to the metrics daemon.
///
/// This is the primary interface for components to record metrics. It wraps
/// an unbounded channel sender and provides typed convenience methods for
/// each event type.
///
/// # Fire-and-Forget Semantics
///
/// All methods are fire-and-forget: they never block and silently ignore
/// failures (e.g., if the daemon has shut down). This ensures metrics
/// collection never impacts pipeline performance.
///
/// # Cloning
///
/// The client is cheaply cloneable - internally it wraps an `Arc` around
/// the channel sender. Clone it freely to distribute to multiple components.
#[derive(Clone)]
pub struct MetricsClient {
    tx: mpsc::UnboundedSender<MetricEvent>,
}

impl MetricsClient {
    /// Creates a new metrics client with the given channel sender.
    pub fn new(tx: mpsc::UnboundedSender<MetricEvent>) -> Self {
        Self { tx }
    }

    /// Sends an event to the daemon (fire-and-forget).
    #[inline]
    fn send(&self, event: MetricEvent) {
        // Ignore send errors - daemon may have shut down
        let _ = self.tx.send(event);
    }

    // =========================================================================
    // Download Events
    // =========================================================================

    /// Records a download starting.
    #[inline]
    pub fn download_started(&self) {
        self.send(MetricEvent::DownloadStarted);
    }

    /// Records a download completing successfully.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes downloaded
    /// * `duration_us` - Time taken in microseconds
    #[inline]
    pub fn download_completed(&self, bytes: u64, duration_us: u64) {
        self.send(MetricEvent::DownloadCompleted { bytes, duration_us });
    }

    /// Records a download failing.
    #[inline]
    pub fn download_failed(&self) {
        self.send(MetricEvent::DownloadFailed);
    }

    /// Records a download retry attempt.
    #[inline]
    pub fn download_retried(&self) {
        self.send(MetricEvent::DownloadRetried);
    }

    // =========================================================================
    // Disk Cache Events
    // =========================================================================

    /// Records a disk cache hit.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Size of the cached chunk
    #[inline]
    pub fn disk_cache_hit(&self, bytes: u64) {
        self.send(MetricEvent::DiskCacheHit { bytes });
    }

    /// Records a disk cache miss.
    #[inline]
    pub fn disk_cache_miss(&self) {
        self.send(MetricEvent::DiskCacheMiss);
    }

    /// Records a disk write starting.
    #[inline]
    pub fn disk_write_started(&self) {
        self.send(MetricEvent::DiskWriteStarted);
    }

    /// Records a disk write completing.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes written
    /// * `duration_us` - Time taken in microseconds
    #[inline]
    pub fn disk_write_completed(&self, bytes: u64, duration_us: u64) {
        self.send(MetricEvent::DiskWriteCompleted { bytes, duration_us });
    }

    /// Sets the initial disk cache size (scanned on startup).
    ///
    /// This should be called once at startup after scanning the disk cache.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Total bytes already in the disk cache
    #[inline]
    pub fn disk_cache_initial_size(&self, bytes: u64) {
        self.send(MetricEvent::DiskCacheInitialSize { bytes });
    }

    /// Records bytes evicted from disk cache by the GC daemon.
    ///
    /// # Arguments
    ///
    /// * `bytes_freed` - Number of bytes freed by eviction
    #[inline]
    pub fn disk_cache_evicted(&self, bytes_freed: u64) {
        self.send(MetricEvent::DiskCacheEvicted { bytes_freed });
    }

    /// Updates the current disk cache size (absolute value from LRU index).
    ///
    /// This should be called after writes and evictions to report the
    /// authoritative cache size directly from the LRU index.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Current total cache size in bytes
    #[inline]
    pub fn disk_cache_size(&self, bytes: u64) {
        self.send(MetricEvent::DiskCacheSizeUpdate { bytes });
    }

    // =========================================================================
    // Memory Cache Events
    // =========================================================================

    /// Records a memory cache hit.
    #[inline]
    pub fn memory_cache_hit(&self) {
        self.send(MetricEvent::MemoryCacheHit);
    }

    /// Records a memory cache miss.
    #[inline]
    pub fn memory_cache_miss(&self) {
        self.send(MetricEvent::MemoryCacheMiss);
    }

    /// Updates the current memory cache size.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Current cache size in bytes
    #[inline]
    pub fn memory_cache_size(&self, bytes: u64) {
        self.send(MetricEvent::MemoryCacheSizeUpdate { bytes });
    }

    // =========================================================================
    // Job Events
    // =========================================================================

    /// Records a job being submitted.
    ///
    /// # Arguments
    ///
    /// * `is_fuse` - True if this is a FUSE request (X-Plane), false for prefetch
    #[inline]
    pub fn job_submitted(&self, is_fuse: bool) {
        self.send(MetricEvent::JobSubmitted { is_fuse });
    }

    /// Records a job starting execution.
    #[inline]
    pub fn job_started(&self) {
        self.send(MetricEvent::JobStarted);
    }

    /// Records a job completing.
    ///
    /// # Arguments
    ///
    /// * `success` - True if the job succeeded
    /// * `duration_us` - Total job duration in microseconds
    #[inline]
    pub fn job_completed(&self, success: bool, duration_us: u64) {
        self.send(MetricEvent::JobCompleted {
            success,
            duration_us,
        });
    }

    /// Records a job being coalesced (waited for existing work).
    #[inline]
    pub fn job_coalesced(&self) {
        self.send(MetricEvent::JobCoalesced);
    }

    /// Records a job timing out.
    #[inline]
    pub fn job_timed_out(&self) {
        self.send(MetricEvent::JobTimedOut);
    }

    // =========================================================================
    // Encode Events
    // =========================================================================

    /// Records an encode operation starting.
    #[inline]
    pub fn encode_started(&self) {
        self.send(MetricEvent::EncodeStarted);
    }

    /// Records an encode operation completing.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Size of the encoded DDS
    /// * `duration_us` - Time taken in microseconds
    #[inline]
    pub fn encode_completed(&self, bytes: u64, duration_us: u64) {
        self.send(MetricEvent::EncodeCompleted { bytes, duration_us });
    }

    // =========================================================================
    // Assembly Events
    // =========================================================================

    /// Records chunk assembly completing.
    ///
    /// # Arguments
    ///
    /// * `duration_us` - Time taken in microseconds
    #[inline]
    pub fn assembly_completed(&self, duration_us: u64) {
        self.send(MetricEvent::AssemblyCompleted { duration_us });
    }

    // =========================================================================
    // FUSE Events
    // =========================================================================

    /// Records a FUSE request starting.
    #[inline]
    pub fn fuse_request_started(&self) {
        self.send(MetricEvent::FuseRequestStarted);
    }

    /// Records a FUSE request completing.
    #[inline]
    pub fn fuse_request_completed(&self) {
        self.send(MetricEvent::FuseRequestCompleted);
    }

    /// Records a FUSE request entering the wait queue.
    #[inline]
    pub fn fuse_request_queued(&self) {
        self.send(MetricEvent::FuseRequestQueued);
    }

    /// Records a FUSE request leaving the wait queue.
    #[inline]
    pub fn fuse_request_dequeued(&self) {
        self.send(MetricEvent::FuseRequestDequeued);
    }
}

impl std::fmt::Debug for MetricsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsClient")
            .field("channel_closed", &self.tx.is_closed())
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_client() -> (MetricsClient, mpsc::UnboundedReceiver<MetricEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (MetricsClient::new(tx), rx)
    }

    #[tokio::test]
    async fn test_client_download_events() {
        let (client, mut rx) = create_client();

        client.download_started();
        client.download_completed(1024, 5000);
        client.download_failed();
        client.download_retried();

        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DownloadStarted)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DownloadCompleted {
                bytes: 1024,
                duration_us: 5000
            })
        ));
        assert!(matches!(rx.recv().await, Some(MetricEvent::DownloadFailed)));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DownloadRetried)
        ));
    }

    #[tokio::test]
    async fn test_client_cache_events() {
        let (client, mut rx) = create_client();

        client.disk_cache_hit(2048);
        client.disk_cache_miss();
        client.disk_write_completed(2048, 1000);
        client.disk_cache_size(9_000_000_000);
        client.memory_cache_hit();
        client.memory_cache_miss();
        client.memory_cache_size(1_000_000);

        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DiskCacheHit { bytes: 2048 })
        ));
        assert!(matches!(rx.recv().await, Some(MetricEvent::DiskCacheMiss)));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DiskWriteCompleted {
                bytes: 2048,
                duration_us: 1000
            })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::DiskCacheSizeUpdate {
                bytes: 9_000_000_000
            })
        ));
        assert!(matches!(rx.recv().await, Some(MetricEvent::MemoryCacheHit)));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::MemoryCacheMiss)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::MemoryCacheSizeUpdate { bytes: 1_000_000 })
        ));
    }

    #[tokio::test]
    async fn test_client_job_events() {
        let (client, mut rx) = create_client();

        client.job_submitted(true);
        client.job_started();
        client.job_completed(true, 100_000);
        client.job_coalesced();
        client.job_timed_out();

        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::JobSubmitted { is_fuse: true })
        ));
        assert!(matches!(rx.recv().await, Some(MetricEvent::JobStarted)));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::JobCompleted {
                success: true,
                duration_us: 100_000
            })
        ));
        assert!(matches!(rx.recv().await, Some(MetricEvent::JobCoalesced)));
        assert!(matches!(rx.recv().await, Some(MetricEvent::JobTimedOut)));
    }

    #[tokio::test]
    async fn test_client_encode_events() {
        let (client, mut rx) = create_client();

        client.encode_started();
        client.encode_completed(5_000_000, 200_000);

        assert!(matches!(rx.recv().await, Some(MetricEvent::EncodeStarted)));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::EncodeCompleted {
                bytes: 5_000_000,
                duration_us: 200_000
            })
        ));
    }

    #[tokio::test]
    async fn test_client_fuse_events() {
        let (client, mut rx) = create_client();

        client.fuse_request_started();
        client.fuse_request_queued();
        client.fuse_request_dequeued();
        client.fuse_request_completed();

        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::FuseRequestStarted)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::FuseRequestQueued)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::FuseRequestDequeued)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(MetricEvent::FuseRequestCompleted)
        ));
    }

    #[test]
    fn test_client_clone() {
        let (client, _rx) = create_client();
        let cloned = client.clone();

        // Both should work - fire-and-forget
        client.download_started();
        cloned.download_started();
    }

    #[test]
    fn test_client_dropped_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = MetricsClient::new(tx);
        drop(rx);

        // Should not panic - fire-and-forget semantics
        client.download_started();
        client.job_completed(true, 1000);
    }

    #[test]
    fn test_client_debug() {
        let (client, _rx) = create_client();
        let debug = format!("{:?}", client);
        assert!(debug.contains("MetricsClient"));
    }
}
