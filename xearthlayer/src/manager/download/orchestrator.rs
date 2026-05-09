//! Multi-part download orchestrator.
//!
//! This module provides the high-level orchestration for downloading
//! multi-part packages, using configurable download strategies.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::http::HttpDownloader;
use super::progress::DownloadProgressCallback;
use super::state::DownloadState;
use super::strategy::ParallelStrategy;
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::traits::PackageDownloader;

/// Multi-part download orchestrator.
///
/// Coordinates the download of all parts of a package using a configurable
/// strategy (sequential or parallel).
#[derive(Debug)]
pub struct MultiPartDownloader {
    /// HTTP downloader for individual files.
    downloader: HttpDownloader,
    /// Number of parallel downloads (determines strategy).
    parallel_downloads: usize,
}

impl Default for MultiPartDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiPartDownloader {
    /// Create a new multi-part downloader with default settings.
    pub fn new() -> Self {
        Self {
            downloader: HttpDownloader::new(),
            parallel_downloads: 4,
        }
    }

    /// Create a new multi-part downloader with custom settings.
    pub fn with_settings(timeout: Duration, parallel_downloads: usize) -> Self {
        Self {
            downloader: HttpDownloader::with_timeout(timeout),
            parallel_downloads: parallel_downloads.max(1),
        }
    }

    /// Get the number of parallel downloads.
    pub fn parallel_downloads(&self) -> usize {
        self.parallel_downloads
    }

    /// Get a reference to the underlying HTTP downloader.
    pub fn downloader(&self) -> &HttpDownloader {
        &self.downloader
    }

    /// Query the total size of all parts via HEAD requests.
    ///
    /// This updates `state.total_size` with the sum of all part sizes.
    /// HEAD requests are made in parallel for efficiency.
    pub fn query_sizes(&self, state: &mut DownloadState) {
        if state.urls.is_empty() {
            return;
        }

        // Query sizes in parallel using threads
        let urls = state.urls.clone();
        let timeout = self.downloader.timeout;

        let handles: Vec<_> = urls
            .into_iter()
            .map(|url| {
                thread::spawn(move || {
                    let downloader = HttpDownloader::with_timeout(timeout);
                    downloader.get_file_size(&url)
                })
            })
            .collect();

        let sizes: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap_or(0)).collect();

        state.total_size = sizes.iter().sum();
        state.part_sizes = sizes
            .iter()
            .map(|&s| if s > 0 { Some(s) } else { None })
            .collect();
    }

    /// Download all parts of a package.
    ///
    /// Always uses [`ParallelStrategy`]; for sequential behaviour, set
    /// `parallel_downloads = 1` and the strategy's internal semaphore
    /// gates work to one at a time. Returns `Ok(())` on success, or an
    /// error if any part failed to download.
    pub fn download_all(
        &self,
        state: &mut DownloadState,
        on_progress: Option<DownloadProgressCallback>,
    ) -> ManagerResult<()> {
        let on_progress = on_progress.map(Arc::new);

        let strategy = ParallelStrategy::new(self.parallel_downloads, self.downloader.timeout);
        strategy.execute(state, on_progress)?;

        // Check for failures
        if state.has_failures() {
            return Err(ManagerError::DownloadFailed {
                url: format!("{} parts failed", state.failure_count()),
                reason: format!("Parts {:?} failed to download", state.failed),
            });
        }

        Ok(())
    }

    /// Retry failed downloads.
    ///
    /// Attempts to re-download any parts that failed in a previous attempt.
    pub fn retry_failed(&self, state: &mut DownloadState) -> ManagerResult<()> {
        let failed_indices = state.take_failures();

        for i in failed_indices {
            let url = &state.urls[i];
            let checksum = &state.checksums[i];
            let dest = &state.destinations[i];

            match self.downloader.download(url, dest, Some(checksum)) {
                Ok(bytes) => {
                    state.record_success(bytes);
                }
                Err(_) => {
                    state.record_failure(i);
                }
            }
        }

        if state.has_failures() {
            Err(ManagerError::DownloadFailed {
                url: format!("{} parts still failed", state.failure_count()),
                reason: format!("Parts {:?} failed to download after retry", state.failed),
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_multi_part_downloader_default() {
        let downloader = MultiPartDownloader::default();
        assert_eq!(downloader.parallel_downloads(), 4);
    }

    #[test]
    fn test_multi_part_downloader_with_settings() {
        let downloader = MultiPartDownloader::with_settings(Duration::from_secs(60), 8);
        assert_eq!(downloader.parallel_downloads(), 8);
        assert_eq!(downloader.downloader().timeout.as_secs(), 60);
    }

    #[test]
    fn test_multi_part_downloader_min_parallel() {
        let downloader = MultiPartDownloader::with_settings(Duration::from_secs(60), 0);
        assert_eq!(downloader.parallel_downloads(), 1);
    }

    #[test]
    fn test_query_sizes_empty() {
        let downloader = MultiPartDownloader::new();
        let mut state = DownloadState::new(vec![], vec![], vec![]);

        downloader.query_sizes(&mut state);

        assert_eq!(state.total_size, 0);
    }

    #[test]
    fn test_download_state_from_new() {
        let state = DownloadState::new(
            vec!["http://a".to_string(), "http://b".to_string()],
            vec!["abc".to_string(), "def".to_string()],
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
        );

        assert_eq!(state.total_parts, 2);
        assert_eq!(state.downloaded_parts, 0);
        assert!(!state.is_complete());
    }
}
