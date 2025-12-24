//! Job registry for tracking active pipeline jobs.
//!
//! The registry tracks all jobs flowing through the pipeline, enabling:
//! - Stage progression monitoring
//! - Stall detection (jobs exceeding time thresholds)
//! - Active recovery of stuck jobs
//!
//! Uses lock-free data structures for minimal contention on the hot path.

use crate::coord::TileCoord;
use crate::pipeline::JobId;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Pipeline stage for a job.
///
/// Jobs progress through stages in order. The stage is stored as an atomic
/// u8 for lock-free updates from any task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JobStage {
    /// Job is queued, waiting for a job slot
    Queued = 0,
    /// Job is downloading chunks from the provider
    Downloading = 1,
    /// Job is assembling chunks into a tile image
    Assembling = 2,
    /// Job is encoding the tile to DDS format
    Encoding = 3,
    /// Job is writing to cache
    Caching = 4,
    /// Job completed successfully
    Completed = 5,
    /// Job was recovered by the control plane due to stall
    Recovered = 6,
}

impl JobStage {
    /// Converts from u8 representation.
    #[inline]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Queued),
            1 => Some(Self::Downloading),
            2 => Some(Self::Assembling),
            3 => Some(Self::Encoding),
            4 => Some(Self::Caching),
            5 => Some(Self::Completed),
            6 => Some(Self::Recovered),
            _ => None,
        }
    }

    /// Returns true if this is a terminal stage.
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Recovered)
    }

    /// Returns the stage name for logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Assembling => "assembling",
            Self::Encoding => "encoding",
            Self::Caching => "caching",
            Self::Completed => "completed",
            Self::Recovered => "recovered",
        }
    }
}

impl std::fmt::Display for JobStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Entry for a tracked job in the registry.
///
/// Contains all metadata needed for monitoring and recovery.
/// The struct is designed to be cheaply clonable via Arc.
pub struct JobEntry {
    /// Unique job identifier
    pub job_id: JobId,
    /// Tile being processed
    pub tile: TileCoord,
    /// When the job started (for stall detection)
    pub started_at: Instant,
    /// Current pipeline stage (atomic for lock-free updates)
    stage: AtomicU8,
    /// Cancellation token for this job
    pub cancellation_token: CancellationToken,
    /// Whether this is a prefetch job (lower priority)
    pub is_prefetch: bool,
}

impl JobEntry {
    /// Creates a new job entry.
    pub fn new(
        job_id: JobId,
        tile: TileCoord,
        cancellation_token: CancellationToken,
        is_prefetch: bool,
    ) -> Self {
        Self {
            job_id,
            tile,
            started_at: Instant::now(),
            stage: AtomicU8::new(JobStage::Queued as u8),
            cancellation_token,
            is_prefetch,
        }
    }

    /// Returns the current stage.
    #[inline]
    pub fn stage(&self) -> JobStage {
        JobStage::from_u8(self.stage.load(Ordering::Acquire)).unwrap_or(JobStage::Queued)
    }

    /// Updates the job stage.
    ///
    /// This is a lock-free atomic operation.
    #[inline]
    pub fn set_stage(&self, stage: JobStage) {
        self.stage.store(stage as u8, Ordering::Release);
    }

    /// Returns elapsed time since job started.
    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Cancels this job by triggering its cancellation token.
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Returns true if this job has been cancelled.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }
}

impl std::fmt::Debug for JobEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobEntry")
            .field("job_id", &self.job_id)
            .field("tile", &self.tile)
            .field("started_at", &self.started_at)
            .field("stage", &self.stage())
            .field("is_prefetch", &self.is_prefetch)
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Registry for tracking all active pipeline jobs.
///
/// Provides lock-free job registration, lookup, and removal using DashMap.
/// Statistics are tracked with atomic counters for minimal overhead.
pub struct JobRegistry {
    /// Active jobs indexed by JobId
    jobs: DashMap<JobId, Arc<JobEntry>>,
    /// Reverse index: tile -> job_id (for coalescer integration)
    jobs_by_tile: DashMap<TileCoord, JobId>,
    /// Total jobs registered (lifetime counter)
    total_jobs: AtomicU64,
    /// Jobs completed successfully
    completed_jobs: AtomicU64,
    /// Jobs recovered due to stall
    recovered_jobs: AtomicU64,
    /// Jobs that failed
    failed_jobs: AtomicU64,
}

impl JobRegistry {
    /// Creates a new empty registry.
    pub fn new() -> Self {
        Self {
            jobs: DashMap::new(),
            jobs_by_tile: DashMap::new(),
            total_jobs: AtomicU64::new(0),
            completed_jobs: AtomicU64::new(0),
            recovered_jobs: AtomicU64::new(0),
            failed_jobs: AtomicU64::new(0),
        }
    }

    /// Registers a new job in the registry.
    ///
    /// Returns the job entry wrapped in Arc for shared access.
    pub fn register(
        &self,
        job_id: JobId,
        tile: TileCoord,
        cancellation_token: CancellationToken,
        is_prefetch: bool,
    ) -> Arc<JobEntry> {
        let entry = Arc::new(JobEntry::new(job_id, tile, cancellation_token, is_prefetch));

        self.jobs.insert(job_id, Arc::clone(&entry));
        self.jobs_by_tile.insert(tile, job_id);
        self.total_jobs.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(
            job_id = %job_id,
            tile = ?tile,
            is_prefetch = is_prefetch,
            "Registered job in control plane"
        );

        entry
    }

    /// Looks up a job by ID.
    pub fn get(&self, job_id: JobId) -> Option<Arc<JobEntry>> {
        self.jobs.get(&job_id).map(|r| Arc::clone(r.value()))
    }

    /// Looks up a job by tile coordinates.
    pub fn get_by_tile(&self, tile: TileCoord) -> Option<Arc<JobEntry>> {
        self.jobs_by_tile
            .get(&tile)
            .and_then(|job_id| self.get(*job_id))
    }

    /// Updates the stage of a job.
    ///
    /// Returns true if the job was found and updated.
    pub fn update_stage(&self, job_id: JobId, stage: JobStage) -> bool {
        if let Some(entry) = self.jobs.get(&job_id) {
            entry.set_stage(stage);
            tracing::trace!(
                job_id = %job_id,
                stage = %stage,
                "Job stage updated"
            );
            true
        } else {
            false
        }
    }

    /// Marks a job as completed and removes it from the registry.
    ///
    /// Call this when a job finishes successfully.
    pub fn complete(&self, job_id: JobId) {
        if let Some((_, entry)) = self.jobs.remove(&job_id) {
            self.jobs_by_tile.remove(&entry.tile);
            self.completed_jobs.fetch_add(1, Ordering::Relaxed);

            tracing::debug!(
                job_id = %job_id,
                elapsed_ms = entry.elapsed().as_millis(),
                "Job completed"
            );
        }
    }

    /// Marks a job as recovered and removes it from the registry.
    ///
    /// Call this after the control plane recovers a stalled job.
    pub fn mark_recovered(&self, job_id: JobId) {
        if let Some((_, entry)) = self.jobs.remove(&job_id) {
            self.jobs_by_tile.remove(&entry.tile);
            self.recovered_jobs.fetch_add(1, Ordering::Relaxed);

            tracing::warn!(
                job_id = %job_id,
                tile = ?entry.tile,
                stage = %entry.stage(),
                elapsed_secs = entry.elapsed().as_secs(),
                "Job recovered by control plane"
            );
        }
    }

    /// Marks a job as failed and removes it from the registry.
    ///
    /// Call this when a job fails (not due to stall recovery).
    pub fn mark_failed(&self, job_id: JobId) {
        if let Some((_, entry)) = self.jobs.remove(&job_id) {
            self.jobs_by_tile.remove(&entry.tile);
            self.failed_jobs.fetch_add(1, Ordering::Relaxed);

            tracing::debug!(
                job_id = %job_id,
                elapsed_ms = entry.elapsed().as_millis(),
                "Job failed"
            );
        }
    }

    /// Returns the number of currently active jobs.
    #[inline]
    pub fn active_count(&self) -> usize {
        self.jobs.len()
    }

    /// Returns jobs that have exceeded the stall threshold.
    ///
    /// Used by the health monitor to detect stuck jobs.
    pub fn find_stalled(&self, threshold: std::time::Duration) -> Vec<Arc<JobEntry>> {
        self.jobs
            .iter()
            .filter(|entry| {
                let elapsed = entry.value().elapsed();
                let stage = entry.value().stage();
                // Only consider non-terminal stages as potentially stalled
                !stage.is_terminal() && elapsed > threshold
            })
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Returns a snapshot of registry statistics.
    pub fn stats(&self) -> RegistryStats {
        // Count jobs by stage
        let mut by_stage = [0usize; 7];
        let mut prefetch_count = 0usize;

        for entry in self.jobs.iter() {
            let stage = entry.value().stage() as usize;
            if stage < by_stage.len() {
                by_stage[stage] += 1;
            }
            if entry.value().is_prefetch {
                prefetch_count += 1;
            }
        }

        RegistryStats {
            active_jobs: self.jobs.len(),
            total_jobs: self.total_jobs.load(Ordering::Relaxed),
            completed_jobs: self.completed_jobs.load(Ordering::Relaxed),
            recovered_jobs: self.recovered_jobs.load(Ordering::Relaxed),
            failed_jobs: self.failed_jobs.load(Ordering::Relaxed),
            jobs_queued: by_stage[JobStage::Queued as usize],
            jobs_downloading: by_stage[JobStage::Downloading as usize],
            jobs_assembling: by_stage[JobStage::Assembling as usize],
            jobs_encoding: by_stage[JobStage::Encoding as usize],
            jobs_caching: by_stage[JobStage::Caching as usize],
            prefetch_jobs: prefetch_count,
        }
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of registry statistics.
#[derive(Debug, Clone, Default)]
pub struct RegistryStats {
    /// Currently active jobs
    pub active_jobs: usize,
    /// Total jobs registered (lifetime)
    pub total_jobs: u64,
    /// Jobs completed successfully
    pub completed_jobs: u64,
    /// Jobs recovered by control plane
    pub recovered_jobs: u64,
    /// Jobs that failed
    pub failed_jobs: u64,
    /// Jobs in Queued stage
    pub jobs_queued: usize,
    /// Jobs in Downloading stage
    pub jobs_downloading: usize,
    /// Jobs in Assembling stage
    pub jobs_assembling: usize,
    /// Jobs in Encoding stage
    pub jobs_encoding: usize,
    /// Jobs in Caching stage
    pub jobs_caching: usize,
    /// Prefetch jobs (subset of active)
    pub prefetch_jobs: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile(row: u32, col: u32) -> TileCoord {
        TileCoord { row, col, zoom: 14 }
    }

    #[test]
    fn test_job_stage_from_u8() {
        assert_eq!(JobStage::from_u8(0), Some(JobStage::Queued));
        assert_eq!(JobStage::from_u8(1), Some(JobStage::Downloading));
        assert_eq!(JobStage::from_u8(5), Some(JobStage::Completed));
        assert_eq!(JobStage::from_u8(6), Some(JobStage::Recovered));
        assert_eq!(JobStage::from_u8(7), None);
        assert_eq!(JobStage::from_u8(255), None);
    }

    #[test]
    fn test_job_stage_is_terminal() {
        assert!(!JobStage::Queued.is_terminal());
        assert!(!JobStage::Downloading.is_terminal());
        assert!(!JobStage::Assembling.is_terminal());
        assert!(!JobStage::Encoding.is_terminal());
        assert!(!JobStage::Caching.is_terminal());
        assert!(JobStage::Completed.is_terminal());
        assert!(JobStage::Recovered.is_terminal());
    }

    #[test]
    fn test_job_entry_stage_updates() {
        let token = CancellationToken::new();
        let entry = JobEntry::new(JobId::new(), test_tile(0, 0), token, false);

        assert_eq!(entry.stage(), JobStage::Queued);

        entry.set_stage(JobStage::Downloading);
        assert_eq!(entry.stage(), JobStage::Downloading);

        entry.set_stage(JobStage::Encoding);
        assert_eq!(entry.stage(), JobStage::Encoding);
    }

    #[test]
    fn test_job_entry_cancellation() {
        let token = CancellationToken::new();
        let entry = JobEntry::new(JobId::new(), test_tile(0, 0), token.clone(), false);

        assert!(!entry.is_cancelled());

        entry.cancel();
        assert!(entry.is_cancelled());
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = JobRegistry::new();
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        let entry = registry.register(job_id, tile, token, false);

        assert_eq!(entry.job_id, job_id);
        assert_eq!(entry.tile, tile);
        assert!(!entry.is_prefetch);

        // Get by job ID
        let found = registry.get(job_id).unwrap();
        assert_eq!(found.job_id, job_id);

        // Get by tile
        let found = registry.get_by_tile(tile).unwrap();
        assert_eq!(found.job_id, job_id);
    }

    #[test]
    fn test_registry_complete() {
        let registry = JobRegistry::new();
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        registry.register(job_id, tile, token, false);
        assert_eq!(registry.active_count(), 1);

        registry.complete(job_id);
        assert_eq!(registry.active_count(), 0);
        assert!(registry.get(job_id).is_none());
        assert!(registry.get_by_tile(tile).is_none());

        let stats = registry.stats();
        assert_eq!(stats.completed_jobs, 1);
    }

    #[test]
    fn test_registry_mark_recovered() {
        let registry = JobRegistry::new();
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        registry.register(job_id, tile, token, false);
        registry.mark_recovered(job_id);

        assert_eq!(registry.active_count(), 0);
        let stats = registry.stats();
        assert_eq!(stats.recovered_jobs, 1);
    }

    #[test]
    fn test_registry_update_stage() {
        let registry = JobRegistry::new();
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        let token = CancellationToken::new();

        registry.register(job_id, tile, token, false);

        assert!(registry.update_stage(job_id, JobStage::Downloading));
        let entry = registry.get(job_id).unwrap();
        assert_eq!(entry.stage(), JobStage::Downloading);

        // Non-existent job
        assert!(!registry.update_stage(JobId::new(), JobStage::Encoding));
    }

    #[test]
    fn test_registry_find_stalled() {
        let registry = JobRegistry::new();
        let token = CancellationToken::new();

        // Register a job
        let job_id = JobId::new();
        let tile = test_tile(100, 200);
        registry.register(job_id, tile, token, false);

        // With zero threshold, job should be stalled
        let stalled = registry.find_stalled(std::time::Duration::ZERO);
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].job_id, job_id);

        // With very long threshold, no jobs stalled
        let stalled = registry.find_stalled(std::time::Duration::from_secs(3600));
        assert!(stalled.is_empty());
    }

    #[test]
    fn test_registry_stats() {
        let registry = JobRegistry::new();

        // Register several jobs
        for i in 0..5 {
            let job_id = JobId::new();
            let tile = test_tile(i, 0);
            let token = CancellationToken::new();
            let entry = registry.register(job_id, tile, token, i % 2 == 0);

            // Set different stages
            match i {
                0 => entry.set_stage(JobStage::Downloading),
                1 => entry.set_stage(JobStage::Assembling),
                2 => entry.set_stage(JobStage::Encoding),
                3 => entry.set_stage(JobStage::Caching),
                _ => {} // Stays Queued
            }
        }

        let stats = registry.stats();
        assert_eq!(stats.active_jobs, 5);
        assert_eq!(stats.total_jobs, 5);
        assert_eq!(stats.jobs_queued, 1);
        assert_eq!(stats.jobs_downloading, 1);
        assert_eq!(stats.jobs_assembling, 1);
        assert_eq!(stats.jobs_encoding, 1);
        assert_eq!(stats.jobs_caching, 1);
        assert_eq!(stats.prefetch_jobs, 3); // 0, 2, 4 are prefetch
    }
}
