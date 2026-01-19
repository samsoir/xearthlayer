//! Tracks DSF tiles accessed during X-Plane's loading bursts.
//!
//! X-Plane loads scenery in bursts: when entering a new DSF tile, it requests
//! hundreds of DDS textures rapidly, then goes quiet for 1-2+ minutes. This
//! module tracks *which* tiles were loaded; quiet period detection is delegated
//! to the existing [`CircuitBreaker`](super::super::CircuitBreaker).
//!
//! # Design Rationale
//!
//! The original design considered inferring heading from tile load patterns,
//! but this was simplified to:
//! - Use telemetry heading when available (efficient 2-edge prediction)
//! - Fall back to all 4 edges when no telemetry (safe but less efficient)
//!
//! This keeps the burst tracker focused on one responsibility: tracking tiles.

use std::collections::HashSet;

use super::DsfTileCoord;

/// Tracks which DSF tiles were accessed during the current loading burst.
///
/// The tracker maintains a set of recently-accessed tiles. When the circuit
/// breaker indicates a quiet period, the prefetcher reads this set to
/// determine which tiles to predict from, then clears it for the next burst.
///
/// # Example
///
/// ```ignore
/// let mut tracker = TileBurstTracker::new();
///
/// // FUSE events come in during X-Plane loading
/// tracker.record_access(DsfTileCoord::new(60, -146));
/// tracker.record_access(DsfTileCoord::new(60, -145));
///
/// // When quiet detected, read tiles for prediction
/// let tiles = tracker.current_tiles().clone();
/// tracker.clear();
/// ```
#[derive(Debug, Default)]
pub struct TileBurstTracker {
    /// DSF tiles accessed during the current burst.
    current_burst: HashSet<DsfTileCoord>,
}

impl TileBurstTracker {
    /// Create a new burst tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a DDS access event.
    ///
    /// Called when FUSE receives a request for a DDS texture. The DSF tile
    /// containing the texture is added to the current burst set.
    ///
    /// Duplicate accesses to the same tile are deduplicated by the HashSet.
    pub fn record_access(&mut self, dsf_tile: DsfTileCoord) {
        self.current_burst.insert(dsf_tile);
    }

    /// Clear the current burst.
    ///
    /// Called after prefetching completes to prepare for the next burst.
    pub fn clear(&mut self) {
        self.current_burst.clear();
    }

    /// Get tiles loaded in the current burst.
    ///
    /// Returns a reference to the set of DSF tiles that have been accessed
    /// since the last `clear()` call.
    pub fn current_tiles(&self) -> &HashSet<DsfTileCoord> {
        &self.current_burst
    }

    /// Get the number of tiles in the current burst.
    pub fn tile_count(&self) -> usize {
        self.current_burst.len()
    }

    /// Check if any tiles have been recorded.
    pub fn has_tiles(&self) -> bool {
        !self.current_burst.is_empty()
    }

    /// Get an iterator over the current tiles.
    pub fn iter(&self) -> impl Iterator<Item = &DsfTileCoord> {
        self.current_burst.iter()
    }

    /// Compute the bounding box of all current tiles.
    ///
    /// Returns `None` if no tiles are in the burst.
    ///
    /// # Returns
    ///
    /// `Some((min_lat, max_lat, min_lon, max_lon))` representing the bounds
    /// of all tiles in the current burst.
    pub fn bounding_box(&self) -> Option<(i32, i32, i32, i32)> {
        if self.current_burst.is_empty() {
            return None;
        }

        let mut min_lat = i32::MAX;
        let mut max_lat = i32::MIN;
        let mut min_lon = i32::MAX;
        let mut max_lon = i32::MIN;

        for tile in &self.current_burst {
            min_lat = min_lat.min(tile.lat);
            max_lat = max_lat.max(tile.lat);
            min_lon = min_lon.min(tile.lon);
            max_lon = max_lon.max(tile.lon);
        }

        Some((min_lat, max_lat, min_lon, max_lon))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tracker_is_empty() {
        let tracker = TileBurstTracker::new();
        assert!(!tracker.has_tiles());
        assert_eq!(tracker.tile_count(), 0);
    }

    #[test]
    fn test_record_single_access() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));

        assert!(tracker.has_tiles());
        assert_eq!(tracker.tile_count(), 1);
        assert!(tracker
            .current_tiles()
            .contains(&DsfTileCoord::new(60, -146)));
    }

    #[test]
    fn test_record_multiple_accesses() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(60, -145));
        tracker.record_access(DsfTileCoord::new(61, -146));

        assert_eq!(tracker.tile_count(), 3);
    }

    #[test]
    fn test_duplicate_access_deduplication() {
        let mut tracker = TileBurstTracker::new();

        // Same tile accessed multiple times
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(60, -146));

        // Should only count as one tile
        assert_eq!(tracker.tile_count(), 1);
    }

    #[test]
    fn test_clear() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(60, -145));

        assert_eq!(tracker.tile_count(), 2);

        tracker.clear();

        assert!(!tracker.has_tiles());
        assert_eq!(tracker.tile_count(), 0);
    }

    #[test]
    fn test_bounding_box_empty() {
        let tracker = TileBurstTracker::new();
        assert!(tracker.bounding_box().is_none());
    }

    #[test]
    fn test_bounding_box_single_tile() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));

        let bbox = tracker.bounding_box().unwrap();
        assert_eq!(bbox, (60, 60, -146, -146));
    }

    #[test]
    fn test_bounding_box_multiple_tiles() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(62, -144));
        tracker.record_access(DsfTileCoord::new(58, -148));

        let (min_lat, max_lat, min_lon, max_lon) = tracker.bounding_box().unwrap();
        assert_eq!(min_lat, 58);
        assert_eq!(max_lat, 62);
        assert_eq!(min_lon, -148);
        assert_eq!(max_lon, -144);
    }

    #[test]
    fn test_iterator() {
        let mut tracker = TileBurstTracker::new();
        tracker.record_access(DsfTileCoord::new(60, -146));
        tracker.record_access(DsfTileCoord::new(60, -145));

        let tiles: Vec<_> = tracker.iter().collect();
        assert_eq!(tiles.len(), 2);
    }
}
