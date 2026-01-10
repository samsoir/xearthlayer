//! Progress reporting for index building.
//!
//! This module provides types for reporting progress during the parallel
//! index building process. Progress is reported via a callback function
//! that can be wired to a TUI or logging system.

use std::sync::Arc;

/// Progress callback for index building.
///
/// The callback receives [`IndexBuildProgress`] updates during index construction.
/// It must be `Send + Sync` to support parallel scanning with rayon.
pub type IndexBuildProgressCallback = Arc<dyn Fn(IndexBuildProgress) + Send + Sync>;

/// Progress information during index building.
#[derive(Debug, Clone)]
pub struct IndexBuildProgress {
    /// Current phase of index building.
    pub phase: IndexBuildPhase,

    /// Source currently being processed (if in scanning phase).
    pub current_source: Option<String>,

    /// Number of sources completed so far.
    pub sources_complete: usize,

    /// Total number of sources to process.
    pub sources_total: usize,

    /// Total files scanned so far (across all sources).
    pub files_scanned: usize,

    /// Total directories scanned so far.
    pub directories_scanned: usize,

    /// Whether using a cached index (skip scanning).
    pub using_cache: bool,
}

impl IndexBuildProgress {
    /// Create a new progress tracker in the discovering phase.
    pub fn new(sources_total: usize) -> Self {
        Self {
            phase: IndexBuildPhase::Discovering,
            current_source: None,
            sources_complete: 0,
            sources_total,
            files_scanned: 0,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Create progress for cache hit scenario.
    pub fn cache_hit(sources_total: usize, files_count: usize) -> Self {
        Self {
            phase: IndexBuildPhase::Complete,
            current_source: None,
            sources_complete: sources_total,
            sources_total,
            files_scanned: files_count,
            directories_scanned: 0,
            using_cache: true,
        }
    }

    // =========================================================================
    // Factory methods for creating progress at specific phases
    // =========================================================================

    /// Create progress for the checking cache phase.
    pub fn at_checking_cache(sources_total: usize) -> Self {
        Self::new(sources_total).checking_cache()
    }

    /// Create progress for starting the scanning phase.
    pub fn at_scanning_start(sources_total: usize) -> Self {
        Self {
            phase: IndexBuildPhase::Scanning,
            current_source: None,
            sources_complete: 0,
            sources_total,
            files_scanned: 0,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Create progress for scanning a specific source.
    pub fn at_scanning_source(
        source_name: &str,
        sources_complete: usize,
        sources_total: usize,
        files_scanned: usize,
    ) -> Self {
        Self {
            phase: IndexBuildPhase::Scanning,
            current_source: Some(source_name.to_string()),
            sources_complete,
            sources_total,
            files_scanned,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Create progress for the merging phase.
    pub fn at_merging(sources_total: usize, files_scanned: usize) -> Self {
        Self {
            phase: IndexBuildPhase::Merging,
            current_source: None,
            sources_complete: sources_total,
            sources_total,
            files_scanned,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Create progress for the saving cache phase.
    pub fn at_saving_cache(sources_total: usize, files_scanned: usize) -> Self {
        Self {
            phase: IndexBuildPhase::SavingCache,
            current_source: None,
            sources_complete: sources_total,
            sources_total,
            files_scanned,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Create progress for the complete phase.
    pub fn at_complete(sources_total: usize, files_scanned: usize) -> Self {
        Self {
            phase: IndexBuildPhase::Complete,
            current_source: None,
            sources_complete: sources_total,
            sources_total,
            files_scanned,
            directories_scanned: 0,
            using_cache: false,
        }
    }

    /// Update to discovering phase.
    pub fn discovering(&self) -> Self {
        Self {
            phase: IndexBuildPhase::Discovering,
            ..self.clone()
        }
    }

    /// Update to checking cache phase.
    pub fn checking_cache(&self) -> Self {
        Self {
            phase: IndexBuildPhase::CheckingCache,
            ..self.clone()
        }
    }

    /// Update to scanning phase with current source.
    pub fn scanning(&self, source_name: &str) -> Self {
        Self {
            phase: IndexBuildPhase::Scanning,
            current_source: Some(source_name.to_string()),
            ..self.clone()
        }
    }

    /// Update with source completion.
    pub fn source_complete(&self, files_added: usize, dirs_added: usize) -> Self {
        Self {
            sources_complete: self.sources_complete + 1,
            files_scanned: self.files_scanned + files_added,
            directories_scanned: self.directories_scanned + dirs_added,
            current_source: None,
            ..self.clone()
        }
    }

    /// Update to merging phase.
    pub fn merging(&self) -> Self {
        Self {
            phase: IndexBuildPhase::Merging,
            current_source: None,
            ..self.clone()
        }
    }

    /// Update to saving cache phase.
    pub fn saving_cache(&self) -> Self {
        Self {
            phase: IndexBuildPhase::SavingCache,
            current_source: None,
            ..self.clone()
        }
    }

    /// Update to complete phase.
    pub fn complete(&self) -> Self {
        Self {
            phase: IndexBuildPhase::Complete,
            current_source: None,
            ..self.clone()
        }
    }

    /// Get progress as a fraction (0.0 to 1.0).
    pub fn progress_fraction(&self) -> f64 {
        if self.sources_total == 0 {
            1.0
        } else {
            self.sources_complete as f64 / self.sources_total as f64
        }
    }
}

impl Default for IndexBuildProgress {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Phase of index building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBuildPhase {
    /// Checking if cached index is valid.
    CheckingCache,

    /// Discovering patches and packages.
    Discovering,

    /// Scanning source directories (parallel).
    Scanning,

    /// Merging partial indexes from parallel scans.
    Merging,

    /// Saving built index to cache.
    SavingCache,

    /// Index building complete.
    Complete,
}

impl IndexBuildPhase {
    /// Get a human-readable description of this phase.
    pub fn description(&self) -> &'static str {
        match self {
            Self::CheckingCache => "Checking cache",
            Self::Discovering => "Discovering sources",
            Self::Scanning => "Scanning sources",
            Self::Merging => "Merging indexes",
            Self::SavingCache => "Saving cache",
            Self::Complete => "Complete",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_new() {
        let progress = IndexBuildProgress::new(4);

        assert_eq!(progress.phase, IndexBuildPhase::Discovering);
        assert_eq!(progress.sources_total, 4);
        assert_eq!(progress.sources_complete, 0);
        assert!(!progress.using_cache);
    }

    #[test]
    fn test_progress_cache_hit() {
        let progress = IndexBuildProgress::cache_hit(4, 1000);

        assert_eq!(progress.phase, IndexBuildPhase::Complete);
        assert_eq!(progress.sources_complete, 4);
        assert_eq!(progress.files_scanned, 1000);
        assert!(progress.using_cache);
    }

    #[test]
    fn test_progress_fraction() {
        let progress = IndexBuildProgress::new(4);
        assert_eq!(progress.progress_fraction(), 0.0);

        let progress = progress.source_complete(100, 10);
        assert_eq!(progress.progress_fraction(), 0.25);

        let progress = progress.source_complete(100, 10);
        assert_eq!(progress.progress_fraction(), 0.5);
    }

    #[test]
    fn test_progress_scanning() {
        let progress = IndexBuildProgress::new(2).scanning("na_ortho");

        assert_eq!(progress.phase, IndexBuildPhase::Scanning);
        assert_eq!(progress.current_source, Some("na_ortho".to_string()));
    }

    #[test]
    fn test_phase_description() {
        assert_eq!(IndexBuildPhase::Scanning.description(), "Scanning sources");
        assert_eq!(IndexBuildPhase::Complete.description(), "Complete");
    }
}
