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
use crate::scene_tracker::SceneTracker;

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
            default_rows: 6,
            default_cols: 8,
            buffer: 1,
            trigger_distance: 1.5,
            load_depth: 3,
        }
    }
}

/// State machine for the scenery window derivation.
#[derive(Debug)]
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
    /// - `Uninitialized`/`Assumed` + bounds -> `Measuring`
    /// - `Measuring` + stable bounds -> `Ready`
    /// - `Measuring` + changed bounds -> reset stability counter
    pub fn update_from_tracker(&mut self, tracker: &dyn SceneTracker) {
        let bounds = match tracker.loaded_bounds() {
            Some(b) => b,
            None => return, // No data yet
        };

        let rows = bounds.height().round() as usize;
        let cols = bounds.width().round() as usize;

        match &self.state {
            WindowState::Uninitialized | WindowState::Assumed => {
                debug!(rows, cols, "scenery window: first bounds observed, measuring");
                self.state = WindowState::Measuring {
                    last_rows: rows,
                    last_cols: cols,
                    stable_checks: 1,
                };
            }
            WindowState::Measuring {
                last_rows,
                last_cols,
                stable_checks,
            } => {
                if rows == *last_rows && cols == *last_cols {
                    let new_count = stable_checks + 1;
                    if new_count >= 2 {
                        debug!(rows, cols, "scenery window: bounds stable, ready");
                        self.state = WindowState::Ready;
                        self.window_size = Some((rows, cols));
                        self.init_monitors_from_bounds(&bounds);
                    } else {
                        self.state = WindowState::Measuring {
                            last_rows: rows,
                            last_cols: cols,
                            stable_checks: new_count,
                        };
                    }
                } else {
                    debug!(
                        old_rows = *last_rows,
                        old_cols = *last_cols,
                        new_rows = rows,
                        new_cols = cols,
                        "scenery window: bounds changed, resetting stability"
                    );
                    self.state = WindowState::Measuring {
                        last_rows: rows,
                        last_cols: cols,
                        stable_checks: 1,
                    };
                }
            }
            WindowState::Ready => {
                // Already ready -- no-op for now (rebuild detection in Task 8)
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
                    lon,
                    rows,
                    cols,
                    "scenery window: lazy-initialized monitors for assumed state"
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

    /// Initialize boundary monitors from real geographic bounds.
    fn init_monitors_from_bounds(&mut self, bounds: &crate::scene_tracker::GeoBounds) {
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
            self.burst_active
                .load(std::sync::atomic::Ordering::Relaxed)
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
    fn test_transition_to_measuring_on_first_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());

        assert!(matches!(window.state(), WindowState::Measuring { .. }));
        assert!(window.window_size().is_none()); // Not ready yet
    }

    #[test]
    fn test_transition_to_ready_after_stable_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        // Same bounds (6x8 degrees) for two consecutive checks
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref()); // -> Measuring
        window.update_from_tracker(tracker.as_ref()); // -> Ready (stable)

        assert!(matches!(window.state(), WindowState::Ready));
        assert_eq!(window.window_size(), Some((6, 8))); // 53-47=6 rows, 11-3=8 cols
        assert!(window.is_ready());
    }

    #[test]
    fn test_measuring_resets_on_changed_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());

        // First bounds
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));
        window.update_from_tracker(tracker.as_ref()); // -> Measuring

        // Different bounds (window expanded)
        tracker.set_bounds(make_bounds(46.0, 53.0, 3.0, 12.0));
        window.update_from_tracker(tracker.as_ref()); // -> still Measuring (reset counter)

        // Same as last check
        window.update_from_tracker(tracker.as_ref()); // -> Ready
        assert!(matches!(window.state(), WindowState::Ready));
        assert_eq!(window.window_size(), Some((7, 9))); // 53-46=7, 12-3=9
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
    fn test_assumed_transitions_to_measuring_on_real_bounds() {
        let tracker = std::sync::Arc::new(MockSceneTracker::new());
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.set_assumed_dimensions(6, 8); // -> Assumed
        window.update_from_tracker(tracker.as_ref()); // -> Measuring (real data available)

        assert!(matches!(window.state(), WindowState::Measuring { .. }));
    }

    #[test]
    fn test_default_config() {
        let config = SceneryWindowConfig::default();
        assert_eq!(config.default_rows, 6);
        assert_eq!(config.default_cols, 8);
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
        tracker.set_bounds(make_bounds(47.0, 53.0, 3.0, 11.0));

        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        window.update_from_tracker(tracker.as_ref());
        window.update_from_tracker(tracker.as_ref());

        // Aircraft in the middle — far from all edges
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
}
