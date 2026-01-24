//! State types for the dashboard.
//!
//! This module contains all state-related enums and structs used by the dashboard.
//! These types are independent of rendering and can be tested in isolation.

use std::time::{Duration, Instant};

/// Events that can occur in the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardEvent {
    /// User requested quit (Ctrl+C or 'q').
    Quit,
    /// User requested cancel of current operation (e.g., pre-warm).
    Cancel,
}

/// State of the dashboard.
///
/// The dashboard transitions through states during startup:
/// 1. Loading - Building the SceneryIndex (shows progress)
/// 2. Running - Normal operation with full dashboard (prewarm runs in background)
#[derive(Debug, Clone, Default)]
pub enum DashboardState {
    /// Building the SceneryIndex - shows loading progress.
    Loading(LoadingProgress),
    /// Normal operation - full dashboard display.
    /// Prewarm (if active) runs in background and shows as status message.
    #[default]
    Running,
}

/// Progress information during SceneryIndex loading.
#[derive(Debug, Clone)]
pub struct LoadingProgress {
    /// Name of the package currently being scanned.
    pub current_package: String,
    /// Number of packages scanned so far.
    pub packages_scanned: usize,
    /// Total number of packages to scan.
    pub total_packages: usize,
    /// Total tiles indexed so far.
    pub tiles_indexed: usize,
    /// When loading started.
    pub start_time: Instant,
    /// Current loading phase (for display).
    pub phase: LoadingPhase,
    /// Whether using a cached index.
    pub using_cache: bool,
}

/// Phase of the loading process.
///
/// These variants are used to show detailed progress during index building.
/// Currently prepared for future TUI integration with OrthoUnionIndex progress.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(dead_code)] // Infrastructure for future TUI integration
pub enum LoadingPhase {
    /// Discovering packages and patches.
    #[default]
    Discovering,
    /// Checking if cached index is valid.
    CheckingCache,
    /// Scanning source directories.
    Scanning,
    /// Merging partial indexes.
    Merging,
    /// Saving index to cache.
    SavingCache,
    /// Loading complete.
    Complete,
}

#[allow(dead_code)] // Infrastructure for future TUI integration
impl LoadingPhase {
    /// Get a human-readable description of the phase.
    pub fn description(&self) -> &'static str {
        match self {
            LoadingPhase::Discovering => "Discovering packages...",
            LoadingPhase::CheckingCache => "Checking cache...",
            LoadingPhase::Scanning => "Scanning sources...",
            LoadingPhase::Merging => "Merging index...",
            LoadingPhase::SavingCache => "Saving cache...",
            LoadingPhase::Complete => "Complete",
        }
    }
}

impl Default for LoadingProgress {
    fn default() -> Self {
        Self {
            current_package: String::new(),
            packages_scanned: 0,
            total_packages: 0,
            tiles_indexed: 0,
            start_time: Instant::now(),
            phase: LoadingPhase::default(),
            using_cache: false,
        }
    }
}

impl LoadingProgress {
    /// Create a new loading progress tracker.
    pub fn new(total_packages: usize) -> Self {
        Self {
            current_package: String::new(),
            packages_scanned: 0,
            total_packages,
            tiles_indexed: 0,
            start_time: Instant::now(),
            phase: LoadingPhase::Discovering,
            using_cache: false,
        }
    }

    /// Update with a new package being scanned.
    #[allow(dead_code)]
    pub fn scanning(&mut self, package_name: &str) {
        self.current_package = package_name.to_string();
    }

    /// Mark a package as completed.
    #[allow(dead_code)]
    pub fn package_completed(&mut self, tiles_added: usize) {
        self.packages_scanned += 1;
        self.tiles_indexed += tiles_added;
    }

    /// Get the elapsed time.
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Get the completion percentage (0.0 to 1.0).
    pub fn progress_fraction(&self) -> f64 {
        if self.total_packages == 0 {
            0.0
        } else {
            self.packages_scanned as f64 / self.total_packages as f64
        }
    }

    /// Update the loading phase.
    #[allow(dead_code)] // Infrastructure for future TUI integration
    pub fn set_phase(&mut self, phase: LoadingPhase) {
        self.phase = phase;
    }

    /// Update progress from index building.
    #[allow(dead_code)] // Infrastructure for future TUI integration
    pub fn update(
        &mut self,
        phase: LoadingPhase,
        current_source: Option<&str>,
        sources_complete: usize,
        sources_total: usize,
        files_scanned: usize,
        using_cache: bool,
    ) {
        self.phase = phase;
        self.current_package = current_source.unwrap_or_default().to_string();
        self.packages_scanned = sources_complete;
        self.total_packages = sources_total;
        self.tiles_indexed = files_scanned;
        self.using_cache = using_cache;
    }
}

/// Progress information during cache pre-warming.
#[derive(Debug, Clone)]
pub struct PrewarmProgress {
    /// ICAO code of the airport.
    pub icao: String,
    /// Number of tiles loaded so far.
    pub tiles_loaded: usize,
    /// Total tiles to load.
    pub total_tiles: usize,
    /// Number of cache hits (tiles already cached).
    pub cache_hits: usize,
    /// When prewarming started (reserved for future elapsed time display).
    #[allow(dead_code)]
    pub start_time: Instant,
}

impl Default for PrewarmProgress {
    fn default() -> Self {
        Self {
            icao: String::new(),
            tiles_loaded: 0,
            total_tiles: 0,
            cache_hits: 0,
            start_time: Instant::now(),
        }
    }
}

impl PrewarmProgress {
    /// Create a new prewarm progress tracker.
    pub fn new(icao: &str, total_tiles: usize) -> Self {
        Self {
            icao: icao.to_string(),
            tiles_loaded: 0,
            total_tiles,
            cache_hits: 0,
            start_time: Instant::now(),
        }
    }

    /// Update progress with a tile loaded.
    pub fn tile_loaded(&mut self, was_cache_hit: bool) {
        self.tiles_loaded += 1;
        if was_cache_hit {
            self.cache_hits += 1;
        }
    }

    /// Update progress with a batch of tiles.
    pub fn tiles_loaded_batch(&mut self, submitted: usize, cached: usize) {
        self.tiles_loaded += submitted + cached;
        self.cache_hits += cached;
    }

    /// Get the elapsed time.
    #[allow(dead_code)]
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Get the completion percentage (0.0 to 1.0).
    pub fn progress_fraction(&self) -> f64 {
        if self.total_tiles == 0 {
            0.0
        } else {
            self.tiles_loaded as f64 / self.total_tiles as f64
        }
    }
}

/// Dashboard configuration.
pub struct DashboardConfig {
    /// Memory cache max size.
    pub memory_cache_max: usize,
    /// Disk cache max size.
    pub disk_cache_max: usize,
    /// Provider name for display (e.g., "Bing", "Google", "Go2").
    pub provider_name: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            memory_cache_max: 2 * 1024 * 1024 * 1024,
            disk_cache_max: 20 * 1024 * 1024 * 1024,
            provider_name: "Unknown".to_string(),
        }
    }
}

/// Job rate metrics for the control plane display.
///
/// Note: Used by legacy ControlPlaneWidget, kept for compatibility.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct JobRates {
    /// Jobs submitted per second (instantaneous rate).
    pub submitted_per_sec: f64,
    /// Jobs completed per second (instantaneous rate).
    pub completed_per_sec: f64,
}

impl JobRates {
    /// Calculate the pressure delta (submitted - completed per second).
    pub fn pressure(&self) -> f64 {
        self.submitted_per_sec - self.completed_per_sec
    }
}

/// Timeout for quit confirmation (auto-cancels after this duration).
pub const QUIT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// Spinner animation frames.
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
