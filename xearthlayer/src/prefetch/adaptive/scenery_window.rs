//! Scenery window configuration for the adaptive prefetch system.
//!
//! The `SceneryWindow` holds configuration for X-Plane's scenery loading
//! window dimensions, used by the prefetch box and retention tracking.

/// Configuration for the SceneryWindow.
#[derive(Debug, Clone)]
pub struct SceneryWindowConfig {
    /// Default window rows (latitude) for assumed state.
    pub default_rows: usize,
    /// Longitude extent in degrees for dynamic column computation.
    /// Columns computed as `ceil(lon_extent / cos(latitude))`.
    pub lon_extent: f64,
    /// Buffer in DSF tiles around the window for retention.
    pub buffer: u8,
}

impl SceneryWindowConfig {
    /// Compute window columns for a given latitude using the cosine formula.
    pub fn compute_cols(&self, latitude: f64) -> usize {
        crate::coord::lon_tiles_for_latitude(latitude, self.lon_extent)
    }
}

impl Default for SceneryWindowConfig {
    fn default() -> Self {
        Self {
            default_rows: 3,
            lon_extent: 3.0,
            buffer: 1,
        }
    }
}

/// Window dimensions model for X-Plane's scenery loading area.
///
/// Provides assumed dimensions based on configuration, used by the
/// prefetch box for retention tracking and region sizing.
pub struct SceneryWindow {
    window_size: Option<(usize, usize)>,
    config: SceneryWindowConfig,
}

impl SceneryWindow {
    /// Create a new `SceneryWindow`.
    pub fn new(config: SceneryWindowConfig) -> Self {
        Self {
            window_size: None,
            config,
        }
    }

    /// Set assumed window dimensions.
    ///
    /// Uses default rows and latitude-dependent columns. This allows
    /// prefetch to begin immediately with reasonable defaults.
    pub fn set_assumed_dimensions(&mut self, rows: usize, latitude: f64) {
        let cols = self.config.compute_cols(latitude);
        self.window_size = Some((rows, cols));
        tracing::debug!(
            rows,
            cols,
            latitude,
            "scenery window: assumed dimensions set"
        );
    }

    /// Returns the configuration.
    pub fn config(&self) -> &SceneryWindowConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_assumed_dimensions() {
        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        // At equator (0deg), lon_extent=3.0 -> cols = ceil(3.0/cos(0)) = 3
        window.set_assumed_dimensions(6, 0.0);
        assert_eq!(window.window_size, Some((6, 3)));
    }

    #[test]
    fn test_set_assumed_dimensions_high_latitude() {
        let mut window = SceneryWindow::new(SceneryWindowConfig::default());
        // At 60N: cols = ceil(3.0/cos(60)) = ceil(6.0) = 6
        window.set_assumed_dimensions(3, 60.0);
        assert_eq!(window.window_size, Some((3, 6)));
    }

    #[test]
    fn test_default_config() {
        let config = SceneryWindowConfig::default();
        assert_eq!(config.default_rows, 3);
        assert!((config.lon_extent - 3.0).abs() < f64::EPSILON);
        assert_eq!(config.buffer, 1);
    }

    #[test]
    fn test_compute_cols_equator() {
        let config = SceneryWindowConfig::default();
        assert_eq!(config.compute_cols(0.0), 3);
    }

    #[test]
    fn test_compute_cols_high_latitude() {
        let config = SceneryWindowConfig::default();
        // At 50N: ceil(3.0/cos(50)) = ceil(4.67) = 5
        assert_eq!(config.compute_cols(50.0), 5);
    }
}
