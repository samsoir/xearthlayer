//! Scenery window model for the adaptive prefetch system.
//!
//! The `SceneryWindow` derives X-Plane's scenery loading window dimensions
//! from observed FUSE requests (via `SceneTracker`), manages a state machine
//! for window derivation, and provides boundary crossing predictions.
//!
//! # State Machine
//!
//! ```text
//! Uninitialized ──→ Measuring ──→ Ready
//!       │                           ↑
//!       └──→ Assumed ───────────────┘
//! ```
//!
//! - **Uninitialized**: No data yet, waiting for first `SceneTracker` bounds.
//! - **Assumed**: Using default dimensions for ocean/sparse starts where
//!   X-Plane may not request enough tiles to derive real dimensions.
//! - **Measuring**: First real bounds observed from `SceneTracker`, waiting
//!   for two consecutive stable checks before committing.
//! - **Ready**: Window dimensions derived and stable, ready for prefetch use.

use tracing::debug;

use super::boundary_monitor::{BoundaryAxis, BoundaryCrossing, BoundaryMonitor};
use crate::geo_index::{DsfRegion, RetainedRegion};
use crate::scene_tracker::{GeoBounds, SceneTracker};

/// Configuration for the SceneryWindow.
#[derive(Debug, Clone)]
pub struct SceneryWindowConfig {
    /// Default window rows (latitude) for assumed state.
    pub default_rows: usize,
    /// Default window columns (longitude) for assumed state.
    pub default_cols: usize,
    /// Buffer in DSF tiles around the window for retention.
    pub buffer: u8,
    /// Trigger distance for boundary monitors (degrees).
    pub trigger_distance: f64,
    /// Number of DSF tiles deep to load per boundary crossing.
    pub load_depth: u8,
}

impl Default for SceneryWindowConfig {
    fn default() -> Self {
        Self {
            default_rows: 9,
            default_cols: 9,
            buffer: 1,
            trigger_distance: 3.0,
            load_depth: 3,
        }
    }
}

/// State machine for the scenery window derivation.
#[derive(Debug, PartialEq)]
pub enum WindowState {
    /// No data yet -- waiting for first SceneTracker bounds.
    Uninitialized,
    /// Using assumed (default) dimensions -- no real data.
    Assumed,
    /// First bounds observed, waiting for stability.
    Measuring {
        /// Last observed row count (latitude span in degrees).
        last_rows: usize,
        /// Last observed column count (longitude span in degrees).
        last_cols: usize,
        /// Number of consecutive stable checks so far.
        stable_checks: u8,
    },
    /// Window dimensions derived and stable.
    Ready,
}

/// Central model that derives X-Plane's scenery loading window.
///
/// Observes the `SceneTracker` to determine window dimensions, then
/// provides boundary crossing predictions via dual `BoundaryMonitor`s
/// (one per axis). Monitors are initialized when the window enters
/// `Ready` state (from real bounds) or lazily on first position check
/// when in `Assumed` state.
pub struct SceneryWindow {
    state: WindowState,
    window_size: Option<(usize, usize)>,
    config: SceneryWindowConfig,
    lat_monitor: Option<BoundaryMonitor>,
    lon_monitor: Option<BoundaryMonitor>,
    last_bounds: Option<(f64, f64, f64, f64)>,
}

impl SceneryWindow {
    /// Create a new `SceneryWindow` in `Uninitialized` state.
    pub fn new(config: SceneryWindowConfig) -> Self {
        Self {
            state: WindowState::Uninitialized,
            window_size: None,
            config,
            lat_monitor: None,
            lon_monitor: None,
            last_bounds: None,
        }
    }

    /// Set assumed window dimensions for ocean/sparse starts.
    ///
    /// Transitions to `Assumed` state with default dimensions. This allows
    /// prefetch to begin immediately with reasonable defaults while waiting
    /// for real data from the `SceneTracker`.
    pub fn set_assumed_dimensions(&mut self, rows: usize, cols: usize) {
        self.state = WindowState::Assumed;
        self.window_size = Some((rows, cols));
        debug!(rows, cols, "scenery window: assumed dimensions set");
    }

    /// Update the window model from `SceneTracker` observations.
    ///
    /// Drives the state machine:
    /// - `Uninitialized`/`Assumed` + bounds -> `Ready` (using configured window size)
    /// - `Ready` + bounds -> slide monitors to track X-Plane's loaded area
    ///
    /// The tracker bounds tell us *where* X-Plane is loading (center position).
    /// The configured window size (default 9×9) determines *how wide* the
    /// boundary monitors span. This avoids the problem where early tracker
    /// measurements undercount X-Plane's actual window (e.g. 3×4 during
    /// initial load vs 9×9 at steady state).
    pub fn update_from_tracker(&mut self, tracker: &dyn SceneTracker) {
        let bounds = match tracker.loaded_bounds() {
            Some(b) => b,
            None => return, // No data yet
        };

        match &self.state {
            WindowState::Uninitialized | WindowState::Assumed | WindowState::Measuring { .. } => {
                // Use configured window size, centered on tracker's reported center.
                let (rows, cols) = self
                    .window_size
                    .unwrap_or((self.config.default_rows, self.config.default_cols));
                let center_lat = (bounds.min_lat + bounds.max_lat) / 2.0;
                let center_lon = (bounds.min_lon + bounds.max_lon) / 2.0;
                let half_rows = rows as f64 / 2.0;
                let half_cols = cols as f64 / 2.0;
                let expanded = GeoBounds {
                    min_lat: center_lat - half_rows,
                    max_lat: center_lat + half_rows,
                    min_lon: center_lon - half_cols,
                    max_lon: center_lon + half_cols,
                };
                debug!(
                    rows,
                    cols,
                    center_lat = format!("{:.2}", center_lat),
                    center_lon = format!("{:.2}", center_lon),
                    "scenery window: ready with configured dimensions"
                );
                self.state = WindowState::Ready;
                self.window_size = Some((rows, cols));
                self.init_monitors_from_bounds(&expanded);
            }
            WindowState::Ready => {
                // Slide the monitors to track X-Plane's evolving loaded area.
                // The SceneTracker's bounds reflect what X-Plane has actually
                // requested, so we keep the window aligned as the aircraft moves.
                if let Some(ref mut lat_mon) = self.lat_monitor {
                    lat_mon.update_edges(bounds.min_lat, bounds.max_lat);
                }
                if let Some(ref mut lon_mon) = self.lon_monitor {
                    lon_mon.update_edges(bounds.min_lon, bounds.max_lon);
                }
                self.last_bounds = Some((
                    bounds.min_lat,
                    bounds.max_lat,
                    bounds.min_lon,
                    bounds.max_lon,
                ));
            }
        }
    }

    /// Returns the derived window size as `(rows, cols)` in DSF tiles.
    pub fn window_size(&self) -> Option<(usize, usize)> {
        self.window_size
    }

    /// Returns `true` if the window is in `Ready` or `Assumed` state.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, WindowState::Ready | WindowState::Assumed)
    }

    /// Returns the current state.
    pub fn state(&self) -> &WindowState {
        &self.state
    }

    /// Returns the configuration.
    pub fn config(&self) -> &SceneryWindowConfig {
        &self.config
    }

    /// Returns the current window bounds as `(lat_min, lat_max, lon_min, lon_max)`.
    ///
    /// Returns `None` if monitors haven't been initialized yet.
    pub fn window_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        match (&self.lat_monitor, &self.lon_monitor) {
            (Some(lat_mon), Some(lon_mon)) => Some((
                lat_mon.window_min(),
                lat_mon.window_max(),
                lon_mon.window_min(),
                lon_mon.window_max(),
            )),
            _ => None,
        }
    }

    /// Check aircraft position against window boundaries.
    ///
    /// Returns boundary crossing predictions sorted by urgency (most urgent first).
    /// Returns empty if the window is not in `Ready` or `Assumed` state.
    ///
    /// For `Assumed` state, monitors are lazily initialized on the first call
    /// using default dimensions centered on the provided position.
    pub fn check_boundaries(&mut self, lat: f64, lon: f64) -> Vec<BoundaryCrossing> {
        if !self.is_ready() {
            return Vec::new();
        }

        // Lazy initialization for Assumed state (no real bounds available yet).
        if self.lat_monitor.is_none() {
            if let Some((rows, cols)) = self.window_size {
                let half_rows = rows as f64 / 2.0;
                let half_cols = cols as f64 / 2.0;
                self.lat_monitor = Some(
                    BoundaryMonitor::new(
                        BoundaryAxis::Latitude,
                        lat - half_rows,
                        lat + half_rows,
                        self.config.trigger_distance,
                    )
                    .with_load_depth(self.config.load_depth),
                );
                self.lon_monitor = Some(
                    BoundaryMonitor::new(
                        BoundaryAxis::Longitude,
                        lon - half_cols,
                        lon + half_cols,
                        self.config.trigger_distance,
                    )
                    .with_load_depth(self.config.load_depth),
                );
                self.last_bounds = Some((
                    lat - half_rows,
                    lat + half_rows,
                    lon - half_cols,
                    lon + half_cols,
                ));
                debug!(
                    lat,
                    lon, rows, cols, "scenery window: lazy-initialized monitors for assumed state"
                );
            }
        }

        let mut predictions = Vec::new();

        if let Some(ref monitor) = self.lat_monitor {
            predictions.extend(monitor.check(lat));
        }
        if let Some(ref monitor) = self.lon_monitor {
            predictions.extend(monitor.check(lon));
        }

        // Sort by urgency descending (most urgent first).
        predictions.sort_by(|a, b| {
            b.urgency
                .partial_cmp(&a.urgency)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        predictions
    }

    /// Update the retained regions in the GeoIndex based on the current window position.
    ///
    /// Computes the retained area as window + buffer centered on the aircraft,
    /// then adds/removes `RetainedRegion` entries. All changes logged at DEBUG.
    pub fn update_retention(&self, lat: f64, lon: f64, geo_index: &crate::geo_index::GeoIndex) {
        if !self.is_ready() {
            return;
        }

        let (rows, cols) = match self.window_size {
            Some(size) => size,
            None => return,
        };

        let buffer = self.config.buffer as i32;
        let half_rows = (rows as i32) / 2;
        let half_cols = (cols as i32) / 2;

        let center_lat = lat.floor() as i32;
        let center_lon = lon.floor() as i32;

        let min_lat = center_lat - half_rows - buffer;
        let max_lat = center_lat + half_rows + buffer;
        let min_lon = center_lon - half_cols - buffer;
        let max_lon = center_lon + half_cols + buffer;

        // Add all regions in the retained area
        for lat_i in min_lat..=max_lat {
            for lon_i in min_lon..=max_lon {
                let region = DsfRegion::new(lat_i, lon_i);
                if !geo_index.contains::<RetainedRegion>(&region) {
                    debug!(lat = lat_i, lon = lon_i, "retention: adding region");
                    geo_index.insert::<RetainedRegion>(region, RetainedRegion);
                }
            }
        }

        // Evict regions outside the retained area
        let retained_regions = geo_index.regions::<RetainedRegion>();
        for region in retained_regions {
            let r_lat = region.lat;
            let r_lon = region.lon;
            if r_lat < min_lat || r_lat > max_lat || r_lon < min_lon || r_lon > max_lon {
                debug!(lat = r_lat, lon = r_lon, "retention: evicting region");
                geo_index.remove::<RetainedRegion>(&region);
            }
        }
    }

    /// Check if a loading burst represents a world rebuild (teleport/settings change).
    ///
    /// A rebuild is detected when:
    /// 1. The burst covers >50% of the current window area
    /// 2. The burst is roughly centered on the aircraft (within 2° of center)
    ///
    /// On rebuild detection, resets the state machine to `Measuring` and clears
    /// monitors and window dimensions.
    ///
    /// Returns `true` if a rebuild was detected.
    pub fn check_for_rebuild(
        &mut self,
        burst_bounds: &GeoBounds,
        aircraft_lat: f64,
        aircraft_lon: f64,
    ) -> bool {
        let (rows, cols) = match self.window_size {
            Some(size) => size,
            None => return false, // Not ready, can't detect rebuild
        };

        let window_area = (rows * cols) as f64;
        let burst_area = burst_bounds.height() * burst_bounds.width();

        let coverage_ratio = burst_area / window_area;

        // Check if burst is centered on aircraft (within 2° tolerance)
        let burst_center = burst_bounds.center();
        let lat_offset = (burst_center.0 - aircraft_lat).abs();
        let lon_offset = (burst_center.1 - aircraft_lon).abs();
        let is_centered = lat_offset < 2.0 && lon_offset < 2.0;

        if coverage_ratio > 0.5 && is_centered {
            debug!(
                coverage_ratio = format!("{:.1}%", coverage_ratio * 100.0),
                burst_height = burst_bounds.height(),
                burst_width = burst_bounds.width(),
                "scenery window: world rebuild detected, resetting to Measuring"
            );
            self.state = WindowState::Measuring {
                last_rows: burst_bounds.height().round() as usize,
                last_cols: burst_bounds.width().round() as usize,
                stable_checks: 1,
            };
            self.window_size = None;
            self.lat_monitor = None;
            self.lon_monitor = None;
            self.last_bounds = None;
            return true;
        }

        false
    }

    /// Initialize boundary monitors from real geographic bounds.
    fn init_monitors_from_bounds(&mut self, bounds: &GeoBounds) {
        self.lat_monitor = Some(
            BoundaryMonitor::new(
                BoundaryAxis::Latitude,
                bounds.min_lat,
                bounds.max_lat,
                self.config.trigger_distance,
            )
            .with_load_depth(self.config.load_depth),
        );
        self.lon_monitor = Some(
            BoundaryMonitor::new(
                BoundaryAxis::Longitude,
                bounds.min_lon,
                bounds.max_lon,
                self.config.trigger_distance,
            )
            .with_load_depth(self.config.load_depth),
        );
        self.last_bounds = Some((
            bounds.min_lat,
            bounds.max_lat,
            bounds.min_lon,
            bounds.max_lon,
        ));
        debug!(
            min_lat = bounds.min_lat,
            max_lat = bounds.max_lat,
            min_lon = bounds.min_lon,
            max_lon = bounds.max_lon,
            "scenery window: boundary monitors initialized"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::boundary_monitor::BoundaryAxis;
    use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
    use std::collections::HashSet;

    /// Mock SceneTracker that returns controlled bounds.
    struct MockSceneTracker {
        bounds: std::sync::Mutex<Option<GeoBounds>>,
        burst_active: std::sync::atomic::AtomicBool,
    }

    impl MockSceneTracker {
        fn new() -> Self {
            Self {
                bounds: std::sync::Mutex::new(None),
                burst_active: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn set_bounds(&self, bounds: GeoBounds) {
            *self.bounds.lock().unwrap() = Some(bounds);
        }

        #[allow(dead_code)]
        fn clear_bounds(&self) {
            *self.bounds.lock().unwrap() = None;
        }
    }

    impl SceneTracker for MockSceneTracker {
        fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
            HashSet::new()
        }
        fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
            false
        }
        fn is_burst_active(&self) -> bool {
            self.burst_active.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
            vec![]
        }
        fn total_requests(&self) -> u64 {
            0
        }
        fn loaded_regions(&self) -> HashSet<GeoRegion> {
            HashSet::new()
        }
        fn is_region_loaded(&self, _region: &GeoRegion) -> bool {
            false
        }
        fn loaded_bounds(&self) -> Option<GeoBounds> {
            *self.bounds.lock().unwrap()
        }
    }

    fn make_bounds(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> GeoBounds {
        GeoBounds {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        }
    }

    #[test]
    fn test_new_starts_uninitialized() {
        let window = SceneryWindow::new(SceneryWindowConfig::default());
        assert!(matches!(window.state(), WindowState::Uninitialized));
        assert!(window.window_size().is_none());
        assert!(!window.is_ready());
    }

    #[test]
    fn test_set_assumed_dimensions() {
        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.set_assumed_dimensions(6, 8);
        assert!(matches!(window.state(), WindowState::Assumed));
        assert_eq!(window.window_size(), Some((6, 8)));
        assert!(window.is_ready()); // Assumed counts as ready for prefetch
    }

    #[test]
    fn test_transition_to_ready_on_first_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());

        // Immediately ready with config dimensions (9×9), not measured (6×8)
        assert!(matches!(window.state(), WindowState::Ready));
        assert_eq!(window.window_size(), Some((9, 9)));
        assert!(window.is_ready());
    }

    #[test]
    fn test_ready_uses_config_dimensions_not_measured() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        // Tracker reports small bounds (3×4) — early measurement
        tracker.set_bounds(make_bounds(49.0, 52.0, 8.0, 12.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());

        // Should use config defaults (9×9), not the measured 3×4
        assert!(matches!(window.state(), WindowState::Ready));
        assert_eq!(window.window_size(), Some((9, 9)));
    }

    #[test]
    fn test_ready_monitors_centered_on_tracker_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        // Center at (50.0, 7.0)
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());

        // Monitors should span 9° centered on (50.0, 7.0):
        // lat: 50.0 - 4.5 = 45.5 to 50.0 + 4.5 = 54.5
        // lon: 7.0 - 4.5 = 2.5 to 7.0 + 4.5 = 11.5
        let bounds = window.window_bounds();
        assert!(bounds.is_some());
        let (min_lat, max_lat, min_lon, max_lon) = bounds.unwrap();
        assert!((min_lat - 45.5).abs() < 0.01);
        assert!((max_lat - 54.5).abs() < 0.01);
        assert!((min_lon - 2.5).abs() < 0.01);
        assert!((max_lon - 11.5).abs() < 0.01);
    }

    #[test]
    fn test_no_bounds_stays_uninitialized() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        // No bounds set (no tiles requested)

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());

        assert!(matches!(window.state(), WindowState::Uninitialized));
    }

    #[test]
    fn test_assumed_transitions_to_ready_on_real_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.set_assumed_dimensions(9, 9); // -> Assumed
        window.update_from_tracker(tracker.as_ref()); // -> Ready (real data positions monitors)

        assert!(matches!(window.state(), WindowState::Ready));
        // Keeps the assumed window size
        assert_eq!(window.window_size(), Some((9, 9)));
    }

    #[test]
    fn test_default_config() {
        let config = SceneryWindowConfig::default();
        assert_eq!(config.default_rows, 9);
        assert_eq!(config.default_cols, 9);
        assert_eq!(config.buffer, 1);
    }

    #[test]
    fn test_check_boundaries_returns_predictions_when_near_edge() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref()); // → Measuring
        window.update_from_tracker(tracker.as_ref()); // → Ready

        // Aircraft near north edge (52.0 is 1.0° from 53.0 max lat)
        let predictions = window.check_boundaries(52.0, 7.0);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].axis, BoundaryAxis::Latitude);
        assert_eq!(predictions[0].dsf_coord, 53);
    }

    #[test]
    fn test_check_boundaries_returns_empty_when_centered() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        // Use a wider window (10° lat, 14° lon) so center is >3.0° from all edges
        tracker.set_bounds(make_bounds(45.0, 55.0, 0.0, 14.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        // Aircraft in the middle — 5.0° from lat edges, 7.0° from lon edges
        let predictions = window.check_boundaries(50.0, 7.0);
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_check_boundaries_both_axes() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        // Aircraft near both north and east edges
        let predictions = window.check_boundaries(52.0, 10.0);
        assert_eq!(predictions.len(), 2);
        // Should be sorted by urgency descending
        assert!(predictions[0].urgency >= predictions[1].urgency);
    }

    #[test]
    fn test_check_boundaries_sorted_by_urgency_descending() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        // Closer to east (0.5° away) than north (1.0° away)
        let predictions = window.check_boundaries(52.0, 10.5);
        assert!(predictions.len() >= 2);
        // Most urgent first
        for i in 1..predictions.len() {
            assert!(predictions[i - 1].urgency >= predictions[i].urgency);
        }
    }

    #[test]
    fn test_check_boundaries_empty_before_ready() {
        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        // Uninitialized — should return empty
        let predictions = window.check_boundaries(50.0, 7.0);
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_retention_adds_regions_within_window_plus_buffer() {
        use crate::geo_index::{DsfRegion, GeoIndex, RetainedRegion};

        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0)); // 6×8 window

        let mut window = SceneryWindow::new(SceneryWindowConfig::default()); // buffer=1
        window.update_from_tracker(tracker.as_ref()); // → Measuring
        window.update_from_tracker(tracker.as_ref()); // → Ready, size=(6,8)

        let geo_index = GeoIndex::new();
        window.update_retention(50.0, 7.0, &geo_index);

        // Window: 6 rows × 8 cols centered on (50.0, 7.0)
        // Half-window: 3 lat, 4 lon
        // Retained area: lat (50-3-1)..(50+3+1) = 46..54, lon (7-4-1)..(7+4+1) = 2..12
        // That's 8 lat × 10 lon = 80 regions
        let count = geo_index.count::<RetainedRegion>();
        assert!(count > 0, "should have retained regions");

        // Check a region that should be in the retained area
        assert!(geo_index.contains::<RetainedRegion>(&DsfRegion::new(50, 7)));
        // Check edges
        assert!(geo_index.contains::<RetainedRegion>(&DsfRegion::new(46, 2)));
        assert!(geo_index.contains::<RetainedRegion>(&DsfRegion::new(53, 11)));
    }

    #[test]
    fn test_retention_evicts_regions_outside_buffer() {
        use crate::geo_index::{DsfRegion, GeoIndex, RetainedRegion};

        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        let geo_index = GeoIndex::new();

        // Pre-insert a region far from the aircraft
        let far_region = DsfRegion::new(40, 0);
        geo_index.insert::<RetainedRegion>(far_region, RetainedRegion);

        window.update_retention(50.0, 7.0, &geo_index);

        // Far region should be evicted
        assert!(!geo_index.contains::<RetainedRegion>(&far_region));
    }

    #[test]
    fn test_retention_preserves_regions_inside_buffer() {
        use crate::geo_index::{DsfRegion, GeoIndex, RetainedRegion};

        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        let geo_index = GeoIndex::new();

        // Pre-insert a region at the edge of the buffer
        let edge_region = DsfRegion::new(46, 2); // Should be inside retained area
        geo_index.insert::<RetainedRegion>(edge_region, RetainedRegion);

        window.update_retention(50.0, 7.0, &geo_index);

        // Edge region should still be there
        assert!(geo_index.contains::<RetainedRegion>(&edge_region));
    }

    #[test]
    fn test_retention_noop_when_not_ready() {
        use crate::geo_index::{DsfRegion, GeoIndex, RetainedRegion};

        let window = SceneryWindow::new(SceneryWindowConfig::default());
        let geo_index = GeoIndex::new();

        // Pre-insert a region
        geo_index.insert::<RetainedRegion>(DsfRegion::new(50, 7), RetainedRegion);

        window.update_retention(50.0, 7.0, &geo_index);

        // Should not evict anything — window not ready
        assert!(geo_index.contains::<RetainedRegion>(&DsfRegion::new(50, 7)));
    }

    #[test]
    fn test_world_rebuild_detected_when_burst_covers_most_of_window() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref()); // → Ready (9×9 configured)

        assert!(matches!(window.state(), WindowState::Ready));

        // Simulate a burst covering most of the 9×9=81 window (7×7 = 49/81 = 60%)
        let burst_bounds = make_bounds(46.5, 53.5, 3.5, 10.5);
        let is_rebuild = window.check_for_rebuild(&burst_bounds, 50.0, 7.0);

        assert!(is_rebuild);
        assert!(matches!(window.state(), WindowState::Measuring { .. }));
        assert!(window.window_size().is_none()); // Reset
    }

    #[test]
    fn test_normal_extension_not_detected_as_rebuild() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref()); // → Ready (9×9 configured)

        // Simulate a burst adding a single row at the north edge (1×8 = 8 out of 81 = 10%)
        let burst_bounds = make_bounds(53.0, 54.0, 3.0, 11.0);
        let is_rebuild = window.check_for_rebuild(&burst_bounds, 50.0, 7.0);

        assert!(!is_rebuild);
        assert!(matches!(window.state(), WindowState::Ready)); // Still ready
    }

    #[test]
    fn test_rebuild_not_detected_when_not_ready() {
        let window = SceneryWindow::new(SceneryWindowConfig::default());
        let burst_bounds = make_bounds(47.0, 53.0, 3.0, 11.0);
        let mut window = window;
        let is_rebuild = window.check_for_rebuild(&burst_bounds, 50.0, 7.0);
        assert!(!is_rebuild);
    }

    #[test]
    fn test_rebuild_resets_monitors() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref()); // → Ready (9×9 configured)

        // Verify monitors are initialized
        let predictions = window.check_boundaries(52.0, 7.0);
        assert!(!predictions.is_empty());

        // Trigger rebuild — burst must cover >50% of 9×9=81 window
        let burst_bounds = make_bounds(46.5, 53.5, 3.5, 10.5);
        window.check_for_rebuild(&burst_bounds, 50.0, 7.0);

        // After rebuild, monitors should be cleared (state is Measuring)
        let predictions = window.check_boundaries(52.0, 7.0);
        assert!(predictions.is_empty()); // Not ready anymore
    }

    #[test]
    fn test_check_boundaries_works_in_assumed_state() {
        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.set_assumed_dimensions(6, 8);
        // Assumed state should allow boundary checks
        // But we need edges set — assumed state should initialize monitors
        // with default edges centered on... well, we haven't given a position yet.
        // In assumed state, monitors should be initialized when first position is given.
        // For now, check_boundaries on assumed state without prior position → empty
        // This test verifies that assumed state doesn't panic
        let _predictions = window.check_boundaries(50.0, 7.0);
    }

    // =========================================================================
    // Window sliding tests — monitors must update as loaded area evolves
    // =========================================================================

    #[test]
    fn test_ready_state_slides_monitors_on_tracker_update() {
        // Scenario: Window in Ready state at lat 51-54.
        // SceneTracker reports new bounds shifted south (50-53).
        // After update, the south boundary should have moved from 51 to 50.
        let config = SceneryWindowConfig {
            trigger_distance: 0.3,
            load_depth: 3,
            ..SceneryWindowConfig::default()
        };
        let mut window = SceneryWindow::new(config);
        let tracker = std::sync::Arc::new(MockSceneTracker::new());

        // Transition to Ready: set bounds twice for stability
        tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());
        assert!(matches!(window.state(), WindowState::Ready));

        // Aircraft at center of window — no crossings
        let crossings = window.check_boundaries(52.5, 10.0);
        assert!(crossings.is_empty(), "No crossing expected at center");

        // Now SceneTracker reports shifted bounds (aircraft moving south)
        tracker.set_bounds(make_bounds(50.0, 53.0, 8.0, 12.0));
        window.update_from_tracker(tracker.as_ref());

        // Aircraft near new south edge (50.0 + 0.2 = 50.2, within 0.3 trigger)
        let crossings = window.check_boundaries(50.2, 10.0);
        assert!(
            !crossings.is_empty(),
            "Should detect crossing near new south edge after window slide"
        );
        assert!(crossings.iter().any(|c| c.axis == BoundaryAxis::Latitude));
    }

    #[test]
    fn test_window_slides_continuously_through_multiple_updates() {
        // Simulates an aircraft flying south through multiple DSF boundaries.
        // Each tracker update should slide the window, enabling new crossings.
        let config = SceneryWindowConfig {
            trigger_distance: 0.3,
            load_depth: 3,
            ..SceneryWindowConfig::default()
        };
        let mut window = SceneryWindow::new(config);
        let tracker = std::sync::Arc::new(MockSceneTracker::new());

        // Initialize to Ready at lat 51-54, lon 8-12
        tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());
        assert!(matches!(window.state(), WindowState::Ready));

        let mut crossing_count = 0;

        // Simulate flying south: each step the loaded area shifts by ~1°
        for step in 0..5 {
            let south_shift = step as f64;
            let new_min = 51.0 - south_shift;
            let new_max = 54.0 - south_shift;
            tracker.set_bounds(make_bounds(new_min, new_max, 8.0, 12.0));
            window.update_from_tracker(tracker.as_ref());

            // Check near the south edge
            let near_south = new_min + 0.2;
            let crossings = window.check_boundaries(near_south, 10.0);
            if !crossings.is_empty() {
                crossing_count += 1;
            }
        }

        // Should get crossings at multiple steps, not just the first
        assert!(
            crossing_count >= 3,
            "Expected crossings at multiple sliding positions, got {}",
            crossing_count
        );
    }

    #[test]
    fn test_window_bounds_reflect_latest_tracker_update() {
        // After sliding, window_bounds() should return the latest edges.
        let config = SceneryWindowConfig::default();
        let mut window = SceneryWindow::new(config);
        let tracker = std::sync::Arc::new(MockSceneTracker::new());

        // Initialize to Ready
        tracker.set_bounds(make_bounds(51.0, 54.0, 8.0, 12.0));
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        let bounds = window.window_bounds();
        assert_eq!(bounds, Some((51.0, 54.0, 8.0, 12.0)));

        // Slide south
        tracker.set_bounds(make_bounds(49.0, 52.0, 7.0, 11.0));
        window.update_from_tracker(tracker.as_ref());

        let bounds = window.window_bounds();
        assert_eq!(
            bounds,
            Some((49.0, 52.0, 7.0, 11.0)),
            "window_bounds should reflect latest tracker update"
        );
    }
}
