//! Priority-based resolution for zoom level overlaps.
//!
//! Determines which tiles to remove based on the configured priority strategy.

use std::collections::HashSet;
use std::path::PathBuf;

use super::types::{DedupeResult, OverlapCoverage, TileReference, ZoomOverlap, ZoomPriority};

/// Set of tiles to remove after resolution.
#[derive(Debug, Default)]
pub struct RemovalSet {
    /// Tile paths marked for removal.
    pub paths: HashSet<PathBuf>,
    /// Detailed information about removed tiles.
    pub tiles: Vec<TileReference>,
}

impl RemovalSet {
    /// Check if a tile path is in the removal set.
    pub fn contains(&self, path: &PathBuf) -> bool {
        self.paths.contains(path)
    }

    /// Get the number of tiles to remove.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Check if the removal set is empty.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Resolve overlaps based on the given priority strategy.
///
/// Returns a `DedupeResult` containing the tiles to remove and preserve.
///
/// # Arguments
///
/// * `tiles` - All tiles in the package
/// * `overlaps` - Detected overlaps between tiles
/// * `priority` - Strategy for resolving overlaps
///
/// # Example
///
/// ```ignore
/// use xearthlayer::publisher::dedupe::{resolve_overlaps, ZoomPriority};
///
/// let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);
/// println!("Tiles to remove: {}", result.tiles_removed.len());
/// ```
pub fn resolve_overlaps(
    tiles: &[TileReference],
    overlaps: &[ZoomOverlap],
    priority: ZoomPriority,
) -> DedupeResult {
    let mut result = DedupeResult {
        tiles_analyzed: tiles.len(),
        zoom_levels_present: get_sorted_zoom_levels(tiles),
        ..Default::default()
    };

    // Count overlaps by pair
    for overlap in overlaps {
        let key = (overlap.higher_zl.zoom, overlap.lower_zl.zoom);
        *result.overlaps_by_pair.entry(key).or_insert(0) += 1;
    }

    // Build removal set based on priority
    let removal_set = build_removal_set(overlaps, priority);

    // Categorize tiles as removed or preserved
    for tile in tiles {
        if removal_set.contains(&tile.ter_path) {
            result.tiles_removed.push(tile.clone());
        } else {
            result.tiles_preserved.push(tile.clone());
        }
    }

    result
}

/// Build a set of tile paths to remove based on priority.
///
/// Only processes overlaps with Complete coverage to avoid creating gaps.
/// Partial overlaps are skipped - the higher ZL tiles will naturally
/// take precedence in X-Plane's rendering without needing to remove
/// the lower ZL tile.
fn build_removal_set(overlaps: &[ZoomOverlap], priority: ZoomPriority) -> RemovalSet {
    let mut removal_set = RemovalSet::default();

    for overlap in overlaps {
        // Only remove tiles when coverage is complete to avoid creating gaps
        if overlap.coverage != OverlapCoverage::Complete {
            continue;
        }

        match priority {
            ZoomPriority::Highest => {
                // Keep higher ZL tiles, remove lower ZL parent
                let tile = &overlap.lower_zl;
                if removal_set.paths.insert(tile.ter_path.clone()) {
                    removal_set.tiles.push(tile.clone());
                }
            }
            ZoomPriority::Lowest => {
                // Keep lower ZL parent, remove ALL higher ZL children
                for tile in &overlap.all_higher_zl {
                    if removal_set.paths.insert(tile.ter_path.clone()) {
                        removal_set.tiles.push(tile.clone());
                    }
                }
            }
            ZoomPriority::Specific(target_zl) => {
                // Keep the one matching target_zl, remove the other(s)
                if overlap.higher_zl.zoom == target_zl {
                    // Keep higher, remove lower
                    let tile = &overlap.lower_zl;
                    if removal_set.paths.insert(tile.ter_path.clone()) {
                        removal_set.tiles.push(tile.clone());
                    }
                } else if overlap.lower_zl.zoom == target_zl {
                    // Keep lower, remove ALL higher children
                    for tile in &overlap.all_higher_zl {
                        if removal_set.paths.insert(tile.ter_path.clone()) {
                            removal_set.tiles.push(tile.clone());
                        }
                    }
                } else {
                    // Neither matches target, keep higher by default (remove lower)
                    let tile = &overlap.lower_zl;
                    if removal_set.paths.insert(tile.ter_path.clone()) {
                        removal_set.tiles.push(tile.clone());
                    }
                }
            }
        }
    }

    removal_set
}

/// Get sorted zoom levels from a collection of tiles.
fn get_sorted_zoom_levels(tiles: &[TileReference]) -> Vec<u8> {
    let mut levels: Vec<u8> = tiles
        .iter()
        .map(|t| t.zoom)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    levels.sort();
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publisher::dedupe::OverlapCoverage;
    use std::path::PathBuf;

    fn make_tile(row: u32, col: u32, zoom: u8) -> TileReference {
        TileReference {
            row,
            col,
            zoom,
            provider: "BI".to_string(),
            lat: 39.0,
            lon: -121.0,
            ter_path: PathBuf::from(format!("{}_{}_{}{}.ter", row, col, "BI", zoom)),
            is_sea: false,
        }
    }

    fn make_overlap(higher: &TileReference, lower: &TileReference) -> ZoomOverlap {
        ZoomOverlap {
            higher_zl: higher.clone(),
            lower_zl: lower.clone(),
            all_higher_zl: vec![higher.clone()],
            zl_diff: higher.zoom - lower.zoom,
            coverage: OverlapCoverage::Complete,
        }
    }

    #[test]
    fn test_resolve_priority_highest() {
        let zl18 = make_tile(100032, 42688, 18);
        let zl16 = make_tile(25008, 10672, 16);
        let tiles = vec![zl18.clone(), zl16.clone()];
        let overlaps = vec![make_overlap(&zl18, &zl16)];

        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

        assert_eq!(result.tiles_removed.len(), 1);
        assert_eq!(result.tiles_removed[0].zoom, 16); // Lower removed
        assert_eq!(result.tiles_preserved.len(), 1);
        assert_eq!(result.tiles_preserved[0].zoom, 18); // Higher kept
    }

    #[test]
    fn test_resolve_priority_lowest() {
        let zl18 = make_tile(100032, 42688, 18);
        let zl16 = make_tile(25008, 10672, 16);
        let tiles = vec![zl18.clone(), zl16.clone()];
        let overlaps = vec![make_overlap(&zl18, &zl16)];

        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Lowest);

        assert_eq!(result.tiles_removed.len(), 1);
        assert_eq!(result.tiles_removed[0].zoom, 18); // Higher removed
        assert_eq!(result.tiles_preserved.len(), 1);
        assert_eq!(result.tiles_preserved[0].zoom, 16); // Lower kept
    }

    #[test]
    fn test_resolve_priority_specific() {
        let zl18 = make_tile(100032, 42688, 18);
        let zl16 = make_tile(25008, 10672, 16);
        let tiles = vec![zl18.clone(), zl16.clone()];
        let overlaps = vec![make_overlap(&zl18, &zl16)];

        // Keep ZL16 specifically
        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Specific(16));

        assert_eq!(result.tiles_removed.len(), 1);
        assert_eq!(result.tiles_removed[0].zoom, 18); // ZL18 removed
        assert_eq!(result.tiles_preserved.len(), 1);
        assert_eq!(result.tiles_preserved[0].zoom, 16); // ZL16 kept
    }

    #[test]
    fn test_resolve_multi_level() {
        let zl18 = make_tile(100032, 42688, 18);
        let zl16 = make_tile(25008, 10672, 16);
        let zl14 = make_tile(6252, 2668, 14);
        let tiles = vec![zl18.clone(), zl16.clone(), zl14.clone()];
        let overlaps = vec![
            make_overlap(&zl18, &zl16),
            make_overlap(&zl18, &zl14),
            make_overlap(&zl16, &zl14),
        ];

        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

        // ZL16 and ZL14 should be removed (ZL18 kept)
        assert_eq!(result.tiles_removed.len(), 2);
        let removed_zooms: Vec<u8> = result.tiles_removed.iter().map(|t| t.zoom).collect();
        assert!(removed_zooms.contains(&16));
        assert!(removed_zooms.contains(&14));

        assert_eq!(result.tiles_preserved.len(), 1);
        assert_eq!(result.tiles_preserved[0].zoom, 18);
    }

    #[test]
    fn test_resolve_no_overlaps() {
        let zl18 = make_tile(100032, 42688, 18);
        let zl16 = make_tile(25009, 10672, 16); // Not parent of zl18
        let tiles = vec![zl18.clone(), zl16.clone()];
        let overlaps = vec![]; // No overlaps

        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

        assert!(result.tiles_removed.is_empty());
        assert_eq!(result.tiles_preserved.len(), 2);
    }

    #[test]
    fn test_removal_set() {
        let mut set = RemovalSet::default();
        assert!(set.is_empty());

        let path1 = PathBuf::from("tile1.ter");
        let path2 = PathBuf::from("tile2.ter");

        set.paths.insert(path1.clone());
        assert_eq!(set.len(), 1);
        assert!(set.contains(&path1));
        assert!(!set.contains(&path2));
    }

    #[test]
    fn test_overlaps_by_pair_counting() {
        let zl18_a = make_tile(100032, 42688, 18);
        let zl18_b = make_tile(100048, 42704, 18);
        let zl16 = make_tile(25008, 10672, 16);
        let tiles = vec![zl18_a.clone(), zl18_b.clone(), zl16.clone()];
        let overlaps = vec![make_overlap(&zl18_a, &zl16), make_overlap(&zl18_b, &zl16)];

        let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

        // Both overlaps are ZL18→ZL16
        assert_eq!(result.overlaps_by_pair.get(&(18, 16)), Some(&2));
    }
}
