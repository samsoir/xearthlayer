//! Serializable views of core types for the frontend.
//!
//! Core types are not `Serialize`, and the frontend needs only part of each,
//! so every command answers with one of these plain structs.

use serde::Serialize;
use xearthlayer::metrics::TelemetrySnapshot;

/// The dashboard's figures, from one [`TelemetrySnapshot`].
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryView {
    pub uptime_secs: u64,
    pub fuse_tiles_served: u64,
    pub fuse_requests_active: usize,
    pub fuse_requests_waiting: usize,
    pub jobs_submitted: u64,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
    pub jobs_timed_out: u64,
    pub jobs_active: usize,
    pub jobs_coalesced: u64,
    pub jobs_per_second: f64,
    pub chunks_downloaded: u64,
    pub chunks_failed: u64,
    pub chunks_retried: u64,
    pub bytes_downloaded: u64,
    pub downloads_active: usize,
    pub bytes_per_second: f64,
    pub peak_bytes_per_second: f64,
    pub encodes_completed: u64,
    pub encodes_active: usize,
    /// X-Plane's own hit rates (FUSE requests only, #171).
    pub memory_hit_rate: f64,
    pub dds_disk_hit_rate: f64,
    pub chunk_disk_hit_rate: f64,
    pub memory_cache_bytes: u64,
    pub dds_disk_cache_bytes: u64,
    pub chunk_disk_cache_bytes: u64,
}

impl From<&TelemetrySnapshot> for TelemetryView {
    fn from(s: &TelemetrySnapshot) -> Self {
        Self {
            uptime_secs: s.uptime.as_secs(),
            fuse_tiles_served: s.fuse_tiles_served,
            fuse_requests_active: s.fuse_requests_active,
            fuse_requests_waiting: s.fuse_requests_waiting,
            jobs_submitted: s.jobs_submitted,
            jobs_completed: s.jobs_completed,
            jobs_failed: s.jobs_failed,
            jobs_timed_out: s.jobs_timed_out,
            jobs_active: s.jobs_active,
            jobs_coalesced: s.jobs_coalesced,
            jobs_per_second: s.jobs_per_second,
            chunks_downloaded: s.chunks_downloaded,
            chunks_failed: s.chunks_failed,
            chunks_retried: s.chunks_retried,
            bytes_downloaded: s.bytes_downloaded,
            downloads_active: s.downloads_active,
            bytes_per_second: s.bytes_per_second,
            peak_bytes_per_second: s.peak_bytes_per_second,
            encodes_completed: s.encodes_completed,
            encodes_active: s.encodes_active,
            memory_hit_rate: s.fuse_memory_cache_hit_rate,
            dds_disk_hit_rate: s.fuse_dds_disk_cache_hit_rate,
            chunk_disk_hit_rate: s.chunk_disk_cache_hit_rate,
            memory_cache_bytes: s.memory_cache_size_bytes,
            dds_disk_cache_bytes: s.dds_disk_cache_size_bytes,
            chunk_disk_cache_bytes: s.chunk_disk_cache_size_bytes,
        }
    }
}

/// Run blocking work off the main thread: Tauri runs synchronous commands on
/// it, and a slow one would freeze the window.
pub async fn blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("Background task failed: {e}"))?
}
