//! Multi-part download orchestrator.
//!
//! Coordinates downloading all parts of a package using a
//! semaphore-bounded sliding window. With `parallel_downloads = 1`,
//! the semaphore reduces this to one active download at a time while
//! keeping the same retry, progress, and cancellation semantics.
//!
//! # First-failure-wins abort (issue #187)
//!
//! When any part exhausts its retries, the orchestrator sets a shared
//! `should_abort` flag. Other workers observe the flag at boundaries
//! they can act on (after acquiring their semaphore permit, between
//! retry attempts, and during backoff sleeps) and exit early without
//! starting or completing further attempts. Workers in the middle of
//! an HTTP download finish their current attempt — interrupting an
//! in-flight reqwest stream would require plumbing cancellation into
//! `HttpDownloader`, which is left as a future change.
//!
//! Cleanup of partial files on disk is the caller's responsibility
//! and is handled by [`crate::manager::install_guard::InstallTempGuard`]
//! at the installer level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

/// Granularity at which workers wake from backoff to check the abort
/// flag. 100 ms is a good tradeoff between abort responsiveness and
/// thread wake-up overhead.
const ABORT_CHECK_GRANULARITY: Duration = Duration::from_millis(100);

/// Outcome of a single worker's attempt to download one part.
///
/// Sent over an `mpsc` channel so the main loop can act on the first
/// hard failure without waiting for every worker to finish.
#[derive(Debug)]
enum WorkerOutcome {
    /// Part downloaded successfully; carries the byte count.
    Success { bytes: u64 },
    /// Part exhausted all retries; carries the last error message
    /// and the part's filename for use in the user-facing error.
    Failed { error: String, filename: String },
    /// Worker observed the abort flag and exited without finishing.
    Aborted,
}

/// Result of a sleep that can be interrupted by the abort flag.
#[derive(Debug, PartialEq, Eq)]
enum SleepResult {
    /// Slept the full requested duration.
    Completed,
    /// Returned early because the abort flag became set.
    Aborted,
}

/// Sleep up to `total`, waking every [`ABORT_CHECK_GRANULARITY`] to
/// check `abort`. Returns immediately if `abort` is already set.
fn interruptible_sleep(total: Duration, abort: &AtomicBool) -> SleepResult {
    let start = Instant::now();
    loop {
        if abort.load(Ordering::SeqCst) {
            return SleepResult::Aborted;
        }
        let elapsed = start.elapsed();
        if elapsed >= total {
            return SleepResult::Completed;
        }
        let remaining = total - elapsed;
        thread::sleep(ABORT_CHECK_GRANULARITY.min(remaining));
    }
}

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
    /// Returns `Ok(())` if every part completed successfully. Returns
    /// [`ManagerError::DownloadFailed`] when one or more parts failed:
    /// the first hard failure is reported with its filename, error
    /// detail, and a parenthetical count of any other parts that also
    /// failed before the abort cascade completed.
    pub fn download_all(
        &self,
        state: &mut DownloadState,
        on_progress: Option<DownloadProgressCallback>,
    ) -> ManagerResult<()> {
        let on_progress = on_progress.map(Arc::new);
        self.run_downloads(state, on_progress)
    }

    /// Spawn one worker per part, gate concurrency via a semaphore,
    /// collect results from a result channel, and abort all remaining
    /// workers on the first hard failure.
    fn run_downloads(
        &self,
        state: &mut DownloadState,
        on_progress: Option<Arc<DownloadProgressCallback>>,
    ) -> ManagerResult<()> {
        let total_parts = state.total_parts;

        // Build filenames from destination paths.
        let filenames: Vec<String> = state
            .destinations
            .iter()
            .map(|d| {
                d.file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("part_{}", 0))
            })
            .collect();

        // Shared progress counters with extended metadata.
        let counters = Arc::new(ProgressCounters::new_extended(
            total_parts,
            filenames.clone(),
            state.part_sizes.clone(),
        ));

        // Start the progress reporter if a callback was provided.
        let _reporter = on_progress
            .as_ref()
            .map(|cb| ProgressReporter::start_detailed(Arc::clone(&counters), Arc::clone(cb)));

        // Concurrency limiter and abort flag, both shared across workers.
        let semaphore = Arc::new(CountingSemaphore::new(self.parallel_downloads));
        let should_abort = Arc::new(AtomicBool::new(false));

        // Single producer per worker, consumed by main loop. The channel
        // closes naturally once every worker has dropped its tx clone.
        let (result_tx, result_rx) = mpsc::channel::<(usize, WorkerOutcome)>();

        let mut handles = Vec::with_capacity(total_parts);
        // Index-based loop: the body needs `i` for four parallel
        // indexed accesses (urls, checksums, destinations, filenames)
        // and as the part identifier in the channel message, so the
        // standard iter().enumerate() rewrite doesn't simplify.
        #[allow(clippy::needless_range_loop)]
        for i in 0..total_parts {
            let url = state.urls[i].clone();
            let checksum = state.checksums[i].clone();
            let dest = state.destinations[i].clone();
            let filename = filenames[i].clone();
            let counters = Arc::clone(&counters);
            let semaphore = Arc::clone(&semaphore);
            let should_abort = Arc::clone(&should_abort);
            let timeout = self.downloader.timeout;
            let tx = result_tx.clone();

            let handle = thread::spawn(move || {
                let outcome = run_part_worker(
                    i,
                    url,
                    checksum,
                    dest,
                    filename,
                    timeout,
                    counters,
                    semaphore,
                    should_abort,
                );
                let _ = tx.send((i, outcome));
            });
            handles.push(handle);
        }
        // Drop our tx so the channel closes once every worker exits.
        drop(result_tx);

        // Receive results until the channel closes. On the first hard
        // failure, set the abort flag; subsequent workers will exit
        // promptly via their next abort check.
        let mut total_bytes = 0u64;
        let mut completed = 0usize;
        let mut failed_parts: Vec<usize> = Vec::new();
        let mut first_failure: Option<(String, String)> = None; // (filename, error)
        while let Ok((index, outcome)) = result_rx.recv() {
            match outcome {
                WorkerOutcome::Success { bytes } => {
                    total_bytes += bytes;
                    completed += 1;
                }
                WorkerOutcome::Failed { error, filename } => {
                    failed_parts.push(index);
                    if first_failure.is_none() {
                        first_failure = Some((filename, error));
                        should_abort.store(true, Ordering::SeqCst);
                    }
                }
                WorkerOutcome::Aborted => {
                    failed_parts.push(index);
                }
            }
        }

        // Drain JoinHandles for clean thread exit. Each handle has
        // already sent its result and its closure has returned, so
        // join() should not block — but we still call it to avoid
        // detached threads.
        for handle in handles {
            let _ = handle.join();
        }

        // Tell the progress reporter to emit a final snapshot and exit.
        counters.signal_done();

        // Update state with results. Sort failed_parts so the displayed
        // list is deterministic across runs (channel receive order is
        // not guaranteed).
        failed_parts.sort_unstable();
        state.downloaded_parts = completed;
        state.bytes_downloaded = total_bytes;
        state.failed = failed_parts;

        // Surface a first-failure-specific error if anything failed.
        if let Some((filename, error)) = first_failure {
            let other_failures = state.failed.len().saturating_sub(1);
            let suffix = match other_failures {
                0 => String::new(),
                1 => " (1 other part also failed)".to_string(),
                n => format!(" ({} other parts also failed)", n),
            };
            return Err(ManagerError::DownloadFailed {
                url: filename,
                reason: format!("{}{}", error, suffix),
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

/// Per-part worker body. Returns the outcome instead of sending so
/// the calling thread can wrap it in the `(index, outcome)` channel
/// message exactly once.
#[allow(clippy::too_many_arguments)]
fn run_part_worker(
    i: usize,
    url: String,
    checksum: String,
    dest: std::path::PathBuf,
    filename: String,
    timeout: Duration,
    counters: Arc<ProgressCounters>,
    semaphore: Arc<CountingSemaphore>,
    should_abort: Arc<AtomicBool>,
) -> WorkerOutcome {
    // Acquire permit — blocks until a slot is available. Workers that
    // were queued behind in-flight ones will wake here, then immediately
    // observe the abort flag below.
    let _permit = semaphore.acquire();

    if should_abort.load(Ordering::SeqCst) {
        return WorkerOutcome::Aborted;
    }

    counters.set_part_state(i, 1); // Downloading
    counters.set_part_attempt(i, 1);

    let mut last_error = String::new();

    for attempt in 1..=MAX_RETRIES {
        if attempt > 1 {
            counters.set_part_state(i, 4); // Retrying
            counters.set_part_attempt(i, attempt);
            counters.update_part(i, 0); // Reset byte counter for retry

            // Exponential backoff: 2s, 4s, 8s — but interruptible.
            let delay = BASE_RETRY_DELAY * 2u32.pow(attempt as u32 - 2);
            if interruptible_sleep(delay, &should_abort) == SleepResult::Aborted {
                std::fs::remove_file(&dest).ok();
                return WorkerOutcome::Aborted;
            }

            counters.set_part_state(i, 1); // Back to downloading
        }

        // Recheck abort just before kicking off another HTTP request,
        // so a flag set during backoff doesn't leak an extra attempt.
        if should_abort.load(Ordering::SeqCst) {
            std::fs::remove_file(&dest).ok();
            return WorkerOutcome::Aborted;
        }

        let downloader = HttpDownloader::with_timeout(timeout);

        // Per-download progress callback updates the shared counter.
        let counters_clone = Arc::clone(&counters);
        let progress_cb: ProgressCallback = Box::new(move |downloaded: u64, _total: u64| {
            counters_clone.update_part(i, downloaded);
        });

        let result =
            downloader.download_with_progress(&url, &dest, Some(checksum.as_str()), progress_cb);

        match result {
            Ok(bytes) => {
                counters.mark_completed(i, bytes);
                counters.set_part_state(i, 2); // Done
                return WorkerOutcome::Success { bytes };
            }
            Err(e) => {
                last_error = e.to_string();
                std::fs::remove_file(&dest).ok();
            }
        }
    }

    counters.set_part_state(i, 3); // Failed
    counters.set_part_attempt(i, MAX_RETRIES);
    counters.set_part_error(i, last_error.clone());
    WorkerOutcome::Failed {
        error: last_error,
        filename,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

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

    #[test]
    fn interruptible_sleep_completes_when_not_aborted() {
        let abort = AtomicBool::new(false);
        let start = Instant::now();
        let result = interruptible_sleep(Duration::from_millis(200), &abort);
        let elapsed = start.elapsed();

        assert_eq!(result, SleepResult::Completed);
        assert!(
            elapsed >= Duration::from_millis(200),
            "should sleep at least the requested duration; got {:?}",
            elapsed
        );
        // Generous upper bound to tolerate scheduler jitter on busy CI.
        assert!(
            elapsed < Duration::from_millis(500),
            "should not sleep dramatically longer than requested; got {:?}",
            elapsed
        );
    }

    #[test]
    fn interruptible_sleep_returns_aborted_when_flag_set_upfront() {
        let abort = AtomicBool::new(true);
        let start = Instant::now();
        let result = interruptible_sleep(Duration::from_secs(10), &abort);
        let elapsed = start.elapsed();

        assert_eq!(result, SleepResult::Aborted);
        assert!(
            elapsed < Duration::from_millis(50),
            "must return immediately when abort is already set; got {:?}",
            elapsed
        );
    }

    #[test]
    fn interruptible_sleep_returns_aborted_when_flag_set_during_sleep() {
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = Arc::clone(&abort);

        // Set abort after 50ms; the sleeper should observe within
        // one ABORT_CHECK_GRANULARITY tick and return.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            abort_clone.store(true, Ordering::SeqCst);
        });

        let start = Instant::now();
        let result = interruptible_sleep(Duration::from_secs(10), &abort);
        let elapsed = start.elapsed();

        assert_eq!(result, SleepResult::Aborted);
        // 50ms (delay before flag set) + up to one granularity tick = ~150ms.
        // Allow generous slack for scheduling.
        assert!(
            elapsed < Duration::from_millis(500),
            "must observe abort within a few granularity ticks; got {:?}",
            elapsed
        );
    }

    #[test]
    fn first_failure_error_message_singular_other_failures() {
        // Hand-construct the error to verify the message format used
        // by run_downloads's first-failure branch. We don't run
        // workers here; that's a manual integration concern.
        let err = ManagerError::DownloadFailed {
            url: "zzXEL_eu_ortho-0.1.1.tar.gz.aa".to_string(),
            reason: "Read error: connection reset (1 other part also failed)".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("zzXEL_eu_ortho-0.1.1.tar.gz.aa"));
        assert!(display.contains("Read error: connection reset"));
        assert!(display.contains("(1 other part also failed)"));
    }

    #[test]
    fn first_failure_error_message_plural_other_failures() {
        let err = ManagerError::DownloadFailed {
            url: "zzXEL_eu_ortho-0.1.1.tar.gz.aa".to_string(),
            reason: "timeout (5 other parts also failed)".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("(5 other parts also failed)"));
    }
}
