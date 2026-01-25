//! Configuration and progress types for the prewarm prefetcher.

/// Progress update from the prewarm prefetcher.
#[derive(Debug, Clone)]
pub enum PrewarmProgress {
    /// Starting prewarm with the given number of tiles.
    Starting { total_tiles: usize },
    /// A tile generation completed successfully.
    TileCompleted,
    /// A tile was already cached (no request needed).
    TileCached,
    /// Batch progress update (more efficient than per-tile messages).
    BatchProgress {
        completed: usize,
        cached: usize,
        failed: usize,
    },
    /// Prewarm completed.
    Complete {
        tiles_completed: usize,
        cache_hits: usize,
        failed: usize,
    },
    /// Prewarm was cancelled.
    Cancelled {
        tiles_completed: usize,
        tiles_pending: usize,
    },
}

/// Configuration for the prewarm prefetcher.
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Grid size in DSF tiles (N = N×N grid).
    /// Default: 4 (4×4 = 16 DSF tiles, ~240nm × 240nm at mid-latitudes)
    ///
    /// Based on observed X-Plane loading behavior, the simulator loads approximately
    /// a 3×4 DSF tile area on startup (~5,000-6,000 texture tiles). A 4×4 grid
    /// provides adequate coverage with a small margin.
    pub grid_size: u32,
    /// Maximum concurrent tile requests per batch.
    pub batch_size: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            grid_size: 4,
            batch_size: 50,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PrewarmConfig::default();
        assert_eq!(config.grid_size, 4);
        assert_eq!(config.batch_size, 50);
    }

    #[test]
    fn test_progress_variants() {
        // Ensure all variants can be created
        let _ = PrewarmProgress::Starting { total_tiles: 100 };
        let _ = PrewarmProgress::TileCompleted;
        let _ = PrewarmProgress::TileCached;
        let _ = PrewarmProgress::BatchProgress {
            completed: 10,
            cached: 5,
            failed: 1,
        };
        let _ = PrewarmProgress::Complete {
            tiles_completed: 90,
            cache_hits: 10,
            failed: 0,
        };
        let _ = PrewarmProgress::Cancelled {
            tiles_completed: 50,
            tiles_pending: 50,
        };
    }
}
