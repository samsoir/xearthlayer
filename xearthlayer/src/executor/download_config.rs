//! Download configuration for chunk downloads.
//!
//! This module provides configuration for the download stage of tile generation,
//! including timeout, retry settings, and HTTP concurrency control.
//!
//! # HTTP Concurrency
//!
//! When multiple tiles are downloaded concurrently, each tile's download task
//! spawns up to 256 async tasks (one per chunk). Without coordination, this
//! creates a multiplicative effect: N tiles × M chunks = N×M concurrent HTTP
//! requests. To prevent this, a shared semaphore limits total HTTP requests
//! across all tiles.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::config::{DEFAULT_MAX_RETRIES, DEFAULT_REQUEST_TIMEOUT_SECS};

/// Default maximum concurrent HTTP requests across all tiles.
///
/// This is the global limit for concurrent HTTP requests to the imagery
/// provider. A reasonable value prevents overwhelming the provider while
/// maintaining good throughput.
///
/// 512 provides good throughput for most providers while allowing
/// the multi-stage tile pipeline to remain saturated.
pub const DEFAULT_MAX_CONCURRENT_HTTP: usize = 512;

/// Configuration for chunk downloads.
///
/// This configuration controls HTTP requests for downloading satellite imagery
/// chunks. It includes timeout, retry settings, and a **shared semaphore** for
/// global HTTP concurrency limiting.
///
/// # Shared Semaphore
///
/// The `http_semaphore` field is critical for preventing HTTP request explosion.
/// When multiple tiles download concurrently, each spawns up to 256 async tasks.
/// Without a shared semaphore, this creates N×256 concurrent HTTP requests.
/// The shared semaphore limits total concurrent requests across all tiles.
#[derive(Clone)]
pub struct DownloadConfig {
    /// HTTP request timeout for chunk downloads.
    ///
    /// Default: 10 seconds
    pub request_timeout: Duration,

    /// Maximum retry attempts per chunk.
    ///
    /// Default: 3
    pub max_retries: u32,

    /// Shared semaphore for limiting concurrent HTTP requests across all tiles.
    ///
    /// This is the global HTTP concurrency limiter. All chunk downloads acquire
    /// a permit from this semaphore before making HTTP requests, ensuring the
    /// total concurrent requests stay within bounds regardless of how many
    /// tiles are being processed.
    ///
    /// Default capacity: 256 permits
    pub http_semaphore: Arc<Semaphore>,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
            http_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_HTTP)),
        }
    }
}

impl DownloadConfig {
    /// Creates a new download configuration with custom timeout and retries.
    ///
    /// Uses a new default-capacity semaphore for HTTP limiting.
    pub fn new(request_timeout: Duration, max_retries: u32) -> Self {
        Self {
            request_timeout,
            max_retries,
            http_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_HTTP)),
        }
    }

    /// Creates a configuration with a shared HTTP semaphore.
    ///
    /// Use this when you want multiple `DownloadConfig` instances to share
    /// the same HTTP concurrency limit (e.g., across a factory).
    pub fn with_semaphore(
        request_timeout: Duration,
        max_retries: u32,
        http_semaphore: Arc<Semaphore>,
    ) -> Self {
        Self {
            request_timeout,
            max_retries,
            http_semaphore,
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

    /// Creates a configuration with a custom HTTP semaphore.
    ///
    /// This allows sharing a semaphore across multiple download tasks,
    /// ensuring total HTTP concurrency stays within bounds.
    pub fn with_http_semaphore(mut self, semaphore: Arc<Semaphore>) -> Self {
        self.http_semaphore = semaphore;
        self
    }

    /// Returns the HTTP semaphore capacity.
    pub fn http_capacity(&self) -> usize {
        self.http_semaphore.available_permits()
    }
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadConfig")
            .field("request_timeout", &self.request_timeout)
            .field("max_retries", &self.max_retries)
            .field(
                "http_semaphore_permits",
                &self.http_semaphore.available_permits(),
            )
            .finish()
    }
}

impl From<&crate::config::ExecutorSettings> for DownloadConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            request_timeout: Duration::from_secs(settings.request_timeout_secs),
            max_retries: settings.max_retries,
            http_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_HTTP)),
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
        assert_eq!(config.http_capacity(), DEFAULT_MAX_CONCURRENT_HTTP);
    }

    #[test]
    fn test_builder_pattern() {
        let config = DownloadConfig::default()
            .with_timeout(Duration::from_secs(30))
            .with_max_retries(5);

        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.max_retries, 5);
    }

    #[test]
    fn test_shared_semaphore() {
        let shared_sem = Arc::new(Semaphore::new(128));

        let config1 = DownloadConfig::default().with_http_semaphore(Arc::clone(&shared_sem));
        let config2 = DownloadConfig::default().with_http_semaphore(Arc::clone(&shared_sem));

        // Both configs share the same semaphore
        assert_eq!(config1.http_capacity(), 128);
        assert_eq!(config2.http_capacity(), 128);

        // Acquiring from one affects the other
        let permit = shared_sem.try_acquire().unwrap();
        assert_eq!(config1.http_capacity(), 127);
        assert_eq!(config2.http_capacity(), 127);
        drop(permit);
    }

    #[test]
    fn test_with_semaphore_constructor() {
        let sem = Arc::new(Semaphore::new(64));
        let config = DownloadConfig::with_semaphore(Duration::from_secs(5), 2, Arc::clone(&sem));

        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.http_capacity(), 64);
    }

    #[test]
    fn test_debug_format() {
        let config = DownloadConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("DownloadConfig"));
        assert!(debug.contains("request_timeout"));
        assert!(debug.contains("http_semaphore_permits"));
    }
}
