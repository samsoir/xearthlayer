//! Predicts which DSF tiles to prefetch based on loaded tiles and heading.
//!
//! The predictor uses a simple algorithm:
//! 1. Find the bounding box of currently loaded tiles
//! 2. Determine forward edges based on heading (or all edges if no heading)
//! 3. Expand outward by `rows_ahead` tiles on each forward edge
//! 4. **Include corner tiles** where edges meet
//!
//! # Heading-Aware vs All-Edges
//!
//! - **With telemetry**: Uses 2 forward edges (e.g., N + E for northeast heading)
//!   This is efficient, prefetching only ~50% of edge tiles.
//!
//! - **Without telemetry**: Uses all 4 edges as fallback.
//!   This is safe but wasteful, prefetching ~100% of edge tiles.
//!
//! # Corner Tiles
//!
//! Corner tiles are critical because X-Plane often loads diagonally when
//! flying at angles like 45° or 225°. The predictor always includes corner
//! tiles where the selected edges meet.

use std::collections::HashSet;

use super::DsfTileCoord;

/// Predicts next DSF tiles to prefetch.
///
/// The predictor expands outward from the bounding box of loaded tiles,
/// prioritizing edges based on aircraft heading when available.
#[derive(Debug, Clone)]
pub struct TilePredictor {
    /// Number of tile rows to prefetch ahead on each selected edge.
    ///
    /// Default: 1 (prefetch the immediate next row of tiles)
    rows_ahead: u32,
}

impl Default for TilePredictor {
    fn default() -> Self {
        Self { rows_ahead: 1 }
    }
}

/// Cardinal and intercardinal directions for edge expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    North,
    South,
    East,
    West,
}

impl TilePredictor {
    /// Create a new tile predictor with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom rows ahead.
    ///
    /// # Arguments
    ///
    /// * `rows_ahead` - Number of tile rows to prefetch ahead (1-3 recommended)
    pub fn with_rows_ahead(rows_ahead: u32) -> Self {
        Self { rows_ahead }
    }

    /// Predict next DSF tiles to prefetch.
    ///
    /// # Arguments
    ///
    /// * `loaded_tiles` - Set of currently loaded DSF tiles
    /// * `heading` - Aircraft heading in degrees (0-360), or `None` for all-edges fallback
    ///
    /// # Returns
    ///
    /// Vector of tiles to prefetch, excluding tiles already in `loaded_tiles`.
    ///
    /// # Algorithm
    ///
    /// 1. Compute bounding box of loaded tiles
    /// 2. Select edges based on heading:
    ///    - With heading: 2 forward edges + corners
    ///    - Without heading: All 4 edges + all corners
    /// 3. Expand `rows_ahead` tiles beyond each selected edge
    /// 4. Include corner tiles where edges meet
    /// 5. Filter out already-loaded tiles
    ///
    /// # Example
    ///
    /// ```ignore
    /// let predictor = TilePredictor::with_rows_ahead(1);
    /// let loaded: HashSet<_> = [DsfTileCoord::new(60, -146)].into_iter().collect();
    ///
    /// // With heading 45° (northeast): predicts N edge, E edge, and NE corner
    /// let predicted = predictor.predict_next_tiles(&loaded, Some(45.0));
    /// ```
    pub fn predict_next_tiles(
        &self,
        loaded_tiles: &HashSet<DsfTileCoord>,
        heading: Option<f32>,
    ) -> Vec<DsfTileCoord> {
        if loaded_tiles.is_empty() {
            return Vec::new();
        }

        // Compute bounding box
        let (min_lat, max_lat, min_lon, max_lon) = bounding_box(loaded_tiles);

        // Determine which edges to expand based on heading
        let (edges, corners) = match heading {
            Some(h) => forward_edges_and_corners(h),
            None => all_edges_and_corners(),
        };

        let mut predicted = HashSet::new();

        // Expand each selected edge
        for edge in &edges {
            self.expand_edge(&mut predicted, *edge, min_lat, max_lat, min_lon, max_lon);
        }

        // Include corner regions
        for (lat_dir, lon_dir) in &corners {
            self.expand_corner(
                &mut predicted,
                *lat_dir,
                *lon_dir,
                min_lat,
                max_lat,
                min_lon,
                max_lon,
            );
        }

        // Filter out already-loaded tiles and return as Vec
        predicted
            .into_iter()
            .filter(|t| !loaded_tiles.contains(t))
            .collect()
    }

    /// Expand tiles along a single edge.
    fn expand_edge(
        &self,
        predicted: &mut HashSet<DsfTileCoord>,
        direction: Direction,
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
    ) {
        match direction {
            Direction::North => {
                // Expand north from the top edge
                for row in 1..=self.rows_ahead as i32 {
                    let new_lat = max_lat + row;
                    if new_lat > 89 {
                        continue; // Clamp to valid range
                    }
                    for lon in min_lon..=max_lon {
                        predicted.insert(DsfTileCoord::new(new_lat, lon));
                    }
                }
            }
            Direction::South => {
                // Expand south from the bottom edge
                for row in 1..=self.rows_ahead as i32 {
                    let new_lat = min_lat - row;
                    if new_lat < -90 {
                        continue;
                    }
                    for lon in min_lon..=max_lon {
                        predicted.insert(DsfTileCoord::new(new_lat, lon));
                    }
                }
            }
            Direction::East => {
                // Expand east from the right edge
                for col in 1..=self.rows_ahead as i32 {
                    let mut new_lon = max_lon + col;
                    // Wrap longitude
                    if new_lon >= 180 {
                        new_lon -= 360;
                    }
                    for lat in min_lat..=max_lat {
                        predicted.insert(DsfTileCoord::new(lat, new_lon));
                    }
                }
            }
            Direction::West => {
                // Expand west from the left edge
                for col in 1..=self.rows_ahead as i32 {
                    let mut new_lon = min_lon - col;
                    // Wrap longitude
                    if new_lon < -180 {
                        new_lon += 360;
                    }
                    for lat in min_lat..=max_lat {
                        predicted.insert(DsfTileCoord::new(lat, new_lon));
                    }
                }
            }
        }
    }

    /// Expand corner region (diagonal tiles).
    ///
    /// Corners are crucial when flying at angles like 45°, 135°, etc.
    /// This method adds tiles in the diagonal quadrant beyond the bounding box.
    #[allow(clippy::too_many_arguments)]
    fn expand_corner(
        &self,
        predicted: &mut HashSet<DsfTileCoord>,
        lat_direction: i32, // +1 for north, -1 for south
        lon_direction: i32, // +1 for east, -1 for west
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
    ) {
        let base_lat = if lat_direction > 0 { max_lat } else { min_lat };
        let base_lon = if lon_direction > 0 { max_lon } else { min_lon };

        // Add corner tiles in the diagonal direction
        for lat_offset in 1..=self.rows_ahead as i32 {
            for lon_offset in 1..=self.rows_ahead as i32 {
                let new_lat = base_lat + lat_direction * lat_offset;
                let mut new_lon = base_lon + lon_direction * lon_offset;

                // Validate latitude
                if !(-90..=89).contains(&new_lat) {
                    continue;
                }

                // Wrap longitude
                if new_lon >= 180 {
                    new_lon -= 360;
                } else if new_lon < -180 {
                    new_lon += 360;
                }

                predicted.insert(DsfTileCoord::new(new_lat, new_lon));
            }
        }
    }
}

/// Compute bounding box of a set of tiles.
fn bounding_box(tiles: &HashSet<DsfTileCoord>) -> (i32, i32, i32, i32) {
    let mut min_lat = i32::MAX;
    let mut max_lat = i32::MIN;
    let mut min_lon = i32::MAX;
    let mut max_lon = i32::MIN;

    for tile in tiles {
        min_lat = min_lat.min(tile.lat);
        max_lat = max_lat.max(tile.lat);
        min_lon = min_lon.min(tile.lon);
        max_lon = max_lon.max(tile.lon);
    }

    (min_lat, max_lat, min_lon, max_lon)
}

/// Determine forward edges and corners based on heading.
///
/// Divides the compass into 4 quadrants:
/// - NE (315° - 45°): North + East edges, NE corner
/// - SE (45° - 135°): South + East edges, SE corner
/// - SW (135° - 225°): South + West edges, SW corner
/// - NW (225° - 315°): North + West edges, NW corner
fn forward_edges_and_corners(heading: f32) -> (Vec<Direction>, Vec<(i32, i32)>) {
    // Normalize heading to 0-360
    let h = ((heading % 360.0) + 360.0) % 360.0;

    // Determine quadrant (using 45° sectors centered on the diagonals)
    // This gives us the 2 forward edges and the corner between them
    // Boundaries are inclusive at the lower bound of each quadrant
    if h > 315.0 || h <= 45.0 {
        // Northeast quadrant: heading roughly north (315-360 or 0-45)
        (
            vec![Direction::North, Direction::East],
            vec![(1, 1)], // NE corner
        )
    } else if h <= 135.0 {
        // Southeast quadrant: heading roughly east (45-135)
        (
            vec![Direction::East, Direction::South],
            vec![(-1, 1)], // SE corner
        )
    } else if h <= 225.0 {
        // Southwest quadrant: heading roughly south (135-225)
        (
            vec![Direction::South, Direction::West],
            vec![(-1, -1)], // SW corner
        )
    } else {
        // Northwest quadrant: heading roughly west (225-315)
        (
            vec![Direction::West, Direction::North],
            vec![(1, -1)], // NW corner
        )
    }
}

/// Return all edges and all corners for fallback mode (no telemetry).
fn all_edges_and_corners() -> (Vec<Direction>, Vec<(i32, i32)>) {
    (
        vec![
            Direction::North,
            Direction::South,
            Direction::East,
            Direction::West,
        ],
        vec![
            (1, 1),   // NE
            (1, -1),  // NW
            (-1, 1),  // SE
            (-1, -1), // SW
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile_set(tiles: &[(i32, i32)]) -> HashSet<DsfTileCoord> {
        tiles
            .iter()
            .map(|(lat, lon)| DsfTileCoord::new(*lat, *lon))
            .collect()
    }

    #[test]
    fn test_empty_loaded_tiles() {
        let predictor = TilePredictor::new();
        let loaded = HashSet::new();

        let predicted = predictor.predict_next_tiles(&loaded, Some(45.0));
        assert!(predicted.is_empty());
    }

    #[test]
    fn test_single_tile_north_heading() {
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146)]);

        // Heading north (0°) should predict N and E edges plus NE corner
        let predicted = predictor.predict_next_tiles(&loaded, Some(0.0));

        // Should include tile directly north
        assert!(
            predicted.contains(&DsfTileCoord::new(61, -146)),
            "Should predict tile to the north"
        );

        // Should include tile to the east
        assert!(
            predicted.contains(&DsfTileCoord::new(60, -145)),
            "Should predict tile to the east"
        );

        // Should include NE corner
        assert!(
            predicted.contains(&DsfTileCoord::new(61, -145)),
            "Should predict NE corner tile"
        );
    }

    #[test]
    fn test_single_tile_south_heading() {
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146)]);

        // Heading south (180°) should predict S and W edges plus SW corner
        let predicted = predictor.predict_next_tiles(&loaded, Some(180.0));

        // Should include tile directly south
        assert!(
            predicted.contains(&DsfTileCoord::new(59, -146)),
            "Should predict tile to the south"
        );

        // Should include tile to the west
        assert!(
            predicted.contains(&DsfTileCoord::new(60, -147)),
            "Should predict tile to the west"
        );

        // Should include SW corner
        assert!(
            predicted.contains(&DsfTileCoord::new(59, -147)),
            "Should predict SW corner tile"
        );
    }

    #[test]
    fn test_no_heading_predicts_all_edges() {
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146)]);

        // No heading = predict all 4 edges + all 4 corners
        let predicted = predictor.predict_next_tiles(&loaded, None);

        // Should include all 4 cardinal directions
        assert!(predicted.contains(&DsfTileCoord::new(61, -146))); // N
        assert!(predicted.contains(&DsfTileCoord::new(59, -146))); // S
        assert!(predicted.contains(&DsfTileCoord::new(60, -145))); // E
        assert!(predicted.contains(&DsfTileCoord::new(60, -147))); // W

        // Should include all 4 corners
        assert!(predicted.contains(&DsfTileCoord::new(61, -145))); // NE
        assert!(predicted.contains(&DsfTileCoord::new(61, -147))); // NW
        assert!(predicted.contains(&DsfTileCoord::new(59, -145))); // SE
        assert!(predicted.contains(&DsfTileCoord::new(59, -147))); // SW
    }

    #[test]
    fn test_excludes_already_loaded() {
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146), (61, -146)]); // Include the north tile

        let predicted = predictor.predict_next_tiles(&loaded, Some(0.0));

        // Should NOT include the already-loaded north tile
        assert!(
            !predicted.contains(&DsfTileCoord::new(61, -146)),
            "Should not predict already-loaded tile"
        );

        // Should predict the tile beyond (2 rows north)
        assert!(
            predicted.contains(&DsfTileCoord::new(62, -146)),
            "Should predict tile beyond loaded area"
        );
    }

    #[test]
    fn test_multiple_rows_ahead() {
        let predictor = TilePredictor::with_rows_ahead(2);
        let loaded = tile_set(&[(60, -146)]);

        let predicted = predictor.predict_next_tiles(&loaded, Some(0.0));

        // Should include 2 rows to the north
        assert!(predicted.contains(&DsfTileCoord::new(61, -146)));
        assert!(predicted.contains(&DsfTileCoord::new(62, -146)));

        // Should include 2 columns to the east
        assert!(predicted.contains(&DsfTileCoord::new(60, -145)));
        assert!(predicted.contains(&DsfTileCoord::new(60, -144)));
    }

    #[test]
    fn test_bounding_box_expansion() {
        let predictor = TilePredictor::with_rows_ahead(1);
        // A 2x2 block of loaded tiles
        let loaded = tile_set(&[(60, -146), (60, -145), (61, -146), (61, -145)]);

        // Heading east (90°) should predict E edge
        let predicted = predictor.predict_next_tiles(&loaded, Some(90.0));

        // Should predict the entire east edge (2 tiles tall)
        assert!(predicted.contains(&DsfTileCoord::new(60, -144)));
        assert!(predicted.contains(&DsfTileCoord::new(61, -144)));
    }

    #[test]
    fn test_latitude_clamping() {
        let predictor = TilePredictor::with_rows_ahead(1);
        // Near north pole
        let loaded = tile_set(&[(89, 0)]);

        let predicted = predictor.predict_next_tiles(&loaded, Some(0.0));

        // Should not predict beyond 89° latitude
        assert!(
            !predicted.iter().any(|t| t.lat > 89),
            "Should not predict tiles beyond 89°N"
        );
    }

    #[test]
    fn test_longitude_wrapping() {
        let predictor = TilePredictor::with_rows_ahead(1);
        // Near the date line
        let loaded = tile_set(&[(0, 179)]);

        // Heading east should wrap to negative longitude
        let predicted = predictor.predict_next_tiles(&loaded, Some(90.0));

        assert!(
            predicted.contains(&DsfTileCoord::new(0, -180)),
            "Should wrap longitude from 179 to -180"
        );
    }

    #[test]
    fn test_heading_quadrants() {
        // Test each quadrant
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146)]);

        // Northeast heading (45°)
        let ne = predictor.predict_next_tiles(&loaded, Some(45.0));
        assert!(ne.contains(&DsfTileCoord::new(61, -146))); // N
        assert!(ne.contains(&DsfTileCoord::new(60, -145))); // E

        // Southeast heading (135°)
        let se = predictor.predict_next_tiles(&loaded, Some(135.0));
        assert!(se.contains(&DsfTileCoord::new(59, -146))); // S
        assert!(se.contains(&DsfTileCoord::new(60, -145))); // E

        // Southwest heading (225°)
        let sw = predictor.predict_next_tiles(&loaded, Some(225.0));
        assert!(sw.contains(&DsfTileCoord::new(59, -146))); // S
        assert!(sw.contains(&DsfTileCoord::new(60, -147))); // W

        // Northwest heading (315°)
        let nw = predictor.predict_next_tiles(&loaded, Some(315.0));
        assert!(nw.contains(&DsfTileCoord::new(61, -146))); // N
        assert!(nw.contains(&DsfTileCoord::new(60, -147))); // W
    }

    #[test]
    fn test_corners_included() {
        let predictor = TilePredictor::with_rows_ahead(1);
        let loaded = tile_set(&[(60, -146)]);

        // Northeast heading - must include NE corner
        let predicted = predictor.predict_next_tiles(&loaded, Some(45.0));
        assert!(
            predicted.contains(&DsfTileCoord::new(61, -145)),
            "NE corner must be included for NE heading"
        );

        // All edges mode - must include all 4 corners
        let all = predictor.predict_next_tiles(&loaded, None);
        assert!(all.contains(&DsfTileCoord::new(61, -145))); // NE
        assert!(all.contains(&DsfTileCoord::new(61, -147))); // NW
        assert!(all.contains(&DsfTileCoord::new(59, -145))); // SE
        assert!(all.contains(&DsfTileCoord::new(59, -147))); // SW
    }
}
