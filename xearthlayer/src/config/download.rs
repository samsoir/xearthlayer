//! Download/orchestrator configuration.

use super::file::{DEFAULT_DOWNLOAD_TIMEOUT_SECS, DEFAULT_MAX_RETRIES, DEFAULT_PARALLEL_DOWNLOADS};

/// Configuration for tile downloading and orchestration.
///
/// Groups all parameters needed to configure the download orchestrator,
/// providing sensible defaults while allowing customization.
///
/// # Example
///
/// ```
/// use xearthlayer::config::DownloadConfig;
///
/// // Using defaults
/// let config = DownloadConfig::default();
/// assert_eq!(config.timeout_secs(), 30);
/// assert_eq!(config.max_retries(), 3);
/// assert_eq!(config.parallel_downloads(), 32);
///
/// // Custom configuration
/// let config = DownloadConfig::new()
///     .with_timeout_secs(60)
///     .with_max_retries(5)
///     .with_parallel_downloads(16);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadConfig {
    /// Maximum time to spend downloading a tile (in seconds)
    timeout_secs: u64,
    /// Number of retry attempts per failed chunk
    max_retries: u32,
    /// Maximum number of concurrent downloads
    parallel_downloads: usize,
}

impl DownloadConfig {
    /// Create a new download configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the download timeout in seconds.
    ///
    /// This is the maximum time to spend downloading all chunks for a single tile.
    /// Default: 30 seconds.
    pub fn with_timeout_secs(mut self, timeout: u64) -> Self {
        self.timeout_secs = timeout;
        self
    }

    /// Set the maximum number of retry attempts per chunk.
    ///
    /// If a chunk download fails, it will be retried up to this many times
    /// before being considered failed. Default: 3 retries.
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Set the maximum number of parallel downloads.
    ///
    /// Controls how many chunk downloads can run concurrently.
    /// Higher values may improve throughput but use more resources.
    /// Default: 32 parallel downloads.
    pub fn with_parallel_downloads(mut self, parallel: usize) -> Self {
        self.parallel_downloads = parallel;
        self
    }

    /// Get the download timeout in seconds.
    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    /// Get the maximum number of retry attempts.
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Get the maximum number of parallel downloads.
    pub fn parallel_downloads(&self) -> usize {
        self.parallel_downloads
    }
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_DOWNLOAD_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            parallel_downloads: DEFAULT_PARALLEL_DOWNLOADS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.timeout_secs(), DEFAULT_DOWNLOAD_TIMEOUT_SECS);
        assert_eq!(config.max_retries(), DEFAULT_MAX_RETRIES);
        assert_eq!(config.parallel_downloads(), DEFAULT_PARALLEL_DOWNLOADS);
    }

    #[test]
    fn test_new_equals_default() {
        let new_config = DownloadConfig::new();
        let default_config = DownloadConfig::default();
        assert_eq!(new_config, default_config);
    }

    #[test]
    fn test_with_timeout_secs() {
        let config = DownloadConfig::new().with_timeout_secs(60);
        assert_eq!(config.timeout_secs(), 60);
        assert_eq!(config.max_retries(), DEFAULT_MAX_RETRIES); // Unchanged
        assert_eq!(config.parallel_downloads(), DEFAULT_PARALLEL_DOWNLOADS); // Unchanged
    }

    #[test]
    fn test_with_max_retries() {
        let config = DownloadConfig::new().with_max_retries(5);
        assert_eq!(config.timeout_secs(), DEFAULT_DOWNLOAD_TIMEOUT_SECS); // Unchanged
        assert_eq!(config.max_retries(), 5);
        assert_eq!(config.parallel_downloads(), DEFAULT_PARALLEL_DOWNLOADS); // Unchanged
    }

    #[test]
    fn test_with_parallel_downloads() {
        let config = DownloadConfig::new().with_parallel_downloads(16);
        assert_eq!(config.timeout_secs(), DEFAULT_DOWNLOAD_TIMEOUT_SECS); // Unchanged
        assert_eq!(config.max_retries(), DEFAULT_MAX_RETRIES); // Unchanged
        assert_eq!(config.parallel_downloads(), 16);
    }

    #[test]
    fn test_builder_chain() {
        let config = DownloadConfig::new()
            .with_timeout_secs(45)
            .with_max_retries(2)
            .with_parallel_downloads(64);

        assert_eq!(config.timeout_secs(), 45);
        assert_eq!(config.max_retries(), 2);
        assert_eq!(config.parallel_downloads(), 64);
    }

    #[test]
    fn test_copy_semantics() {
        let config1 = DownloadConfig::new().with_timeout_secs(60);
        let config2 = config1; // Copy, not move
        assert_eq!(config1.timeout_secs(), config2.timeout_secs());
    }

    #[test]
    fn test_equality() {
        let config1 = DownloadConfig::new().with_timeout_secs(30);
        let config2 = DownloadConfig::new().with_timeout_secs(30);
        let config3 = DownloadConfig::new().with_timeout_secs(60);

        assert_eq!(config1, config2);
        assert_ne!(config1, config3);
    }

    #[test]
    fn test_debug_impl() {
        let config = DownloadConfig::new();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("DownloadConfig"));
        assert!(debug_str.contains("timeout_secs"));
        assert!(debug_str.contains(&DEFAULT_DOWNLOAD_TIMEOUT_SECS.to_string()));
    }
}
