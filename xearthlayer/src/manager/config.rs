//! Configuration for the Package Manager.

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for the Package Manager.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Directory where packages are installed.
    ///
    /// Typically the X-Plane Custom Scenery folder.
    pub install_dir: PathBuf,

    /// Directory for temporary downloads and extraction.
    pub staging_dir: PathBuf,

    /// URLs of package libraries to fetch packages from.
    ///
    /// Libraries are checked in order; first match wins.
    pub library_urls: Vec<String>,

    /// HTTP request timeout.
    pub timeout: Duration,

    /// Maximum concurrent downloads.
    pub max_concurrent_downloads: usize,

    /// Whether to verify checksums after download.
    pub verify_checksums: bool,

    /// Whether to keep downloaded archives after installation.
    pub keep_archives: bool,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            install_dir: PathBuf::from("."),
            staging_dir: crate::paths::temp_dir(),
            library_urls: Vec::new(),
            timeout: Duration::from_secs(30),
            max_concurrent_downloads: 4,
            verify_checksums: true,
            keep_archives: false,
        }
    }
}

impl ManagerConfig {
    /// Create a new configuration with the given install directory.
    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            install_dir,
            ..Default::default()
        }
    }

    /// Add a library URL to the configuration.
    pub fn with_library(mut self, url: impl Into<String>) -> Self {
        self.library_urls.push(url.into());
        self
    }

    /// Set the staging directory.
    pub fn with_staging_dir(mut self, path: PathBuf) -> Self {
        self.staging_dir = path;
        self
    }

    /// Set the HTTP timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the maximum concurrent downloads.
    pub fn with_max_concurrent_downloads(mut self, max: usize) -> Self {
        self.max_concurrent_downloads = max;
        self
    }

    /// Enable or disable checksum verification.
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// Enable or disable keeping downloaded archives.
    pub fn with_keep_archives(mut self, keep: bool) -> Self {
        self.keep_archives = keep;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ManagerConfig::default();
        assert!(config.library_urls.is_empty());
        assert_eq!(config.max_concurrent_downloads, 4);
        assert!(config.verify_checksums);
        assert!(!config.keep_archives);
    }

    #[test]
    fn test_builder_pattern() {
        let config = ManagerConfig::new(PathBuf::from("/custom/scenery"))
            .with_library("https://example.com/library.txt")
            .with_library("https://backup.example.com/library.txt")
            .with_timeout(Duration::from_secs(60))
            .with_max_concurrent_downloads(8)
            .with_verify_checksums(false);

        assert_eq!(config.install_dir, PathBuf::from("/custom/scenery"));
        assert_eq!(config.library_urls.len(), 2);
        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.max_concurrent_downloads, 8);
        assert!(!config.verify_checksums);
    }
}
