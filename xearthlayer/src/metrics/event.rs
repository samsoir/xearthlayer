//! Metric events for the emission layer.
//!
//! This module defines all the metric events that can be emitted by various
//! components of the pipeline. Events are fire-and-forget - producers send
//! them to the daemon without waiting for acknowledgment.
//!
//! # Event Granularity
//!
//! Events are designed at the appropriate granularity for each component:
//! - Download events: per-chunk (256 per tile)
//! - Disk cache events: per-chunk
//! - Memory cache events: tile-level (checked in daemon)
//! - Job events: per-job (one per tile request)
//! - FUSE events: per-request

/// Events emitted by pipeline components to the metrics daemon.
///
/// Each event represents an atomic occurrence that updates metrics state.
/// Events are processed sequentially by the daemon to maintain consistency.
#[derive(Clone, Debug)]
pub enum MetricEvent {
    // =========================================================================
    // Download Events (per-chunk granularity - 256 per tile)
    // =========================================================================
    /// A chunk download has started.
    DownloadStarted,

    /// A chunk download completed successfully.
    DownloadCompleted {
        /// Number of bytes downloaded.
        bytes: u64,
        /// Time taken in microseconds.
        duration_us: u64,
    },

    /// A chunk download failed after all retries.
    DownloadFailed,

    /// A download retry is being attempted.
    DownloadRetried,

    // =========================================================================
    // Disk Cache Events (per-chunk granularity)
    // =========================================================================
    /// A chunk was found in the disk cache.
    DiskCacheHit {
        /// Size of the cached chunk in bytes.
        bytes: u64,
    },

    /// A chunk was not found in the disk cache.
    DiskCacheMiss,

    /// A disk cache write operation started.
    DiskWriteStarted,

    /// A disk cache write operation completed.
    DiskWriteCompleted {
        /// Number of bytes written.
        bytes: u64,
        /// Time taken in microseconds.
        duration_us: u64,
    },

    /// Set the initial disk cache size (scanned on startup).
    DiskCacheInitialSize {
        /// Total bytes already in the disk cache at startup.
        bytes: u64,
    },

    /// Disk cache eviction completed (background GC).
    DiskCacheEvicted {
        /// Number of bytes freed by eviction.
        bytes_freed: u64,
    },

    /// Update the current disk cache size (absolute value from LRU index).
    ///
    /// Emitted after writes and evictions to track the authoritative cache size
    /// directly, replacing the fragile `initial + written - evicted` formula.
    DiskCacheSizeUpdate {
        /// Current total size in bytes (from LRU index).
        bytes: u64,
    },

    // =========================================================================
    // Memory Cache Events (tile-level, tracked in daemon)
    // =========================================================================
    /// A tile was found in the memory cache.
    MemoryCacheHit,

    /// A tile was not found in the memory cache.
    MemoryCacheMiss,

    /// Update the current memory cache size.
    MemoryCacheSizeUpdate {
        /// Current size in bytes.
        bytes: u64,
    },

    // =========================================================================
    // Job Lifecycle Events
    // =========================================================================
    /// A job was submitted to the executor.
    JobSubmitted {
        /// True if this is a FUSE (X-Plane) request, false for prefetch.
        is_fuse: bool,
    },

    /// A job started executing.
    JobStarted,

    /// A job completed execution.
    JobCompleted {
        /// True if the job succeeded.
        success: bool,
        /// Total job duration in microseconds.
        duration_us: u64,
    },

    /// A job was coalesced (waited for existing work).
    JobCoalesced,

    /// A job timed out.
    JobTimedOut,

    // =========================================================================
    // Encode Events
    // =========================================================================
    /// A DDS encode operation started.
    EncodeStarted,

    /// A DDS encode operation completed.
    EncodeCompleted {
        /// Size of the encoded DDS in bytes.
        bytes: u64,
        /// Time taken in microseconds.
        duration_us: u64,
    },

    // =========================================================================
    // Assembly Events
    // =========================================================================
    /// Chunk assembly completed.
    AssemblyCompleted {
        /// Time taken in microseconds.
        duration_us: u64,
    },

    // =========================================================================
    // FUSE Request Events
    // =========================================================================
    /// A FUSE request started being handled.
    FuseRequestStarted,

    /// A FUSE request completed.
    FuseRequestCompleted,

    /// A FUSE request entered the wait queue.
    FuseRequestQueued,

    /// A FUSE request was removed from the wait queue.
    FuseRequestDequeued,
}

impl MetricEvent {
    /// Returns a short name for this event type (useful for debugging).
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::DownloadStarted => "download_started",
            Self::DownloadCompleted { .. } => "download_completed",
            Self::DownloadFailed => "download_failed",
            Self::DownloadRetried => "download_retried",
            Self::DiskCacheHit { .. } => "disk_cache_hit",
            Self::DiskCacheMiss => "disk_cache_miss",
            Self::DiskWriteStarted => "disk_write_started",
            Self::DiskWriteCompleted { .. } => "disk_write_completed",
            Self::DiskCacheInitialSize { .. } => "disk_cache_initial_size",
            Self::DiskCacheEvicted { .. } => "disk_cache_evicted",
            Self::DiskCacheSizeUpdate { .. } => "disk_cache_size_update",
            Self::MemoryCacheHit => "memory_cache_hit",
            Self::MemoryCacheMiss => "memory_cache_miss",
            Self::MemoryCacheSizeUpdate { .. } => "memory_cache_size_update",
            Self::JobSubmitted { .. } => "job_submitted",
            Self::JobStarted => "job_started",
            Self::JobCompleted { .. } => "job_completed",
            Self::JobCoalesced => "job_coalesced",
            Self::JobTimedOut => "job_timed_out",
            Self::EncodeStarted => "encode_started",
            Self::EncodeCompleted { .. } => "encode_completed",
            Self::AssemblyCompleted { .. } => "assembly_completed",
            Self::FuseRequestStarted => "fuse_request_started",
            Self::FuseRequestCompleted => "fuse_request_completed",
            Self::FuseRequestQueued => "fuse_request_queued",
            Self::FuseRequestDequeued => "fuse_request_dequeued",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_types() {
        assert_eq!(
            MetricEvent::DownloadStarted.event_type(),
            "download_started"
        );
        assert_eq!(
            MetricEvent::DownloadCompleted {
                bytes: 100,
                duration_us: 1000
            }
            .event_type(),
            "download_completed"
        );
        assert_eq!(
            MetricEvent::JobSubmitted { is_fuse: true }.event_type(),
            "job_submitted"
        );
    }

    #[test]
    fn test_event_debug() {
        let event = MetricEvent::DownloadCompleted {
            bytes: 1024,
            duration_us: 5000,
        };
        let debug = format!("{:?}", event);
        assert!(debug.contains("DownloadCompleted"));
        assert!(debug.contains("1024"));
    }

    #[test]
    fn test_event_clone() {
        let event = MetricEvent::JobSubmitted { is_fuse: true };
        let cloned = event.clone();
        assert_eq!(event.event_type(), cloned.event_type());
    }
}
