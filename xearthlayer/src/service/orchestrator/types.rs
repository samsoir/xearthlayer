//! Type definitions for the service orchestrator.

use std::path::PathBuf;

use tokio::task::JoinHandle;

/// Handle to a running prefetch system.
pub struct PrefetchHandle {
    /// Join handle for the prefetch task.
    #[allow(dead_code)]
    pub(super) handle: JoinHandle<()>,
}

/// Result of mounting consolidated ortho.
pub struct MountResult {
    /// Whether mounting succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
    /// Number of sources mounted.
    pub source_count: usize,
    /// Number of files indexed.
    pub file_count: usize,
    /// Names of patches mounted.
    pub patch_names: Vec<String>,
    /// Regions of packages mounted.
    pub package_regions: Vec<String>,
    /// Mountpoint path.
    pub mountpoint: PathBuf,
}

/// Progress updates during service initialization.
#[derive(Debug, Clone)]
pub enum StartupProgress {
    /// Scanning disk cache size.
    ScanningDiskCache,
    /// Mounting FUSE filesystem.
    Mounting {
        /// Current phase of index building.
        phase: crate::ortho_union::IndexBuildPhase,
        /// Source being processed.
        current_source: Option<String>,
        /// Number of sources completed.
        sources_complete: usize,
        /// Total sources to process.
        sources_total: usize,
        /// Files scanned so far.
        files_scanned: usize,
        /// Whether using cached index.
        using_cache: bool,
    },
    /// Creating overlay symlinks.
    CreatingOverlay,
    /// Starting APT telemetry receiver.
    StartingTelemetry,
    /// Building scenery index.
    BuildingSceneryIndex {
        /// Package being indexed.
        package_name: String,
        /// Package index (0-based).
        package_index: usize,
        /// Total packages to index.
        total_packages: usize,
        /// Tiles indexed so far.
        tiles_indexed: usize,
        /// Whether loaded from cache.
        from_cache: bool,
    },
    /// Scenery index complete.
    SceneryIndexComplete {
        /// Total tiles indexed.
        total_tiles: usize,
        /// Land tiles.
        land_tiles: usize,
        /// Sea tiles.
        sea_tiles: usize,
    },
    /// Starting prefetch system.
    StartingPrefetch,
    /// All services initialized.
    Complete,
}

/// Result of service initialization.
pub struct StartupResult {
    /// Mount result details.
    pub mount: MountResult,
    /// Overlay creation succeeded.
    pub overlay_success: bool,
    /// Overlay error message if failed.
    pub overlay_error: Option<String>,
    /// Scenery index tile count.
    pub scenery_tiles: usize,
    /// Whether scenery index was loaded from cache.
    pub scenery_from_cache: bool,
}
