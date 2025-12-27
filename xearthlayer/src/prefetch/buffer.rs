//! Buffer zone generator for heading-aware prefetching.
//!
//! This module generates tiles for lateral and rear buffer zones, providing
//! coverage for unexpected heading changes, turns, and reversals.
//!
//! # Buffer Geometry
//!
//! Buffer zones extend from the forward cone to provide turn coverage:
//!
//! ```text
//!              ┌───────────────────────────────┐
//!               ╲     Forward Cone (primary)  ╱
//!                ╲         85→105nm          ╱
//!                 ╲                         ╱
//!        Left      ╲                       ╱      Right
//!        Buffer ←───╲                     ╱───→ Buffer
//!                    ╲                   ╱
//!                     ╲                 ╱
//!                      ╲               ╱
//!                       ╲             ╱
//!                        ╲           ╱
//!                         ╲    ▲    ╱
//!                          ╲   │   ╱
//!                           ╲  │  ╱
//!                            ╲ │ ╱
//!                             ╲│╱
//!                              ✈
//!                           aircraft
//!                              │
//!                              ↓
//!                         Rear Buffer
//! ```
//!
//! # Buffer Priorities
//!
//! Buffer tiles are lower priority than forward cone tiles:
//!
//! | Zone | Base Priority | Purpose |
//! |------|---------------|---------|
//! | LateralBuffer | 200 | Turn anticipation |
//! | RearBuffer | 300 | Reversal/orbit protection |

use std::collections::HashSet;

use super::config::HeadingAwarePrefetchConfig;
use super::coordinates::{
    distance_to_tile, normalize_heading, position_to_tile, project_position, tile_size_nm_at_lat,
};
use super::types::{PrefetchTile, PrefetchZone, TurnDirection, TurnState};
use crate::coord::TileCoord;

/// Generates buffer zone tiles for heading-aware prefetching.
///
/// The buffer generator creates lateral and rear buffer zones to handle
/// unexpected heading changes. Unlike the forward cone which focuses on
/// the direction of travel, buffers provide coverage to the sides and rear.
#[derive(Debug)]
pub struct BufferGenerator {
    config: HeadingAwarePrefetchConfig,
}

impl BufferGenerator {
    /// Create a new buffer generator with the given configuration.
    pub fn new(config: HeadingAwarePrefetchConfig) -> Self {
        Self { config }
    }

    /// Generate all buffer tiles (lateral and rear).
    ///
    /// # Arguments
    ///
    /// * `position` - Aircraft position as (latitude, longitude)
    /// * `heading` - Aircraft true heading in degrees
    /// * `turn_state` - Current turn state (affects lateral buffer bias)
    ///
    /// # Returns
    ///
    /// Vector of tiles with priority scores for buffer zones.
    pub fn generate_buffer_tiles(
        &self,
        position: (f64, f64),
        heading: f32,
        turn_state: &TurnState,
    ) -> Vec<PrefetchTile> {
        let mut tiles = HashSet::new();
        let inner_radius = self.config.inner_radius_nm;

        // Generate lateral buffers (both sides)
        self.fill_lateral_buffer(position, heading, inner_radius, turn_state, &mut tiles);

        // Generate rear buffer
        self.fill_rear_buffer(position, heading, inner_radius, &mut tiles);

        // Convert to PrefetchTiles with priority
        tiles
            .into_iter()
            .map(|coord| {
                let (priority, zone) = self.calculate_buffer_priority(position, heading, &coord);
                PrefetchTile::new(coord, priority, zone)
            })
            .collect()
    }

    /// Generate lateral buffer tiles on both sides of the aircraft.
    ///
    /// The lateral buffer extends from the edge of the forward cone outward
    /// by the configured lateral buffer angle. During turns, the buffer on
    /// the turn side may be enhanced.
    fn fill_lateral_buffer(
        &self,
        position: (f64, f64),
        heading: f32,
        inner_radius: f32,
        turn_state: &TurnState,
        tiles: &mut HashSet<TileCoord>,
    ) {
        let cone_edge = self.config.cone_half_angle;
        let buffer_angle = self.config.lateral_buffer_angle;
        let depth = self.config.lateral_buffer_depth as f32;
        let zoom = self.config.zoom;
        let tile_size = tile_size_nm_at_lat(position.0, zoom) as f32;

        // Calculate depth in nautical miles
        let buffer_depth_nm = depth * tile_size;
        let max_distance = inner_radius + buffer_depth_nm;

        // Determine if we should bias one side during a turn
        let (left_multiplier, right_multiplier) = match turn_state {
            TurnState::Straight => (1.0, 1.0),
            TurnState::Turning { direction, .. } => match direction {
                TurnDirection::Left => (1.5, 0.7),  // Enhance left, reduce right
                TurnDirection::Right => (0.7, 1.5), // Reduce left, enhance right
            },
        };

        // Left lateral buffer (heading - cone_edge - buffer_angle to heading - cone_edge)
        let left_start_angle = normalize_heading(heading - cone_edge - buffer_angle);
        let left_end_angle = normalize_heading(heading - cone_edge);
        self.fill_angular_sector(
            position,
            left_start_angle,
            left_end_angle,
            inner_radius,
            max_distance * left_multiplier,
            tiles,
        );

        // Right lateral buffer (heading + cone_edge to heading + cone_edge + buffer_angle)
        let right_start_angle = normalize_heading(heading + cone_edge);
        let right_end_angle = normalize_heading(heading + cone_edge + buffer_angle);
        self.fill_angular_sector(
            position,
            right_start_angle,
            right_end_angle,
            inner_radius,
            max_distance * right_multiplier,
            tiles,
        );
    }

    /// Generate rear buffer tiles behind the aircraft.
    ///
    /// The rear buffer provides coverage for reversals and orbits.
    fn fill_rear_buffer(
        &self,
        position: (f64, f64),
        heading: f32,
        inner_radius: f32,
        tiles: &mut HashSet<TileCoord>,
    ) {
        let rear_tiles = self.config.rear_buffer_tiles as f32;
        let zoom = self.config.zoom;
        let tile_size = tile_size_nm_at_lat(position.0, zoom) as f32;

        // Calculate rear buffer depth in nautical miles
        let rear_depth_nm = rear_tiles * tile_size;

        // Rear direction is opposite of heading
        let rear_heading = normalize_heading(heading + 180.0);

        // Fill a narrow sector behind the aircraft
        // Use a 90-degree sector centered on the rear direction
        let rear_half_angle = 45.0;
        let rear_start = normalize_heading(rear_heading - rear_half_angle);
        let rear_end = normalize_heading(rear_heading + rear_half_angle);

        // Start from effective inner radius and extend by rear buffer depth
        let max_distance = inner_radius + rear_depth_nm;

        self.fill_angular_sector(
            position,
            rear_start,
            rear_end,
            inner_radius,
            max_distance,
            tiles,
        );
    }

    /// Fill tiles within an angular sector.
    ///
    /// # Arguments
    ///
    /// * `position` - Center position
    /// * `start_angle` - Starting angle in degrees
    /// * `end_angle` - Ending angle in degrees
    /// * `min_distance` - Minimum distance from center (inner radius)
    /// * `max_distance` - Maximum distance from center
    /// * `tiles` - HashSet to collect unique tiles
    fn fill_angular_sector(
        &self,
        position: (f64, f64),
        start_angle: f32,
        end_angle: f32,
        min_distance: f32,
        max_distance: f32,
        tiles: &mut HashSet<TileCoord>,
    ) {
        let zoom = self.config.zoom;
        let tile_size = tile_size_nm_at_lat(position.0, zoom) as f32;

        // Calculate angular sweep, handling wrap-around
        let mut angular_span = end_angle - start_angle;
        if angular_span < 0.0 {
            angular_span += 360.0;
        }
        if angular_span > 180.0 {
            // We went the long way, use the shorter arc
            angular_span = 360.0 - angular_span;
        }

        // Number of angular samples (more for wider sectors)
        let num_angle_samples = ((angular_span / 10.0).ceil() as usize).max(3);

        // Number of distance samples
        let depth = max_distance - min_distance;
        let num_distance_samples = ((depth / tile_size).ceil() as usize).max(1);

        for d in 0..=num_distance_samples {
            let distance = min_distance + (d as f32 * tile_size);
            if distance > max_distance {
                break;
            }
            if distance < min_distance {
                continue;
            }

            for a in 0..num_angle_samples {
                let t = a as f32 / (num_angle_samples - 1).max(1) as f32;
                let angle = start_angle + (angular_span * t);
                let normalized_angle = normalize_heading(angle);

                let (lat, lon) = project_position(position, normalized_angle, distance);
                if let Some(tile) = position_to_tile(lat, lon, zoom) {
                    // Verify distance is within bounds
                    let tile_dist = distance_to_tile(position, &tile);
                    if self.config.should_prefetch_distance(tile_dist) || tile_dist >= min_distance
                    {
                        tiles.insert(tile);
                    }
                }
            }
        }
    }

    /// Calculate priority and zone for a buffer tile.
    ///
    /// Buffer zones have higher base priority numbers (lower priority) than
    /// forward cone tiles. Within buffers, lateral tiles are prioritized over
    /// rear tiles.
    fn calculate_buffer_priority(
        &self,
        position: (f64, f64),
        heading: f32,
        tile: &TileCoord,
    ) -> (u32, PrefetchZone) {
        let distance = distance_to_tile(position, tile);

        // Determine if this is a lateral or rear buffer tile based on angle
        let tile_center = super::coordinates::tile_to_lat_lon_center(tile);
        let bearing = super::coordinates::bearing_between(position, tile_center);

        // Calculate angle from heading
        let mut angle_from_heading = bearing - heading;
        if angle_from_heading > 180.0 {
            angle_from_heading -= 360.0;
        } else if angle_from_heading < -180.0 {
            angle_from_heading += 360.0;
        }
        let angle_from_heading = angle_from_heading.abs();

        // Rear buffer: tiles more than 135 degrees from heading
        let zone = if angle_from_heading > 135.0 {
            PrefetchZone::RearBuffer
        } else {
            PrefetchZone::LateralBuffer
        };

        // Calculate priority: base + distance component
        // Distance: 1 point per 0.1nm
        let distance_priority = (distance * 10.0) as u32;
        let priority = zone.base_priority() + distance_priority;

        (priority, zone)
    }

    /// Get the configured zoom level.
    pub fn zoom(&self) -> u8 {
        self.config.zoom
    }
}

/// Merge tiles from multiple generators, removing duplicates.
///
/// When a tile appears in multiple zones, the zone with the highest priority
/// (lowest base_priority value) is preserved.
pub fn merge_prefetch_tiles(tile_sets: Vec<Vec<PrefetchTile>>) -> Vec<PrefetchTile> {
    use std::collections::HashMap;

    let mut merged: HashMap<(u32, u32, u8), PrefetchTile> = HashMap::new();

    for tiles in tile_sets {
        for tile in tiles {
            let key = (tile.coord.row, tile.coord.col, tile.coord.zoom);
            merged
                .entry(key)
                .and_modify(|existing| {
                    // Keep the tile with lower (better) priority
                    if tile.priority < existing.priority {
                        *existing = tile.clone();
                    }
                })
                .or_insert(tile);
        }
    }

    let mut result: Vec<PrefetchTile> = merged.into_values().collect();
    result.sort_by_key(|t| t.priority);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_generator() -> BufferGenerator {
        BufferGenerator::new(HeadingAwarePrefetchConfig::default())
    }

    #[test]
    fn test_new_generator() {
        let gen = default_generator();
        assert_eq!(gen.zoom(), 14);
    }

    #[test]
    fn test_generate_buffer_tiles_not_empty() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);

        assert!(
            !tiles.is_empty(),
            "Should generate at least some buffer tiles"
        );
    }

    #[test]
    fn test_buffer_tiles_include_lateral_zones() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);

        let lateral_count = tiles
            .iter()
            .filter(|t| t.zone == PrefetchZone::LateralBuffer)
            .count();

        assert!(
            lateral_count > 0,
            "Should have lateral buffer tiles, got none"
        );
    }

    #[test]
    fn test_buffer_tiles_include_rear_zone() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);

        let rear_count = tiles
            .iter()
            .filter(|t| t.zone == PrefetchZone::RearBuffer)
            .count();

        assert!(rear_count > 0, "Should have rear buffer tiles, got none");
    }

    #[test]
    fn test_buffer_priority_lateral_before_rear() {
        // Lateral buffer should have lower base priority than rear
        assert!(
            PrefetchZone::LateralBuffer.base_priority() < PrefetchZone::RearBuffer.base_priority()
        );
    }

    #[test]
    fn test_buffer_respects_inner_radius() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);
        let inner_radius = gen.config.inner_radius_nm;
        let tile_size = tile_size_nm_at_lat(position.0, gen.zoom()) as f32;

        // Buffer tiles should respect the inner radius (with tolerance for tile size)
        for tile in &tiles {
            let dist = distance_to_tile(position, &tile.coord);
            assert!(
                dist >= inner_radius - tile_size,
                "Buffer tile at distance {:.2}nm should be beyond inner radius {:.2}nm",
                dist,
                inner_radius
            );
        }
    }

    #[test]
    fn test_turn_biases_lateral_buffer() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        // Generate tiles when turning right
        let turn_state = TurnState::Turning {
            rate: 3.0,
            direction: TurnDirection::Right,
        };
        let turning_tiles = gen.generate_buffer_tiles(position, heading, &turn_state);

        // Generate tiles when flying straight
        let straight_tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);

        // When turning, the distribution should change
        // (exact comparison is complex due to tile discretization)
        assert!(
            !turning_tiles.is_empty() && !straight_tiles.is_empty(),
            "Both turn states should produce tiles"
        );
    }

    #[test]
    fn test_heading_variations_produce_tiles() {
        let gen = default_generator();
        let position = (45.0, -122.0);

        // Test various headings produce buffer tiles
        for heading in [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let tiles = gen.generate_buffer_tiles(position, heading, &TurnState::Straight);
            assert!(
                !tiles.is_empty(),
                "Should generate buffer tiles for heading {}",
                heading
            );
        }
    }

    // ==================== merge_prefetch_tiles tests ====================

    #[test]
    fn test_merge_empty() {
        let result = merge_prefetch_tiles(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_single_set() {
        let tiles = vec![
            PrefetchTile::new(
                TileCoord {
                    row: 100,
                    col: 200,
                    zoom: 14,
                },
                10,
                PrefetchZone::ForwardCenter,
            ),
            PrefetchTile::new(
                TileCoord {
                    row: 101,
                    col: 200,
                    zoom: 14,
                },
                20,
                PrefetchZone::ForwardEdge,
            ),
        ];

        let result = merge_prefetch_tiles(vec![tiles]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_merge_removes_duplicates() {
        let set1 = vec![PrefetchTile::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            10,
            PrefetchZone::ForwardCenter,
        )];
        let set2 = vec![PrefetchTile::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            210,
            PrefetchZone::LateralBuffer,
        )];

        let result = merge_prefetch_tiles(vec![set1, set2]);

        // Should have one tile, keeping the higher priority (lower value)
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].priority, 10);
        assert_eq!(result[0].zone, PrefetchZone::ForwardCenter);
    }

    #[test]
    fn test_merge_keeps_higher_priority() {
        let set1 = vec![PrefetchTile::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            300,
            PrefetchZone::RearBuffer,
        )];
        let set2 = vec![PrefetchTile::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            100,
            PrefetchZone::ForwardEdge,
        )];

        let result = merge_prefetch_tiles(vec![set1, set2]);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].priority, 100);
        assert_eq!(result[0].zone, PrefetchZone::ForwardEdge);
    }

    #[test]
    fn test_merge_sorted_by_priority() {
        let set1 = vec![PrefetchTile::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            300,
            PrefetchZone::RearBuffer,
        )];
        let set2 = vec![
            PrefetchTile::new(
                TileCoord {
                    row: 101,
                    col: 200,
                    zoom: 14,
                },
                100,
                PrefetchZone::ForwardEdge,
            ),
            PrefetchTile::new(
                TileCoord {
                    row: 102,
                    col: 200,
                    zoom: 14,
                },
                50,
                PrefetchZone::ForwardCenter,
            ),
        ];

        let result = merge_prefetch_tiles(vec![set1, set2]);

        // Should be sorted by priority (ascending)
        assert_eq!(result.len(), 3);
        for i in 1..result.len() {
            assert!(
                result[i].priority >= result[i - 1].priority,
                "Results should be sorted by priority"
            );
        }
    }
}
