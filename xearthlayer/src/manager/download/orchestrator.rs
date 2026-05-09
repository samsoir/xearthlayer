//! Multi-part download orchestrator.
//!
//! Coordinates downloading all parts of a package using a
//! semaphore-bounded sliding window. With `parallel_downloads = 1`,
//! the semaphore reduces this to one active download at a time while
//! keeping the same retry, progress, and (in subsequent commits in
//! this branch) cancellation semantics.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::http::HttpDownloader;
use super::progress::{DownloadProgressCallback, ProgressCounters, ProgressReporter};
use super::semaphore::CountingSemaphore;
use super::state::DownloadState;
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::traits::{PackageDownloader, ProgressCallback};

/// Maximum number of attempts per part.
///
/// One initial attempt plus two retries.
const MAX_RETRIES: u8 = 3;

/// Base delay for exponential backoff (2 seconds).
const BASE_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Multi-part download orchestrator.
///
/// Coordinates the download of all parts of a package with a
/// semaphore-bounded sliding window of concurrent workers.
#[derive(Debug)]
pub struct MultiPartDownloader {
    /// HTTP downloader for individual files.
    downloader: HttpDownloader,
    /// Number of parallel downloads (minimum 1).
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
    /// Returns `Ok(())` if every part completed successfully.
    /// Returns [`ManagerError::DownloadFailed`] when one or more parts
    /// failed to download after [`MAX_RETRIES`] attempts.
    pub fn download_all(
        &self,
        state: &mut DownloadState,
        on_progress: Option<DownloadProgressCallback>,
    ) -> ManagerResult<()> {
        let on_progress = on_progress.map(Arc::new);

        self.run_downloads(state, on_progress)?;

        // Check for failures
        if state.has_failures() {
            return Err(ManagerError::DownloadFailed {
                url: format!("{} parts failed", state.failure_count()),
                reason: format!("Parts {:?} failed to download", state.failed),
            });
        }

        Ok(())
    }

    /// Spawn one worker per part, gate concurrency via a counting
    /// semaphore, and collect results into `state` once all workers
    /// terminate.
    ///
    /// Returns `Ok(())` for any normal completion (success, partial
    /// failure, or all-failed). The caller maps `state.failed` into
    /// an outward-facing error.
    fn run_downloads(
        &self,
        state: &mut DownloadState,
        on_progress: Option<Arc<DownloadProgressCallback>>,
    ) -> ManagerResult<()> {
        let total_parts = state.total_parts;

        // Build filenames from destination paths
        let filenames: Vec<String> = state
            .destinations
            .iter()
            .map(|d| {
                d.file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("part_{}", 0))
            })
            .collect();

        // Create shared progress counters with extended metadata
        let counters = Arc::new(ProgressCounters::new_extended(
            total_parts,
            filenames,
            state.part_sizes.clone(),
        ));

        // Start progress reporter if callback provided
        let _reporter = on_progress
            .as_ref()
            .map(|cb| ProgressReporter::start_detailed(Arc::clone(&counters), Arc::clone(cb)));

        // Create semaphore for concurrency limiting
        let semaphore = Arc::new(CountingSemaphore::new(self.parallel_downloads));

        // Spawn all threads (semaphore gates actual concurrency)
        let mut handles = Vec::with_capacity(total_parts);

        for i in 0..total_parts {
            let url = state.urls[i].clone();
            let checksum = state.checksums[i].clone();
            let dest = state.destinations[i].clone();
            let counters = Arc::clone(&counters);
            let semaphore = Arc::clone(&semaphore);
            let timeout = self.downloader.timeout;

            let handle = thread::spawn(move || {
                // Acquire permit — blocks until a slot is available
                let _permit = semaphore.acquire();

                // Mark as downloading
                counters.set_part_state(i, 1); // Downloading
                counters.set_part_attempt(i, 1);

                let mut last_error = String::new();

                for attempt in 1..=MAX_RETRIES {
                    if attempt > 1 {
                        // Mark as retrying
                        counters.set_part_state(i, 4); // Retrying
                        counters.set_part_attempt(i, attempt);

                        // Reset byte counter for retry
                        counters.update_part(i, 0);

                        // Exponential backoff: 2s, 4s, 8s
                        let delay = BASE_RETRY_DELAY * 2u32.pow(attempt as u32 - 2);
                        thread::sleep(delay);

                        // Back to downloading
                        counters.set_part_state(i, 1); // Downloading
                    }

                    let downloader = HttpDownloader::with_timeout(timeout);

                    // Create per-download progress callback
                    let counters_clone = Arc::clone(&counters);
                    let part_index = i;
                    let progress_cb: ProgressCallback =
                        Box::new(move |downloaded: u64, _total: u64| {
                            counters_clone.update_part(part_index, downloaded);
                        });

                    let result = downloader.download_with_progress(
                        &url,
                        &dest,
                        Some(checksum.as_str()),
                        progress_cb,
                    );

                    match result {
                        Ok(bytes) => {
                            counters.mark_completed(i, bytes);
                            counters.set_part_state(i, 2); // Done
                            return Some(bytes);
                        }
                        Err(e) => {
                            last_error = e.to_string();
                            // Remove partial file for clean retry
                            std::fs::remove_file(&dest).ok();
                        }
                    }
                }

                // All retries exhausted
                counters.set_part_state(i, 3); // Failed
                counters.set_part_attempt(i, MAX_RETRIES);
                counters.set_part_error(i, last_error);
                None
            });

            handles.push((i, handle));
        }

        // Collect results
        let mut failed_parts = Vec::new();
        let mut total_bytes = 0u64;
        let mut completed = 0usize;

        for (index, handle) in handles {
            match handle.join() {
                Ok(Some(bytes)) => {
                    total_bytes += bytes;
                    completed += 1;
                }
                Ok(None) => {
                    failed_parts.push(index);
                }
                Err(_) => {
                    // Thread panicked
                    failed_parts.push(index);
                }
            }
        }

        // Signal done and let reporter emit final snapshot
        counters.signal_done();

        // Update state with results
        state.downloaded_parts = completed;
        state.bytes_downloaded = total_bytes;
        state.failed = failed_parts;

        // Reporter will be dropped here, which stops it cleanly

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
