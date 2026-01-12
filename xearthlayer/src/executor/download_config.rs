//! Download configuration for chunk downloads.
//!
//! This module provides configuration for the download stage of tile generation,
//! including timeout and retry settings.

use std::time::Duration;

use crate::config::{DEFAULT_MAX_RETRIES, DEFAULT_REQUEST_TIMEOUT_SECS};

/// Configuration for chunk downloads.
///
/// This is a focused configuration for the download task, containing only
/// the settings needed for HTTP requests and retry logic.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// HTTP request timeout for chunk downloads.
    ///
    /// Default: 10 seconds
    pub request_timeout: Duration,

    /// Maximum retry attempts per chunk.
    ///
    /// Default: 3
    pub max_retries: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

impl DownloadConfig {
    /// Creates a new download configuration with custom settings.
    pub fn new(request_timeout: Duration, max_retries: u32) -> Self {
        Self {
            request_timeout,
            max_retries,
        }
    }

    /// Creates a configuration with a custom timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Creates a configuration with custom retry count.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }
}

impl From<&crate::config::ExecutorSettings> for DownloadConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            request_timeout: Duration::from_secs(settings.request_timeout_secs),
            max_retries: settings.max_retries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.request_timeout, Duration::from_secs(10));
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_builder_pattern() {
        let config = DownloadConfig::default()
            .with_timeout(Duration::from_secs(30))
            .with_max_retries(5);

        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.max_retries, 5);
    }
}
