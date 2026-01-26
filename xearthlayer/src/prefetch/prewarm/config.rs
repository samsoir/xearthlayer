//! Configuration types for the prewarm prefetcher.

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
}
