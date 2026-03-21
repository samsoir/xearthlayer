//! Tile progress tracking for TUI display.
//!
//! Tracks DDS tile generation progress aggregated by 1°×1° DSF region.
//! The executor reports individual tile events; this module coalesces them
//! into per-region summaries for the TUI queue display.
//!
//! # Architecture
//!
//! ```text
//! Executor                    TileProgressTracker              TUI
//!    │                              │                           │
//!    │ tile_started(tile)           │                           │
//!    ├─────────────────────────────►│ derive DSF region         │
//!    │                              │ increment tiles_total     │
//!    │                              │                           │
//!    │ tile_completed(tile)         │                           │
//!    ├─────────────────────────────►│ increment tiles_completed │
//!    │                              │                           │
//!    │                              │ snapshot()                │
//!    │                              │◄────────────────────────────
//!    │                              │ Vec<RegionProgressEntry>  │
//!    │                              ├────────────────────────────►
//! ```

use crate::coord::TileCoord;
use crate::geo_index::DsfRegion;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Maximum number of region entries to return in a snapshot.
pub const MAX_DISPLAY_REGIONS: usize = 4;

/// Progress entry for a DSF region (1°×1° area).
///
/// Aggregates progress across all tiles being generated in this region.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionProgressEntry {
    /// The 1°×1° DSF region.
    pub region: DsfRegion,
    /// Number of tiles submitted for generation in this region.
    pub tiles_total: u16,
    /// Number of tiles that have completed generation.
    pub tiles_completed: u16,
    /// When the first tile in this region was submitted.
    pub started_at: Instant,
}

impl RegionProgressEntry {
    /// Get progress as a percentage (0-100).
    pub fn progress_percent(&self) -> u8 {
        if self.tiles_total == 0 {
            return 0;
        }
        ((self.tiles_completed as u32 * 100) / self.tiles_total as u32) as u8
    }

    /// Check if all tiles in this region are complete.
    pub fn is_complete(&self) -> bool {
        self.tiles_total > 0 && self.tiles_completed >= self.tiles_total
    }

    /// Format the region coordinate for display (e.g., "15E,48N").
    pub fn format_coordinate(&self) -> String {
        let lat = self.region.lat;
        let lon = self.region.lon;
        let lat_dir = if lat >= 0 { "N" } else { "S" };
        let lon_dir = if lon >= 0 { "E" } else { "W" };
        format!("{}{},{}{}", lon.abs(), lon_dir, lat.abs(), lat_dir)
    }
}

/// Internal tracking state for a single region.
#[derive(Debug, Clone)]
struct RegionState {
    tiles_total: u16,
    tiles_completed: u16,
    /// Per-tile task counters (tasks_completed out of 2).
    /// Tracks individual tiles so we know when a tile finishes.
    tile_tasks: HashMap<TileCoord, u8>,
    started_at: Instant,
}

/// Thread-safe tracker for tile generation progress, aggregated by DSF region.
///
/// Individual tile events are rolled up into per-region summaries.
/// Completed regions are automatically removed.
#[derive(Debug, Default)]
pub struct TileProgressTracker {
    regions: RwLock<HashMap<DsfRegion, RegionState>>,
}

impl TileProgressTracker {
    /// Create a new tile progress tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            regions: RwLock::new(HashMap::new()),
        })
    }

    /// Record that a tile has started generation.
    ///
    /// Derives the DSF region from the tile coordinate and increments
    /// the region's total tile count.
    pub fn tile_started(&self, tile: TileCoord) {
        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);

        let mut regions = self.regions.write().unwrap();
        let state = regions.entry(region).or_insert_with(|| RegionState {
            tiles_total: 0,
            tiles_completed: 0,
            tile_tasks: HashMap::new(),
            started_at: Instant::now(),
        });

        // Only count if this tile hasn't been seen before
        if !state.tile_tasks.contains_key(&tile) {
            state.tiles_total += 1;
            state.tile_tasks.insert(tile, 0);
        }
    }

    /// Record that a task completed for a tile.
    ///
    /// Each tile has 2 tasks (DownloadChunks + BuildAndCacheDds).
    /// When both tasks complete, the tile is marked as completed
    /// in its region's counter.
    pub fn task_completed(&self, tile: TileCoord) {
        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);

        let mut regions = self.regions.write().unwrap();
        if let Some(state) = regions.get_mut(&region) {
            if let Some(tasks) = state.tile_tasks.get_mut(&tile) {
                *tasks += 1;
                if *tasks >= 2 {
                    // Tile is done — increment region completed count
                    state.tiles_completed += 1;
                    state.tile_tasks.remove(&tile);
                }
            }

            // Remove region if fully complete
            if state.tiles_completed >= state.tiles_total && state.tile_tasks.is_empty() {
                regions.remove(&region);
            }
        }
    }

    /// Mark a tile as failed (remove from tracking without counting as completed).
    pub fn tile_failed(&self, tile: TileCoord) {
        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);

        let mut regions = self.regions.write().unwrap();
        if let Some(state) = regions.get_mut(&region) {
            if state.tile_tasks.remove(&tile).is_some() {
                // Reduce total since this tile won't complete
                state.tiles_total = state.tiles_total.saturating_sub(1);
            }

            // Remove region if nothing left
            if state.tiles_total == 0
                || (state.tiles_completed >= state.tiles_total && state.tile_tasks.is_empty())
            {
                regions.remove(&region);
            }
        }
    }

    /// Get a snapshot of current region progress for display.
    ///
    /// Returns up to [`MAX_DISPLAY_REGIONS`] entries, sorted by
    /// most recently started first.
    pub fn snapshot(&self) -> Vec<RegionProgressEntry> {
        let regions = self.regions.read().unwrap();
        let mut entries: Vec<RegionProgressEntry> = regions
            .iter()
            .map(|(region, state)| RegionProgressEntry {
                region: *region,
                tiles_total: state.tiles_total,
                tiles_completed: state.tiles_completed,
                started_at: state.started_at,
            })
            .collect();

        // Sort by most recently started first
        entries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        entries.truncate(MAX_DISPLAY_REGIONS);
        entries
    }

    /// Get the number of regions currently being tracked.
    pub fn active_count(&self) -> usize {
        self.regions.read().unwrap().len()
    }

    /// Clear all tracked entries.
    pub fn clear(&self) {
        self.regions.write().unwrap().clear();
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
/// Listens for job/task events and updates the progress tracker
/// for DDS generation jobs. Extracts tile coordinates from the job ID
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
                    if status != crate::executor::JobStatus::Succeeded {
                        self.tracker.tile_failed(tile);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile(row: u32, col: u32) -> TileCoord {
        TileCoord { row, col, zoom: 12 }
    }

    /// Create two tiles that map to the same DSF region.
    fn tiles_in_same_region() -> (TileCoord, TileCoord) {
        // At zoom 12, tiles are ~0.088° wide. Two adjacent tiles
        // in the same 1° region:
        let t1 = TileCoord {
            row: 1500,
            col: 2200,
            zoom: 12,
        };
        let t2 = TileCoord {
            row: 1500,
            col: 2201,
            zoom: 12,
        };

        // Verify they're in the same DSF region
        let (lat1, lon1) = t1.to_lat_lon();
        let (lat2, lon2) = t2.to_lat_lon();
        assert_eq!(lat1.floor() as i32, lat2.floor() as i32);
        assert_eq!(lon1.floor() as i32, lon2.floor() as i32);

        (t1, t2)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // RegionProgressEntry tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_entry_progress_percent() {
        let entry = RegionProgressEntry {
            region: DsfRegion::new(48, 15),
            tiles_total: 10,
            tiles_completed: 3,
            started_at: Instant::now(),
        };
        assert_eq!(entry.progress_percent(), 30);
    }

    #[test]
    fn test_entry_progress_percent_zero_total() {
        let entry = RegionProgressEntry {
            region: DsfRegion::new(48, 15),
            tiles_total: 0,
            tiles_completed: 0,
            started_at: Instant::now(),
        };
        assert_eq!(entry.progress_percent(), 0);
    }

    #[test]
    fn test_entry_is_complete() {
        let mut entry = RegionProgressEntry {
            region: DsfRegion::new(48, 15),
            tiles_total: 5,
            tiles_completed: 4,
            started_at: Instant::now(),
        };
        assert!(!entry.is_complete());

        entry.tiles_completed = 5;
        assert!(entry.is_complete());
    }

    #[test]
    fn test_entry_format_coordinate() {
        let entry = RegionProgressEntry {
            region: DsfRegion::new(48, 15),
            tiles_total: 1,
            tiles_completed: 0,
            started_at: Instant::now(),
        };
        assert_eq!(entry.format_coordinate(), "15E,48N");
    }

    #[test]
    fn test_entry_format_coordinate_negative() {
        let entry = RegionProgressEntry {
            region: DsfRegion::new(-34, 151),
            tiles_total: 1,
            tiles_completed: 0,
            started_at: Instant::now(),
        };
        assert_eq!(entry.format_coordinate(), "151E,34S");
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
    fn test_tracker_tile_started_creates_region() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);

        assert_eq!(tracker.active_count(), 1);
        let snap = tracker.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tiles_total, 1);
        assert_eq!(snap[0].tiles_completed, 0);
        assert_eq!(snap[0].progress_percent(), 0);
    }

    #[test]
    fn test_tracker_same_region_coalesces() {
        let tracker = TileProgressTracker::new();
        let (t1, t2) = tiles_in_same_region();

        tracker.tile_started(t1);
        tracker.tile_started(t2);

        // Should be ONE region with 2 tiles
        assert_eq!(tracker.active_count(), 1);
        let snap = tracker.snapshot();
        assert_eq!(snap[0].tiles_total, 2);
        assert_eq!(snap[0].tiles_completed, 0);
    }

    #[test]
    fn test_tracker_duplicate_tile_ignored() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.tile_started(tile); // duplicate

        let snap = tracker.snapshot();
        assert_eq!(snap[0].tiles_total, 1);
    }

    #[test]
    fn test_tracker_tile_completes_after_two_tasks() {
        let tracker = TileProgressTracker::new();
        let (t1, t2) = tiles_in_same_region();

        tracker.tile_started(t1);
        tracker.tile_started(t2);

        // Complete first tile (2 tasks)
        tracker.task_completed(t1);
        tracker.task_completed(t1);

        let snap = tracker.snapshot();
        assert_eq!(snap[0].tiles_total, 2);
        assert_eq!(snap[0].tiles_completed, 1);
        assert_eq!(snap[0].progress_percent(), 50);
    }

    #[test]
    fn test_tracker_region_auto_removed_when_complete() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.task_completed(tile); // task 1/2
        tracker.task_completed(tile); // task 2/2 — tile done, region complete

        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_region_stays_until_all_tiles_complete() {
        let tracker = TileProgressTracker::new();
        let (t1, t2) = tiles_in_same_region();

        tracker.tile_started(t1);
        tracker.tile_started(t2);

        // Complete t1 fully
        tracker.task_completed(t1);
        tracker.task_completed(t1);

        // Region still active (t2 pending)
        assert_eq!(tracker.active_count(), 1);
        let snap = tracker.snapshot();
        assert_eq!(snap[0].tiles_completed, 1);
        assert_eq!(snap[0].tiles_total, 2);

        // Complete t2
        tracker.task_completed(t2);
        tracker.task_completed(t2);

        // Region removed
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_tile_failed() {
        let tracker = TileProgressTracker::new();
        let (t1, t2) = tiles_in_same_region();

        tracker.tile_started(t1);
        tracker.tile_started(t2);

        tracker.tile_failed(t1);

        let snap = tracker.snapshot();
        assert_eq!(snap[0].tiles_total, 1); // reduced from 2
    }

    #[test]
    fn test_tracker_tile_failed_removes_empty_region() {
        let tracker = TileProgressTracker::new();
        let tile = test_tile(100, 200);

        tracker.tile_started(tile);
        tracker.tile_failed(tile);

        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_snapshot_capped_at_max() {
        let tracker = TileProgressTracker::new();

        // Create tiles in distinct DSF regions
        for i in 0..(MAX_DISPLAY_REGIONS + 3) {
            // Use zoom 1 so each tile covers a large area → different DSF regions
            let tile = TileCoord {
                row: i as u32,
                col: 0,
                zoom: 1,
            };
            tracker.tile_started(tile);
        }

        let snap = tracker.snapshot();
        assert!(snap.len() <= MAX_DISPLAY_REGIONS);
    }

    #[test]
    fn test_tracker_clear() {
        let tracker = TileProgressTracker::new();

        tracker.tile_started(test_tile(1, 0));
        tracker.tile_started(test_tile(2, 0));
        tracker.clear();

        assert_eq!(tracker.active_count(), 0);
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
    }

    #[test]
    fn test_parse_dds_job_id_invalid_format() {
        assert!(TileProgressSink::parse_dds_job_id("dds-100_200").is_none());
        assert!(TileProgressSink::parse_dds_job_id("dds-100").is_none());
        assert!(TileProgressSink::parse_dds_job_id("dds-abc_200_ZL16").is_none());
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
    }

    #[test]
    fn test_sink_task_completed() {
        use crate::executor::JobId;
        use crate::executor::TaskResultKind;
        use std::time::Duration;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("dds-100_200_ZL16"),
        });

        sink.emit(TelemetryEvent::TaskCompleted {
            job_id: JobId::new("dds-100_200_ZL16"),
            task_name: "DownloadChunks".to_string(),
            result: TaskResultKind::Success,
            duration: Duration::from_millis(100),
        });

        // Still active (only 1/2 tasks done for this tile)
        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn test_sink_job_completed_removes_on_failure() {
        use crate::executor::{JobId, JobStatus};
        use std::time::Duration;

        let tracker = TileProgressTracker::new();
        let sink = TileProgressSink::new(Arc::clone(&tracker));

        sink.emit(TelemetryEvent::JobStarted {
            job_id: JobId::new("dds-100_200_ZL16"),
        });

        sink.emit(TelemetryEvent::JobCompleted {
            job_id: JobId::new("dds-100_200_ZL16"),
            status: JobStatus::Failed,
            duration: Duration::from_millis(100),
            tasks_succeeded: 0,
            tasks_failed: 1,
            children_succeeded: 0,
            children_failed: 0,
        });

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

        assert_eq!(tracker.active_count(), 0);
    }
}
