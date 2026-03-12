//! Configuration types for the prewarm prefetcher.

/// Configuration for the prewarm prefetcher.
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Grid rows in DSF tiles (latitude extent).
    /// Default: 3 (matches empirically measured ~3° latitude window).
    pub grid_rows: u32,
    /// Grid columns in DSF tiles (longitude extent).
    /// Default: 4 (matches empirically measured ~3°/cos(lat) longitude window at mid-latitudes).
    pub grid_cols: u32,
    /// Maximum concurrent tile requests per batch.
    pub batch_size: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            grid_rows: 3,
            grid_cols: 4,
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
        assert_eq!(config.grid_rows, 3);
        assert_eq!(config.grid_cols, 4);
        assert_eq!(config.batch_size, 50);
    }
}
