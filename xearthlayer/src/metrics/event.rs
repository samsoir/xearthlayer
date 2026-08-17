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
//! - Chunk disk cache events: per-chunk (256 per tile)
//! - DDS disk cache events: per-tile
//! - Memory cache events: tile-level (checked in daemon)
//! - Job events: per-job (one per tile request)
//! - FUSE events: per-request

use crate::cache::DiskTier;

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
    // Chunk Disk Cache Events (per-chunk granularity)
    // =========================================================================
    /// A chunk was found in the chunk disk cache.
    ChunkDiskCacheHit {
        /// Size of the cached chunk in bytes.
        bytes: u64,
    },

    /// A chunk was not found in the chunk disk cache.
    ChunkDiskCacheMiss,

    // =========================================================================
    // DDS Disk Cache Events (per-tile granularity)
    // =========================================================================
    /// A DDS tile was found in the DDS disk cache.
    DdsDiskCacheHit {
        /// Size of the cached DDS tile in bytes.
        bytes: u64,
        /// `true` if this was a FUSE (X-Plane) request, `false` for prefetch/prewarm.
        is_fuse: bool,
    },

    /// A DDS tile was not found in the DDS disk cache.
    DdsDiskCacheMiss {
        /// `true` if this was a FUSE (X-Plane) request, `false` for prefetch/prewarm.
        is_fuse: bool,
    },

    /// A disk cache write operation started.
    DiskWriteStarted,

    /// A disk cache write operation completed.
    DiskWriteCompleted {
        /// Number of bytes written.
        bytes: u64,
        /// Which cache tier the bytes belong to.
        tier: DiskTier,
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

    /// Update the current chunk disk cache size (absolute value from LRU index).
    ///
    /// Emitted by the chunk `DiskCacheProvider` after writes and evictions.
    DiskCacheSizeUpdate {
        /// Current total size in bytes (from LRU index).
        bytes: u64,
    },

    /// Update the current DDS disk cache size (absolute value from LRU index).
    ///
    /// Emitted by the DDS `DiskCacheProvider` after writes and evictions.
    DdsDiskCacheSizeUpdate {
        /// Current total size in bytes (from LRU index).
        bytes: u64,
    },

    /// Update the current chunk LRU index entry count.
    ///
    /// Emitted by the chunk `DiskCacheProvider` after writes and evictions.
    /// The index is a memory consumer in its own right — millions of entries on
    /// a full cache — so it is tracked alongside the byte totals.
    ChunkIndexEntriesUpdate {
        /// Current number of entries in the chunk LRU index.
        entries: u64,
    },

    // =========================================================================
    // Memory Cache Events (tile-level, tracked in daemon)
    // =========================================================================
    /// A tile was found in the memory cache.
    MemoryCacheHit {
        /// `true` if this was a FUSE (X-Plane) request, `false` for prefetch/prewarm.
        is_fuse: bool,
    },

    /// A tile was not found in the memory cache.
    MemoryCacheMiss {
        /// `true` if this was a FUSE (X-Plane) request, `false` for prefetch/prewarm.
        is_fuse: bool,
    },

    /// Update the current memory cache size.
    MemoryCacheSizeUpdate {
        /// Current size in bytes.
        bytes: u64,
    },

    /// A fire-and-forget memory cache write operation started.
    ///
    /// Mirrors `DiskWriteStarted`/`DiskWriteCompleted` but for the memory-cache
    /// spawn in `BuildAndCacheDdsTask`, which previously emitted no start/complete
    /// pair at all — only `MemoryCacheSizeUpdate`, a size gauge that only moves
    /// after `cache.put()` actually completes. A backlog concentrated in that
    /// spawn (each task pinning its own ~11.2 MB DDS clone) would present as
    /// rising RSS with both `disk_writes_active` and `chunk_index_entries` flat,
    /// misreading as allocator retention (candidate 2) instead of the
    /// fire-and-forget backlog (candidate 1). See issue #209.
    MemCacheWriteStarted,

    /// A fire-and-forget memory cache write operation completed.
    MemCacheWriteCompleted,

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
    /// A FUSE tile was served (from any source: memory, DDS disk, or job).
    ///
    /// This counts total FUSE tile responses, not just job submissions.
    /// Used by the TUI to show actual tile request throughput.
    FuseTileServed,

    /// A FUSE request started being handled.
    FuseRequestStarted,

    /// A FUSE request completed.
    FuseRequestCompleted,

    /// A FUSE request entered the wait queue.
    FuseRequestQueued,

    /// A FUSE request was removed from the wait queue.
    FuseRequestDequeued,

    // =========================================================================
    // Prefetch Region State Events (#176)
    // =========================================================================
    /// Current region-state distribution, reported each maintenance cycle.
    ///
    /// This is a gauge, not a counter: each event replaces the previous values.
    PrefetchRegionState {
        /// Regions with tiles submitted, awaiting confirmation.
        in_progress: usize,
        /// Regions confirmed fully cached.
        prefetched: usize,
        /// Regions with no ortho scenery.
        no_coverage: usize,
        /// Regions currently deferred for making no progress (#226).
        ///
        /// Deliberately its own bucket rather than folded into
        /// `no_coverage`: `regions_nocoverage` non-zero only over water is an
        /// acceptance criterion, and a deferred land region counted there
        /// would be exactly the false alarm #226 exists to remove.
        deferred: usize,
    },

    /// FUSE generated a tile in a region prefetch claimed was handled.
    ///
    /// Counted on every occurrence, including those where the rate limit
    /// suppressed the demotion — so the metric shows the true divergence
    /// rate rather than the demotion rate.
    PrefetchStateDiverged,

    /// A region's state was cleared in response to observed divergence.
    PrefetchRegionDemoted,

    /// A region was deferred after making no progress since the last evaluation.
    PrefetchRegionDeferred,

    /// A region's deferral was cleared because X-Plane demanded a tile there.
    ///
    /// The post-fix analogue of [`Self::PrefetchRegionDemoted`]: prefetch gave
    /// up on the region (deferred it), then the sim asked for tiles inside it
    /// anyway. Counted, not logged above `debug!` — it fires once per
    /// FUSE-generated tile in the region, which is too frequent for the
    /// default log level — so this counter is the only default-level signal
    /// for whether the 20/30/40/60s deferral ladder is well-tuned (#226).
    PrefetchDeferralCleared,

    /// Regions promoted from `InProgress` to `Prefetched` via the normal
    /// completion path (all tiles in the region confirmed present).
    ///
    /// `count` batches multiple regions promoted in the same maintenance
    /// cycle into one event, mirroring how the coordinator reports them.
    PrefetchRegionsPromotedNormal {
        /// Number of regions promoted in this batch.
        count: usize,
    },

    /// A region promoted via the rescue path (recovered from a state that
    /// would otherwise have left it stuck, e.g. a missed normal promotion).
    PrefetchRegionPromotedRescue,
}

impl MetricEvent {
    /// Returns a short name for this event type (useful for debugging).
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::DownloadStarted => "download_started",
            Self::DownloadCompleted { .. } => "download_completed",
            Self::DownloadFailed => "download_failed",
            Self::DownloadRetried => "download_retried",
            Self::ChunkDiskCacheHit { .. } => "chunk_disk_cache_hit",
            Self::ChunkDiskCacheMiss => "chunk_disk_cache_miss",
            Self::DdsDiskCacheHit { .. } => "dds_disk_cache_hit",
            Self::DdsDiskCacheMiss { .. } => "dds_disk_cache_miss",
            Self::DiskWriteStarted => "disk_write_started",
            Self::DiskWriteCompleted { .. } => "disk_write_completed",
            Self::DiskCacheInitialSize { .. } => "disk_cache_initial_size",
            Self::DiskCacheEvicted { .. } => "disk_cache_evicted",
            Self::DiskCacheSizeUpdate { .. } => "disk_cache_size_update",
            Self::DdsDiskCacheSizeUpdate { .. } => "dds_disk_cache_size_update",
            Self::ChunkIndexEntriesUpdate { .. } => "chunk_index_entries_update",
            Self::MemoryCacheHit { .. } => "memory_cache_hit",
            Self::MemoryCacheMiss { .. } => "memory_cache_miss",
            Self::MemoryCacheSizeUpdate { .. } => "memory_cache_size_update",
            Self::MemCacheWriteStarted => "mem_cache_write_started",
            Self::MemCacheWriteCompleted => "mem_cache_write_completed",
            Self::JobSubmitted { .. } => "job_submitted",
            Self::JobStarted => "job_started",
            Self::JobCompleted { .. } => "job_completed",
            Self::JobCoalesced => "job_coalesced",
            Self::JobTimedOut => "job_timed_out",
            Self::EncodeStarted => "encode_started",
            Self::EncodeCompleted { .. } => "encode_completed",
            Self::AssemblyCompleted { .. } => "assembly_completed",
            Self::FuseTileServed => "fuse_tile_served",
            Self::FuseRequestStarted => "fuse_request_started",
            Self::FuseRequestCompleted => "fuse_request_completed",
            Self::FuseRequestQueued => "fuse_request_queued",
            Self::FuseRequestDequeued => "fuse_request_dequeued",
            Self::PrefetchRegionState { .. } => "prefetch_region_state",
            Self::PrefetchStateDiverged => "prefetch_state_diverged",
            Self::PrefetchRegionDemoted => "prefetch_region_demoted",
            Self::PrefetchRegionDeferred => "prefetch_region_deferred",
            Self::PrefetchDeferralCleared => "prefetch_deferral_cleared",
            Self::PrefetchRegionsPromotedNormal { .. } => "prefetch_regions_promoted_normal",
            Self::PrefetchRegionPromotedRescue => "prefetch_region_promoted_rescue",
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

    #[test]
    fn test_dds_disk_cache_event_types() {
        assert_eq!(
            MetricEvent::DdsDiskCacheHit {
                bytes: 1024,
                is_fuse: false
            }
            .event_type(),
            "dds_disk_cache_hit"
        );
        assert_eq!(
            MetricEvent::DdsDiskCacheMiss { is_fuse: false }.event_type(),
            "dds_disk_cache_miss"
        );
    }

    #[test]
    fn test_mem_cache_write_event_types() {
        assert_eq!(
            MetricEvent::MemCacheWriteStarted.event_type(),
            "mem_cache_write_started"
        );
        assert_eq!(
            MetricEvent::MemCacheWriteCompleted.event_type(),
            "mem_cache_write_completed"
        );
    }

    #[test]
    fn test_chunk_disk_cache_event_types() {
        assert_eq!(
            MetricEvent::ChunkDiskCacheHit { bytes: 1024 }.event_type(),
            "chunk_disk_cache_hit"
        );
        assert_eq!(
            MetricEvent::ChunkDiskCacheMiss.event_type(),
            "chunk_disk_cache_miss"
        );
    }
}
