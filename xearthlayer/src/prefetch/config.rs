//! Configuration for heading-aware prefetch system.
//!
//! This module defines configuration structures for:
//! - Heading-aware cone prefetching parameters
//! - Turn detection thresholds
//! - FUSE inference fallback settings
//!
//! All values have sensible defaults tuned for typical GA and airliner flight profiles.

use std::time::Duration;

// ==================== X-Plane Loaded Zone Exclusion Defaults ====================

/// X-Plane's loaded zone radius in nautical miles.
///
/// Flight testing showed X-Plane maintains a ~90nm radius of loaded tiles
/// around the aircraft. Tiles within this zone are handled by X-Plane's
/// own loading mechanism - prefetching them would be redundant.
pub const XPLANE_LOADED_ZONE_NM: f32 = 90.0;

/// Default margin inside X-Plane's loaded zone boundary in nautical miles.
///
/// Prefetch starts slightly inside the 90nm boundary to ensure tiles are
/// ready before X-Plane reaches the edge. This accounts for timing and
/// network latency.
pub const DEFAULT_INNER_MARGIN_NM: f32 = 5.0;

/// Default buffer beyond X-Plane's loaded zone in nautical miles.
///
/// How far beyond the 90nm boundary to prefetch. Provides lookahead for
/// sustained flight in one direction.
pub const DEFAULT_OUTER_BUFFER_NM: f32 = 15.0;

// ==================== Cone Parameter Defaults ====================

/// Default half-angle of the forward cone in degrees.
///
/// Creates a 60° total cone width (30° to each side of heading).
pub const DEFAULT_CONE_HALF_ANGLE: f32 = 30.0;

/// Default inner radius - where prefetch zone starts.
///
/// This is `XPLANE_LOADED_ZONE_NM - DEFAULT_INNER_MARGIN_NM`.
/// Prefetch begins just inside X-Plane's 90nm boundary.
pub const DEFAULT_INNER_RADIUS_NM: f32 = XPLANE_LOADED_ZONE_NM - DEFAULT_INNER_MARGIN_NM; // 85nm

/// Default outer radius - where prefetch zone ends.
///
/// This is `XPLANE_LOADED_ZONE_NM + DEFAULT_OUTER_BUFFER_NM`.
/// Prefetch extends beyond X-Plane's 90nm boundary.
pub const DEFAULT_OUTER_RADIUS_NM: f32 = XPLANE_LOADED_ZONE_NM + DEFAULT_OUTER_BUFFER_NM; // 105nm

// ==================== Buffer Parameter Defaults ====================

/// Default angle for lateral buffers in degrees from cone edge.
///
/// Provides coverage for unexpected turns.
pub const DEFAULT_LATERAL_BUFFER_ANGLE: f32 = 45.0;

/// Default depth of lateral buffer in tiles.
pub const DEFAULT_LATERAL_BUFFER_DEPTH: u8 = 3;

/// Default number of tiles behind the aircraft to cache.
pub const DEFAULT_REAR_BUFFER_TILES: u8 = 3;

// ==================== Turn Detection Defaults ====================

/// Default turn rate threshold in degrees per second.
///
/// Heading changes faster than this trigger turn mode.
pub const DEFAULT_TURN_RATE_THRESHOLD: f32 = 1.0;

/// Default factor to widen cone during turns.
///
/// Cone half-angle is multiplied by this during active turns.
pub const DEFAULT_TURN_WIDENING_FACTOR: f32 = 1.5;

/// Default time to hold widened cone after turn ends in seconds.
pub const DEFAULT_TURN_HOLD_TIME_SECS: f32 = 10.0;

// ==================== General Defaults ====================

/// Default zoom level for prefetch tiles.
///
/// Matches X-Plane's Z14 tile requests for optimal cache hits.
pub const DEFAULT_PREFETCH_ZOOM: u8 = 14;

/// Default maximum tiles to prefetch per cycle.
///
/// Limits bandwidth usage and pipeline load.
pub const DEFAULT_MAX_TILES_PER_CYCLE: usize = 100;

/// Default interval between prefetch cycles in milliseconds.
pub const DEFAULT_CYCLE_INTERVAL_MS: u64 = 1000;

/// Default TTL for recently-attempted tiles in seconds.
///
/// Prevents hammering tiles that failed to download.
pub const DEFAULT_ATTEMPT_TTL_SECS: u64 = 60;

// ==================== FUSE Inference Defaults ====================

/// Default maximum age of requests to consider for inference in seconds.
pub const DEFAULT_FUSE_MAX_REQUEST_AGE_SECS: u64 = 30;

/// Default minimum requests needed before attempting inference.
pub const DEFAULT_FUSE_MIN_REQUESTS_FOR_INFERENCE: usize = 10;

/// Default confidence threshold for using inferred state.
///
/// Below this threshold, falls back to radial prefetch.
pub const DEFAULT_FUSE_CONFIDENCE_THRESHOLD: f32 = 0.5;

/// Default factor to widen cone when using inferred heading.
///
/// Accounts for uncertainty in the inferred heading.
pub const DEFAULT_FUSE_WIDE_CONE_MULTIPLIER: f32 = 1.5;

/// Default smoothing factor for heading inference.
///
/// Lower values smooth more (0.0-1.0).
pub const DEFAULT_FUSE_HEADING_SMOOTHING: f32 = 0.3;

// ==================== FUSE Inference Fuzzy Margin Defaults ====================

/// Default cone half-angle for FUSE inference mode in degrees.
///
/// Wider than telemetry mode (30°) to account for heading uncertainty.
/// Creates a 90-120° total cone width vs 60° in telemetry mode.
pub const DEFAULT_FUSE_CONE_HALF_ANGLE: f32 = 45.0;

/// Default prefetch depth beyond the frontier in tiles.
///
/// How many tiles beyond X-Plane's loaded frontier to prefetch.
/// Higher than telemetry mode to provide margin for inference uncertainty.
pub const DEFAULT_FUSE_PREFETCH_DEPTH_TILES: u8 = 4;

/// Default multiplier for lateral buffer width in FUSE mode.
///
/// Lateral buffers are widened by this factor compared to telemetry mode
/// to account for potential heading estimation errors.
pub const DEFAULT_FUSE_LATERAL_BUFFER_MULTIPLIER: f32 = 1.75;

/// Default extra cone widening when confidence is low in degrees.
///
/// Added to `cone_half_angle` when inference confidence is below threshold.
/// Provides additional safety margin during uncertain conditions.
pub const DEFAULT_FUSE_LOW_CONFIDENCE_CONE_WIDENING: f32 = 15.0;

/// Default number of frontier snapshots to retain for movement detection.
///
/// Used to detect heading from envelope expansion direction over time.
pub const DEFAULT_FUSE_FRONTIER_HISTORY_SIZE: usize = 10;

/// Configuration for the heading-aware prefetcher.
///
/// Controls the shape and depth of the forward prefetch cone,
/// buffer zones for unexpected maneuvers, and timing parameters.
///
/// Uses an "annular cone" design where the prefetch zone is a ring
/// around X-Plane's 90nm loaded zone—prefetching tiles just before
/// and beyond the boundary that X-Plane maintains.
///
/// ```text
///       ┌─────────────────────────────────────────────┐
///        ╲         outer_radius_nm (105nm)           ╱
///         ╲                                         ╱
///          ╲           PREFETCH ZONE               ╱
///           ╲          (85nm → 105nm)             ╱
///            ╲                                   ╱
///             ╲─────── inner_radius_nm (85nm) ──╱
///              ╲                               ╱
///               ╲    X-PLANE'S 90nm ZONE      ╱
///                ╲   (no prefetch here)      ╱
///                 ╲         ✈               ╱
///                  ╲     aircraft          ╱
///                   ╲                     ╱
///                    ╲                   ╱
///                     ╲                 ╱
///                      ╲_______________╱
/// ```
#[derive(Debug, Clone)]
pub struct HeadingAwarePrefetchConfig {
    // ==================== Prefetch Zone Boundaries ====================
    /// Inner radius in nautical miles - where prefetch zone STARTS.
    ///
    /// Tiles closer than this are within X-Plane's ~90nm loaded zone and
    /// don't need prefetching. Default: 85nm (90nm - 5nm margin).
    pub inner_radius_nm: f32,

    /// Outer radius in nautical miles - where prefetch zone ENDS.
    ///
    /// How far beyond X-Plane's 90nm boundary to prefetch.
    /// Default: 105nm (90nm + 15nm buffer).
    pub outer_radius_nm: f32,

    // ==================== Cone Parameters ====================
    /// Half-angle of the forward cone in degrees.
    ///
    /// The total cone width is 2× this value. A 30° half-angle creates
    /// a 60° forward cone.
    pub cone_half_angle: f32,

    // ==================== Buffer Parameters ====================
    /// Angle for lateral buffers in degrees from cone edge.
    ///
    /// Extends coverage beyond the forward cone to handle unexpected turns.
    pub lateral_buffer_angle: f32,

    /// Depth of lateral buffer in tiles.
    ///
    /// How many tiles deep the lateral buffer extends.
    pub lateral_buffer_depth: u8,

    /// Number of tiles behind the aircraft to keep in cache.
    ///
    /// Provides coverage if the aircraft turns around.
    pub rear_buffer_tiles: u8,

    // ==================== Turn Detection ====================
    /// Turn rate threshold in degrees/second.
    ///
    /// Heading changes faster than this trigger turn mode.
    pub turn_rate_threshold: f32,

    /// Factor to widen cone during turns.
    ///
    /// Cone half-angle is multiplied by this during active turns.
    pub turn_widening_factor: f32,

    /// Time to hold widened cone after turn ends in seconds.
    ///
    /// Provides buffer for establishing new heading after a turn.
    pub turn_hold_time_secs: f32,

    // ==================== General ====================
    /// Zoom level for prefetch tiles.
    ///
    /// Should match X-Plane's tile requests for best cache hit rate.
    pub zoom: u8,

    /// Maximum tiles to prefetch per cycle.
    ///
    /// Limits bandwidth usage and prevents overwhelming the pipeline.
    pub max_tiles_per_cycle: usize,

    /// Interval between prefetch cycles in milliseconds.
    pub cycle_interval_ms: u64,

    /// How long to skip recently-attempted tiles in seconds.
    ///
    /// Prevents hammering tiles that failed to download.
    pub attempt_ttl_secs: u64,

    /// Secondary zoom levels for multi-zoom prefetch.
    ///
    /// When configured, the prefetcher generates tiles at multiple zoom levels.
    /// Each entry defines a separate prefetch zone with its own boundaries.
    /// The primary zoom level (above) is always used; these are additional.
    pub secondary_zoom_levels: Vec<ZoomLevelPrefetchConfig>,
}

/// Configuration for a secondary zoom level in multi-zoom prefetch.
///
/// Allows prefetching at different zoom levels with independent zone boundaries.
/// For example, ZL12 tiles can be prefetched at a different (typically outer)
/// zone compared to ZL14 tiles.
///
/// ```text
///                    outer_radius_nm
///                         │
///     ┌───────────────────▼───────────────────┐
///      ╲     ZL12 PREFETCH ZONE (88-100nm)   ╱
///       ╲                                   ╱
///        ╲─────── inner_radius_nm ─────────╱
///         ╲                               ╱
///          ╲    ZL14 ZONE (85-95nm)      ╱
///           ╲           ✈              ╱
///            ╲                        ╱
///             ╲______________________╱
/// ```
#[derive(Debug, Clone)]
pub struct ZoomLevelPrefetchConfig {
    /// Zoom level for this configuration.
    pub zoom: u8,

    /// Inner radius in nautical miles - where this zoom's prefetch zone starts.
    pub inner_radius_nm: f32,

    /// Outer radius in nautical miles - where this zoom's prefetch zone ends.
    pub outer_radius_nm: f32,

    /// Maximum tiles to prefetch per cycle for this zoom level.
    pub max_tiles_per_cycle: usize,

    /// Priority weight (lower = higher priority).
    ///
    /// Used when merging tiles from multiple zoom levels. Tiles with lower
    /// priority values are submitted first when bandwidth is limited.
    /// Typically ZL14 (close scenery) should have higher priority than ZL12.
    pub priority_weight: u32,
}

impl Default for ZoomLevelPrefetchConfig {
    fn default() -> Self {
        Self {
            zoom: 12,
            inner_radius_nm: 88.0,
            outer_radius_nm: 100.0,
            max_tiles_per_cycle: 25,
            priority_weight: 100, // Lower priority than ZL14's default 0
        }
    }
}

impl ZoomLevelPrefetchConfig {
    /// Create a ZL12 configuration with default values.
    pub fn zl12() -> Self {
        Self::default()
    }

    /// Create a custom zoom level configuration.
    pub fn new(zoom: u8, inner_radius_nm: f32, outer_radius_nm: f32) -> Self {
        Self {
            zoom,
            inner_radius_nm,
            outer_radius_nm,
            ..Self::default()
        }
    }
}

impl Default for HeadingAwarePrefetchConfig {
    fn default() -> Self {
        Self {
            // Prefetch zone boundaries (around X-Plane's 90nm loaded zone)
            inner_radius_nm: DEFAULT_INNER_RADIUS_NM, // 85nm
            outer_radius_nm: DEFAULT_OUTER_RADIUS_NM, // 105nm

            // Cone parameters
            cone_half_angle: DEFAULT_CONE_HALF_ANGLE,

            // Buffer parameters
            lateral_buffer_angle: DEFAULT_LATERAL_BUFFER_ANGLE,
            lateral_buffer_depth: DEFAULT_LATERAL_BUFFER_DEPTH,
            rear_buffer_tiles: DEFAULT_REAR_BUFFER_TILES,

            // Turn detection
            turn_rate_threshold: DEFAULT_TURN_RATE_THRESHOLD,
            turn_widening_factor: DEFAULT_TURN_WIDENING_FACTOR,
            turn_hold_time_secs: DEFAULT_TURN_HOLD_TIME_SECS,

            // General
            zoom: DEFAULT_PREFETCH_ZOOM,
            max_tiles_per_cycle: DEFAULT_MAX_TILES_PER_CYCLE,
            cycle_interval_ms: DEFAULT_CYCLE_INTERVAL_MS,
            attempt_ttl_secs: DEFAULT_ATTEMPT_TTL_SECS,

            // Multi-zoom (empty by default, enable via builder or config)
            secondary_zoom_levels: Vec::new(),
        }
    }
}

impl HeadingAwarePrefetchConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the prefetch zone depth in nautical miles.
    ///
    /// This is the width of the annular prefetch zone.
    pub fn prefetch_zone_depth(&self) -> f32 {
        self.outer_radius_nm - self.inner_radius_nm
    }

    /// Get the attempt TTL as a Duration.
    pub fn attempt_ttl(&self) -> Duration {
        Duration::from_secs(self.attempt_ttl_secs)
    }

    /// Get the cycle interval as a Duration.
    pub fn cycle_interval(&self) -> Duration {
        Duration::from_millis(self.cycle_interval_ms)
    }

    /// Get the turn hold time as a Duration.
    pub fn turn_hold_time(&self) -> Duration {
        Duration::from_secs_f32(self.turn_hold_time_secs)
    }

    // ==================== Prefetch Zone Methods ====================

    /// Check if a tile at the given distance should be prefetched.
    ///
    /// Returns `true` if the tile is within the prefetch zone:
    /// - Beyond `inner_radius_nm` (outside X-Plane's loaded zone)
    /// - Within `outer_radius_nm` (our prefetch boundary)
    ///
    /// # Arguments
    ///
    /// * `distance_nm` - Distance from aircraft to tile center in nautical miles
    pub fn should_prefetch_distance(&self, distance_nm: f32) -> bool {
        distance_nm >= self.inner_radius_nm && distance_nm <= self.outer_radius_nm
    }
}

/// Configuration for FUSE-based inference when telemetry is unavailable.
///
/// When X-Plane's XGPS2 telemetry is disabled or unavailable, the prefetcher
/// can infer aircraft position and heading from the pattern of FUSE tile requests.
///
/// Uses a **dynamic envelope model** that tracks X-Plane's actual tile requests
/// to build a "loaded envelope" and infers movement from frontier expansion.
/// This is less efficient than telemetry mode but more adaptive—acceptable
/// for a graceful degradation fallback.
///
/// ```text
///     ┌─────────────────────────────────────────────────────────┐
///      ╲           PREFETCH ZONE (fuzzy margins)               ╱
///       ╲          prefetch_depth_tiles beyond frontier       ╱
///        ╲                                                   ╱
///         ╲─────── X-PLANE'S LOADED FRONTIER ───────────────╱
///          ╲                                               ╱
///           ╲       LOADED ENVELOPE (tracked tiles)       ╱
///            ╲              ✈ centroid                   ╱
///             ╲                                         ╱
///              ╲       (wider cone_half_angle)         ╱
///               ╲                                     ╱
///                ╲___________________________________╱
/// ```
#[derive(Debug, Clone)]
pub struct FuseInferenceConfig {
    // ==================== Request Tracking ====================
    /// Maximum age of requests to consider for inference in seconds.
    ///
    /// Older requests are pruned from the analysis window.
    /// Default: 30 seconds.
    pub max_request_age_secs: u64,

    /// Minimum requests needed before attempting inference.
    ///
    /// Ensures enough data points for meaningful pattern detection.
    /// Default: 10 requests.
    pub min_requests_for_inference: usize,

    /// Confidence threshold for using inferred state.
    ///
    /// If confidence is below this, falls back to radial prefetch.
    /// Default: 0.5.
    pub confidence_threshold: f32,

    /// Smoothing factor for heading inference (EMA).
    ///
    /// Lower values smooth more (0.0-1.0). Default: 0.3.
    pub heading_smoothing: f32,

    /// Number of frontier snapshots to retain for movement detection.
    ///
    /// Used to detect heading from envelope expansion direction.
    /// Default: 10 snapshots.
    pub frontier_history_size: usize,

    // ==================== Fuzzy Margins ====================
    /// Half-angle of the prefetch cone in degrees.
    ///
    /// Wider than telemetry mode (30°) to account for heading uncertainty.
    /// Creates a 90° total cone width vs 60° in telemetry mode.
    /// Default: 45°.
    pub cone_half_angle: f32,

    /// Prefetch depth beyond the frontier in tiles.
    ///
    /// How many tiles beyond X-Plane's loaded frontier to prefetch.
    /// Default: 4 tiles.
    pub prefetch_depth_tiles: u8,

    /// Multiplier for lateral buffer width.
    ///
    /// Lateral buffers are widened by this factor compared to telemetry mode.
    /// Default: 1.75x.
    pub lateral_buffer_multiplier: f32,

    /// Extra cone widening when confidence is low in degrees.
    ///
    /// Added to `cone_half_angle` when inference confidence is below threshold.
    /// Provides additional safety margin during uncertain conditions.
    /// Default: 15°.
    pub low_confidence_cone_widening: f32,

    // ==================== Deprecated (kept for compatibility) ====================
    /// Factor to widen cone when using inferred heading.
    ///
    /// DEPRECATED: Use `cone_half_angle` instead. This field is kept for
    /// backwards compatibility but is no longer used.
    #[deprecated(note = "Use cone_half_angle instead")]
    pub wide_cone_multiplier: f32,
}

impl Default for FuseInferenceConfig {
    #[allow(deprecated)]
    fn default() -> Self {
        Self {
            // Request tracking
            max_request_age_secs: DEFAULT_FUSE_MAX_REQUEST_AGE_SECS,
            min_requests_for_inference: DEFAULT_FUSE_MIN_REQUESTS_FOR_INFERENCE,
            confidence_threshold: DEFAULT_FUSE_CONFIDENCE_THRESHOLD,
            heading_smoothing: DEFAULT_FUSE_HEADING_SMOOTHING,
            frontier_history_size: DEFAULT_FUSE_FRONTIER_HISTORY_SIZE,

            // Fuzzy margins
            cone_half_angle: DEFAULT_FUSE_CONE_HALF_ANGLE,
            prefetch_depth_tiles: DEFAULT_FUSE_PREFETCH_DEPTH_TILES,
            lateral_buffer_multiplier: DEFAULT_FUSE_LATERAL_BUFFER_MULTIPLIER,
            low_confidence_cone_widening: DEFAULT_FUSE_LOW_CONFIDENCE_CONE_WIDENING,

            // Deprecated
            wide_cone_multiplier: DEFAULT_FUSE_WIDE_CONE_MULTIPLIER,
        }
    }
}

impl FuseInferenceConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the max request age as a Duration.
    pub fn max_request_age(&self) -> Duration {
        Duration::from_secs(self.max_request_age_secs)
    }

    /// Get effective cone half-angle based on confidence.
    ///
    /// When confidence is low, the cone is widened by `low_confidence_cone_widening`
    /// to provide additional safety margin.
    ///
    /// # Arguments
    ///
    /// * `confidence` - Current inference confidence (0.0 to 1.0)
    pub fn effective_cone_half_angle(&self, confidence: f32) -> f32 {
        if confidence < self.confidence_threshold {
            self.cone_half_angle + self.low_confidence_cone_widening
        } else {
            self.cone_half_angle
        }
    }

    /// Get effective lateral buffer depth in tiles.
    ///
    /// Base lateral buffer depth (from heading config) multiplied by
    /// `lateral_buffer_multiplier` to account for inference uncertainty.
    ///
    /// # Arguments
    ///
    /// * `base_lateral_depth` - Base lateral buffer depth from HeadingAwarePrefetchConfig
    pub fn effective_lateral_depth(&self, base_lateral_depth: u8) -> u8 {
        ((base_lateral_depth as f32) * self.lateral_buffer_multiplier).ceil() as u8
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // ==================== HeadingAwarePrefetchConfig Tests ====================

    #[test]
    fn test_heading_config_default() {
        let config = HeadingAwarePrefetchConfig::default();

        assert_eq!(config.cone_half_angle, DEFAULT_CONE_HALF_ANGLE);
        assert_eq!(config.inner_radius_nm, DEFAULT_INNER_RADIUS_NM);
        assert_eq!(config.outer_radius_nm, DEFAULT_OUTER_RADIUS_NM);
        assert_eq!(config.zoom, DEFAULT_PREFETCH_ZOOM);
    }

    #[test]
    fn test_prefetch_zone_based_on_90nm_boundary() {
        let config = HeadingAwarePrefetchConfig::default();

        // Inner radius should be 90nm - 5nm margin = 85nm
        assert_eq!(config.inner_radius_nm, 85.0);

        // Outer radius should be 90nm + 15nm buffer = 105nm
        assert_eq!(config.outer_radius_nm, 105.0);

        // Verify the constants are calculated from 90nm base
        assert_eq!(
            DEFAULT_INNER_RADIUS_NM,
            XPLANE_LOADED_ZONE_NM - DEFAULT_INNER_MARGIN_NM
        );
        assert_eq!(
            DEFAULT_OUTER_RADIUS_NM,
            XPLANE_LOADED_ZONE_NM + DEFAULT_OUTER_BUFFER_NM
        );
    }

    #[test]
    fn test_duration_conversions() {
        let config = HeadingAwarePrefetchConfig::default();

        assert_eq!(
            config.attempt_ttl(),
            Duration::from_secs(DEFAULT_ATTEMPT_TTL_SECS)
        );
        assert_eq!(
            config.cycle_interval(),
            Duration::from_millis(DEFAULT_CYCLE_INTERVAL_MS)
        );
        assert_eq!(
            config.turn_hold_time(),
            Duration::from_secs_f32(DEFAULT_TURN_HOLD_TIME_SECS)
        );
    }

    // ==================== FuseInferenceConfig Tests ====================

    #[test]
    #[allow(deprecated)]
    fn test_fuse_config_default() {
        let config = FuseInferenceConfig::default();

        // Request tracking parameters
        assert_eq!(
            config.max_request_age_secs,
            DEFAULT_FUSE_MAX_REQUEST_AGE_SECS
        );
        assert_eq!(
            config.min_requests_for_inference,
            DEFAULT_FUSE_MIN_REQUESTS_FOR_INFERENCE
        );
        assert_eq!(
            config.confidence_threshold,
            DEFAULT_FUSE_CONFIDENCE_THRESHOLD
        );
        assert_eq!(config.heading_smoothing, DEFAULT_FUSE_HEADING_SMOOTHING);
        assert_eq!(
            config.frontier_history_size,
            DEFAULT_FUSE_FRONTIER_HISTORY_SIZE
        );

        // Fuzzy margin parameters
        assert_eq!(config.cone_half_angle, DEFAULT_FUSE_CONE_HALF_ANGLE);
        assert_eq!(
            config.prefetch_depth_tiles,
            DEFAULT_FUSE_PREFETCH_DEPTH_TILES
        );
        assert_eq!(
            config.lateral_buffer_multiplier,
            DEFAULT_FUSE_LATERAL_BUFFER_MULTIPLIER
        );
        assert_eq!(
            config.low_confidence_cone_widening,
            DEFAULT_FUSE_LOW_CONFIDENCE_CONE_WIDENING
        );

        // Deprecated field (still available for compatibility)
        assert_eq!(
            config.wide_cone_multiplier,
            DEFAULT_FUSE_WIDE_CONE_MULTIPLIER
        );
    }

    #[test]
    fn test_fuse_config_max_age_duration() {
        let config = FuseInferenceConfig::default();
        assert_eq!(
            config.max_request_age(),
            Duration::from_secs(DEFAULT_FUSE_MAX_REQUEST_AGE_SECS)
        );
    }

    #[test]
    fn test_fuse_effective_cone_half_angle_high_confidence() {
        let config = FuseInferenceConfig::default();
        // Confidence above threshold (0.5) should use base cone angle
        let effective = config.effective_cone_half_angle(0.8);
        assert_eq!(effective, DEFAULT_FUSE_CONE_HALF_ANGLE);
    }

    #[test]
    fn test_fuse_effective_cone_half_angle_low_confidence() {
        let config = FuseInferenceConfig::default();
        // Confidence below threshold should widen the cone
        let effective = config.effective_cone_half_angle(0.3);
        assert_eq!(
            effective,
            DEFAULT_FUSE_CONE_HALF_ANGLE + DEFAULT_FUSE_LOW_CONFIDENCE_CONE_WIDENING
        );
        // 45° + 15° = 60°
        assert_eq!(effective, 60.0);
    }

    #[test]
    fn test_fuse_effective_cone_half_angle_at_threshold() {
        let config = FuseInferenceConfig::default();
        // Confidence exactly at threshold (0.5) should still use base cone
        let effective = config.effective_cone_half_angle(0.5);
        assert_eq!(effective, DEFAULT_FUSE_CONE_HALF_ANGLE);
    }

    #[test]
    fn test_fuse_effective_lateral_depth() {
        let config = FuseInferenceConfig::default();
        // Base depth of 3 tiles × 1.75 multiplier = 5.25 → 6 tiles (ceiling)
        let effective = config.effective_lateral_depth(3);
        assert_eq!(effective, 6);

        // Base depth of 4 tiles × 1.75 = 7 tiles
        let effective = config.effective_lateral_depth(4);
        assert_eq!(effective, 7);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_fuse_fuzzy_margin_constants_ordering() {
        // FUSE inference should use wider cone than telemetry mode
        assert!(DEFAULT_FUSE_CONE_HALF_ANGLE > DEFAULT_CONE_HALF_ANGLE);
        // 45° > 30°

        // FUSE inference should have more generous lateral buffers
        assert!(DEFAULT_FUSE_LATERAL_BUFFER_MULTIPLIER > 1.0);
    }

    // ==================== Prefetch Zone Boundary Tests ====================

    #[test]
    fn test_should_prefetch_distance_in_zone() {
        let config = HeadingAwarePrefetchConfig::default();
        // Prefetch zone: 85nm to 105nm

        // Distance 90nm (at X-Plane boundary) should be prefetched
        assert!(config.should_prefetch_distance(90.0));

        // Distance 95nm should be prefetched
        assert!(config.should_prefetch_distance(95.0));

        // Distance at inner boundary (85nm) should be prefetched
        assert!(config.should_prefetch_distance(85.0));

        // Distance at outer boundary (105nm) should be prefetched
        assert!(config.should_prefetch_distance(105.0));
    }

    #[test]
    fn test_should_prefetch_distance_inside_xplane_zone() {
        let config = HeadingAwarePrefetchConfig::default();
        // Tiles within X-Plane's ~85nm inner radius are already loaded

        // Distance 80nm is inside exclusion zone (less than 85nm)
        assert!(!config.should_prefetch_distance(80.0));

        // Distance 50nm is well inside exclusion zone
        assert!(!config.should_prefetch_distance(50.0));

        // Distance 0nm (aircraft position) is inside exclusion zone
        assert!(!config.should_prefetch_distance(0.0));

        // Distance 84.9nm is just inside exclusion zone
        assert!(!config.should_prefetch_distance(84.9));
    }

    #[test]
    fn test_should_prefetch_distance_beyond_outer() {
        let config = HeadingAwarePrefetchConfig::default();
        // Outer boundary: 105nm

        // Distance 106nm is beyond prefetch zone
        assert!(!config.should_prefetch_distance(106.0));

        // Distance 120nm is well beyond prefetch zone
        assert!(!config.should_prefetch_distance(120.0));
    }

    #[test]
    fn test_prefetch_zone_width() {
        let config = HeadingAwarePrefetchConfig::default();

        // Prefetch zone should span 20nm (from 85nm to 105nm)
        let zone_width = config.outer_radius_nm - config.inner_radius_nm;
        assert_eq!(zone_width, 20.0);
    }

    #[test]
    fn test_custom_prefetch_zone() {
        let mut config = HeadingAwarePrefetchConfig::default();

        // Custom zone: 80nm to 110nm
        config.inner_radius_nm = 80.0;
        config.outer_radius_nm = 110.0;

        assert!(config.should_prefetch_distance(80.0));
        assert!(config.should_prefetch_distance(95.0));
        assert!(config.should_prefetch_distance(110.0));
        assert!(!config.should_prefetch_distance(79.9));
        assert!(!config.should_prefetch_distance(110.1));
    }
}
