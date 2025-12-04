//! Scenery processor abstraction for converting scenery sources to XEarthLayer packages.
//!
//! This module defines the `SceneryProcessor` trait that all scenery source processors
//! must implement. Currently supports X-Plane 12 scenery format with extensibility
//! for future X-Plane versions.

use std::fmt;
use std::path::Path;

use super::{PublishResult, Repository};
use crate::package::PackageType;

mod ortho4xp;
mod overlay;
mod tile;

pub use ortho4xp::Ortho4XPProcessor;
pub use overlay::OverlayProcessor;
pub use tile::{ProcessSummary, TileInfo, TileWarning};

/// Scenery format identifier.
///
/// Identifies which X-Plane scenery system a processor targets.
/// This allows the package system to handle different scenery formats
/// appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SceneryFormat {
    /// X-Plane 12 scenery format (DSF tiles, terrain, textures).
    XPlane12,
    // Future: XPlane13 when the new scenery system is released
}

impl fmt::Display for SceneryFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SceneryFormat::XPlane12 => write!(f, "X-Plane 12"),
        }
    }
}

/// Result of scanning a scenery source.
///
/// Contains the discovered tiles and any warnings encountered during scanning.
#[derive(Debug, Clone)]
pub struct SceneryScanResult {
    /// The tiles discovered during scanning.
    pub tiles: Vec<TileInfo>,

    /// Warnings encountered during scanning (non-fatal issues).
    pub warnings: Vec<TileWarning>,
}

impl SceneryScanResult {
    /// Create a new scan result with tiles and no warnings.
    pub fn new(tiles: Vec<TileInfo>) -> Self {
        Self {
            tiles,
            warnings: Vec::new(),
        }
    }

    /// Create a new scan result with tiles and warnings.
    pub fn with_warnings(tiles: Vec<TileInfo>, warnings: Vec<TileWarning>) -> Self {
        Self { tiles, warnings }
    }

    /// Returns true if the scan found any tiles.
    pub fn has_tiles(&self) -> bool {
        !self.tiles.is_empty()
    }

    /// Returns true if any warnings were generated.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// Trait for scenery source processors.
///
/// A scenery processor converts output from a scenery creation tool (like Ortho4XP)
/// into the XEarthLayer package format. Different tools may organize their output
/// differently, but all produce compatible X-Plane scenery.
///
/// # Implementing a New Processor
///
/// To support a new scenery source:
///
/// 1. Create a new struct implementing `SceneryProcessor`
/// 2. Implement `scan()` to discover tiles in the source's output format
/// 3. Implement `process()` to copy/organize files into package structure
/// 4. Return the appropriate `SceneryFormat` from `scenery_format()`
///
/// # Example
///
/// ```ignore
/// use xearthlayer::publisher::{SceneryProcessor, SceneryFormat, SceneryScanResult};
///
/// struct MyProcessor;
///
/// impl SceneryProcessor for MyProcessor {
///     fn name(&self) -> &str {
///         "My Scenery Tool"
///     }
///
///     fn scenery_format(&self) -> SceneryFormat {
///         SceneryFormat::XPlane12
///     }
///
///     fn scan(&self, source: &Path) -> PublishResult<SceneryScanResult> {
///         // Discover tiles...
///     }
///
///     fn process(
///         &self,
///         scan_result: &SceneryScanResult,
///         region: &str,
///         package_type: PackageType,
///         repo: &Repository,
///     ) -> PublishResult<ProcessSummary> {
///         // Process tiles into package...
///     }
/// }
/// ```
pub trait SceneryProcessor: Send + Sync {
    /// Returns the human-readable name of this processor.
    ///
    /// Used for logging and user-facing messages.
    fn name(&self) -> &str;

    /// Returns the scenery format this processor produces.
    ///
    /// This determines how the resulting package will be handled
    /// by the package manager (mounting strategy, file expectations, etc.).
    fn scenery_format(&self) -> SceneryFormat;

    /// Scan a source directory for tiles.
    ///
    /// Discovers all valid tiles in the source directory and returns
    /// information about each one. May also return warnings for
    /// tiles that have issues but can still be processed.
    ///
    /// # Arguments
    ///
    /// * `source` - Path to the scenery source directory
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The source path doesn't exist or isn't accessible
    /// - No valid tiles are found
    /// - A critical error prevents scanning
    fn scan(&self, source: &Path) -> PublishResult<SceneryScanResult>;

    /// Process scanned tiles into a package.
    ///
    /// Takes the tiles discovered by `scan()` and organizes them into
    /// the XEarthLayer package structure within the repository.
    ///
    /// # Arguments
    ///
    /// * `scan_result` - The result from `scan()`
    /// * `region` - The region identifier for the package
    /// * `package_type` - The type of package to create (Ortho or Overlay)
    /// * `repo` - The repository to create the package in
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - File operations fail (copy, create directory)
    /// - The package already exists (unless updating)
    fn process(
        &self,
        scan_result: &SceneryScanResult,
        region: &str,
        package_type: PackageType,
        repo: &Repository,
    ) -> PublishResult<ProcessSummary>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scenery_format_display() {
        assert_eq!(format!("{}", SceneryFormat::XPlane12), "X-Plane 12");
    }

    #[test]
    fn test_scenery_format_equality() {
        assert_eq!(SceneryFormat::XPlane12, SceneryFormat::XPlane12);
    }

    #[test]
    fn test_scenery_format_clone() {
        let format = SceneryFormat::XPlane12;
        let cloned = format;
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_scan_result_new() {
        let result = SceneryScanResult::new(vec![]);
        assert!(!result.has_tiles());
        assert!(!result.has_warnings());
    }

    #[test]
    fn test_scan_result_with_tiles() {
        let tile = TileInfo {
            id: "+37-118".to_string(),
            path: std::path::PathBuf::from("/test"),
            latitude: 37,
            longitude: -118,
            dsf_files: vec![],
            ter_files: vec![],
            mask_files: vec![],
            dds_files: vec![],
        };
        let result = SceneryScanResult::new(vec![tile]);
        assert!(result.has_tiles());
        assert!(!result.has_warnings());
    }

    #[test]
    fn test_scan_result_with_warnings() {
        let warning = TileWarning {
            tile_id: "+37-118".to_string(),
            message: "test warning".to_string(),
        };
        let result = SceneryScanResult::with_warnings(vec![], vec![warning]);
        assert!(!result.has_tiles());
        assert!(result.has_warnings());
    }
}
