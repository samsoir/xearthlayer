//! Adaptive prefetch strategy trait.
//!
//! Defines the interface for flight-phase-specific prefetch strategies.
//! Each strategy calculates which tiles to prefetch based on aircraft
//! position, track, and system performance.
//!
//! # Strategy Pattern
//!
//! The adaptive prefetch system uses the Strategy pattern to allow
//! different prefetch algorithms for different flight phases:
//!
//! - **GroundStrategy**: Ring around current position for taxi/parking
//! - **BoundaryStrategy**: Boundary-driven DSF region generation for cruise flight
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::adaptive::{
//!     AdaptivePrefetchStrategy, GroundStrategy, PrefetchPlan,
//! };
//!
//! let strategy = GroundStrategy::new(&config);
//! let plan = strategy.calculate_prefetch(
//!     (lat, lon),
//!     track,
//!     &calibration,
//!     &cached_tiles,
//! );
//!
//! for tile in plan.tiles {
//!     dds_client.request_tile(tile, Priority::Prefetch).await;
//! }
//! ```

use std::collections::HashSet;

use crate::coord::TileCoord;

use super::calibration::PerformanceCalibration;
use super::phase_detector::FlightPhase;

/// Strategy for calculating which tiles to prefetch.
///
/// Implementations provide different prefetch algorithms optimized for
/// specific flight phases (ground, cruise, etc.).
pub trait AdaptivePrefetchStrategy: Send + Sync {
    /// Calculate the next batch of tiles to prefetch.
    ///
    /// # Arguments
    ///
    /// * `position` - Current aircraft position (lat, lon) in degrees
    /// * `track` - Ground track in degrees (0-360, true north)
    /// * `calibration` - Performance calibration for time estimation
    /// * `already_cached` - Set of tiles already in cache (to skip)
    ///
    /// # Returns
    ///
    /// A `PrefetchPlan` containing the tiles to prefetch and metadata.
    fn calculate_prefetch(
        &self,
        position: (f64, f64),
        track: f64,
        calibration: &PerformanceCalibration,
        already_cached: &HashSet<TileCoord>,
    ) -> PrefetchPlan;

    /// Human-readable name for logging.
    fn name(&self) -> &'static str;

    /// Whether this strategy should be active for the given flight phase.
    fn is_applicable(&self, phase: &FlightPhase) -> bool;
}

/// Result of a prefetch calculation.
///
/// Contains the tiles to prefetch along with metadata about the plan.
/// The coordinator uses this metadata for logging and UI updates.
#[derive(Debug, Clone)]
pub struct PrefetchPlan {
    /// Tiles to prefetch, ordered by priority (closest first).
    pub tiles: Vec<TileCoord>,

    /// Estimated time to complete prefetch (milliseconds).
    ///
    /// Based on calibration throughput and tile count.
    pub estimated_completion_ms: u64,

    /// Name of the strategy that generated this plan.
    pub strategy: &'static str,

    /// Number of tiles that were skipped (already cached).
    pub skipped_cached: usize,

    /// Total tiles considered before filtering.
    pub total_considered: usize,

    /// Optional metadata about the calculation for logging/UI.
    pub metadata: Option<PrefetchPlanMetadata>,
}

/// Metadata about how a prefetch plan was calculated.
///
/// Used by the coordinator for logging and UI reporting.
#[derive(Debug, Clone)]
pub struct PrefetchPlanMetadata {
    /// Source of bounds data (e.g., "explicit", "cached_tiles", "default").
    pub bounds_source: &'static str,

    /// Number of DSF tiles in the prefetch region.
    pub dsf_tile_count: usize,

    /// Bounding box used for calculation (lat_min, lat_max, lon_min, lon_max).
    pub bounds: Option<(i32, i32, i32, i32)>,

    /// Track quadrant used (for cruise strategy).
    pub track_quadrant: Option<&'static str>,
}

impl PrefetchPlan {
    /// Create an empty plan (no tiles to prefetch).
    pub fn empty(strategy: &'static str) -> Self {
        Self {
            tiles: Vec::new(),
            estimated_completion_ms: 0,
            strategy,
            skipped_cached: 0,
            total_considered: 0,
            metadata: None,
        }
    }

    /// Create a plan with the given tiles.
    pub fn with_tiles(
        tiles: Vec<TileCoord>,
        calibration: &PerformanceCalibration,
        strategy: &'static str,
        skipped_cached: usize,
        total_considered: usize,
    ) -> Self {
        let estimated_completion_ms = calibration.estimate_batch_time_ms(tiles.len());
        Self {
            tiles,
            estimated_completion_ms,
            strategy,
            skipped_cached,
            total_considered,
            metadata: None,
        }
    }

    /// Create a plan with tiles and metadata.
    pub fn with_tiles_and_metadata(
        tiles: Vec<TileCoord>,
        calibration: &PerformanceCalibration,
        strategy: &'static str,
        skipped_cached: usize,
        total_considered: usize,
        metadata: PrefetchPlanMetadata,
    ) -> Self {
        let estimated_completion_ms = calibration.estimate_batch_time_ms(tiles.len());
        Self {
            tiles,
            estimated_completion_ms,
            strategy,
            skipped_cached,
            total_considered,
            metadata: Some(metadata),
        }
    }

    /// Check if the plan has any tiles to prefetch.
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Get the number of tiles to prefetch.
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }
}

impl PrefetchPlanMetadata {
    /// Create metadata for ground strategy.
    pub fn ground(
        bounds_source: &'static str,
        dsf_tile_count: usize,
        bounds: (i32, i32, i32, i32),
    ) -> Self {
        Self {
            bounds_source,
            dsf_tile_count,
            bounds: Some(bounds),
            track_quadrant: None,
        }
    }

    /// Create metadata for cruise strategy.
    pub fn cruise(dsf_tile_count: usize, track_quadrant: &'static str) -> Self {
        Self {
            bounds_source: "track",
            dsf_tile_count,
            bounds: None,
            track_quadrant: Some(track_quadrant),
        }
    }
}

/// Direction quadrant for band calculation.
///
/// X-Plane loads scenery in bands aligned with latitude/longitude.
/// Cardinal directions load a single band, while diagonal directions
/// load BOTH lat and lon bands simultaneously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackQuadrant {
    /// Northbound (337.5° - 22.5°): Load latitude bands ahead
    North,
    /// Eastbound (67.5° - 112.5°): Load longitude bands ahead
    East,
    /// Southbound (157.5° - 202.5°): Load latitude bands ahead
    South,
    /// Westbound (247.5° - 292.5°): Load longitude bands ahead
    West,
    /// Northeast (22.5° - 67.5°): Load BOTH lat and lon bands
    Northeast,
    /// Southeast (112.5° - 157.5°): Load BOTH lat and lon bands
    Southeast,
    /// Southwest (202.5° - 247.5°): Load BOTH lat and lon bands
    Southwest,
    /// Northwest (292.5° - 337.5°): Load BOTH lat and lon bands
    Northwest,
}

impl TrackQuadrant {
    /// Determine the quadrant for a given track angle.
    ///
    /// Track is in degrees, 0-360, where 0 is true north.
    pub fn from_track(track: f64) -> Self {
        // Normalize track to 0-360
        let track = ((track % 360.0) + 360.0) % 360.0;

        match track {
            t if !(22.5..337.5).contains(&t) => TrackQuadrant::North,
            t if t < 67.5 => TrackQuadrant::Northeast,
            t if t < 112.5 => TrackQuadrant::East,
            t if t < 157.5 => TrackQuadrant::Southeast,
            t if t < 202.5 => TrackQuadrant::South,
            t if t < 247.5 => TrackQuadrant::Southwest,
            t if t < 292.5 => TrackQuadrant::West,
            _ => TrackQuadrant::Northwest,
        }
    }

    /// Check if this is a diagonal quadrant (requires both lat and lon bands).
    pub fn is_diagonal(&self) -> bool {
        matches!(
            self,
            TrackQuadrant::Northeast
                | TrackQuadrant::Southeast
                | TrackQuadrant::Southwest
                | TrackQuadrant::Northwest
        )
    }

    /// Check if this quadrant moves in the positive latitude direction.
    pub fn is_northbound(&self) -> bool {
        matches!(
            self,
            TrackQuadrant::North | TrackQuadrant::Northeast | TrackQuadrant::Northwest
        )
    }

    /// Check if this quadrant moves in the positive longitude direction.
    pub fn is_eastbound(&self) -> bool {
        matches!(
            self,
            TrackQuadrant::East | TrackQuadrant::Northeast | TrackQuadrant::Southeast
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ─────────────────────────────────────────────────────────────────────────
    // Property-based tests
    // ─────────────────────────────────────────────────────────────────────────

    proptest! {
        /// Any track angle maps to exactly one valid quadrant
        #[test]
        fn prop_any_track_maps_to_quadrant(track in -1000.0f64..1000.0f64) {
            let quadrant = TrackQuadrant::from_track(track);
            // Just verify it returns a valid quadrant (no panic)
            let _ = quadrant.is_diagonal();
        }

        /// Normalization: track and track+360 map to the same quadrant
        #[test]
        fn prop_track_normalization_360(track in 0.0f64..360.0f64) {
            let q1 = TrackQuadrant::from_track(track);
            let q2 = TrackQuadrant::from_track(track + 360.0);
            let q3 = TrackQuadrant::from_track(track - 360.0);
            let q4 = TrackQuadrant::from_track(track + 720.0);

            prop_assert_eq!(q1, q2, "track {} and track+360 should match", track);
            prop_assert_eq!(q1, q3, "track {} and track-360 should match", track);
            prop_assert_eq!(q1, q4, "track {} and track+720 should match", track);
        }

        /// Cardinal directions (N/E/S/W) are not diagonal
        #[test]
        fn prop_cardinal_not_diagonal(
            track in prop_oneof![
                0.0f64..22.5f64,      // North (partial)
                337.5f64..360.0f64,   // North (partial)
                67.5f64..112.5f64,    // East
                157.5f64..202.5f64,   // South
                247.5f64..292.5f64,   // West
            ]
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(!quadrant.is_diagonal(),
                "Cardinal track {} should not be diagonal, got {:?}", track, quadrant);
        }

        /// Diagonal directions (NE/SE/SW/NW) are diagonal
        #[test]
        fn prop_diagonal_is_diagonal(
            track in prop_oneof![
                22.5f64..67.5f64,     // Northeast
                112.5f64..157.5f64,   // Southeast
                202.5f64..247.5f64,   // Southwest
                292.5f64..337.5f64,   // Northwest
            ]
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(quadrant.is_diagonal(),
                "Diagonal track {} should be diagonal, got {:?}", track, quadrant);
        }

        /// Northbound tracks are in the northern half (NW through NE)
        #[test]
        fn prop_northbound_consistency(
            track in prop_oneof![
                0.0f64..67.5f64,      // N and NE
                292.5f64..360.0f64,   // NW and N
            ]
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(quadrant.is_northbound(),
                "Track {} should be northbound, got {:?}", track, quadrant);
        }

        /// Southbound tracks are in the southern half (SE through SW)
        #[test]
        fn prop_southbound_consistency(
            track in 112.5f64..292.5f64
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(!quadrant.is_northbound(),
                "Track {} should be southbound, got {:?}", track, quadrant);
        }

        /// Eastbound tracks are in the eastern half (NE through SE)
        #[test]
        fn prop_eastbound_consistency(
            track in 22.5f64..157.5f64
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(quadrant.is_eastbound(),
                "Track {} should be eastbound, got {:?}", track, quadrant);
        }

        /// Westbound tracks are in the western half (SW through NW)
        #[test]
        fn prop_westbound_consistency(
            track in 202.5f64..337.5f64
        ) {
            let quadrant = TrackQuadrant::from_track(track);
            prop_assert!(!quadrant.is_eastbound(),
                "Track {} should be westbound, got {:?}", track, quadrant);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Unit tests (specific values and edge cases)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_track_quadrant_cardinal() {
        // North
        assert_eq!(TrackQuadrant::from_track(0.0), TrackQuadrant::North);
        assert_eq!(TrackQuadrant::from_track(359.0), TrackQuadrant::North);
        assert_eq!(TrackQuadrant::from_track(10.0), TrackQuadrant::North);

        // East
        assert_eq!(TrackQuadrant::from_track(90.0), TrackQuadrant::East);
        assert_eq!(TrackQuadrant::from_track(80.0), TrackQuadrant::East);
        assert_eq!(TrackQuadrant::from_track(100.0), TrackQuadrant::East);

        // South
        assert_eq!(TrackQuadrant::from_track(180.0), TrackQuadrant::South);
        assert_eq!(TrackQuadrant::from_track(170.0), TrackQuadrant::South);
        assert_eq!(TrackQuadrant::from_track(190.0), TrackQuadrant::South);

        // West
        assert_eq!(TrackQuadrant::from_track(270.0), TrackQuadrant::West);
        assert_eq!(TrackQuadrant::from_track(260.0), TrackQuadrant::West);
        assert_eq!(TrackQuadrant::from_track(280.0), TrackQuadrant::West);
    }

    #[test]
    fn test_track_quadrant_diagonal() {
        // Northeast
        assert_eq!(TrackQuadrant::from_track(45.0), TrackQuadrant::Northeast);
        assert_eq!(TrackQuadrant::from_track(30.0), TrackQuadrant::Northeast);
        assert_eq!(TrackQuadrant::from_track(60.0), TrackQuadrant::Northeast);

        // Southeast
        assert_eq!(TrackQuadrant::from_track(135.0), TrackQuadrant::Southeast);
        assert_eq!(TrackQuadrant::from_track(120.0), TrackQuadrant::Southeast);
        assert_eq!(TrackQuadrant::from_track(150.0), TrackQuadrant::Southeast);

        // Southwest
        assert_eq!(TrackQuadrant::from_track(225.0), TrackQuadrant::Southwest);
        assert_eq!(TrackQuadrant::from_track(210.0), TrackQuadrant::Southwest);
        assert_eq!(TrackQuadrant::from_track(240.0), TrackQuadrant::Southwest);

        // Northwest
        assert_eq!(TrackQuadrant::from_track(315.0), TrackQuadrant::Northwest);
        assert_eq!(TrackQuadrant::from_track(300.0), TrackQuadrant::Northwest);
        assert_eq!(TrackQuadrant::from_track(330.0), TrackQuadrant::Northwest);
    }

    #[test]
    fn test_track_quadrant_is_diagonal() {
        assert!(!TrackQuadrant::North.is_diagonal());
        assert!(!TrackQuadrant::East.is_diagonal());
        assert!(!TrackQuadrant::South.is_diagonal());
        assert!(!TrackQuadrant::West.is_diagonal());

        assert!(TrackQuadrant::Northeast.is_diagonal());
        assert!(TrackQuadrant::Southeast.is_diagonal());
        assert!(TrackQuadrant::Southwest.is_diagonal());
        assert!(TrackQuadrant::Northwest.is_diagonal());
    }

    #[test]
    fn test_track_quadrant_directions() {
        // Northbound
        assert!(TrackQuadrant::North.is_northbound());
        assert!(TrackQuadrant::Northeast.is_northbound());
        assert!(TrackQuadrant::Northwest.is_northbound());
        assert!(!TrackQuadrant::South.is_northbound());
        assert!(!TrackQuadrant::Southeast.is_northbound());

        // Eastbound
        assert!(TrackQuadrant::East.is_eastbound());
        assert!(TrackQuadrant::Northeast.is_eastbound());
        assert!(TrackQuadrant::Southeast.is_eastbound());
        assert!(!TrackQuadrant::West.is_eastbound());
        assert!(!TrackQuadrant::Northwest.is_eastbound());
    }

    #[test]
    fn test_prefetch_plan_empty() {
        let plan = PrefetchPlan::empty("test");
        assert!(plan.is_empty());
        assert_eq!(plan.tile_count(), 0);
        assert_eq!(plan.strategy, "test");
    }

    #[test]
    fn test_prefetch_plan_with_tiles() {
        use std::time::Instant;

        let calibration = PerformanceCalibration {
            throughput_tiles_per_sec: 20.0,
            avg_tile_generation_ms: 50,
            tile_generation_stddev_ms: 10,
            confidence: 0.9,
            recommended_strategy: super::super::calibration::StrategyMode::Opportunistic,
            calibrated_at: Instant::now(),
            baseline_throughput: 20.0,
            sample_count: 100,
        };

        let tiles = vec![
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            TileCoord {
                row: 101,
                col: 200,
                zoom: 14,
            },
        ];

        let plan = PrefetchPlan::with_tiles(tiles, &calibration, "cruise", 5, 7);

        assert!(!plan.is_empty());
        assert_eq!(plan.tile_count(), 2);
        assert_eq!(plan.skipped_cached, 5);
        assert_eq!(plan.total_considered, 7);
        assert!(plan.estimated_completion_ms > 0);
    }

    #[test]
    fn test_track_normalization() {
        // Negative track
        assert_eq!(TrackQuadrant::from_track(-90.0), TrackQuadrant::West);
        assert_eq!(TrackQuadrant::from_track(-180.0), TrackQuadrant::South);

        // Track > 360
        assert_eq!(TrackQuadrant::from_track(450.0), TrackQuadrant::East);
        assert_eq!(TrackQuadrant::from_track(720.0), TrackQuadrant::North);
    }
}
