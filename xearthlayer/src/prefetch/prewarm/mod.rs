//! Pre-warm prefetcher for cold-start cache warming.
//!
//! This module provides a one-shot prefetcher that loads tiles around a
//! specific airport before the flight starts, using terrain file scanning.
//!
//! # Architecture
//!
//! The prewarm system is decomposed into focused sub-modules:
//!
//! ```text
//! start_prewarm() (entry point)
//!     │
//!     ├─► grid.rs: DSF grid generation
//!     │     └─ generate_dsf_grid(), DsfGridBounds
//!     │
//!     ├─► scanner.rs: Terrain file discovery
//!     │     └─ TerrainScanner trait, FileTerrainScanner
//!     │
//!     ├─► context.rs: Job execution with authoritative tracking
//!     │     └─ PrewarmContext, PrewarmStatus, PrewarmHandle
//!     │
//!     └─► config.rs: Configuration types
//!           └─ PrewarmConfig
//! ```
//!
//! # Workflow
//!
//! 1. Compute an N×N grid of DSF (1°×1°) tiles centered on the target airport
//! 2. Scan `terrain/` folders to find tiles within the grid bounds
//! 3. Start prewarm context (filters memory cache + disk tiles, submits jobs, tracks completion)
//! 4. Query status via PrewarmHandle
//!
//! # Example
//!
//! ```ignore
//! // Find tiles to prewarm
//! let scanner = FileTerrainScanner::new(ortho_index);
//! let dsf_tiles = generate_dsf_grid(lat, lon, 4);
//! let bounds = DsfGridBounds::from_tiles(&dsf_tiles);
//! let tiles = scanner.scan(&bounds);
//!
//! // Start prewarm and get handle
//! let handle = start_prewarm(
//!     "KSFO".to_string(),
//!     tiles,
//!     dds_client,
//!     memory_cache,
//!     ortho_index,
//!     &runtime,
//! );
//!
//! // Query status in UI loop
//! let status = handle.status();
//! if status.is_complete {
//!     println!("Done: {}/{}", status.completed, status.total);
//! }
//! ```

mod config;
mod context;
mod grid;
mod scanner;

// Public API
pub use config::PrewarmConfig;
pub use context::{start_prewarm, PrewarmHandle, PrewarmStatus};
pub use grid::{generate_dsf_grid, DsfGridBounds};
pub use scanner::{FileTerrainScanner, TerrainScanner};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Verify core types are exported
        let _config = PrewarmConfig::default();
    }

    #[test]
    fn test_generate_dsf_grid_exported() {
        let tiles = generate_dsf_grid(43.0, 1.0, 4);
        assert_eq!(tiles.len(), 16);
    }
}
