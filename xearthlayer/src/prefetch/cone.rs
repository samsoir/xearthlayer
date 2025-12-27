//! Forward cone generator for heading-aware prefetching.
//!
//! This module generates tiles within a forward-facing **annular cone** based on
//! aircraft position and heading. The cone prefetches tiles around X-Plane's
//! 90nm loaded zone boundary—the ring where tiles transition from loaded to unloaded.
//!
//! # Annular Cone Geometry
//!
//! The prefetch zone is a ring around X-Plane's 90nm loaded boundary:
//!
//! ```text
//!       ┌─────────────────────────────────────────────┐
//!        ╲           outer_radius (105nm)            ╱
//!         ╲                                         ╱
//!          ╲         PREFETCH ZONE                 ╱
//!           ╲        (85nm → 105nm)               ╱
//!            ╲                                   ╱
//!             ╲───── inner_radius (85nm) ───────╱  ← half_angle
//!              ╲                               ╱
//!               ╲   X-PLANE'S 90nm ZONE       ╱
//!                ╲  (handled by X-Plane)     ╱
//!                 ╲                         ╱
//!                  ╲                       ╱
//!                   ╲                     ╱
//!                    ╲                   ╱
//!                     ╲                 ╱
//!                      ╲               ╱
//!                       ╲      ▲      ╱
//!                        ╲     │     ╱
//!                         ╲    │    ╱
//!                          ╲   │   ╱
//!                           ╲  │  ╱
//!                            ╲ │ ╱
//!                             ╲│╱
//!                              ✈
//!                           aircraft
//! ```
//!
//! - **Inner radius (85nm):** Just inside X-Plane's 90nm boundary
//! - **Outer radius (105nm):** Extends beyond X-Plane's current loaded zone
//! - **Prefetch zone:** The 20nm ring between inner and outer radii

use std::collections::HashSet;

use super::config::HeadingAwarePrefetchConfig;
use super::coordinates::{
    angle_to_tile, distance_to_tile, normalize_heading, position_to_tile, project_position,
    tiles_along_ray_bounded,
};
use super::types::{PrefetchTile, PrefetchZone, TurnState};
use crate::coord::TileCoord;

/// Generates tiles within a forward cone for heading-aware prefetching.
///
/// The cone generator calculates which tiles fall within the aircraft's
/// forward field of view and assigns priorities based on distance and angle.
#[derive(Debug)]
pub struct ConeGenerator {
    config: HeadingAwarePrefetchConfig,
}

impl ConeGenerator {
    /// Create a new cone generator with the given configuration.
    pub fn new(config: HeadingAwarePrefetchConfig) -> Self {
        Self { config }
    }

    /// Get the inner radius of the prefetch zone.
    pub fn inner_radius_nm(&self) -> f32 {
        self.config.inner_radius_nm
    }

    /// Get the outer radius of the prefetch zone.
    pub fn outer_radius_nm(&self) -> f32 {
        self.config.outer_radius_nm
    }

    /// Generate tiles within the forward annular cone.
    ///
    /// The annular cone targets tiles in the prefetch zone around X-Plane's
    /// ~90nm loaded boundary. Only tiles between `inner_radius_nm` and
    /// `outer_radius_nm` are included.
    ///
    /// # Arguments
    ///
    /// * `position` - Aircraft position as (latitude, longitude)
    /// * `heading` - Aircraft true heading in degrees
    /// * `turn_state` - Current turn state (may widen the cone)
    ///
    /// # Returns
    ///
    /// Vector of tiles with priority scores, sorted by priority (lowest first).
    /// Only includes tiles in the prefetch zone (85-105nm by default).
    pub fn generate_cone_tiles(
        &self,
        position: (f64, f64),
        heading: f32,
        turn_state: &TurnState,
    ) -> Vec<PrefetchTile> {
        let inner_radius = self.config.inner_radius_nm;
        let outer_radius = self.config.outer_radius_nm;

        // Determine effective cone half-angle (may be widened during turns)
        let effective_half_angle = self.effective_half_angle(turn_state);

        // Generate tiles by filling the annular cone area
        let tiles = self.fill_annular_cone(
            position,
            heading,
            inner_radius,
            outer_radius,
            effective_half_angle,
        );

        // Convert to PrefetchTiles with priority, filtering by distance
        let mut prefetch_tiles: Vec<PrefetchTile> = tiles
            .into_iter()
            .filter_map(|coord| {
                let distance = distance_to_tile(position, &coord);
                // Double-check distance is within prefetch zone
                if self.config.should_prefetch_distance(distance) {
                    let (priority, zone) = self.calculate_priority_and_zone(
                        position,
                        heading,
                        effective_half_angle,
                        &coord,
                    );
                    Some(PrefetchTile::new(coord, priority, zone))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (lower = higher priority)
        prefetch_tiles.sort_by_key(|t| t.priority);

        prefetch_tiles
    }

    /// Get the effective cone half-angle, accounting for turn state.
    fn effective_half_angle(&self, turn_state: &TurnState) -> f32 {
        match turn_state {
            TurnState::Straight => self.config.cone_half_angle,
            TurnState::Turning { .. } => {
                self.config.cone_half_angle * self.config.turn_widening_factor
            }
        }
    }

    /// Fill the annular cone with tiles using a ray-based approach.
    ///
    /// Traces rays from the inner radius to the outer lookahead distance,
    /// at regular angular intervals from -half_angle to +half_angle.
    /// Tiles within the inner radius (X-Plane preload zone) are excluded.
    fn fill_annular_cone(
        &self,
        position: (f64, f64),
        heading: f32,
        inner_radius_nm: f32,
        outer_radius_nm: f32,
        half_angle: f32,
    ) -> Vec<TileCoord> {
        let mut tiles = HashSet::new();
        let zoom = self.config.zoom;

        // Number of rays across the cone (more rays = denser fill)
        // Use at least 5 rays, more for wider cones
        let num_rays = (half_angle / 5.0).ceil() as i32 * 2 + 1;
        let num_rays = num_rays.max(5) as usize;

        // Generate rays from -half_angle to +half_angle
        for i in 0..num_rays {
            let t = i as f32 / (num_rays - 1) as f32; // 0.0 to 1.0
            let ray_angle = heading - half_angle + (2.0 * half_angle * t);
            let normalized_angle = normalize_heading(ray_angle);

            // Get tiles along this ray, starting from inner radius
            let ray_tiles = tiles_along_ray_bounded(
                position,
                normalized_angle,
                inner_radius_nm,
                outer_radius_nm,
                zoom,
            );
            for tile in ray_tiles {
                tiles.insert((tile.row, tile.col, tile.zoom));
            }
        }

        // Also fill intermediate areas by sampling at different distances
        // This helps fill any gaps between rays
        let tile_size_nm = super::coordinates::tile_size_nm_at_lat(position.0, zoom) as f32;
        let effective_depth = outer_radius_nm - inner_radius_nm;
        let num_distance_samples = (effective_depth / tile_size_nm).ceil() as usize;

        for d in 0..=num_distance_samples {
            let distance = inner_radius_nm + (d as f32 * tile_size_nm);
            if distance > outer_radius_nm {
                break;
            }

            // Sample across the cone at this distance
            let arc_samples = ((2.0 * half_angle * distance / tile_size_nm) as usize).max(3);
            for a in 0..arc_samples {
                let t = a as f32 / (arc_samples - 1).max(1) as f32;
                let angle = heading - half_angle + (2.0 * half_angle * t);
                let normalized_angle = normalize_heading(angle);

                let (lat, lon) = project_position(position, normalized_angle, distance);
                if let Some(tile) = position_to_tile(lat, lon, zoom) {
                    tiles.insert((tile.row, tile.col, tile.zoom));
                }
            }
        }

        // Convert back to TileCoords
        tiles
            .into_iter()
            .map(|(row, col, zoom)| TileCoord { row, col, zoom })
            .collect()
    }

    /// Calculate priority and zone for a tile.
    ///
    /// Priority is based on:
    /// - Distance from aircraft (closer = higher priority)
    /// - Angle from heading (center = higher priority)
    ///
    /// Zone is determined by angle from heading:
    /// - ForwardCenter: within half the cone angle
    /// - ForwardEdge: beyond half angle but within full cone
    fn calculate_priority_and_zone(
        &self,
        position: (f64, f64),
        heading: f32,
        half_angle: f32,
        tile: &TileCoord,
    ) -> (u32, PrefetchZone) {
        let distance = distance_to_tile(position, tile);
        let angle = angle_to_tile(position, heading, tile).abs();

        // Determine zone based on angle
        let zone = if angle < half_angle / 2.0 {
            PrefetchZone::ForwardCenter
        } else {
            PrefetchZone::ForwardEdge
        };

        // Calculate priority: base + distance component + angle component
        // Distance: 1 point per 0.1nm (tiles further away = higher priority number)
        // Angle: 1 point per 5 degrees off-center
        let distance_priority = (distance * 10.0) as u32;
        let angle_priority = (angle / 5.0) as u32;

        let priority = zone.base_priority() + distance_priority + angle_priority;

        (priority, zone)
    }

    /// Get the configured zoom level.
    pub fn zoom(&self) -> u8 {
        self.config.zoom
    }

    /// Generate tiles for a specific zoom level with custom zone boundaries.
    ///
    /// This is used for multi-zoom prefetching where different zoom levels
    /// have different prefetch zones (e.g., ZL12 tiles at 88-100nm, ZL14 at 85-95nm).
    ///
    /// # Arguments
    ///
    /// * `position` - Aircraft position as (latitude, longitude)
    /// * `heading` - Aircraft true heading in degrees
    /// * `turn_state` - Current turn state (may widen the cone)
    /// * `zoom` - Zoom level to generate tiles for
    /// * `inner_radius_nm` - Inner radius of the prefetch zone
    /// * `outer_radius_nm` - Outer radius of the prefetch zone
    /// * `priority_offset` - Offset to add to priorities (for prioritizing between zoom levels)
    ///
    /// # Returns
    ///
    /// Vector of tiles with priority scores (priority_offset added to all priorities).
    #[allow(clippy::too_many_arguments)]
    pub fn generate_cone_tiles_for_zoom(
        &self,
        position: (f64, f64),
        heading: f32,
        turn_state: &TurnState,
        zoom: u8,
        inner_radius_nm: f32,
        outer_radius_nm: f32,
        priority_offset: u32,
    ) -> Vec<PrefetchTile> {
        // Determine effective cone half-angle (may be widened during turns)
        let effective_half_angle = self.effective_half_angle(turn_state);

        // Generate tiles by filling the annular cone area
        let tiles = self.fill_annular_cone_with_zoom(
            position,
            heading,
            inner_radius_nm,
            outer_radius_nm,
            effective_half_angle,
            zoom,
        );

        // Convert to PrefetchTiles with priority, filtering by distance
        let mut prefetch_tiles: Vec<PrefetchTile> = tiles
            .into_iter()
            .filter_map(|coord| {
                let distance = distance_to_tile(position, &coord);
                // Check distance is within the specified prefetch zone
                if distance >= inner_radius_nm && distance <= outer_radius_nm {
                    let (priority, zone) = self.calculate_priority_and_zone(
                        position,
                        heading,
                        effective_half_angle,
                        &coord,
                    );
                    Some(PrefetchTile::new(coord, priority + priority_offset, zone))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (lower = higher priority)
        prefetch_tiles.sort_by_key(|t| t.priority);

        prefetch_tiles
    }

    /// Fill the annular cone with tiles at a specific zoom level.
    ///
    /// Like `fill_annular_cone` but accepts zoom as a parameter.
    fn fill_annular_cone_with_zoom(
        &self,
        position: (f64, f64),
        heading: f32,
        inner_radius_nm: f32,
        outer_radius_nm: f32,
        half_angle: f32,
        zoom: u8,
    ) -> Vec<TileCoord> {
        let mut tiles = HashSet::new();

        // Number of rays across the cone (more rays = denser fill)
        // Use at least 5 rays, more for wider cones
        let num_rays = (half_angle / 5.0).ceil() as i32 * 2 + 1;
        let num_rays = num_rays.max(5) as usize;

        // Generate rays from -half_angle to +half_angle
        for i in 0..num_rays {
            let t = i as f32 / (num_rays - 1) as f32; // 0.0 to 1.0
            let ray_angle = heading - half_angle + (2.0 * half_angle * t);
            let normalized_angle = normalize_heading(ray_angle);

            // Get tiles along this ray, starting from inner radius
            let ray_tiles = tiles_along_ray_bounded(
                position,
                normalized_angle,
                inner_radius_nm,
                outer_radius_nm,
                zoom,
            );
            for tile in ray_tiles {
                tiles.insert((tile.row, tile.col, tile.zoom));
            }
        }

        // Also fill intermediate areas by sampling at different distances
        // This helps fill any gaps between rays
        let tile_size_nm = super::coordinates::tile_size_nm_at_lat(position.0, zoom) as f32;
        let effective_depth = outer_radius_nm - inner_radius_nm;
        let num_distance_samples = (effective_depth / tile_size_nm).ceil() as usize;

        for d in 0..=num_distance_samples {
            let distance = inner_radius_nm + (d as f32 * tile_size_nm);
            if distance > outer_radius_nm {
                break;
            }

            // Sample across the cone at this distance
            let arc_samples = ((2.0 * half_angle * distance / tile_size_nm) as usize).max(3);
            for a in 0..arc_samples {
                let t = a as f32 / (arc_samples - 1).max(1) as f32;
                let angle = heading - half_angle + (2.0 * half_angle * t);
                let normalized_angle = normalize_heading(angle);

                let (lat, lon) = project_position(position, normalized_angle, distance);
                if let Some(tile) = position_to_tile(lat, lon, zoom) {
                    tiles.insert((tile.row, tile.col, tile.zoom));
                }
            }
        }

        // Convert back to TileCoords
        tiles
            .into_iter()
            .map(|(row, col, zoom)| TileCoord { row, col, zoom })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn default_generator() -> ConeGenerator {
        ConeGenerator::new(HeadingAwarePrefetchConfig::default())
    }

    #[test]
    fn test_new_generator() {
        let gen = default_generator();
        assert_eq!(gen.zoom(), 14);
        // Default prefetch zone: 85nm to 105nm
        assert_eq!(gen.inner_radius_nm(), 85.0);
        assert_eq!(gen.outer_radius_nm(), 105.0);
    }

    #[test]
    fn test_effective_half_angle_straight() {
        let gen = default_generator();
        let angle = gen.effective_half_angle(&TurnState::Straight);
        assert_eq!(angle, gen.config.cone_half_angle);
    }

    #[test]
    fn test_effective_half_angle_turning() {
        let gen = default_generator();
        let turn_state = TurnState::Turning {
            rate: 3.0,
            direction: super::super::types::TurnDirection::Right,
        };
        let angle = gen.effective_half_angle(&turn_state);
        assert!(
            angle > gen.config.cone_half_angle,
            "Cone should widen during turns"
        );
        assert_eq!(
            angle,
            gen.config.cone_half_angle * gen.config.turn_widening_factor
        );
    }

    #[test]
    fn test_generate_cone_tiles_not_empty() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        assert!(!tiles.is_empty(), "Should generate at least some tiles");
    }

    #[test]
    fn test_generate_cone_tiles_sorted_by_priority() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 45.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        // Verify sorted by priority (ascending)
        for i in 1..tiles.len() {
            assert!(
                tiles[i].priority >= tiles[i - 1].priority,
                "Tiles should be sorted by priority"
            );
        }
    }

    #[test]
    fn test_generate_cone_tiles_includes_forward_center() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        // Should have at least some ForwardCenter tiles
        let center_count = tiles
            .iter()
            .filter(|t| t.zone == PrefetchZone::ForwardCenter)
            .count();
        assert!(
            center_count > 0,
            "Should have ForwardCenter tiles, got none"
        );
    }

    #[test]
    fn test_generate_cone_tiles_zone_distribution() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 90.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        let center = tiles
            .iter()
            .filter(|t| t.zone == PrefetchZone::ForwardCenter)
            .count();
        let edge = tiles
            .iter()
            .filter(|t| t.zone == PrefetchZone::ForwardEdge)
            .count();

        // Should have tiles in both zones
        assert!(center > 0, "Should have center tiles");
        assert!(edge > 0, "Should have edge tiles");
    }

    #[test]
    fn test_generate_cone_tiles_heading_variations() {
        let gen = default_generator();
        let position = (45.0, -122.0);

        // Test various headings produce reasonable results
        for heading in [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);
            assert!(
                !tiles.is_empty(),
                "Should generate tiles for heading {}",
                heading
            );
        }
    }

    #[test]
    fn test_priority_closer_is_higher() {
        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        // The first few tiles should have lower distance (closer to inner radius)
        if tiles.len() > 5 {
            let first_5_avg_priority: f32 =
                tiles[..5].iter().map(|t| t.priority as f32).sum::<f32>() / 5.0;
            let last_5_avg_priority: f32 = tiles[tiles.len() - 5..]
                .iter()
                .map(|t| t.priority as f32)
                .sum::<f32>()
                / 5.0;

            assert!(
                first_5_avg_priority < last_5_avg_priority,
                "First tiles should have lower (better) priority than last tiles"
            );
        }
    }

    // ==================== Prefetch zone boundary tests ====================

    #[test]
    fn test_prefetch_zone_excludes_inner_radius() {
        use super::super::coordinates::distance_to_tile;

        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);
        let inner_radius = gen.config.inner_radius_nm;

        // All tiles should be at or beyond the inner radius
        // (with some tolerance for tile size)
        let tile_size_nm =
            super::super::coordinates::tile_size_nm_at_lat(position.0, gen.zoom()) as f32;
        for tile in &tiles {
            let dist = distance_to_tile(position, &tile.coord);
            assert!(
                dist >= inner_radius - tile_size_nm,
                "Tile at distance {:.2}nm should be beyond inner radius {:.2}nm (tolerance: {:.2}nm)",
                dist,
                inner_radius,
                tile_size_nm
            );
        }
    }

    #[test]
    fn test_prefetch_zone_respects_outer_radius() {
        use super::super::coordinates::distance_to_tile;

        let gen = default_generator();
        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);
        let outer_radius = gen.config.outer_radius_nm;
        let tile_size_nm =
            super::super::coordinates::tile_size_nm_at_lat(position.0, gen.zoom()) as f32;

        // All tiles should be within the outer radius
        for tile in &tiles {
            let dist = distance_to_tile(position, &tile.coord);
            assert!(
                dist <= outer_radius + tile_size_nm,
                "Tile at distance {:.2}nm should be within outer radius {:.2}nm",
                dist,
                outer_radius
            );
        }
    }

    #[test]
    fn test_prefetch_zone_with_custom_boundaries() {
        use super::super::coordinates::distance_to_tile;

        // Create generator with custom boundaries (80nm to 100nm)
        let mut config = HeadingAwarePrefetchConfig::default();
        config.inner_radius_nm = 80.0;
        config.outer_radius_nm = 100.0;
        let gen = ConeGenerator::new(config);

        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        assert_eq!(gen.inner_radius_nm(), 80.0);
        assert_eq!(gen.outer_radius_nm(), 100.0);

        let tile_size_nm =
            super::super::coordinates::tile_size_nm_at_lat(position.0, gen.zoom()) as f32;
        for tile in &tiles {
            let dist = distance_to_tile(position, &tile.coord);
            assert!(
                dist >= 80.0 - tile_size_nm && dist <= 100.0 + tile_size_nm,
                "Tile at distance {:.2}nm should be within 80-100nm zone",
                dist
            );
        }
    }

    #[test]
    fn test_prefetch_zone_near_aircraft_with_zero_inner() {
        use super::super::coordinates::distance_to_tile;

        // Create generator with zero inner radius (test edge case)
        let mut config = HeadingAwarePrefetchConfig::default();
        config.inner_radius_nm = 0.0;
        config.outer_radius_nm = 10.0;
        let gen = ConeGenerator::new(config);

        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        // With zero inner radius, should include tiles close to aircraft
        let min_dist = tiles
            .iter()
            .map(|t| distance_to_tile(position, &t.coord))
            .fold(f32::INFINITY, f32::min);

        let tile_size_nm =
            super::super::coordinates::tile_size_nm_at_lat(position.0, gen.zoom()) as f32;
        assert!(
            min_dist < tile_size_nm,
            "Zero inner radius should include tiles close to aircraft, min distance was {:.2}nm",
            min_dist
        );
    }

    #[test]
    fn test_larger_inner_radius_produces_fewer_tiles() {
        // Create two generators with different inner radii
        let mut config_small = HeadingAwarePrefetchConfig::default();
        config_small.inner_radius_nm = 80.0;
        config_small.outer_radius_nm = 110.0;
        let gen_small = ConeGenerator::new(config_small);

        let mut config_large = HeadingAwarePrefetchConfig::default();
        config_large.inner_radius_nm = 95.0;
        config_large.outer_radius_nm = 110.0;
        let gen_large = ConeGenerator::new(config_large);

        let position = (45.0, -122.0);
        let heading = 0.0;

        let tiles_small = gen_small.generate_cone_tiles(position, heading, &TurnState::Straight);
        let tiles_large = gen_large.generate_cone_tiles(position, heading, &TurnState::Straight);

        // Larger inner radius (narrower zone) should produce fewer tiles
        assert!(
            tiles_large.len() < tiles_small.len(),
            "Narrower zone ({} tiles, 95-110nm) should have fewer tiles than wider zone ({} tiles, 80-110nm)",
            tiles_large.len(),
            tiles_small.len()
        );
    }

    // ==================== Multi-zoom tile generation tests ====================

    #[test]
    fn test_generate_cone_tiles_for_zoom_produces_valid_coords() {
        // Test that ZL12 tiles have valid coordinates for zoom 12
        let gen = default_generator(); // Has zoom=14 as default
        let position = (44.5, -114.2); // Near Idaho
        let heading = 45.0;

        // Generate ZL12 tiles (zoom=12, inner 88nm, outer 100nm)
        let tiles = gen.generate_cone_tiles_for_zoom(
            position,
            heading,
            &TurnState::Straight,
            12,    // ZL12
            88.0,  // inner radius
            100.0, // outer radius
            0,     // priority offset
        );

        assert!(!tiles.is_empty(), "Should generate ZL12 tiles");

        // Verify all tiles have valid zoom 12 coordinates
        let max_coord_z12 = 2u32.pow(12) - 1; // 4095
        for tile in &tiles {
            assert_eq!(
                tile.coord.zoom, 12,
                "All tiles should have zoom=12, got zoom={}",
                tile.coord.zoom
            );
            assert!(
                tile.coord.row <= max_coord_z12,
                "Row {} exceeds max {} for zoom 12",
                tile.coord.row,
                max_coord_z12
            );
            assert!(
                tile.coord.col <= max_coord_z12,
                "Col {} exceeds max {} for zoom 12",
                tile.coord.col,
                max_coord_z12
            );
        }

        // Also verify chunk coordinates would be valid
        let max_chunk_coord = 2u32.pow(16) - 1; // 65535 for chunk_zoom=16
        for tile in &tiles {
            let global_row = tile.coord.row * 16;
            let global_col = tile.coord.col * 16;
            assert!(
                global_row <= max_chunk_coord,
                "Chunk row {} exceeds max {} for chunk_zoom 16",
                global_row,
                max_chunk_coord
            );
            assert!(
                global_col <= max_chunk_coord,
                "Chunk col {} exceeds max {} for chunk_zoom 16",
                global_col,
                max_chunk_coord
            );
        }
    }

    #[test]
    fn test_generate_cone_tiles_for_zoom_different_from_default() {
        // Verify that generate_cone_tiles_for_zoom uses the passed zoom,
        // not the config's default zoom
        let gen = default_generator(); // zoom=14 in config
        let position = (45.0, -122.0);
        let heading = 0.0;

        // Generate at zoom 12 (not 14)
        let z12_tiles = gen.generate_cone_tiles_for_zoom(
            position,
            heading,
            &TurnState::Straight,
            12,
            85.0,
            95.0,
            0,
        );

        // Generate at default zoom 14
        let z14_tiles = gen.generate_cone_tiles(position, heading, &TurnState::Straight);

        // All z12 tiles should have zoom=12
        for tile in &z12_tiles {
            assert_eq!(tile.coord.zoom, 12);
        }

        // All z14 tiles should have zoom=14
        for tile in &z14_tiles {
            assert_eq!(tile.coord.zoom, 14);
        }

        // z12 tiles should have lower row/col values than z14 tiles
        // (approximately 4x lower due to zoom difference)
        if !z12_tiles.is_empty() && !z14_tiles.is_empty() {
            let z12_avg_row: f64 =
                z12_tiles.iter().map(|t| t.coord.row as f64).sum::<f64>() / z12_tiles.len() as f64;
            let z14_avg_row: f64 =
                z14_tiles.iter().map(|t| t.coord.row as f64).sum::<f64>() / z14_tiles.len() as f64;

            // ZL14 should have ~4x the row values of ZL12
            let ratio = z14_avg_row / z12_avg_row;
            assert!(
                (ratio - 4.0).abs() < 0.5,
                "Expected row ratio ~4.0, got {:.2}",
                ratio
            );
        }
    }
}
