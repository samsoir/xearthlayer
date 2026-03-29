# Parallel Package Downloads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make package downloads concurrent with per-part progress bars, configurable concurrency, and per-part retry with exponential backoff.

**Architecture:** Enhance the existing `manager/download/` module — extend `ProgressCounters` with per-part state, replace the batch execution model in `ParallelStrategy` with a semaphore-based sliding window, add a `RetryDownloader` decorator, and replace the CLI's single-line progress with `indicatif::MultiProgress`. The new `DownloadProgressCallback` replaces `MultiPartProgressCallback` at all call sites.

**Tech Stack:** Rust, indicatif (new CLI dep), std::sync (Condvar-based semaphore), reqwest (existing)

**Spec:** `docs/dev/specs/2026-03-24-parallel-package-downloads.md`

---

## File Structure

### Modified Files (library — `xearthlayer/src/`)

| File | Change |
|------|--------|
| `config/settings.rs` | Add `concurrent_downloads: usize` to `PackagesSettings` |
| `config/keys.rs` | Add `PackagesConcurrentDownloads` variant, `RangeIntegerSpec` |
| `config/parser.rs` | Parse `concurrent_downloads` from `[packages]` section |
| `manager/download/progress.rs` | New `PartState`, `PartProgress`, `DownloadProgress` types; extend `ProgressCounters`; replace `MultiPartProgressCallback` |
| `manager/download/strategy.rs` | Replace batch loop with semaphore sliding window; integrate `RetryDownloader`; use new callback |
| `manager/download/orchestrator.rs` | Store per-part sizes; pass new callback type through |
| `manager/download/state.rs` | Add `part_sizes: Vec<Option<u64>>` |
| `manager/download/mod.rs` | Update re-exports |
| `manager/installer.rs` | Read config key; update callback bridge |
| `manager/traits.rs` | No changes needed (trait stays the same) |

### New Files

| File | Purpose |
|------|---------|
| `manager/download/retry.rs` | `RetryDownloader` decorator (3 retries, exponential backoff) |
| `manager/download/semaphore.rs` | `CountingSemaphore` (Condvar + Mutex, no external dep) |

### Modified Files (CLI — `xearthlayer-cli/src/`)

| File | Change |
|------|--------|
| `Cargo.toml` | Add `indicatif` dependency |
| `commands/packages/services.rs` | Replace single-line progress with `indicatif::MultiProgress` |

---

## Task 1: Config Key — `packages.concurrent_downloads`

**Files:**
- Modify: `xearthlayer/src/config/settings.rs`
- Modify: `xearthlayer/src/config/keys.rs`
- Modify: `xearthlayer/src/config/parser.rs`

- [ ] **Step 1: Add field to `PackagesSettings`**

In `xearthlayer/src/config/settings.rs`, add to `PackagesSettings` struct (after `temp_dir`):

```rust
    /// Number of concurrent part downloads (1-10, default: 5).
    pub concurrent_downloads: usize,
```

Update the `Default` impl for `PackagesSettings` (search for it in the same file or `parser.rs`) to include `concurrent_downloads: 5`.

- [ ] **Step 2: Add `PackagesConcurrentDownloads` to `ConfigKey` enum**

Note: `IntegerRangeSpec` already exists at line 988 of `keys.rs`. No new spec type needed.

Add the variant to the enum (in the `// Packages settings` section):
```rust
    PackagesConcurrentDownloads,
```

Add the FromStr mapping:
```rust
"packages.concurrent_downloads" => Ok(ConfigKey::PackagesConcurrentDownloads),
```

Add the name() mapping:
```rust
ConfigKey::PackagesConcurrentDownloads => "packages.concurrent_downloads",
```

Add the get() implementation:
```rust
ConfigKey::PackagesConcurrentDownloads => config.packages.concurrent_downloads.to_string(),
```

Add the set_unchecked() implementation:
```rust
ConfigKey::PackagesConcurrentDownloads => {
    config.packages.concurrent_downloads = value.parse().unwrap();
}
```

Add the specification() (uses existing `IntegerRangeSpec`):
```rust
ConfigKey::PackagesConcurrentDownloads => Box::new(IntegerRangeSpec::new(1, 10)),
```

- [ ] **Step 4: Add INI parsing in `parser.rs`**

In the `[packages]` section parsing (after `temp_dir` parsing):

```rust
        if let Some(v) = section.get("concurrent_downloads") {
            if let Ok(n) = v.trim().parse::<usize>() {
                config.packages.concurrent_downloads = n.clamp(1, 10);
            }
        }
```

- [ ] **Step 5: Verify compilation and run config tests**

Run: `cargo test -p xearthlayer config:: --lib`
Expected: All pass

- [ ] **Step 6: Commit**

```
feat(config): add packages.concurrent_downloads setting

Configurable concurrent download count for package parts (1-10,
default 5). Uses existing IntegerRangeSpec for bounded validation.
```

---

## Task 2: Progress Types — `PartState`, `PartProgress`, `DownloadProgress`

**Files:**
- Modify: `xearthlayer/src/manager/download/progress.rs`

- [ ] **Step 1: Write tests for new types**

Add to the existing test module in `progress.rs`:

```rust
#[test]
fn test_part_state_default_is_queued() {
    let state = PartState::default();
    assert!(matches!(state, PartState::Queued));
}

#[test]
fn test_download_progress_total_bytes_none_when_any_unknown() {
    let progress = DownloadProgress {
        parts: vec![
            PartProgress {
                index: 0,
                filename: "part.aa".to_string(),
                bytes_downloaded: 100,
                total_bytes: Some(200),
                state: PartState::Downloading,
            },
            PartProgress {
                index: 1,
                filename: "part.ab".to_string(),
                bytes_downloaded: 0,
                total_bytes: None,
                state: PartState::Queued,
            },
        ],
        total_bytes_downloaded: 100,
        total_bytes: None,
    };
    assert!(progress.total_bytes.is_none());
}
```

- [ ] **Step 2: Add the new types above `MultiPartProgressCallback`**

```rust
/// State of an individual download part.
#[derive(Debug, Clone, Default)]
pub enum PartState {
    /// Waiting to start.
    #[default]
    Queued,
    /// Actively downloading.
    Downloading,
    /// Download complete, checksum verified.
    Done,
    /// Download failed.
    Failed { reason: String, attempt: u8 },
    /// Retrying after failure.
    Retrying { attempt: u8 },
}

/// Progress snapshot for a single download part.
#[derive(Debug, Clone)]
pub struct PartProgress {
    pub index: usize,
    pub filename: String,
    pub bytes_downloaded: u64,
    pub total_bytes: Option<u64>,
    pub state: PartState,
}

/// Aggregate download progress snapshot with per-part detail.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub parts: Vec<PartProgress>,
    pub total_bytes_downloaded: u64,
    pub total_bytes: Option<u64>,
}

/// Callback invoked with download progress snapshots.
pub type DownloadProgressCallback = Box<dyn Fn(&DownloadProgress) + Send + Sync>;
```

Keep `MultiPartProgressCallback` for now — it will be removed in Task 7 when all consumers are migrated.

Also in this task: remove the dead `DownloadProgress` struct from `manager/traits.rs` (line 44, marked `#[allow(dead_code)]`) to avoid name collision with the new type.

- [ ] **Step 3: Run tests**

Run: `cargo test -p xearthlayer --lib test_part_state test_download_progress`
Expected: Pass

- [ ] **Step 4: Commit**

```
feat(manager/download): add per-part progress types

PartState, PartProgress, DownloadProgress, and DownloadProgressCallback
for per-part download tracking. Foundation for indicatif progress bars.
```

---

## Task 3: Extend `ProgressCounters` with Per-Part State

**Files:**
- Modify: `xearthlayer/src/manager/download/progress.rs`

- [ ] **Step 1: Write tests for extended counters**

```rust
#[test]
fn test_counters_track_per_part_state() {
    let counters = ProgressCounters::new(
        3,
        vec!["a.aa".into(), "a.ab".into(), "a.ac".into()],
        vec![Some(100), Some(200), None],
    );
    assert_eq!(counters.part_state(0), 0); // Queued
    counters.set_part_state(0, 1); // Downloading
    assert_eq!(counters.part_state(0), 1);
    counters.set_part_state(0, 2); // Done
    assert_eq!(counters.part_state(0), 2);
}

#[test]
fn test_counters_store_error_reason() {
    let counters = ProgressCounters::new(
        2,
        vec!["a.aa".into(), "a.ab".into()],
        vec![Some(100), Some(200)],
    );
    counters.set_part_error(0, "connection timeout".to_string());
    assert_eq!(counters.part_error(0), Some("connection timeout".to_string()));
    assert_eq!(counters.part_error(1), None);
}

#[test]
fn test_counters_build_snapshot() {
    let counters = ProgressCounters::new(
        2,
        vec!["a.aa".into(), "a.ab".into()],
        vec![Some(100), Some(200)],
    );
    counters.set_part_state(0, 1); // Downloading
    counters.update_part(0, 50);
    let snapshot = counters.build_snapshot();
    assert_eq!(snapshot.parts.len(), 2);
    assert_eq!(snapshot.parts[0].bytes_downloaded, 50);
    assert_eq!(snapshot.parts[0].total_bytes, Some(100));
    assert!(matches!(snapshot.parts[0].state, PartState::Downloading));
    assert!(matches!(snapshot.parts[1].state, PartState::Queued));
    assert_eq!(snapshot.total_bytes_downloaded, 50);
    assert_eq!(snapshot.total_bytes, Some(300));
}
```

- [ ] **Step 2: Extend `ProgressCounters`**

Refactor the `ProgressCounters` struct. The current implementation has `bytes_per_part`, `total_size`, `parts_completed`, and `done` fields. Replace with:

```rust
pub struct ProgressCounters {
    /// Per-part bytes downloaded.
    part_bytes: Vec<AtomicU64>,
    /// Per-part total bytes (0 = unknown).
    part_totals: Vec<AtomicU64>,
    /// Per-part state (0=Queued, 1=Downloading, 2=Done, 3=Failed, 4=Retrying).
    part_states: Vec<AtomicU8>,
    /// Per-part retry attempt count.
    part_attempts: Vec<AtomicU8>,
    /// Per-part error reasons (written on failure only).
    part_errors: Vec<Mutex<Option<String>>>,
    /// Part filenames (immutable).
    filenames: Vec<String>,
    /// Signal for reporter thread to stop.
    done: Arc<AtomicBool>,
}
```

Add methods: `new_extended(count, filenames, totals)` (new constructor — keep existing `new(count)` for backward compat until Task 7), `update_part(index, bytes)`, `set_part_state(index, state_u8)`, `part_state(index)`, `set_part_error(index, reason)`, `part_error(index)`, `set_part_attempt(index, attempt)`, `set_done()`, `is_done()`, `build_snapshot() -> DownloadProgress`.

**IMPORTANT:** Keep the existing `ProgressCounters::new(num_parts)` constructor and all existing methods working so `ParallelStrategy` (unchanged until Task 7) still compiles. The new constructor `new_extended()` is used by the rewritten strategy in Task 7.

The `build_snapshot()` method reads all atomics and mutexes to produce a `DownloadProgress`:
- Maps state u8 to `PartState` enum (0=Queued, 1=Downloading, 2=Done, 3=Failed, 4=Retrying)
- For state 3 (Failed), reads the corresponding `part_errors` mutex
- Computes `total_bytes` as `Some(sum)` only if all `part_totals > 0`
- Derive `PartialEq` on `PartState` for cleaner test assertions

Add a test for the Failed state with error reason:
```rust
#[test]
fn test_counters_build_snapshot_with_failed_state() {
    let counters = ProgressCounters::new_extended(
        2,
        vec!["a.aa".into(), "a.ab".into()],
        vec![Some(100), Some(200)],
    );
    counters.set_part_state(1, 3); // Failed
    counters.set_part_error(1, "connection timeout".to_string());
    let snapshot = counters.build_snapshot();
    match &snapshot.parts[1].state {
        PartState::Failed { reason, .. } => assert_eq!(reason, "connection timeout"),
        other => panic!("Expected Failed, got {:?}", other),
    }
}
```

- [ ] **Step 3: Add new reporter method alongside existing one**

Add `ProgressReporter::start_detailed()` that accepts `Arc<DownloadProgressCallback>` and calls `counters.build_snapshot()`. Keep existing `start()` / `start_default()` methods unchanged until Task 7.

Also: remove `#[cfg(test)]` gate from `ProgressReporter::stop()` if present — it will be needed in production code in Task 7.

- [ ] **Step 4: Run tests**

Run: `cargo test -p xearthlayer --lib test_counters`
Expected: All pass

- [ ] **Step 5: Commit**

```
feat(manager/download): extend ProgressCounters with per-part state

Atomic per-part state tracking, error reasons via Mutex, retry counts,
and build_snapshot() for constructing DownloadProgress snapshots.
```

---

## Task 4: Counting Semaphore

**Files:**
- Create: `xearthlayer/src/manager/download/semaphore.rs`
- Modify: `xearthlayer/src/manager/download/mod.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn test_semaphore_limits_concurrency() {
        let sem = Arc::new(CountingSemaphore::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..6).map(|_| {
            let sem = sem.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            thread::spawn(move || {
                let _permit = sem.acquire();
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                active.fetch_sub(1, Ordering::SeqCst);
            })
        }).collect();

        for h in handles { h.join().unwrap(); }
        assert!(max_active.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn test_semaphore_drop_releases_permit() {
        let sem = Arc::new(CountingSemaphore::new(1));
        {
            let _permit = sem.acquire();
            // permit held
        }
        // permit dropped — should be able to acquire again immediately
        let start = Instant::now();
        let _permit = sem.acquire();
        assert!(start.elapsed() < Duration::from_millis(100)); // generous tolerance for CI
    }
}
```

- [ ] **Step 2: Implement `CountingSemaphore`**

```rust
use std::sync::{Condvar, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct CountingSemaphore {
    count: Mutex<usize>,
    condvar: Condvar,
}

pub struct SemaphorePermit<'a> {
    semaphore: &'a CountingSemaphore,
}

impl CountingSemaphore {
    pub fn new(permits: usize) -> Self {
        Self {
            count: Mutex::new(permits),
            condvar: Condvar::new(),
        }
    }

    pub fn acquire(&self) -> SemaphorePermit<'_> {
        let mut count = self.count.lock().unwrap();
        while *count == 0 {
            count = self.condvar.wait(count).unwrap();
        }
        *count -= 1;
        SemaphorePermit { semaphore: self }
    }

    fn release(&self) {
        let mut count = self.count.lock().unwrap();
        *count += 1;
        self.condvar.notify_one();
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}
```

- [ ] **Step 3: Add `mod semaphore;` to `download/mod.rs`**

- [ ] **Step 4: Run tests**

Run: `cargo test -p xearthlayer --lib semaphore`
Expected: All pass

- [ ] **Step 5: Commit**

```
feat(manager/download): add CountingSemaphore for sliding window

Condvar-based counting semaphore with RAII permit. Replaces batch
execution model for concurrent downloads.
```

---

## Task 5: RetryDownloader Decorator

**Files:**
- Create: `xearthlayer/src/manager/download/retry.rs`
- Modify: `xearthlayer/src/manager/download/mod.rs`

- [ ] **Step 1: Write tests with mock downloader**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    struct FailNTimes {
        fail_count: AtomicUsize,
        fail_until: usize,
    }

    impl FailNTimes {
        fn new(fail_until: usize) -> Self {
            Self {
                fail_count: AtomicUsize::new(0),
                fail_until,
            }
        }
    }

    impl PackageDownloader for FailNTimes {
        fn download(&self, _url: &str, dest: &Path, _checksum: Option<&str>) -> ManagerResult<u64> {
            let count = self.fail_count.fetch_add(1, Ordering::SeqCst);
            if count < self.fail_until {
                Err(ManagerError::DownloadFailed {
                    url: "test".into(),
                    reason: "simulated failure".into(),
                })
            } else {
                std::fs::write(dest, b"ok").unwrap();
                Ok(2)
            }
        }

        fn download_with_progress(&self, url: &str, dest: &Path, checksum: Option<&str>, _cb: ProgressCallback) -> ManagerResult<u64> {
            self.download(url, dest, checksum)
        }
    }

    #[test]
    fn test_retry_succeeds_after_failures() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("test.bin");
        let inner = FailNTimes::new(2); // Fails twice, succeeds on 3rd
        let retry = RetryDownloader::new(inner);
        let result = retry.download("http://test", &dest, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_retry_exhausted() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("test.bin");
        let inner = FailNTimes::new(10); // Always fails
        let retry = RetryDownloader::new(inner);
        let result = retry.download("http://test", &dest, None);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Implement `RetryDownloader`**

```rust
use std::path::Path;
use std::thread;
use std::time::Duration;
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::traits::{PackageDownloader, ProgressCallback};

const MAX_RETRIES: u8 = 3;
const BASE_DELAY_MS: u64 = 2000;

pub struct RetryDownloader<D: PackageDownloader> {
    inner: D,
}

impl<D: PackageDownloader> RetryDownloader<D> {
    pub fn new(inner: D) -> Self {
        Self { inner }
    }

    fn should_delete_partial(err: &ManagerError) -> bool {
        matches!(err, ManagerError::ChecksumMismatch { .. })
            || matches!(err, ManagerError::DownloadFailed { reason, .. } if reason.contains("416"))
    }
}

impl<D: PackageDownloader> PackageDownloader for RetryDownloader<D> {
    fn download(&self, url: &str, dest: &Path, checksum: Option<&str>) -> ManagerResult<u64> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = BASE_DELAY_MS * 2u64.pow(attempt as u32 - 1);
                thread::sleep(Duration::from_millis(delay));
            }
            match self.inner.download(url, dest, checksum) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    if Self::should_delete_partial(&e) {
                        let _ = std::fs::remove_file(dest);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap())
    }

    fn download_with_progress(&self, url: &str, dest: &Path, checksum: Option<&str>, _cb: ProgressCallback) -> ManagerResult<u64> {
        // Note: ParallelStrategy manages progress via ProgressCounters, not via
        // this callback. The retry decorator delegates to inner.download() which
        // already supports resume. Per-part progress updates flow through the
        // ProgressCounters atomics in the strategy layer, not through this trait method.
        self.download(url, dest, checksum)
    }
}
```

Note: `ParallelStrategy` manages progress tracking via `ProgressCounters` atomics, not through the `download_with_progress` callback. The retry decorator simply delegates to `self.download()` which supports resume via partial files. Per-part byte updates are handled by the strategy layer wrapping each download call with a counter-updating closure.

- [ ] **Step 3: Add `mod retry;` to `download/mod.rs` and export `RetryDownloader`**

- [ ] **Step 4: Run tests**

Run: `cargo test -p xearthlayer --lib retry`
Expected: All pass

- [ ] **Step 5: Commit**

```
feat(manager/download): add RetryDownloader with exponential backoff

Decorator that retries failed downloads up to 3 times with 2s/4s/8s
backoff. Deletes partial files on checksum mismatch or HTTP 416.
```

---

## Task 6: Per-Part Sizes in `DownloadState` and `query_sizes`

**Files:**
- Modify: `xearthlayer/src/manager/download/state.rs`
- Modify: `xearthlayer/src/manager/download/orchestrator.rs`

- [ ] **Step 1: Add `part_sizes` to `DownloadState`**

```rust
/// Per-part sizes from HEAD requests. None = unknown (0 from server).
pub part_sizes: Vec<Option<u64>>,
```

Initialize to `Vec::new()` in the constructor, or `vec![None; total_parts]`.

- [ ] **Step 2: Update `query_sizes()` to store per-part sizes**

In `orchestrator.rs`, the current `query_sizes()` does parallel HEAD requests and sums sizes. Modify to also store individual sizes:

```rust
// After collecting sizes from HEAD requests:
state.part_sizes = sizes.iter().map(|&s| if s > 0 { Some(s) } else { None }).collect();
```

- [ ] **Step 3: Run existing tests**

Run: `cargo test -p xearthlayer --lib "download::orchestrator\|download::state"`
Expected: All pass

- [ ] **Step 4: Commit**

```
feat(manager/download): store per-part sizes from HEAD requests

DownloadState.part_sizes populated by query_sizes() for individual
part progress tracking.
```

---

## Task 7: Sliding Window in `ParallelStrategy` + Callback Migration

**Files:**
- Modify: `xearthlayer/src/manager/download/strategy.rs`
- Modify: `xearthlayer/src/manager/download/orchestrator.rs`
- Modify: `xearthlayer/src/manager/download/mod.rs`
- Modify: `xearthlayer/src/manager/installer.rs`

This is the largest task — it replaces the batch model with sliding window, migrates all callback consumers from `MultiPartProgressCallback` to `DownloadProgressCallback`, and wires `RetryDownloader` and per-part state tracking into the parallel strategy.

- [ ] **Step 1: Update `DownloadStrategy` trait**

Change the callback parameter:

```rust
pub trait DownloadStrategy {
    fn execute(
        &self,
        state: &mut DownloadState,
        progress: Option<Arc<DownloadProgressCallback>>,
    ) -> ManagerResult<()>;
}
```

- [ ] **Step 2: Update `SequentialStrategy::execute()`**

Adapt to use the new callback type. For sequential, create a single-part `DownloadProgress` snapshot manually on each part completion.

- [ ] **Step 3: Rewrite `ParallelStrategy::execute()`**

Replace the batch loop with semaphore-based sliding window:

```rust
fn execute(&self, state: &mut DownloadState, progress: Option<Arc<DownloadProgressCallback>>) -> ManagerResult<()> {
    let counters = Arc::new(ProgressCounters::new_extended(
        state.total_parts,
        state.urls.iter().enumerate().map(|(i, _)| {
            state.destinations[i].file_name().unwrap().to_string_lossy().to_string()
        }).collect(),
        state.part_sizes.clone(),
    ));

    // Start reporter if callback provided
    let reporter = progress.map(|cb| {
        ProgressReporter::start_detailed(counters.clone(), cb)
    });

    let semaphore = Arc::new(CountingSemaphore::new(self.max_concurrent));

    let handles: Vec<_> = (0..state.total_parts).map(|i| {
        let sem = semaphore.clone();
        let counters = counters.clone();
        let url = state.urls[i].clone();
        let dest = state.destinations[i].clone();
        let checksum = state.checksums[i].clone();
        let timeout = self.timeout;

        std::thread::spawn(move || {
            let _permit = sem.acquire();
            counters.set_part_state(i, 1); // Downloading

            let downloader = HttpDownloader::with_timeout(timeout);

            // Retry loop with per-part progress updates.
            // We don't use RetryDownloader here because the ProgressCallback
            // is not Clone — instead we inline the retry logic and create a
            // fresh progress callback on each attempt (closures over Arc are cheap).
            let mut last_err = None;
            for attempt in 0..=3u8 {
                if attempt > 0 {
                    counters.set_part_state(i, 4); // Retrying
                    counters.set_part_attempt(i, attempt);
                    let delay = 2000u64 * 2u64.pow(attempt as u32 - 1);
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    counters.set_part_state(i, 1); // Downloading again
                }

                let counters_for_progress = counters.clone();
                let progress_cb: ProgressCallback = Box::new(move |downloaded, _total| {
                    counters_for_progress.update_part(i, downloaded);
                });

                match downloader.download_with_progress(&url, &dest, Some(&checksum), progress_cb) {
                    Ok(bytes) => { last_err = None; break; }
                    Err(e) => {
                        if RetryDownloader::<HttpDownloader>::should_delete_partial(&e) {
                            let _ = std::fs::remove_file(&dest);
                        }
                        last_err = Some(e);
                    }
                }
            }

            match last_err {
                None => {
                    counters.set_part_state(i, 2); // Done
                    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                    Ok((i, bytes))
                }
                Some(e) => {
                    counters.set_part_error(i, e.to_string());
                    counters.set_part_state(i, 3); // Failed
                    Err((i, e))
                }
            }
        })
    }).collect();

    // Join all threads, collect failures
    for handle in handles {
        match handle.join().unwrap() {
            Ok((_i, bytes)) => state.record_success(bytes),
            Err((i, _)) => state.record_failure(i),
        }
    }

    // Signal reporter to stop — set done flag, then drop reporter
    // (Drop impl joins the reporter thread)
    counters.set_done();
    drop(reporter);

    Ok(())
}
```

- [ ] **Step 4: Update `MultiPartDownloader::download_all()`**

Change callback parameter from `MultiPartProgressCallback` to `DownloadProgressCallback`.

- [ ] **Step 5: Update `installer.rs` callback bridge**

The installer currently wraps `MultiPartProgressCallback` to bridge to `InstallProgressCallback`. Update to construct `DownloadProgressCallback` instead:

```rust
let download_progress: Option<DownloadProgressCallback> = on_progress.as_ref().map(|cb| {
    let cb = cb.clone();
    Box::new(move |progress: &DownloadProgress| {
        let ratio = match progress.total_bytes {
            Some(total) if total > 0 => progress.total_bytes_downloaded as f64 / total as f64,
            _ => {
                let done = progress.parts.iter().filter(|p| matches!(p.state, PartState::Done)).count();
                done as f64 / progress.parts.len() as f64
            }
        };
        let msg = format!(
            "{:.1} / {:.1} MB",
            progress.total_bytes_downloaded as f64 / 1_048_576.0,
            progress.total_bytes.unwrap_or(0) as f64 / 1_048_576.0,
        );
        cb(InstallStage::Downloading, ratio, &msg);
    }) as DownloadProgressCallback
});
```

- [ ] **Step 6: Remove `MultiPartProgressCallback` type**

Delete from `progress.rs` and update `mod.rs` exports.

- [ ] **Step 7: Read config and pass to downloader**

In `installer.rs`, read `config.packages.concurrent_downloads` and pass to `MultiPartDownloader::with_settings()`.

- [ ] **Step 8: Verify compilation and run all tests**

Run: `cargo test -p xearthlayer --lib`
Expected: All pass (some download tests may need updating for new callback type)

- [ ] **Step 9: Commit**

```
feat(manager/download): sliding window downloads with new progress callback

Replace batch execution with semaphore-based sliding window. Migrate
all callback consumers from MultiPartProgressCallback to
DownloadProgressCallback. Wire RetryDownloader and per-part state
tracking. Read concurrent_downloads from config.
```

---

## Task 8: indicatif Progress UI

**Files:**
- Modify: `xearthlayer-cli/Cargo.toml`
- Modify: `xearthlayer-cli/src/commands/packages/services.rs`

- [ ] **Step 1: Add indicatif dependency**

In `xearthlayer-cli/Cargo.toml`:

```toml
indicatif = "0.17"
```

- [ ] **Step 2: Replace progress callback in `services.rs`**

Replace `create_progress_callback()` with a version that creates `indicatif::MultiProgress` bars for the download stage. The non-download stages (Extracting, Verifying, etc.) keep the existing simple print behavior.

```rust
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use xearthlayer::manager::download::progress::{DownloadProgress, PartState};

fn create_install_progress(
    &self,
    parts: &[ArchivePart],
) -> (MultiProgress, InstallProgressCallback) {
    let mp = MultiProgress::new();

    // Create per-part bars
    let style = ProgressStyle::with_template(
        "  {prefix:.dim} [{bar:20.cyan/dim}] {percent:>3}% {binary_bytes_per_sec:>12}"
    ).unwrap().progress_chars("##-");

    let queued_style = ProgressStyle::with_template("  {prefix:.dim} {msg}").unwrap();
    let done_style = ProgressStyle::with_template("  {prefix:.green} {msg:.green}").unwrap();
    let fail_style = ProgressStyle::with_template("  {prefix:.red} {msg:.red}").unwrap();

    let bars: Vec<ProgressBar> = parts.iter().map(|part| {
        let bar = mp.add(ProgressBar::new(0));
        bar.set_style(queued_style.clone());
        bar.set_prefix(part.filename.clone());
        bar.set_message("(queued)");
        bar
    }).collect();

    let footer = mp.add(ProgressBar::new(0));
    footer.set_style(
        ProgressStyle::with_template(
            "\n  Total: [{bar:20.green/dim}] {percent:>3}% {binary_bytes_per_sec:>12}"
        ).unwrap().progress_chars("##-")
    );

    let bars = Arc::new(bars);
    let footer = Arc::new(footer);

    // ... build InstallProgressCallback that delegates download stage
    // to indicatif, and other stages to simple print
}
```

The callback switches behavior based on `InstallStage`:
- `Downloading` — update indicatif bars (this requires the `DownloadProgressCallback` to be wired separately from the `InstallProgressCallback`, or a shared reference to the bars)
- Other stages — print simple single-line progress

Note: The exact wiring depends on how `PackageInstaller` exposes the download callback. The installer may need a separate `set_download_progress()` method that accepts `DownloadProgressCallback`, or the CLI constructs the `DownloadProgressCallback` and passes it alongside `InstallProgressCallback`.

The implementer should read the current `installer.rs` flow carefully and choose the cleanest integration point.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p xearthlayer-cli`
Expected: Compiles

- [ ] **Step 4: Manual test**

Run a package install or update to verify the progress bars render correctly.

- [ ] **Step 5: Commit**

```
feat(cli): add indicatif multi-progress bars for package downloads

Per-part progress bars showing download %, speed, and state (queued,
downloading, done, failed, retrying). Replaces single-line progress.
```

---

## Task 9: Pre-Commit Verification and Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/configuration.md`

- [ ] **Step 1: Run `make pre-commit`**

Fix any issues.

- [ ] **Step 2: Update `docs/configuration.md`**

Add `concurrent_downloads` to the `[packages]` section documentation:

```
| `concurrent_downloads` | Integer | `5` | Number of concurrent part downloads (1-10) |
```

- [ ] **Step 3: Update `CLAUDE.md`**

Add the new config key to the Configuration section.

- [ ] **Step 4: Commit**

```
docs: add concurrent_downloads to configuration reference
```

---

## Task 10: Manual Validation

- [ ] **Step 1: Test with `concurrent_downloads = 1`** — verify sequential behavior preserved
- [ ] **Step 2: Test with `concurrent_downloads = 5`** — verify parallel bars render
- [ ] **Step 3: Test a large package install** — verify all parts download, checksums pass, extraction works
- [ ] **Step 4: Test network interruption** — kill network mid-download, verify retry behavior
