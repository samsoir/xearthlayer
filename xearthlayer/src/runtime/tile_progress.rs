//! Tile progress tracking for TUI display.
//!
//! Tracks the progress of active DDS tile generation jobs for display
//! in the dashboard UI. Each tile shows completion percentage based on
//! tasks completed (DownloadChunks → BuildAndCacheDds).
//!
//! # Architecture
//!
//! ```text
//! Executor                    TileProgressTracker              TUI
//!    │                              │                           │
//!    │ task_started(tile)           │                           │
//!    ├─────────────────────────────►│                           │
//!    │                              │ add entry (0%)            │
//!    │                              │                           │
//!    │ task_completed(tile, 1/2)    │                           │
//!    ├─────────────────────────────►│                           │
//!    │                              │ update to 50%             │
//!    │                              │                           │
//!    │                              │ snapshot()                │
//!    │                              │◄────────────────────────────
//!    │                              │ Vec<TileProgressEntry>    │
//!    │                              ├────────────────────────────►
//! ```

use crate::coord::TileCoord;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

/// Maximum number of tile entries to keep for display.
/// Older entries are removed when this limit is exceeded.
pub const MAX_DISPLAY_ENTRIES: usize = 4;

/// Progress entry for a single tile being generated.
#[derive(Debug, Clone, PartialEq)]
pub struct TileProgressEntry {
    /// Tile coordinates being generated.
    pub tile: TileCoord,
    /// Number of tasks completed (0, 1, or 2).
    pub tasks_completed: u8,
    /// Total number of tasks (always 2 for DDS generation).
    pub tasks_total: u8,
}

impl TileProgressEntry {
    /// Create a new progress entry for a tile.
    pub fn new(tile: TileCoord) -> Self {
        Self {
            tile,
            tasks_completed: 0,
            tasks_total: 2, // DownloadChunks + BuildAndCacheDds
        }
    }

    /// Get progress as a percentage (0-100).
    pub fn progress_percent(&self) -> u8 {
        if self.tasks_total == 0 {
            return 100;
        }
        ((self.tasks_completed as u16 * 100) / self.tasks_total as u16) as u8
    }

    /// Check if the tile generation is complete.
    pub fn is_complete(&self) -> bool {
        self.tasks_completed >= self.tasks_total
    }

    /// Format the tile coordinate for display (e.g., "140E,35S").
    pub fn format_coordinate(&self) -> String {
        let (lat, lon) = self.tile.to_lat_lon();
        let lat_dir = if lat >= 0.0 { "N" } else { "S" };
        let lon_dir = if lon >= 0.0 { "E" } else { "W" };
        format!(
            "{:.0}{},{}{}",
            lon.abs(),
            lon_dir,
            lat.abs().floor(),
            lat_dir
        )
    }
}

/// Thread-safe tracker for active tile generation progress.
///
/// Maintains a bounded queue of recent tile progress entries for TUI display.
/// Entries are added when tile generation starts and removed when complete
/// or when the queue exceeds [`MAX_DISPLAY_ENTRIES`].
#[derive(Debug, Default)]
pub struct TileProgressTracker {
    /// Active tile progress entries (most recent first).
    entries: RwLock<VecDeque<TileProgressEntry>>,
}

impl TileProgressTracker {
    /// Create a new tile progress tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(VecDeque::with_capacity(MAX_DISPLAY_ENTRIES + 1)),
        })
    }

    /// Start tracking a new tile.
    ///
    /// Adds the tile to the front of the queue. If the queue exceeds
    /// [`MAX_DISPLAY_ENTRIES`], the oldest entry is removed.
    pub fn tile_started(&self, tile: TileCoord) {
        let mut entries = self.entries.write().unwrap();

        // Check if tile is already being tracked
        if entries.iter().any(|e| e.tile == tile) {
            return;
        }

        // Add to front (most recent first)
        entries.push_front(TileProgressEntry::new(tile));

        // Trim to max size
        while entries.len() > MAX_DISPLAY_ENTRIES {
            entries.pop_back();
        }
    }

    /// Update progress for a tile (task completed).
    ///
    /// Increments the tasks_completed counter for the specified tile.
    /// If the tile reaches 100% completion, it is removed from tracking.
    pub fn task_completed(&self, tile: TileCoord) {
        let mut entries = self.entries.write().unwrap();

        if let Some(entry) = entries.iter_mut().find(|e| e.tile == tile) {
            entry.tasks_completed += 1;

            // Remove completed entries
            if entry.is_complete() {
                entries.retain(|e| e.tile != tile);
            }
        }
    }

    /// Mark a tile as failed/cancelled (remove from tracking).
    pub fn tile_failed(&self, tile: TileCoord) {
        let mut entries = self.entries.write().unwrap();
        entries.retain(|e| e.tile != tile);
    }

    /// Get a snapshot of current progress entries for display.
    ///
    /// Returns entries ordered by most recent first.
    pub fn snapshot(&self) -> Vec<TileProgressEntry> {
        let entries = self.entries.read().unwrap();
        entries.iter().cloned().collect()
    }

    /// Get the number of tiles currently being tracked.
    pub fn active_count(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    /// Clear all tracked entries.
    pub fn clear(&self) {
        self.entries.write().unwrap().clear();
    }
}

/// Shared tile progress tracker.
pub type SharedTileProgressTracker = Arc<TileProgressTracker>;

// =============================================================================
// Telemetry Sink Integration
// =============================================================================

use crate::executor::{TelemetryEvent, TelemetrySink};

/// Telemetry sink that updates the tile progress tracker.
///
/// This sink listens for job/task events and updates the progress tracker
/// for DDS generation jobs. It extracts tile coordinates from the job ID
/// format: `dds-{row}_{col}_ZL{zoom}`.
#[derive(Debug)]
pub struct TileProgressSink {
    tracker: SharedTileProgressTracker,
}

impl TileProgressSink {
    /// Create a new tile progress sink.
    pub fn new(tracker: SharedTileProgressTracker) -> Arc<Self> {
        Arc::new(Self { tracker })
    }

    /// Parse tile coordinates from a DDS job ID.
    ///
    /// Job ID format: `dds-{row}_{col}_ZL{zoom}`
    /// Returns (row, col, zoom) if parsing succeeds.
    fn parse_dds_job_id(job_id: &str) -> Option<TileCoord> {
        if !job_id.starts_with("dds-") {
            return None;
        }

        let parts: Vec<&str> = job_id[4..].split('_').collect();
        if parts.len() != 3 {
            return None;
        }

        let row: u32 = parts[0].parse().ok()?;
        let col: u32 = parts[1].parse().ok()?;

        // Parse zoom from "ZL{zoom}"
        let zoom_str = parts[2].strip_prefix("ZL")?;
        let zoom: u8 = zoom_str.parse().ok()?;

        Some(TileCoord { row, col, zoom })
    }
}

impl TelemetrySink for TileProgressSink {
    fn emit(&self, event: TelemetryEvent) {
        match event {
            TelemetryEvent::JobStarted { job_id } => {
                if let Some(tile) = Self::parse_dds_job_id(job_id.as_str()) {
                    self.tracker.tile_started(tile);
                }
            }
            TelemetryEvent::TaskCompleted { job_id, .. } => {
                if let Some(tile) = Self::parse_dds_job_id(job_id.as_str()) {
                    self.tracker.task_completed(tile);
                }
            }
            TelemetryEvent::JobCompleted { job_id, status, .. } => {
                if let Some(tile) = Self::parse_dds_job_id(job_id.as_str()) {
                    // Remove from tracker if job failed or was cancelled
                    if status != crate::executor::JobStatus::Succeeded {
                        self.tracker.tile_failed(tile);
                    }
                    // Successful completions are auto-removed when tasks_completed reaches 2
                }
            }
            _ => {} // Ignore other events
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile(row: u32, col: u32) -> TileCoord {
        TileCoord { row, col, zoom: 16 }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // TileProgressEntry tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_entry_new() {
        let tile = test_tile(100, 200);
        let entry = TileProgressEntry::new(tile);

        assert_eq!(entry.tile, tile);
        assert_eq!(entry.tasks_completed, 0);
        assert_eq!(entry.tasks_total, 2);
    }

    #[test]
    fn test_entry_progress_percent() {
        let tile = test_tile(100, 200);
        let mut entry = TileProgressEntry::new(tile);

        assert_eq!(entry.progress_percent(), 0);

        entry.tasks_completed = 1;
        assert_eq!(entry.progress_percent(), 50);

        entry.tasks_completed = 2;
        assert_eq!(entry.progress_percent(), 100);
    }

    #[test]
    fn test_entry_is_complete() {
        let tile = test_tile(100, 200);
        let mut entry = TileProgressEntry::new(tile);

        assert!(!entry.is_complete());

        entry.tasks_completed = 1;
        assert!(!entry.is_complete());

        entry.tasks_completed = 2;
        assert!(entry.is_complete());
    }

    #[test]
    fn test_entry_format_coordinate() {
        // Test tile at approximately 140E, 35S (Sydney area)
        // Row/col calculation is complex, so we test the format logic
        let tile = TileCoord {
            row: 39000,
            col: 59000,
            zoom: 16,
        };
        let entry = TileProgressEntry::new(tile);
        let formatted = entry.format_coordinate();

        // Should contain cardinal directions
        assert!(
            formatted.contains('E') || formatted.contains('W'),
            "Should have E/W: {}",
            formatted
        );
        assert!(
            formatted.contains('N') || formatted.contains('S'),
            "Should have N/S: {}",
            formatted
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // TileProgressTracker tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_tracker_new() {
        let tracker = TileProgressTracker::new();
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_tile_started() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);

        assert_eq!(tracker.active_count(), 1);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].tile, tile);
        assert_eq!(snapshot[0].progress_percent(), 0);
    }

    #[test]
    fn test_tracker_duplicate_tile_ignored() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.tile_started(tile); // Duplicate

        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn test_tracker_task_completed() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.task_completed(tile);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].progress_percent(), 50);
    }

    #[test]
    fn test_tracker_tile_auto_removed_on_complete() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.task_completed(tile); // 50%
        tracker.task_completed(tile); // 100% - should be removed

        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_tile_failed() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.tile_failed(tile);

        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_max_entries() {
        let tracker = TileProgressTracker::new();

        // Add more tiles than MAX_DISPLAY_ENTRIES
        for i in 0..(MAX_DISPLAY_ENTRIES + 3) {
            tracker.tile_started(test_tile(i as u32, 0));
        }

        // Should be capped at MAX_DISPLAY_ENTRIES
        assert_eq!(tracker.active_count(), MAX_DISPLAY_ENTRIES);
    }

    #[test]
    fn test_tracker_most_recent_first() {
        let tracker = TileProgressTracker::new();

        tracker.tile_started(test_tile(1, 0));
        tracker.tile_started(test_tile(2, 0));
        tracker.tile_started(test_tile(3, 0));

        let snapshot = tracker.snapshot();
        // Most recently added should be first
        assert_eq!(snapshot[0].tile.row, 3);
        assert_eq!(snapshot[1].tile.row, 2);
        assert_eq!(snapshot[2].tile.row, 1);
    }

    #[test]
    fn test_tracker_clear() {
        let tracker = TileProgressTracker::new();

        tracker.tile_started(test_tile(1, 0));
        tracker.tile_started(test_tile(2, 0));
        tracker.clear();

        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_snapshot_is_independent() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        let snapshot1 = tracker.snapshot();

        // Modify tracker after snapshot
        tracker.task_completed(tile);
        let snapshot2 = tracker.snapshot();

        // Snapshots should be independent
        assert_eq!(snapshot1[0].progress_percent(), 0);
        assert_eq!(snapshot2[0].progress_percent(), 50);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // TileProgressSink tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_dds_job_id_valid() {
        let tile = TileProgressSink::parse_dds_job_id("dds-100_200_ZL16");
        assert!(tile.is_some());
        let tile = tile.unwrap();
        assert_eq!(tile.row, 100);
        assert_eq!(tile.col, 200);
        assert_eq!(tile.zoom, 16);
    }

    #[test]
    fn test_parse_dds_job_id_invalid_prefix() {
        assert!(TileProgressSink::parse_dds_job_id("prefetch-100_200_ZL16").is_none());
        assert!(TileProgressSink::parse_dds_job_id("xyz-100_200_ZL16").is_none());
    }

    #[test]
    fn test_parse_dds_job_id_invalid_format() {
        assert!(TileProgressSink::parse_dds_job_id("dds-100_200").is_none()); // Missing zoom
        assert!(TileProgressSink::parse_dds_job_id("dds-100").is_none()); // Missing parts
        assert!(TileProgressSink::parse_dds_job_id("dds-abc_200_ZL16").is_none());
        // Non-numeric
    }

    #[test]
    fn test_sink_job_started() {
        use crate::executor::JobId;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("dds-100_200_ZL16"),
        });

        assert_eq!(tracker.active_count(), 1);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].tile.row, 100);
        assert_eq!(snapshot[0].tile.col, 200);
    }

    #[test]
    fn test_sink_task_completed() {
        use crate::executor::JobId;
        use crate::executor::TaskResultKind;
        use std::time::Duration;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        // Start job
        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("dds-100_200_ZL16"),
        });

        // Complete first task
        sink.emit(TelemetryEvent::TaskCompleted {
            job_id: JobId::new("dds-100_200_ZL16"),
            task_name: "DownloadChunks".to_string(),
            result: TaskResultKind::Success,
            duration: Duration::from_millis(100),
        });

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].progress_percent(), 50);
    }

    #[test]
    fn test_sink_job_completed_removes_on_failure() {
        use crate::executor::{JobId, JobStatus};
        use std::time::Duration;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        // Start job
        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("dds-100_200_ZL16"),
        });
        assert_eq!(tracker.active_count(), 1);

        // Job fails
        sink.emit(TelemetryEvent::JobCompleted {
            job_id: JobId::new("dds-100_200_ZL16"),
            status: JobStatus::Failed,
            duration: Duration::from_millis(100),
            tasks_succeeded: 0,
            tasks_failed: 1,
            children_succeeded: 0,
            children_failed: 0,
        });

        // Should be removed
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_sink_ignores_non_dds_jobs() {
        use crate::executor::JobId;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("prefetch-47.60_-122.33_ZL14_r5"),
        });

        // Prefetch jobs should be ignored
        assert_eq!(tracker.active_count(), 0);
    }
}
