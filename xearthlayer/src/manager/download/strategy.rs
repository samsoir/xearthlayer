//! Multi-part download strategy.
//!
//! Downloads multiple parts concurrently using a semaphore-bounded
//! sliding window. There is intentionally no separate sequential
//! strategy: setting `concurrency = 1` reduces this strategy to one
//! active download at a time while keeping the same retry, progress,
//! and (in subsequent phases) cancellation semantics.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::http::HttpDownloader;
use super::progress::{DownloadProgressCallback, ProgressCounters, ProgressReporter};
use super::semaphore::CountingSemaphore;
use super::state::DownloadState;
use crate::manager::error::ManagerResult;
use crate::manager::traits::{PackageDownloader, ProgressCallback};

/// Parallel download strategy.
///
/// Downloads multiple parts concurrently using a semaphore-based sliding window,
/// with real-time per-part progress reporting and inline retry with exponential backoff.
#[derive(Debug)]
pub struct ParallelStrategy {
    /// Maximum number of concurrent downloads.
    pub concurrency: usize,
    /// Timeout for each download.
    pub timeout: Duration,
}

impl ParallelStrategy {
    /// Create a new parallel strategy.
    ///
    /// # Arguments
    ///
    /// * `concurrency` - Maximum number of concurrent downloads (minimum 1)
    /// * `timeout` - Timeout for each download
    pub fn new(concurrency: usize, timeout: Duration) -> Self {
        Self {
            concurrency: concurrency.max(1),
            timeout,
        }
    }
}

impl Default for ParallelStrategy {
    fn default() -> Self {
        Self::new(4, Duration::from_secs(300))
    }
}

/// Maximum number of retry attempts per part.
const MAX_RETRIES: u8 = 3;

/// Base delay for exponential backoff (2 seconds).
const BASE_RETRY_DELAY: Duration = Duration::from_secs(2);

impl ParallelStrategy {
    /// Execute the multi-part download.
    ///
    /// Returns `Ok(())` on completion (successful or partially failed —
    /// inspect `state.failed` for per-part outcomes). Returns `Err` only
    /// for non-recoverable errors that prevent any part from running.
    pub fn execute(
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
        let semaphore = Arc::new(CountingSemaphore::new(self.concurrency));

        // Spawn all threads (semaphore gates actual concurrency)
        let mut handles = Vec::with_capacity(total_parts);

        for i in 0..total_parts {
            let url = state.urls[i].clone();
            let checksum = state.checksums[i].clone();
            let dest = state.destinations[i].clone();
            let counters = Arc::clone(&counters);
            let semaphore = Arc::clone(&semaphore);
            let timeout = self.timeout;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parallel_strategy_new() {
        let strategy = ParallelStrategy::new(8, Duration::from_secs(60));
        assert_eq!(strategy.concurrency, 8);
        assert_eq!(strategy.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_parallel_strategy_min_concurrency() {
        let strategy = ParallelStrategy::new(0, Duration::from_secs(60));
        assert_eq!(strategy.concurrency, 1);
    }

    #[test]
    fn test_parallel_strategy_default() {
        let strategy = ParallelStrategy::default();
        assert_eq!(strategy.concurrency, 4);
        assert_eq!(strategy.timeout, Duration::from_secs(300));
    }
}
