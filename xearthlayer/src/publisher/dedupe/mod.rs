//! Zoom level overlap detection and deduplication for scenery packages.
//!
//! This module provides tools for detecting and resolving overlapping zoom level
//! tiles in XEarthLayer scenery packages. When multiple zoom levels (e.g., ZL16
//! and ZL18) cover the same geographic area, X-Plane loads both sets of terrain
//! triangles, causing Z-fighting (depth buffer conflicts) that manifests as
//! visual striping artifacts.
//!
//! # Overview
//!
//! The deduplication workflow:
//! 1. Scan `.ter` files to extract tile coordinates and zoom levels
//! 2. Detect overlaps by checking parent/child relationships between zoom levels
//! 3. Resolve overlaps based on priority (highest ZL, lowest ZL, or specific)
//! 4. Generate audit reports of changes
//!
//! # Coordinate Relationship
//!
//! Adjacent zoom levels follow a 4:1 coordinate ratio:
//! ```text
//! Tile at ZL(N) (row, col) → ZL(N-2) (row ÷ 4, col ÷ 4)
//!
//! Example:
//!   ZL18 (100032, 42688) → ZL16 (25008, 10672)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::publisher::dedupe::{OverlapDetector, ZoomPriority, DedupeFilter};
//!
//! let detector = OverlapDetector::new();
//! let tiles = detector.scan_package("/path/to/package")?;
//! let overlaps = detector.detect_overlaps(&tiles);
//!
//! println!("Found {} overlapping tile pairs", overlaps.len());
//! ```

mod detector;
mod report;
mod resolver;
mod types;

pub use detector::OverlapDetector;
pub use report::{DedupeAuditReport, GapAuditReport};
pub use resolver::{resolve_overlaps, RemovalSet};
pub use types::{
    CoverageGap, DedupeError, DedupeFilter, DedupeResult, GapAnalysisResult, MissingTile,
    OverlapCoverage, TileCoord, TileReference, ZoomOverlap, ZoomPriority, MAX_ZOOM_LEVEL,
    MIN_ZOOM_LEVEL,
};

#[cfg(test)]
mod tests;
