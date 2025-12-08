//! Job and task models for the async pipeline.
//!
//! A Job represents a request from X-Plane for a DDS file. It is the unit of
//! work from the filesystem's perspective. Jobs flow through the pipeline
//! stages and produce a JobResult containing the generated DDS data.

use crate::coord::TileCoord;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Global counter for generating unique job IDs.
static JOB_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Unique identifier for a job in the pipeline.
///
/// Job IDs are monotonically increasing and unique within a process lifetime.
/// They are used for:
/// - Correlating log messages and telemetry
/// - Request coalescing (deduplication)
/// - Debugging failed tiles (see E5: Diagnostic Magenta Chunks)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);

impl JobId {
    /// Creates a new unique job ID.
    ///
    /// IDs are guaranteed to be unique within the process lifetime.
    pub fn new() -> Self {
        Self(JOB_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw numeric value of this job ID.
    ///
    /// Useful for logging and encoding into diagnostic chunks.
    #[inline]
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// Priority level for job scheduling.
///
/// Currently not used for scheduling decisions, but reserved for future
/// optimization (e.g., prioritizing tiles closer to the camera).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Priority {
    /// Low priority - background prefetching
    Low,
    /// Normal priority - standard tile requests
    #[default]
    Normal,
    /// High priority - tiles in immediate view
    High,
}

/// A request from X-Plane for a DDS tile.
///
/// Jobs are created by the FUSE dispatch layer and flow through the async
/// pipeline. Multiple FUSE threads may create jobs for the same tile
/// simultaneously; these are coalesced into a single job with multiple waiters.
pub struct Job {
    /// Unique identifier for this job
    pub id: JobId,

    /// Tile coordinates being requested
    pub tile_coords: TileCoord,

    /// Priority for scheduling (reserved for future use)
    pub priority: Priority,

    /// When the job was created
    pub created_at: Instant,

    /// Channel to send result back to FUSE handler(s)
    ///
    /// For coalesced requests, the result is wrapped in Arc and sent to
    /// multiple receivers.
    pub result_sender: oneshot::Sender<JobResult>,
}

impl Job {
    /// Creates a new job for the given tile coordinates.
    ///
    /// # Arguments
    ///
    /// * `tile_coords` - The tile to generate
    /// * `priority` - Scheduling priority
    /// * `result_sender` - Channel for sending back the result
    pub fn new(
        tile_coords: TileCoord,
        priority: Priority,
        result_sender: oneshot::Sender<JobResult>,
    ) -> Self {
        Self {
            id: JobId::new(),
            tile_coords,
            priority,
            created_at: Instant::now(),
            result_sender,
        }
    }

    /// Returns the elapsed time since this job was created.
    #[inline]
    pub fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("tile_coords", &self.tile_coords)
            .field("priority", &self.priority)
            .field("created_at", &self.created_at)
            .field("result_sender", &"<oneshot::Sender>")
            .finish()
    }
}

/// Result of processing a job through the pipeline.
///
/// Contains either the generated DDS data or indicates that a placeholder
/// was returned due to failures.
#[derive(Debug, Clone)]
pub struct JobResult {
    /// The job ID this result corresponds to
    pub job_id: JobId,

    /// The generated DDS texture data
    pub dds_data: Vec<u8>,

    /// Total time spent processing this job
    pub duration: Duration,

    /// Number of chunks that failed to download (0-256)
    pub failed_chunks: u16,

    /// Whether this result was served from cache
    pub cache_hit: bool,
}

impl JobResult {
    /// Creates a new successful job result.
    pub fn success(job_id: JobId, dds_data: Vec<u8>, duration: Duration) -> Self {
        Self {
            job_id,
            dds_data,
            duration,
            failed_chunks: 0,
            cache_hit: false,
        }
    }

    /// Creates a job result with partial failures.
    pub fn partial(
        job_id: JobId,
        dds_data: Vec<u8>,
        duration: Duration,
        failed_chunks: u16,
    ) -> Self {
        Self {
            job_id,
            dds_data,
            duration,
            failed_chunks,
            cache_hit: false,
        }
    }

    /// Creates a cache hit result.
    pub fn cache_hit(job_id: JobId, dds_data: Vec<u8>, duration: Duration) -> Self {
        Self {
            job_id,
            dds_data,
            duration,
            failed_chunks: 0,
            cache_hit: true,
        }
    }

    /// Returns true if all chunks were successfully downloaded.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.failed_chunks == 0
    }

    /// Returns the size of the generated DDS data in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.dds_data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_id_unique() {
        let id1 = JobId::new();
        let id2 = JobId::new();
        let id3 = JobId::new();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_job_id_monotonic() {
        let id1 = JobId::new();
        let id2 = JobId::new();

        assert!(id2.as_u64() > id1.as_u64());
    }

    #[test]
    fn test_job_id_display() {
        let id = JobId(42);
        assert_eq!(format!("{}", id), "job-42");
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Low < Priority::Normal);
        assert!(Priority::Normal < Priority::High);
    }

    #[test]
    fn test_priority_default() {
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[test]
    fn test_job_creation() {
        let (tx, _rx) = oneshot::channel();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let job = Job::new(tile, Priority::High, tx);

        assert_eq!(job.tile_coords, tile);
        assert_eq!(job.priority, Priority::High);
        assert!(job.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_job_result_success() {
        let id = JobId::new();
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let duration = Duration::from_millis(100);

        let result = JobResult::success(id, data.clone(), duration);

        assert_eq!(result.job_id, id);
        assert_eq!(result.dds_data, data);
        assert_eq!(result.duration, duration);
        assert_eq!(result.failed_chunks, 0);
        assert!(!result.cache_hit);
        assert!(result.is_complete());
    }

    #[test]
    fn test_job_result_partial() {
        let id = JobId::new();
        let result = JobResult::partial(id, vec![1, 2, 3], Duration::from_secs(1), 5);

        assert_eq!(result.failed_chunks, 5);
        assert!(!result.is_complete());
    }

    #[test]
    fn test_job_result_cache_hit() {
        let id = JobId::new();
        let result = JobResult::cache_hit(id, vec![1, 2, 3], Duration::from_millis(1));

        assert!(result.cache_hit);
        assert!(result.is_complete());
    }

    #[test]
    fn test_job_result_size() {
        let id = JobId::new();
        let data = vec![0u8; 1024];
        let result = JobResult::success(id, data, Duration::ZERO);

        assert_eq!(result.size(), 1024);
    }
}
