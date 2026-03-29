# Parallel Package Downloads with Progress UI

**Date:** 2026-03-24
**Status:** Draft
**Issue:** #115

## Problem

Package installation downloads archive parts sequentially with minimal progress feedback — a single line showing aggregate percentage. For packages with 10+ parts at 500MB each, this means long waits with poor visibility into what's happening.

The parallel download infrastructure already exists (`ParallelStrategy`, `ProgressCounters`, background `ProgressReporter`), but:
1. Concurrency is hardcoded at 4 — not user-configurable
2. The progress callback only reports aggregate totals — no per-part breakdown
3. The CLI renders a single `\r` line — no per-part progress bars

## Solution

1. Add `packages.concurrent_downloads` config key (default 5, range 1-10)
2. Enhance the progress callback to include per-part state
3. Render per-part progress bars using `indicatif::MultiProgress` in the CLI

## Configuration

New key in `[packages]` section of `config.ini`:

```ini
[packages]
concurrent_downloads = 5   # Number of concurrent part downloads (1-10, default: 5)
```

- Added to `ConfigKey` enum as `PackagesConcurrentDownloads`
- Validated: integer, range 1-10
- Read by `PackageInstaller` during construction from config
- Passed to `MultiPartDownloader` via existing `with_parallel_downloads()` API

## Enhanced Progress Callback

### Current State

```rust
pub type MultiPartProgressCallback = Box<dyn Fn(u64, u64, usize, usize) + Send + Sync>;
// (bytes_downloaded, total_bytes, parts_completed, total_parts)
```

Aggregate only — the UI can show a percentage but nothing about individual parts.

### Migration

`MultiPartProgressCallback` is replaced by `DownloadProgressCallback`. All consumers are updated in a single pass:

| Location | Current | New |
|----------|---------|-----|
| `DownloadStrategy::execute()` | `Option<Arc<MultiPartProgressCallback>>` | `Option<Arc<DownloadProgressCallback>>` |
| `ProgressReporter::start()` | `Arc<MultiPartProgressCallback>` | `Arc<DownloadProgressCallback>` |
| `MultiPartDownloader::download_all()` | `Option<MultiPartProgressCallback>` | `Option<DownloadProgressCallback>` |
| `PackageInstaller` (bridge callback) | Constructs `MultiPartProgressCallback` | Constructs `DownloadProgressCallback` |

The old `MultiPartProgressCallback` type is removed — no deprecation period needed since this is an internal API with no external consumers.

### New Types

```rust
/// State of an individual download part.
#[derive(Debug, Clone)]
pub enum PartState {
    /// Waiting to start (queued behind concurrency limit).
    Queued,
    /// Actively downloading.
    Downloading,
    /// Download complete, checksum verified.
    Done,
    /// Download failed with error message. Includes retry attempt number (0-based).
    Failed { reason: String, attempt: u8 },
    /// Retrying after a failure. Includes which retry attempt this is (1-based).
    Retrying { attempt: u8 },
}

/// Progress snapshot for a single download part.
#[derive(Debug, Clone)]
pub struct PartProgress {
    /// Zero-based index in the parts list.
    pub index: usize,
    /// Filename of this part (e.g., "zzXEL_eu_ortho-1.0.0.tar.gz.aa").
    pub filename: String,
    /// Bytes downloaded so far.
    pub bytes_downloaded: u64,
    /// Total bytes for this part (None if HEAD request didn't return Content-Length).
    pub total_bytes: Option<u64>,
    /// Current state of this part.
    pub state: PartState,
}

/// Aggregate download progress snapshot including per-part detail.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    /// Per-part progress, ordered by part index.
    pub parts: Vec<PartProgress>,
    /// Total bytes downloaded across all parts.
    pub total_bytes_downloaded: u64,
    /// Total expected bytes across all parts (None if any part size is unknown).
    pub total_bytes: Option<u64>,
}

/// Callback invoked by the progress reporter with a snapshot of download state.
pub type DownloadProgressCallback = Box<dyn Fn(&DownloadProgress) + Send + Sync>;
```

### Data Flow

```
ParallelStrategy manages semaphore-based work queue
  │
  ├─ Spawns thread per part, acquires semaphore permit before downloading
  │   └─ Each thread updates ProgressCounters (atomic bytes + state)
  │
  └─ ProgressReporter (background thread, 100ms poll interval)
       │
       ├─ Reads ProgressCounters snapshot
       ├─ Reads error reasons from Mutex-protected Vec
       ├─ Builds DownloadProgress with per-part PartProgress entries
       └─ Invokes DownloadProgressCallback(&progress)
             │
             └─ CLI callback updates indicatif MultiProgress bars
```

### ProgressCounters Enhancement

The existing `ProgressCounters` tracks per-part byte counts via atomics. Extend with per-part state and error reasons:

```rust
pub struct ProgressCounters {
    /// Per-part bytes downloaded (indexed by part index).
    pub part_bytes: Vec<AtomicU64>,
    /// Per-part total bytes (populated from query_sizes, 0 = unknown).
    pub part_totals: Vec<AtomicU64>,
    /// Per-part state (encoded as AtomicU8: 0=Queued, 1=Downloading, 2=Done, 3=Failed, 4=Retrying).
    pub part_states: Vec<AtomicU8>,
    /// Per-part retry attempt count.
    pub part_attempts: Vec<AtomicU8>,
    /// Per-part error reasons (Mutex-protected, only written on failure).
    pub part_errors: Vec<Mutex<Option<String>>>,
    /// Part filenames (immutable after construction).
    pub filenames: Vec<String>,
}
```

The `AtomicU8` encodes the state discriminant for the fast path (polling 100ms). The `part_errors` `Mutex<Option<String>>` stores the error reason only when a part enters `Failed` state — the `ProgressReporter` reads it to build the full `PartState::Failed { reason }` in the snapshot. This hybrid approach keeps the happy path lock-free while supporting the string payload for failures.

### Per-Part Size Population

The existing `query_sizes()` method in `MultiPartDownloader` issues parallel HEAD requests to determine total download size. Currently it only stores the aggregate in `DownloadState.total_size`. Extend to store per-part sizes:

- `DownloadState` gains `part_sizes: Vec<Option<u64>>` populated by `query_sizes()`
- Note: `get_file_size()` currently returns `u64` (0 for unknown). Map `0 → None` when populating `part_sizes`.
- `ProgressCounters::new()` accepts initial per-part totals from `DownloadState.part_sizes` (copied into `part_totals` atomics for lock-free reporter access — intentional duplication)
- This means the UI shows accurate per-part totals from the start (no brief "unknown" period)

## Execution Model: Sliding Window

The current `ParallelStrategy` uses a **batch model** — spawns N threads, waits for ALL to complete, then spawns the next batch. This wastes concurrency when one part in a batch is slower than others.

Replace with a **sliding window** using a semaphore:

```rust
// Note: Rust std has no Semaphore. Use Condvar + Mutex<usize> or a lightweight
// crate. Since download threads are std::thread (blocking reqwest), not async.
let semaphore = Arc::new(Semaphore::new(max_concurrent));

// Spawn ALL threads immediately, each acquires permit before downloading
let handles: Vec<_> = parts.iter().enumerate().map(|(i, part)| {
    let sem = semaphore.clone();
    let counters = counters.clone();
    thread::spawn(move || {
        let _permit = sem.acquire(); // blocks until slot available
        counters.set_state(i, PartState::Downloading);
        // download with retry...
    })
}).collect();

// Join all
for handle in handles {
    handle.join()?;
}
```

This ensures N parts are always in-flight. When a fast part completes, the next queued part starts immediately — no waiting for the slowest part in a batch.

## Retry Policy

Per-part retry on download failure, implemented as a `RetryDownloader` decorator wrapping `HttpDownloader`:

```rust
pub struct RetryDownloader {
    inner: HttpDownloader,
    max_retries: u8,     // hardcoded: 3
    base_delay_ms: u64,  // hardcoded: 2000 (2s, 4s, 8s exponential)
}
```

- **Max retries:** 3 (hardcoded, not configurable)
- **Backoff:** Exponential — 2s, 4s, 8s
- **Scope:** Per-part only. A failing part does not affect other downloads.
- **State transitions:** `Downloading → Failed → Retrying → Downloading → ...`
- **After exhaustion:** Part marked `Failed`, installation continues. Final report shows which parts failed.
- **Resume on retry:** The existing `HttpDownloader::download_with_resume()` already handles Range-based resume from partial files. `RetryDownloader` simply calls it again — no new Range logic needed.
- **HTTP 416 handling:** If the server returns 416 (Range Not Satisfiable), the partial file is deleted before retrying to avoid an infinite retry loop.
- **Checksum mismatch:** Delete the partial file and retry from scratch.

`RetryDownloader` implements the `PackageDownloader` trait so `ParallelStrategy` can use it as a drop-in replacement for `HttpDownloader`. It updates `ProgressCounters` state and attempt count so the UI can display retry status.

### ProgressReporter Stop Condition

The existing `ProgressReporter` uses a `done: Arc<AtomicBool>` signal to stop its polling loop. This is retained. The `ParallelStrategy` sets `done = true` after all threads are joined (all parts in terminal state: `Done` or `Failed`).

## Terminal UI

### Library: indicatif

Add `indicatif` as a dependency of `xearthlayer-cli` (not the library crate). The library crate remains UI-agnostic — it only invokes the `DownloadProgressCallback`.

### Layout

```
Installing EU ortho (10 parts, 4.8 GB)

  zzXEL_eu_ortho-1.0.0.tar.gz.aa  [##########----------]  50%  12.3 MB/s
  zzXEL_eu_ortho-1.0.0.tar.gz.ab  [########------------]  40%  11.8 MB/s
  zzXEL_eu_ortho-1.0.0.tar.gz.ac  [######--------------]  30%  12.1 MB/s
  zzXEL_eu_ortho-1.0.0.tar.gz.ad  [####----------------]  20%  11.5 MB/s
  zzXEL_eu_ortho-1.0.0.tar.gz.ae  [##------------------]  10%  12.0 MB/s
  zzXEL_eu_ortho-1.0.0.tar.gz.af  (queued)
  zzXEL_eu_ortho-1.0.0.tar.gz.ag  (queued)
  ...

  Total: 2.1 / 4.8 GB  (43%)  59.7 MB/s
```

### Speed Display

Per-part download speed is **not** reported by the library. `indicatif` calculates it automatically from position updates using its `{bytes_per_sec}` or `{binary_bytes_per_sec}` template variables. The library only reports bytes downloaded; the UI derives the rate.

### Part States in UI

| State | Display |
|-------|---------|
| `Queued` | `(queued)` — grey/dim text |
| `Downloading` | `[####----] 45% 12.3 MB/s` — animated progress bar |
| `Done` | `[done]` — green, bar removed to reduce clutter |
| `Failed` | `(failed: reason)` — red text |
| `Retrying` | `(retry 2/3)` — yellow text |

### Implementation

The CLI creates the `indicatif` components and passes a callback closure:

```rust
// In packages/services.rs
fn create_download_progress_callback(
    parts: &[ArchivePart],
) -> (MultiProgress, DownloadProgressCallback) {
    let mp = MultiProgress::new();
    let bars: Vec<ProgressBar> = parts.iter().map(|part| {
        let bar = mp.add(ProgressBar::new(0)); // length set when total known
        bar.set_style(/* template with filename, bar, %, speed */);
        bar.set_message("(queued)");
        bar
    }).collect();
    let footer = mp.add(ProgressBar::new(0));
    // ... setup footer style

    let bars = Arc::new(bars);
    let footer = Arc::new(footer);

    let callback = Box::new(move |progress: &DownloadProgress| {
        for part in &progress.parts {
            let bar = &bars[part.index];
            match &part.state {
                PartState::Queued => bar.set_message("(queued)"),
                PartState::Downloading => {
                    if let Some(total) = part.total_bytes {
                        bar.set_length(total);
                    }
                    bar.set_position(part.bytes_downloaded);
                }
                PartState::Done => bar.finish_with_message("[done]"),
                PartState::Failed { reason, .. } => {
                    bar.abandon_with_message(format!("(failed: {})", reason));
                }
                PartState::Retrying { attempt } => {
                    bar.set_message(format!("(retry {}/3)", attempt));
                }
            }
        }
        // Update footer with aggregate
        if let Some(total) = progress.total_bytes {
            footer.set_length(total);
        }
        footer.set_position(progress.total_bytes_downloaded);
    });

    (mp, callback)
}
```

### Backward Compatibility

When `concurrent_downloads = 1`, the system uses `SequentialStrategy`. Both `SequentialStrategy` and `ParallelStrategy` are updated to accept the new `DownloadProgressCallback` (they share the `DownloadStrategy` trait). The progress callback still works — there's just one active part at a time. The UI renders the same layout but only one bar is ever in `Downloading` state.

## Architecture

### Separation of Concerns

```
xearthlayer-cli (UI layer)
  └─ packages/services.rs
       ├─ Creates indicatif MultiProgress + bars
       ├─ Builds DownloadProgressCallback closure over bars
       └─ Passes callback to PackageInstaller

xearthlayer (library layer — UI-agnostic)
  └─ manager/installer.rs
       ├─ Reads concurrent_downloads from config
       └─ Passes to MultiPartDownloader
            └─ download/strategy.rs (ParallelStrategy — sliding window)
                 ├─ Spawns threads, semaphore-gated concurrency
                 ├─ Each thread: RetryDownloader wraps HttpDownloader
                 └─ download/progress.rs (ProgressReporter)
                      ├─ Polls ProgressCounters every 100ms
                      └─ Invokes DownloadProgressCallback(&snapshot)
```

### Files Modified

| File | Change |
|------|--------|
| `xearthlayer/src/config/keys.rs` | Add `PackagesConcurrentDownloads` variant |
| `xearthlayer/src/config/file.rs` | Add getter with default 5 |
| `xearthlayer/src/manager/download/progress.rs` | Extend `ProgressCounters` with per-part state + errors, add `DownloadProgress` types, replace `MultiPartProgressCallback` |
| `xearthlayer/src/manager/download/strategy.rs` | Replace batch model with semaphore sliding window, integrate `RetryDownloader` |
| `xearthlayer/src/manager/download/orchestrator.rs` | Store per-part sizes from `query_sizes()` in `DownloadState` |
| `xearthlayer/src/manager/download/state.rs` | Add `part_sizes: Vec<Option<u64>>` field |
| `xearthlayer/src/manager/installer.rs` | Read config key, pass to downloader, update callback construction |

### New Files

| File | Purpose |
|------|---------|
| `xearthlayer/src/manager/download/retry.rs` | `RetryDownloader` decorator (3 retries, exponential backoff, 416 handling) |

### CLI Files Modified

| File | Change |
|------|--------|
| `xearthlayer-cli/Cargo.toml` | Add `indicatif` dependency |
| `xearthlayer-cli/src/commands/packages/services.rs` | Replace single-line progress with `MultiProgress` bars |

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Part download timeout | Retry up to 3 times with backoff, resume from partial file |
| Part checksum mismatch | Delete partial file, retry from scratch |
| Server returns 416 (Range Not Satisfiable) | Delete partial file, retry from zero |
| All retries exhausted for a part | Mark failed, continue other parts, report at end |
| All parts failed | Installation fails with summary of errors |
| Some parts failed | Installation fails — partial packages are not usable |
| Network completely down | All active parts fail → retries exhaust → installation fails |

## Testing Strategy

- **Unit tests** for `ProgressCounters` state tracking and `DownloadProgress` snapshot building
- **Unit tests** for `RetryDownloader` (mock `HttpDownloader` that fails N times then succeeds)
- **Unit tests** for `PartState` transitions and error reason storage
- **Unit tests** for sliding-window semaphore behaviour (verify N concurrent, not batched)
- **Config tests** for `PackagesConcurrentDownloads` validation (range 1-10, default 5)
- **No indicatif tests** — the UI callback is a thin rendering layer. Testing it would require terminal capture which adds complexity for low value.

## Future Considerations

- **Bandwidth limiting** — `packages.max_download_speed` to cap combined throughput
- **Part prioritization** — download parts in order so reassembly can start early (streaming extraction)
- **Graceful cancellation** — Ctrl+C handler to stop active downloads cleanly (currently threads are joined on drop)
