//! Types for the heading-aware prefetch system.
//!
//! This module defines types specific to heading-aware prefetching,
//! including turn detection, priority-based tile selection, prefetch zones,
//! and input mode selection for graceful degradation.
//!
//! Types from other modules are re-exported for convenience:
//! - [`TileCoord`] from `crate::coord` - tile coordinates
//! - [`AircraftState`] from `super::state` - aircraft position and velocity

// Re-export common types used across prefetch modules
pub use super::state::AircraftState;
pub use crate::coord::TileCoord;

/// Input mode for the heading-aware prefetcher.
///
/// The prefetcher automatically selects the best mode based on data availability,
/// degrading gracefully when higher-quality inputs become unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// UDP telemetry available - use precise ConeGenerator.
    ///
    /// This is the highest-quality mode, using X-Plane's UDP broadcast
    /// for exact position, heading, and ground speed.
    Telemetry,

    /// FUSE inference active - use dynamic envelope with fuzzy margins.
    ///
    /// When telemetry is stale (>5s), falls back to inferring position
    /// and heading from FUSE file access patterns.
    FuseInference,

    /// No heading data - fall back to simple radial.
    ///
    /// When no heading can be determined, uses a simple 7×7 grid
    /// around the inferred or last-known position.
    RadialFallback,
}

impl std::fmt::Display for InputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputMode::Telemetry => write!(f, "telemetry"),
            InputMode::FuseInference => write!(f, "fuse-inference"),
            InputMode::RadialFallback => write!(f, "radial-fallback"),
        }
    }
}

/// Turn state for heading-aware prefetching.
///
/// Tracks whether the aircraft is flying straight or turning,
/// which affects cone geometry and buffer sizing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum TurnState {
    /// Aircraft is flying straight (heading rate below threshold).
    #[default]
    Straight,
    /// Aircraft is turning at the given rate.
    Turning {
        /// Turn rate in degrees per second (positive = right, negative = left).
        rate: f32,
        /// Direction of turn.
        direction: TurnDirection,
    },
}

/// Direction of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDirection {
    /// Turning left (heading decreasing).
    Left,
    /// Turning right (heading increasing).
    Right,
}

/// A tile to be prefetched with its priority and zone classification.
///
/// Priority is used to order prefetch requests when the number of tiles
/// to prefetch exceeds the per-cycle limit.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefetchTile {
    /// The tile coordinates.
    pub coord: TileCoord,
    /// Priority score (lower = higher priority, fetched first).
    pub priority: u32,
    /// Which zone this tile belongs to.
    pub zone: PrefetchZone,
}

impl PrefetchTile {
    /// Create a new prefetch tile.
    pub fn new(coord: TileCoord, priority: u32, zone: PrefetchZone) -> Self {
        Self {
            coord,
            priority,
            zone,
        }
    }
}

/// Classification of tile location relative to aircraft heading.
///
/// Zones are used for priority calculation and to ensure balanced coverage.
/// Tiles closer to the flight path get higher priority (lower priority number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrefetchZone {
    /// Directly ahead in the forward cone (highest priority).
    ForwardCenter,
    /// At the edges of the forward cone.
    ForwardEdge,
    /// Lateral buffer zones for unexpected turns.
    LateralBuffer,
    /// Behind the aircraft (lowest priority backup).
    RearBuffer,
}

impl PrefetchZone {
    /// Base priority for this zone (lower = higher priority).
    ///
    /// This is added to distance-based priority to determine final order.
    pub fn base_priority(&self) -> u32 {
        match self {
            PrefetchZone::ForwardCenter => 0,
            PrefetchZone::ForwardEdge => 100,
            PrefetchZone::LateralBuffer => 200,
            PrefetchZone::RearBuffer => 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turn_state_default() {
        let state = TurnState::default();
        assert_eq!(state, TurnState::Straight);
    }

    #[test]
    fn test_turn_state_turning() {
        let state = TurnState::Turning {
            rate: 3.0,
            direction: TurnDirection::Right,
        };
        match state {
            TurnState::Turning { rate, direction } => {
                assert_eq!(rate, 3.0);
                assert_eq!(direction, TurnDirection::Right);
            }
            _ => panic!("Expected Turning state"),
        }
    }

    #[test]
    fn test_prefetch_tile_new() {
        let coord = TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        };
        let tile = PrefetchTile::new(coord, 50, PrefetchZone::ForwardCenter);

        assert_eq!(tile.coord.row, 100);
        assert_eq!(tile.coord.col, 200);
        assert_eq!(tile.priority, 50);
        assert_eq!(tile.zone, PrefetchZone::ForwardCenter);
    }

    #[test]
    fn test_zone_base_priority_ordering() {
        // Verify priority ordering: ForwardCenter < ForwardEdge < LateralBuffer < RearBuffer
        assert!(
            PrefetchZone::ForwardCenter.base_priority() < PrefetchZone::ForwardEdge.base_priority()
        );
        assert!(
            PrefetchZone::ForwardEdge.base_priority() < PrefetchZone::LateralBuffer.base_priority()
        );
        assert!(
            PrefetchZone::LateralBuffer.base_priority() < PrefetchZone::RearBuffer.base_priority()
        );
    }

    #[test]
    fn test_input_mode_display() {
        assert_eq!(InputMode::Telemetry.to_string(), "telemetry");
        assert_eq!(InputMode::FuseInference.to_string(), "fuse-inference");
        assert_eq!(InputMode::RadialFallback.to_string(), "radial-fallback");
    }

    #[test]
    fn test_input_mode_equality() {
        assert_eq!(InputMode::Telemetry, InputMode::Telemetry);
        assert_ne!(InputMode::Telemetry, InputMode::FuseInference);
        assert_ne!(InputMode::FuseInference, InputMode::RadialFallback);
    }
}
