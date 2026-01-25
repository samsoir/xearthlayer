//! DSF grid generation and bounds calculation.
//!
//! This module handles the geographic computation of DSF (Digital Surface Format)
//! tile grids centered on a given location. X-Plane uses 1°×1° DSF tiles for
//! scenery organization.

use crate::prefetch::tile_based::DsfTileCoord;

/// Bounds for a grid of DSF tiles.
///
/// Represents the geographic extent of a rectangular grid of 1°×1° DSF tiles.
/// Used for filtering terrain files by location.
#[derive(Debug, Clone, Copy)]
pub struct DsfGridBounds {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
}

impl DsfGridBounds {
    /// Compute bounds from a list of DSF tiles.
    ///
    /// The bounds encompass all tiles in the list, with max values being
    /// exclusive (DSF tiles are 1° wide, so max = min + count).
    pub fn from_tiles(tiles: &[DsfTileCoord]) -> Self {
        if tiles.is_empty() {
            return Self {
                min_lat: 0.0,
                max_lat: 0.0,
                min_lon: 0.0,
                max_lon: 0.0,
            };
        }

        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap() as f64;
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap() as f64 + 1.0; // DSF tile is 1° wide
        let min_lon = tiles.iter().map(|t| t.lon).min().unwrap() as f64;
        let max_lon = tiles.iter().map(|t| t.lon).max().unwrap() as f64 + 1.0; // DSF tile is 1° wide

        Self {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        }
    }

    /// Check if a lat/lon point is within these bounds.
    ///
    /// Uses half-open interval: min is inclusive, max is exclusive.
    #[inline]
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat < self.max_lat && lon >= self.min_lon && lon < self.max_lon
    }
}

/// Generate an N×N grid of DSF tiles centered on the given coordinates.
///
/// The grid is centered on the DSF tile containing (lat, lon), with
/// tiles extending (grid_size / 2) in each direction.
///
/// # Arguments
///
/// * `lat` - Latitude of center point
/// * `lon` - Longitude of center point
/// * `grid_size` - Number of tiles per side (e.g., 8 for 8×8 grid)
///
/// # Returns
///
/// Vector of DSF tile coordinates covering the grid area.
///
/// # Example
///
/// ```ignore
/// // 8×8 grid around Toulouse (LFBO)
/// let tiles = generate_dsf_grid(43.6294, 1.3678, 8);
/// assert_eq!(tiles.len(), 64); // 8 × 8 = 64 tiles
/// ```
pub fn generate_dsf_grid(lat: f64, lon: f64, grid_size: u32) -> Vec<DsfTileCoord> {
    let center = DsfTileCoord::from_lat_lon(lat, lon);
    let half = (grid_size / 2) as i32;

    let mut tiles = Vec::with_capacity((grid_size * grid_size) as usize);

    // Generate grid centered on center tile
    // For grid_size=4: offsets from -2 to +1 (4 tiles per axis)
    for lat_offset in -half..=(half - 1) {
        for lon_offset in -half..=(half - 1) {
            tiles.push(center.offset(lat_offset, lon_offset));
        }
    }

    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_dsf_grid_8x8() {
        // LFBO airport: 43.6294, 1.3678
        let tiles = generate_dsf_grid(43.6294, 1.3678, 8);

        // Should be 8×8 = 64 tiles
        assert_eq!(tiles.len(), 64);

        // Center should be DSF tile (43, 1)
        let center = DsfTileCoord::from_lat_lon(43.6294, 1.3678);
        assert_eq!(center.lat, 43);
        assert_eq!(center.lon, 1);

        // Grid should include tiles from -4 to +3 offset from center
        // So for lat: 43-4=39 to 43+3=46
        // For lon: 1-4=-3 to 1+3=4
        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap();
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap();
        let min_lon = tiles.iter().map(|t| t.lon).min().unwrap();
        let max_lon = tiles.iter().map(|t| t.lon).max().unwrap();

        assert_eq!(min_lat, 39); // 43 - 4
        assert_eq!(max_lat, 46); // 43 + 3
        assert_eq!(min_lon, -3); // 1 - 4
        assert_eq!(max_lon, 4); // 1 + 3
    }

    #[test]
    fn test_generate_dsf_grid_4x4() {
        let tiles = generate_dsf_grid(0.0, 0.0, 4);

        // Should be 4×4 = 16 tiles
        assert_eq!(tiles.len(), 16);

        // Grid should be -2 to +1 offset from center (0, 0)
        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap();
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap();

        assert_eq!(min_lat, -2);
        assert_eq!(max_lat, 1);
    }

    #[test]
    fn test_generate_dsf_grid_near_pole() {
        // Test near north pole - should clamp to valid range
        let tiles = generate_dsf_grid(88.5, 0.0, 4);

        // All tiles should have valid lat (clamped to 89 max)
        for tile in &tiles {
            assert!(tile.lat <= 89, "Latitude should be clamped to 89");
            assert!(tile.lat >= -90);
        }
    }

    #[test]
    fn test_generate_dsf_grid_near_antimeridian() {
        // Test near antimeridian - should wrap correctly
        let tiles = generate_dsf_grid(0.0, 178.0, 8);

        // Should have tiles that wrap from positive to negative longitude
        let has_positive = tiles.iter().any(|t| t.lon > 0);
        let has_negative = tiles.iter().any(|t| t.lon < 0);

        assert!(has_positive);
        assert!(has_negative);
    }

    #[test]
    fn test_dsf_grid_bounds() {
        let tiles = generate_dsf_grid(43.6294, 1.3678, 8);
        let bounds = DsfGridBounds::from_tiles(&tiles);

        // Bounds should cover lat 39-47 (8 tiles starting at 39) and lon -3 to 5
        assert_eq!(bounds.min_lat, 39.0);
        assert_eq!(bounds.max_lat, 47.0); // 46 + 1
        assert_eq!(bounds.min_lon, -3.0);
        assert_eq!(bounds.max_lon, 5.0); // 4 + 1

        // Test contains
        assert!(bounds.contains(43.5, 1.5)); // Center area
        assert!(bounds.contains(39.0, -3.0)); // Corner (inclusive)
        assert!(!bounds.contains(47.0, 5.0)); // Just outside
        assert!(!bounds.contains(38.9, 1.0)); // Below min_lat
    }

    #[test]
    fn test_empty_bounds() {
        let bounds = DsfGridBounds::from_tiles(&[]);
        assert_eq!(bounds.min_lat, 0.0);
        assert_eq!(bounds.max_lat, 0.0);
        assert!(!bounds.contains(0.0, 0.0)); // Empty bounds contain nothing
    }
}
