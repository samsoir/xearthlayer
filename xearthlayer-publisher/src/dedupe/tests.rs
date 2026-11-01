//! Integration tests for the dedupe module.
//!
//! These tests verify that the detector and resolver work together correctly.
//!
//! Key concepts:
//! - Complete coverage: All 16 ZL18 children (4×4 grid) exist for a ZL16 parent
//! - Partial coverage: Some but not all children exist (creates gaps if removed)
//! - Only complete overlaps are safe to dedupe

use super::*;
use std::path::PathBuf;

/// Create a test tile reference.
fn make_tile(row: u32, col: u32, zoom: u8, lat: f32, lon: f32) -> TileReference {
    TileReference {
        row,
        col,
        zoom,
        provider: "BI".to_string(),
        lat,
        lon,
        ter_path: PathBuf::from(format!("{}_{}_{}{}.ter", row, col, "BI", zoom)),
        is_sea: false,
    }
}

/// Create all 16 ZL18 children for a ZL16 parent tile.
/// ZL16 (parent_row, parent_col) → ZL18 children at (row*4..row*4+3, col*4..col*4+3)
fn make_complete_zl18_coverage(
    parent_row: u32,
    parent_col: u32,
    lat: f32,
    lon: f32,
) -> Vec<TileReference> {
    let mut children = Vec::new();
    let base_row = parent_row * 4;
    let base_col = parent_col * 4;

    for row_off in 0..4 {
        for col_off in 0..4 {
            children.push(make_tile(
                base_row + row_off,
                base_col + col_off,
                18,
                lat + (row_off as f32) * 0.01,
                lon + (col_off as f32) * 0.01,
            ));
        }
    }
    children
}

/// Create a ZoomOverlap for two tiles (test helper).
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
fn test_partial_coverage_not_removed() {
    // Only 1 ZL18 child exists - this is PARTIAL coverage
    // Should NOT remove the ZL16 tile (would create a 15/16 gap)
    let zl18 = make_tile(100032, 42688, 18, 39.15, -121.36);
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36); // parent of (100032..100035, 42688..42691)

    let tiles = vec![zl18.clone(), zl16.clone()];

    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    // Should detect 1 overlap with PARTIAL coverage
    assert_eq!(overlaps.len(), 1);
    assert_eq!(overlaps[0].coverage, OverlapCoverage::Partial);

    // Resolve with highest priority - should NOT remove the ZL16 (partial coverage)
    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

    assert_eq!(result.tiles_analyzed, 2);
    assert_eq!(result.tiles_removed.len(), 0); // Nothing removed - gap protection!
    assert_eq!(result.tiles_preserved.len(), 2); // Both preserved
}

#[test]
fn test_complete_coverage_removed() {
    // All 16 ZL18 children exist - this is COMPLETE coverage
    // Safe to remove the ZL16 tile
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);
    let zl18_children = make_complete_zl18_coverage(25008, 10672, 39.15, -121.36);

    let mut tiles = zl18_children.clone();
    tiles.push(zl16.clone());

    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    // Should detect 1 overlap with COMPLETE coverage
    assert_eq!(overlaps.len(), 1);
    assert_eq!(overlaps[0].coverage, OverlapCoverage::Complete);

    // Resolve with highest priority - ZL16 should be removed
    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

    assert_eq!(result.tiles_removed.len(), 1);
    assert_eq!(result.tiles_removed[0].zoom, 16); // Lower removed
    assert_eq!(result.tiles_preserved.len(), 16); // All 16 ZL18 tiles kept
}

#[test]
fn test_complete_coverage_lowest_priority() {
    // All 16 ZL18 children exist
    // With lowest priority, remove all ZL18 children and keep ZL16
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);
    let zl18_children = make_complete_zl18_coverage(25008, 10672, 39.15, -121.36);

    let mut tiles = zl18_children.clone();
    tiles.push(zl16.clone());

    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    // Resolve with lowest priority (smaller package)
    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Lowest);

    // With complete coverage, we can remove higher ZL tiles
    assert_eq!(result.tiles_removed.len(), 16); // All 16 ZL18 removed
    assert_eq!(result.tiles_preserved.len(), 1);
    assert_eq!(result.tiles_preserved[0].zoom, 16); // ZL16 kept
}

#[test]
fn test_partial_cascade_protection() {
    // ZL18 → ZL16 → ZL14 overlaps, but only 1 tile at each level
    // All should be marked as PARTIAL (only 1 of 16/256 children)
    let zl18 = make_tile(100032, 42688, 18, 39.15, -121.36);
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);
    let zl14 = make_tile(6252, 2668, 14, 39.15, -121.36);

    let tiles = vec![zl18.clone(), zl16.clone(), zl14.clone()];

    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    // Should detect 3 overlaps, all PARTIAL
    assert_eq!(overlaps.len(), 3);
    for overlap in &overlaps {
        assert_eq!(overlap.coverage, OverlapCoverage::Partial);
    }

    // With highest priority, NOTHING should be removed (all partial)
    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

    assert_eq!(result.tiles_removed.len(), 0); // Gap protection!
    assert_eq!(result.tiles_preserved.len(), 3); // All preserved
}

#[test]
fn test_specific_zoom_with_complete_coverage() {
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);
    let zl18_children = make_complete_zl18_coverage(25008, 10672, 39.15, -121.36);

    let mut tiles = zl18_children.clone();
    tiles.push(zl16.clone());

    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    // Keep ZL16 specifically - all ZL18 children removed
    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Specific(16));

    assert_eq!(result.tiles_removed.len(), 16); // All ZL18 removed
    assert_eq!(result.tiles_preserved[0].zoom, 16); // ZL16 kept
}

#[test]
fn test_no_overlap_different_areas() {
    // Two ZL18 tiles in different areas (not parent/child)
    let tile1 = make_tile(100032, 42688, 18, 39.15, -121.36);
    let tile2 = make_tile(100000, 42000, 18, 38.50, -120.50);

    let tiles = vec![tile1, tile2];
    let detector = OverlapDetector::new();
    let overlaps = detector.detect_overlaps(&tiles);

    assert!(overlaps.is_empty());

    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);
    assert!(result.tiles_removed.is_empty());
    assert_eq!(result.tiles_preserved.len(), 2);
}

#[test]
fn test_filter_by_tile_coord() {
    let tile_in_cell = make_tile(100032, 42688, 18, 39.15, -121.36);
    let tile_out_of_cell = make_tile(100000, 42000, 18, 38.50, -120.50);

    // Filter for tiles in cell (39, -122)
    let filter = DedupeFilter::for_tile(39, -122);

    assert!(filter.matches(&tile_in_cell));
    assert!(!filter.matches(&tile_out_of_cell));
}

#[test]
fn test_overlaps_by_pair_stats() {
    // Multiple overlaps of the same type
    let zl18_a = make_tile(100032, 42688, 18, 39.15, -121.36);
    let zl18_b = make_tile(100048, 42704, 18, 39.20, -121.30);
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);

    let tiles = vec![zl18_a.clone(), zl18_b.clone(), zl16.clone()];

    // Manually create overlaps (both ZL18 tiles overlap the same ZL16)
    let overlaps = vec![make_overlap(&zl18_a, &zl16), make_overlap(&zl18_b, &zl16)];

    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

    // Should report 2 overlaps for the (18, 16) pair
    assert_eq!(result.overlaps_by_pair.get(&(18, 16)), Some(&2));
    assert_eq!(result.total_overlaps(), 2);
    assert!(result.has_overlaps());
}

#[test]
fn test_removal_set_deduplication() {
    // Same tile appears in multiple overlaps
    let zl18 = make_tile(100032, 42688, 18, 39.15, -121.36);
    let zl16 = make_tile(25008, 10672, 16, 39.15, -121.36);
    let zl14 = make_tile(6252, 2668, 14, 39.15, -121.36);

    let tiles = vec![zl18.clone(), zl16.clone(), zl14.clone()];

    // ZL16 appears in two overlaps but should only be removed once
    let overlaps = vec![make_overlap(&zl18, &zl16), make_overlap(&zl16, &zl14)];

    let result = resolve_overlaps(&tiles, &overlaps, ZoomPriority::Highest);

    // ZL16 and ZL14 removed (ZL16 once, not twice)
    assert_eq!(result.tiles_removed.len(), 2);
}
