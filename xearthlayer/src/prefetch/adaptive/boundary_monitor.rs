//! Position-based boundary monitor for the adaptive prefetch system.
//!
//! Each `BoundaryMonitor` watches one axis (latitude or longitude) and
//! produces `BoundaryCrossing` predictions when the aircraft approaches
//! the edge of the inferred X-Plane scenery window. Two monitors (one
//! per axis) mirror X-Plane's own dual boundary monitor architecture.

/// Which axis a boundary crossing occurs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryAxis {
    /// Latitude boundary -- crossing triggers a ROW load.
    Latitude,
    /// Longitude boundary -- crossing triggers a COLUMN load.
    Longitude,
}

/// A predicted boundary crossing with urgency.
#[derive(Debug, Clone)]
pub struct BoundaryCrossing {
    /// Which axis is being crossed.
    pub axis: BoundaryAxis,
    /// The DSF coordinate of the first row/column to load.
    pub dsf_coord: i16,
    /// How urgent: 0.0 = just triggered, 1.0 = imminent.
    pub urgency: f64,
    /// How many DSF tiles deep to load (typically 3).
    pub depth: u8,
    /// Expansion direction for depth: +1 (north/east) or -1 (south/west).
    pub direction: i8,
}

/// Default load depth matching X-Plane's observed 3-deep strip loading.
const DEFAULT_LOAD_DEPTH: u8 = 3;

/// Monitors one axis (latitude or longitude) for boundary crossings.
///
/// Compares the aircraft's position on this axis to the window edges.
/// When the aircraft is within `trigger_distance` of an edge, produces
/// a `BoundaryCrossing` prediction.
#[derive(Debug, Clone)]
pub struct BoundaryMonitor {
    axis: BoundaryAxis,
    window_min: f64,
    window_max: f64,
    trigger_distance: f64,
    load_depth: u8,
}

impl BoundaryMonitor {
    /// Creates a new monitor for the given axis with window edges.
    pub fn new(
        axis: BoundaryAxis,
        window_min: f64,
        window_max: f64,
        trigger_distance: f64,
    ) -> Self {
        Self {
            axis,
            window_min,
            window_max,
            trigger_distance,
            load_depth: DEFAULT_LOAD_DEPTH,
        }
    }

    /// Creates a new monitor with custom load depth.
    pub fn with_load_depth(mut self, depth: u8) -> Self {
        self.load_depth = depth;
        self
    }

    /// Update the window edges (called when window slides or is re-derived).
    pub fn update_edges(&mut self, new_min: f64, new_max: f64) {
        self.window_min = new_min;
        self.window_max = new_max;
    }

    /// Check the aircraft position against window edges.
    ///
    /// Returns boundary crossing predictions for any edge within
    /// `trigger_distance`. May return 0, 1, or 2 predictions
    /// (both edges if the window is small).
    pub fn check(&self, position: f64) -> Vec<BoundaryCrossing> {
        let mut predictions = Vec::new();

        let dist_to_max = self.window_max - position;
        let dist_to_min = position - self.window_min;

        // Check max edge (north or east)
        if dist_to_max <= self.trigger_distance && dist_to_max >= 0.0 {
            let urgency = 1.0 - (dist_to_max / self.trigger_distance);
            let dsf_coord = self.window_max as i16;
            predictions.push(BoundaryCrossing {
                axis: self.axis,
                dsf_coord,
                urgency: urgency.clamp(0.0, 1.0),
                depth: self.load_depth,
                direction: 1,
            });
        }

        // Check min edge (south or west)
        if dist_to_min <= self.trigger_distance && dist_to_min >= 0.0 {
            let urgency = 1.0 - (dist_to_min / self.trigger_distance);
            let dsf_coord = (self.window_min as i16) - 1;
            predictions.push(BoundaryCrossing {
                axis: self.axis,
                dsf_coord,
                urgency: urgency.clamp(0.0, 1.0),
                depth: self.load_depth,
                direction: -1,
            });
        }

        predictions
    }

    /// Current window minimum edge.
    pub fn window_min(&self) -> f64 {
        self.window_min
    }

    /// Current window maximum edge.
    pub fn window_max(&self) -> f64 {
        self.window_max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_trigger_when_far_from_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(50.0); // middle of window
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_trigger_near_max_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(52.0); // 1.0° from north edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].axis, BoundaryAxis::Latitude);
        assert_eq!(predictions[0].dsf_coord, 53); // next row north
        assert!(predictions[0].urgency > 0.0);
    }

    #[test]
    fn test_trigger_near_min_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(48.0); // 1.0° from south edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].dsf_coord, 46); // next row south
    }

    #[test]
    fn test_urgency_increases_closer_to_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let far = monitor.check(51.8);   // 1.2° from edge
        let close = monitor.check(52.7); // 0.3° from edge
        assert!(close[0].urgency > far[0].urgency);
    }

    #[test]
    fn test_no_trigger_both_edges_far() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Longitude, 3.0, 11.0, 1.5);
        let predictions = monitor.check(7.0); // middle
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_longitude_trigger_east_edge() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Longitude, 3.0, 11.0, 1.5);
        let predictions = monitor.check(10.0); // 1.0° from east edge
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].axis, BoundaryAxis::Longitude);
        assert_eq!(predictions[0].dsf_coord, 11);
    }

    #[test]
    fn test_default_depth_is_three() {
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        let predictions = monitor.check(52.0);
        assert_eq!(predictions[0].depth, 3);
    }

    #[test]
    fn test_update_edges() {
        let mut monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 47.0, 53.0, 1.5);
        monitor.update_edges(48.0, 54.0); // window slid north
        // Now 52.8 is 1.2° from north edge (54.0) — should trigger
        let predictions = monitor.check(52.8);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].dsf_coord, 54);
    }

    #[test]
    fn test_trigger_near_both_edges_small_window() {
        // Window only 3° wide — position near both edges
        let monitor = BoundaryMonitor::new(BoundaryAxis::Latitude, 49.0, 52.0, 1.5);
        let predictions = monitor.check(50.5); // 1.5° from both edges
        // Should trigger both directions
        assert_eq!(predictions.len(), 2);
    }
}
