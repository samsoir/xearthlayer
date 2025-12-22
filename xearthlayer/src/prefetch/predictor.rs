//! Tile prediction engine based on aircraft state.
//!
//! Calculates which tiles should be pre-fetched based on:
//! - Flight path cone: Tiles along the aircraft's projected path (default 100nm)
//! - Radial buffer: Tiles around the current position (default 60nm)
//!
//! The prediction areas are large to match X-Plane's scenery loading behavior,
//! which pre-loads ~50k tiles on startup covering a large area.

use std::collections::HashSet;

use crate::coord::{to_tile_coords, TileCoord};

use super::state::AircraftState;

/// Distance intervals for flight path prediction (nautical miles ahead).
/// Spread across the cone distance to get good coverage.
const PREDICTION_DISTANCES_NM: [f32; 8] = [5.0, 10.0, 20.0, 35.0, 50.0, 65.0, 80.0, 100.0];

/// Approximate nautical miles per degree at the equator.
const NM_PER_DEGREE: f64 = 60.0;

/// A predicted tile with its priority.
#[derive(Debug, Clone)]
pub struct PredictedTile {
    /// The tile coordinates.
    pub tile: TileCoord,
    /// Priority (lower is higher priority, 0 = highest).
    pub priority: u32,
}

/// Tile prediction engine.
///
/// Uses aircraft state to predict which tiles should be pre-fetched
/// to reduce loading delays during flight.
///
/// Default configuration predicts tiles in:
/// - A 60nm radius around the current position (radial buffer)
/// - A 100nm cone ahead of the aircraft (flight path)
///
/// At zoom level 14 and mid-latitudes, this covers approximately:
/// - Radial: ~13,000 tiles
/// - Cone: ~11,000 tiles
/// - Combined (with overlap): ~18,000-22,000 unique tiles
pub struct TilePredictor {
    /// Cone half-angle in degrees (e.g., 45 means ±45° from heading).
    cone_angle: f32,
    /// Maximum lookahead distance in nautical miles.
    cone_distance_nm: f32,
    /// Radius of radial buffer in nautical miles.
    radial_radius_nm: f32,
}

impl TilePredictor {
    /// Create a new tile predictor with the given settings.
    ///
    /// # Arguments
    ///
    /// * `cone_angle` - Half-angle of the prediction cone in degrees
    /// * `cone_distance_nm` - Maximum lookahead distance in nautical miles
    /// * `radial_radius_nm` - Radius of radial buffer in nautical miles
    pub fn new(cone_angle: f32, cone_distance_nm: f32, radial_radius_nm: f32) -> Self {
        Self {
            cone_angle,
            cone_distance_nm,
            radial_radius_nm,
        }
    }

    /// Create a predictor with default settings optimized for X-Plane.
    ///
    /// - 45° cone half-angle
    /// - 100nm cone distance
    /// - 60nm radial radius
    pub fn default_xplane() -> Self {
        Self::new(45.0, 100.0, 60.0)
    }

    /// Predict tiles to pre-fetch based on current aircraft state.
    ///
    /// Returns tiles sorted by priority (lowest first = highest priority).
    ///
    /// # Arguments
    ///
    /// * `state` - Current aircraft position and motion
    /// * `zoom` - Zoom level for tile predictions
    ///
    /// # Returns
    ///
    /// Vector of predicted tiles with priorities. Priority order:
    /// 1. Cone tiles by distance-to-reach (closest first)
    /// 2. Radial tiles by distance from current position
    pub fn predict(&self, state: &AircraftState, zoom: u8) -> Vec<PredictedTile> {
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        let mut predictions = Vec::new();

        // Get current tile for radial calculations
        let current_tile = match to_tile_coords(state.latitude, state.longitude, zoom) {
            Ok(tile) => tile,
            Err(_) => return predictions, // Invalid position, return empty
        };

        // Add current tile first (highest priority)
        seen.insert((current_tile.row, current_tile.col));
        predictions.push(PredictedTile {
            tile: current_tile,
            priority: 0,
        });

        // Phase 1: Flight path cone predictions (distance-based)
        let mut priority = 1;
        for &distance_nm in &PREDICTION_DISTANCES_NM {
            if distance_nm > self.cone_distance_nm {
                break;
            }

            // Get tiles along the cone at this distance
            let cone_tiles = self.predict_cone_at_distance(state, zoom, distance_nm, &current_tile);

            for tile in cone_tiles {
                let key = (tile.row, tile.col);
                if !seen.contains(&key) {
                    seen.insert(key);
                    predictions.push(PredictedTile { tile, priority });
                }
            }
            priority += 1;
        }

        // Phase 2: Radial buffer around current position
        let radial_priority_base = priority;
        let radial_tiles = self.predict_radial(state.latitude, zoom, &current_tile);

        for (tile, distance) in radial_tiles {
            let key = (tile.row, tile.col);
            if !seen.contains(&key) {
                seen.insert(key);
                // Priority based on distance from center (grouped into bands)
                predictions.push(PredictedTile {
                    tile,
                    priority: radial_priority_base + distance / 10,
                });
            }
        }

        // Sort by priority (stable sort to preserve order within same priority)
        predictions.sort_by_key(|p| p.priority);

        predictions
    }

    /// Predict tiles within the flight path cone at a given distance ahead.
    fn predict_cone_at_distance(
        &self,
        state: &AircraftState,
        zoom: u8,
        distance_nm: f32,
        _current_tile: &TileCoord,
    ) -> Vec<TileCoord> {
        let mut tiles = Vec::new();

        // Convert distance to degrees (adjusted for latitude)
        let lat_rad = state.latitude.to_radians();
        let distance_deg = (distance_nm as f64) / NM_PER_DEGREE;

        // Get the position at this distance along the heading
        let heading_rad = (state.heading as f64).to_radians();
        let delta_lat = distance_deg * heading_rad.cos();
        let delta_lon = distance_deg * heading_rad.sin() / lat_rad.cos().max(0.01);

        let center_lat = (state.latitude + delta_lat).clamp(-85.05112878, 85.05112878);
        let mut center_lon = state.longitude + delta_lon;
        if center_lon > 180.0 {
            center_lon -= 360.0;
        } else if center_lon < -180.0 {
            center_lon += 360.0;
        }

        // Convert to tile
        let center_tile = match to_tile_coords(center_lat, center_lon, zoom) {
            Ok(tile) => tile,
            Err(_) => return tiles,
        };

        // Always add center tile (even if same as current - dedup happens in caller)
        tiles.push(center_tile);

        // Calculate cone spread at this distance
        let spread_tiles =
            self.calculate_cone_spread_at_distance(state.latitude, zoom, distance_nm);

        if spread_tiles > 0 {
            // Calculate perpendicular direction (heading ± 90°)
            let perp_heading_left = heading_rad - std::f64::consts::FRAC_PI_2;
            let perp_heading_right = heading_rad + std::f64::consts::FRAC_PI_2;

            // Add tiles on both sides of the center line
            let tile_size_nm = self.tile_size_nm(state.latitude, zoom);
            for offset in 1..=spread_tiles {
                let offset_nm = (offset as f64) * tile_size_nm;
                let offset_deg = offset_nm / NM_PER_DEGREE;

                // Left side of cone
                let (lat_l, lon_l) =
                    self.offset_position(center_lat, center_lon, perp_heading_left, offset_deg);
                if let Ok(tile) = to_tile_coords(lat_l, lon_l, zoom) {
                    tiles.push(tile);
                }

                // Right side of cone
                let (lat_r, lon_r) =
                    self.offset_position(center_lat, center_lon, perp_heading_right, offset_deg);
                if let Ok(tile) = to_tile_coords(lat_r, lon_r, zoom) {
                    tiles.push(tile);
                }
            }
        }

        tiles
    }

    /// Calculate how many tiles wide the cone is at a given distance ahead.
    fn calculate_cone_spread_at_distance(&self, latitude: f64, zoom: u8, distance_nm: f32) -> u32 {
        // Width of cone at this distance (tan of half-angle)
        let cone_width_nm = (distance_nm as f64) * (self.cone_angle as f64).to_radians().tan();

        // Convert to tiles
        let tile_size_nm = self.tile_size_nm(latitude, zoom);
        (cone_width_nm / tile_size_nm).ceil() as u32
    }

    /// Get approximate tile size in degrees at a given zoom level.
    fn tile_size_degrees(&self, zoom: u8) -> f64 {
        360.0 / (2.0_f64.powi(zoom as i32))
    }

    /// Get approximate tile size in nautical miles at a given zoom level and latitude.
    fn tile_size_nm(&self, latitude: f64, zoom: u8) -> f64 {
        let tile_size_deg = self.tile_size_degrees(zoom);
        // Convert degrees to nautical miles, adjusted for latitude
        tile_size_deg * NM_PER_DEGREE * latitude.to_radians().cos().abs().max(0.1)
    }

    /// Offset a position by a given distance in a given direction.
    fn offset_position(
        &self,
        lat: f64,
        lon: f64,
        heading_rad: f64,
        distance_deg: f64,
    ) -> (f64, f64) {
        let delta_lat = distance_deg * heading_rad.cos();
        let delta_lon = distance_deg * heading_rad.sin() / lat.to_radians().cos().max(0.01);

        let new_lat = (lat + delta_lat).clamp(-85.05112878, 85.05112878);
        let mut new_lon = lon + delta_lon;

        // Normalize longitude
        if new_lon > 180.0 {
            new_lon -= 360.0;
        } else if new_lon < -180.0 {
            new_lon += 360.0;
        }

        (new_lat, new_lon)
    }

    /// Predict tiles in the radial buffer around the current position.
    ///
    /// The buffer uses nautical miles and is converted to tiles based on
    /// the zoom level and latitude.
    ///
    /// Returns tiles with their distance from center (in tiles).
    fn predict_radial(&self, latitude: f64, zoom: u8, center: &TileCoord) -> Vec<(TileCoord, u32)> {
        let mut tiles = Vec::new();

        // Convert nautical miles to tiles
        let tile_size_nm = self.tile_size_nm(latitude, zoom);
        let radius_tiles = (self.radial_radius_nm as f64 / tile_size_nm).ceil() as i32;

        // Cap at a reasonable maximum to prevent excessive computation
        let radius = radius_tiles.min(200);

        for dr in -radius..=radius {
            for dc in -radius..=radius {
                if dr == 0 && dc == 0 {
                    continue; // Skip center (already added)
                }

                // Check if within circular radius
                let distance_sq = (dr * dr + dc * dc) as f64;
                let distance = distance_sq.sqrt();

                if distance <= radius as f64 {
                    let new_row = center.row as i64 + dr as i64;
                    let new_col = center.col as i64 + dc as i64;

                    // Validate tile coordinates
                    let max_coord = 2i64.pow(center.zoom as u32);
                    if new_row >= 0 && new_row < max_coord && new_col >= 0 && new_col < max_coord {
                        tiles.push((
                            TileCoord {
                                row: new_row as u32,
                                col: new_col as u32,
                                zoom: center.zoom,
                            },
                            distance.ceil() as u32,
                        ));
                    }
                }
            }
        }

        // Sort by distance
        tiles.sort_by_key(|(_, dist)| *dist);
        tiles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_state(lat: f64, lon: f64, heading: f32, ground_speed: f32) -> AircraftState {
        AircraftState::new(lat, lon, heading, ground_speed, 10000.0)
    }

    #[test]
    fn test_predict_stationary_aircraft() {
        // 45° cone, 100nm ahead, 5nm radial (small for faster test)
        let predictor = TilePredictor::new(45.0, 100.0, 5.0);
        let state = create_state(45.0, -122.0, 90.0, 0.0); // Not moving

        let predictions = predictor.predict(&state, 14);

        // Should have radial tiles
        assert!(!predictions.is_empty());

        // First tile should be current position with priority 0
        assert_eq!(predictions[0].priority, 0);
    }

    #[test]
    fn test_predict_moving_aircraft() {
        let predictor = TilePredictor::new(45.0, 100.0, 5.0);
        let state = create_state(45.0, -122.0, 90.0, 450.0); // 450 knots east

        let predictions = predictor.predict(&state, 14);

        // Should have tiles
        assert!(!predictions.is_empty());

        // Verify priorities are in order
        for window in predictions.windows(2) {
            assert!(window[0].priority <= window[1].priority);
        }
    }

    #[test]
    fn test_predict_includes_current_tile() {
        let predictor = TilePredictor::new(45.0, 100.0, 5.0);
        let state = create_state(45.0, -122.0, 0.0, 200.0);

        let predictions = predictor.predict(&state, 14);

        assert!(!predictions.is_empty());
        assert_eq!(predictions[0].priority, 0);

        // Current tile should be at the aircraft's position
        let current = to_tile_coords(45.0, -122.0, 14).unwrap();
        assert_eq!(predictions[0].tile.row, current.row);
        assert_eq!(predictions[0].tile.col, current.col);
    }

    #[test]
    fn test_no_duplicate_tiles() {
        let predictor = TilePredictor::new(45.0, 50.0, 10.0);
        let state = create_state(45.0, -122.0, 45.0, 300.0);

        let predictions = predictor.predict(&state, 14);

        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for pred in &predictions {
            let key = (pred.tile.row, pred.tile.col);
            assert!(
                seen.insert(key),
                "Duplicate tile at ({}, {})",
                pred.tile.row,
                pred.tile.col
            );
        }
    }

    #[test]
    fn test_radial_produces_tiles() {
        // 5nm radius at zoom 14 and 45° latitude should produce several tiles
        let predictor = TilePredictor::new(45.0, 100.0, 5.0);
        let center = TileCoord {
            row: 5000,
            col: 2000,
            zoom: 14,
        };

        let radial = predictor.predict_radial(45.0, 14, &center);

        // Should have some tiles
        assert!(!radial.is_empty(), "Radial should produce tiles");
    }

    #[test]
    fn test_radial_sorted_by_distance() {
        let predictor = TilePredictor::new(45.0, 100.0, 10.0);
        let center = TileCoord {
            row: 5000,
            col: 2000,
            zoom: 14,
        };

        let radial = predictor.predict_radial(45.0, 14, &center);

        for window in radial.windows(2) {
            assert!(
                window[0].1 <= window[1].1,
                "Radial tiles not sorted by distance"
            );
        }
    }

    #[test]
    fn test_cone_spread_increases_with_distance() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);

        let spread_10nm = predictor.calculate_cone_spread_at_distance(45.0, 14, 10.0);
        let spread_50nm = predictor.calculate_cone_spread_at_distance(45.0, 14, 50.0);
        let spread_100nm = predictor.calculate_cone_spread_at_distance(45.0, 14, 100.0);

        assert!(
            spread_10nm <= spread_50nm,
            "Spread at 10nm ({}) should be <= 50nm ({})",
            spread_10nm,
            spread_50nm
        );
        assert!(
            spread_50nm <= spread_100nm,
            "Spread at 50nm ({}) should be <= 100nm ({})",
            spread_50nm,
            spread_100nm
        );
    }

    #[test]
    fn test_tile_size_decreases_with_zoom() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);

        let size_10 = predictor.tile_size_degrees(10);
        let size_12 = predictor.tile_size_degrees(12);
        let size_15 = predictor.tile_size_degrees(15);

        assert!(size_10 > size_12, "Zoom 10 tiles should be larger");
        assert!(size_12 > size_15, "Zoom 12 tiles should be larger than 15");
    }

    #[test]
    fn test_tile_size_nm_at_latitude() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);

        // At the equator, tiles are larger in nm
        let size_equator = predictor.tile_size_nm(0.0, 14);
        // At 60° latitude, tiles are smaller in nm (due to longitude convergence)
        let size_60 = predictor.tile_size_nm(60.0, 14);

        assert!(
            size_equator > size_60,
            "Tiles at equator ({}) should be larger than at 60° ({})",
            size_equator,
            size_60
        );
    }

    #[test]
    fn test_predict_respects_cone_distance_limit() {
        // Create predictor with 20nm cone distance
        let predictor_short = TilePredictor::new(45.0, 20.0, 5.0);
        let state = create_state(45.0, -122.0, 90.0, 450.0);

        let predictions_short = predictor_short.predict(&state, 14);

        // With a longer cone distance, we should have more predictions
        let predictor_long = TilePredictor::new(45.0, 100.0, 5.0);
        let predictions_long = predictor_long.predict(&state, 14);

        // Short cone should produce fewer or equal predictions
        assert!(
            predictions_short.len() <= predictions_long.len(),
            "Short cone ({}) should not produce more predictions than long ({})",
            predictions_short.len(),
            predictions_long.len()
        );
    }

    #[test]
    fn test_predict_at_map_edge() {
        let predictor = TilePredictor::new(45.0, 100.0, 10.0);
        // Near the edge of the map
        let state = create_state(85.0, 179.0, 45.0, 300.0);

        // Should not panic, just return what it can
        let predictions = predictor.predict(&state, 14);
        assert!(!predictions.is_empty());
    }

    #[test]
    fn test_predict_invalid_position() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        // Create state with invalid coordinates (beyond limits)
        let state = AircraftState::new(90.0, 0.0, 0.0, 300.0, 10000.0);

        // Should return empty, not panic
        let predictions = predictor.predict(&state, 14);
        assert!(predictions.is_empty());
    }

    #[test]
    fn test_default_xplane_predictor() {
        let predictor = TilePredictor::default_xplane();

        // Verify default values
        assert_eq!(predictor.cone_angle, 45.0);
        assert_eq!(predictor.cone_distance_nm, 100.0);
        assert_eq!(predictor.radial_radius_nm, 60.0);
    }

    #[test]
    fn test_large_prediction_count() {
        // Test with realistic X-Plane settings
        let predictor = TilePredictor::default_xplane();
        let state = create_state(45.0, -122.0, 90.0, 450.0);

        let predictions = predictor.predict(&state, 14);

        // Should produce a significant number of tiles
        // At 45° lat, zoom 14: ~0.93nm per tile
        // 60nm radius = ~65 tiles radius = ~13,000 tiles
        // Plus cone tiles
        assert!(
            predictions.len() > 1000,
            "Should predict many tiles, got {}",
            predictions.len()
        );
    }
}
