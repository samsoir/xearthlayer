# XEarthLayer Package Manager Design

This document describes the design of the XEarthLayer Package Manager component.

## Overview

The Package Manager is responsible for discovering, downloading, installing, updating, and removing XEarthLayer Scenery Packages on the user's system. It provides both library and CLI interfaces.

## Responsibilities

1. **Discover** available packages from remote library
2. **Download** packages with resume and parallel support
3. **Install** packages to local storage
4. **Update** packages when new versions are available
5. **Remove** packages from the system
6. **Mount** ortho packages via FUSE (coordinate with existing mount system)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Package Manager                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐   │
│  │   Library    │  │   Package    │  │      Download        │   │
│  │   Client     │  │   Scanner    │  │      Manager         │   │
│  │              │  │              │  │                      │   │
│  │ Fetch remote │  │ Scan local   │  │ Parallel downloads   │   │
│  │ library idx  │  │ packages     │  │ Resume support       │   │
│  │ Parse meta   │  │ Build state  │  │ Checksum verify      │   │
│  └──────────────┘  └──────────────┘  └──────────────────────┘   │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐   │
│  │   Installer  │  │   Mount      │  │      Update          │   │
│  │              │  │   Coordinator│  │      Checker         │   │
│  │ Extract pkg  │  │              │  │                      │   │
│  │ Create links │  │ Track mounts │  │ Compare local/remote │   │
│  │ Verify files │  │ Start/stop   │  │ Notify user          │   │
│  └──────────────┘  └──────────────┘  └──────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Configuration

Settings in `~/.xearthlayer/config.ini`:

```ini
[packages]
# URL to the package library index
library_url = https://packages.xearthlayer.org/xearthlayer_package_library.txt

# Local directory for package storage
install_location = ~/.xearthlayer/packages

# X-Plane Custom Scenery path for overlay symlinks
custom_scenery_path = /path/to/X-Plane 12/Custom Scenery

# Automatically install matching overlay when installing ortho
auto_install_overlays = true

# Temporary directory for downloads
temp_dir = /tmp/xearthlayer
```

## Local Package Discovery

The Manager discovers installed packages by scanning the `install_location` directory for `xearthlayer_scenery_package.txt` files.

### Scan Process

1. List directories in `install_location`
2. For each directory, check for `xearthlayer_scenery_package.txt`
3. Parse metadata file to extract version and package info
4. Build in-memory state of installed packages

### Package State

```rust
pub struct InstalledPackage {
    /// Region identifier (e.g., "EUROPE")
    pub region: String,
    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,
    /// Installed version
    pub version: Version,
    /// Path to package directory
    pub path: PathBuf,
    /// Whether currently mounted (ortho only)
    pub mounted: bool,
}
```

No persistent index file - state is always derived from scanning. This ensures:
- Single source of truth (the actual files)
- Self-healing if user manually modifies packages
- No sync issues between index and reality

## Remote Library Fetching

### Process

1. Fetch `xearthlayer_package_library.txt` from `library_root`
2. Parse library index
3. Check the specification version: a MAJOR above `SUPPORTED_SPEC_MAJOR` (`package/spec.rs`) is refused with an instruction to upgrade XEarthLayer. The same check applies when a package's metadata file is fetched
4. Compare sequence number with cached value for quick change detection
5. Build available packages list

### Caching

- Cache library index locally with timestamp
- Re-fetch if older than configurable TTL (default: 1 hour)
- Force refresh available via CLI flag

## Update Detection

Compare installed packages against remote library:

```rust
pub struct UpdateAvailable {
    pub region: String,
    pub package_type: PackageType,
    pub installed_version: Version,
    pub available_version: Version,
}
```

User is notified of available updates on first run of XEarthLayer (not repeatedly).

## Download Manager

### Features

- **Resumable downloads**: Track progress, resume interrupted downloads via HTTP Range headers
- **Parallel parts**: Sliding-window concurrency with semaphore-gated execution
- **Configurable concurrency**: `packages.concurrent_downloads` (1-10, default 5)
- **Per-part retry**: 3 retries with exponential backoff (2s, 4s, 8s) via `RetryDownloader` decorator
- **Per-part progress**: `DownloadProgress` snapshot with per-part `PartState` (Queued, Downloading, Done, Failed, Retrying)
- **Verification**: SHA-256 checksum each part before proceeding
- **Progress UI**: `indicatif::MultiProgress` bars in CLI (library layer is UI-agnostic)

### Download State

For interrupted downloads, maintain state file:

```
~/.xearthlayer/packages/.downloads/
├── zzXEL_na-1.0.0.download_state
└── zzXEL_na-1.0.0.tar.gz.aa  (partial)
```

State file format (JSON for simplicity):
```json
{
  "package": "zzXEL_na_ortho",
  "version": "1.0.0",
  "total_parts": 5,
  "completed_parts": ["aa", "ab"],
  "current_part": "ac",
  "current_part_bytes": 524288000,
  "started_at": "2025-12-20T10:30:00Z"
}
```

### HTTP Range Requests

For resume support, use HTTP Range header:
```
Range: bytes=524288000-
```

Most CDNs support this. Fall back to full re-download if not supported.

### Sliding Window Execution

`ParallelStrategy` uses a semaphore-based sliding window instead of batch-based parallelism. All download threads are spawned immediately; each acquires a semaphore permit before downloading. When a fast part completes, the next queued part starts immediately -- no waiting for the slowest part in a batch.

### Per-Part Progress Architecture

```
ParallelStrategy (semaphore-gated threads)
  --> ProgressCounters (atomic per-part bytes + state)
      --> ProgressReporter (100ms poll interval)
          --> DownloadProgressCallback(&DownloadProgress)
              --> CLI: indicatif MultiProgress bars
```

`ProgressCounters` uses `AtomicU8` for per-part state (lock-free hot path) and `Mutex<Option<String>>` for error reasons (cold path, only written on failure). Per-part sizes are populated from HEAD requests at construction time so the UI shows accurate totals from the start.

### Retry Policy

Per-part retry via `RetryDownloader` decorator wrapping `HttpDownloader`:
- **Max retries:** 3 (hardcoded)
- **Backoff:** Exponential -- 2s, 4s, 8s
- **Resume on retry:** Existing `download_with_resume()` handles Range-based resume
- **HTTP 416:** Delete partial file before retrying to avoid infinite loop
- **Checksum mismatch:** Delete partial file and retry from scratch
- **Scope:** Per-part only. A failing part does not affect other downloads

## Installation Process

Package installs and removes run through `InstallQueue`
(`manager/queue/`), a live queue that decouples the two halves of an install.
Downloading is network bound and installing is disk bound, so downloads run
ahead of installs for as long as the disk has room (#292). Callers submit jobs
and watch events; `install_batch` is the blocking entry the CLI uses.

### Per package

1. **Prepare.** Validate the title and every archive and part file name (each
   must be a single plain path component, since all of them come from metadata
   fetched over HTTP and are joined onto paths). Refuse a version that is
   already installed. Create the package's temp directory under an
   `InstallTempGuard`, then size its parts with HEAD requests.
2. **Download** (network bound). Fetch every part into the temp directory,
   verifying checksums as parts arrive.
3. **Install** (disk bound). Reassemble the parts into one archive and free the
   parts (#260), extract the `tar.gz` in process with the `tar` crate over
   flate2's `zlib-rs` backend (#274), stage the extracted tree beside its
   destination, then swap it in by renaming any old version aside first
   (`manager/aside.rs`). Dropping the guard removes the temp directory on every
   exit path.

### Install queue

**Threads.** One downloader thread takes install jobs in submission order and
downloads one package at a time (its parts stay parallel). One installer thread
runs the ready jobs one at a time, in the order they became ready. Packages
never install in parallel: they would only compete for the same disk. A
failure at any stage is that job's result alone, and a stage that panics fails
only its own job (`ManagerError::Panicked`).

**Stages.** The `Stages` trait is the seam between the queue and the work:
`prepare`, `download_size`, `download`, `install`, `remove`, `available_bytes`,
plus the temp directory and packages root. `PackageInstaller` implements it for
production; tests use a gated fake. `install` receives an `InstallContext`
with `begin_swap` and `end_swap`.

**Disk ledger.** Finished archives wait on disk for their turn, so
`DiskLedger` admits a package to download only if, with `s` the download sizes
of the admitted packages in install order and `SPACE_BUFFER_MULTIPLIER` (4) the
install peak factor,

```
max over j of  4*s[j] + sum(s[i] for i after j)  <=  capacity
```

holds for every admitted `j`. Packages ahead of `j` have left the disk by the
time it installs, and the archives queued behind it are complete. The ledger
answers one of three ways:

| Answer | Meaning | Job |
|---|---|---|
| `Admitted` | It fits | Downloads |
| `Wait { needed, available }` | It fits once packages ahead have installed | Emits `WaitingForSpace`, waits on a condvar, retried on every release |
| `Never { required }` | `4 * s` alone exceeds the capacity | Fails with `InsufficientDiskSpace`; the downloader moves on |

The check is made at admission, so the installer never waits for space and the
two threads cannot deadlock. An entry is released when its job installs, fails
or is cancelled. Removes never touch the ledger: the space they free returns to
the packages directory, which may be another filesystem.

**Capacity** is the free space of the temp filesystem, read once per busy
period (when the first job asks to download after the queue was empty) and never
mid-install, because it would then count a half-extracted tree as used.
Writers other than the queue are unseen until the next busy period.

**Removes** skip the downloader and go straight to the install queue, so a
remove can overtake a download that is waiting for space. `submit` refuses a
second unfinished job for the same region and package type
(`ManagerError::AlreadyQueued`), which keeps that overtaking safe and guards a
UI against double selection. It also refuses once the queue is shutting down or
abandoned (`QueueClosed`). `submit_batch` submits many installs under one lock
and `finish_batch` waits for them, so a cancel sees all of a batch or none of it.

**Cancellation.** Every job has a `CancelToken`, checked per buffer by the HTTP
download loop and per tar entry by the extractor.

| Cancelled while | Result |
|---|---|
| Queued, or downloaded and waiting | Removed from the queue immediately. A downloaded package is marked `Cancelling`, its temp directory is deleted outside the state lock, and only then is the job finished and its ledger entry released, so a waiter that returns finds the directory gone. `abandon` finishes it at once instead |
| Preparing, waiting for space, downloading | The token is raised; the owning thread notices, aborts every part worker, and the temp guard deletes the parts before the job finishes |
| Reassembling or extracting | Checked per buffer and per tar entry; the guard deletes the partial tree; the old version is untouched |
| Staging | As above; the staging folder is deleted |
| The swap | `TooLate`; the swap runs to completion |
| A remove that has started | `TooLate` |

The swap is two renames in the destination's parent directory: the old version
is set aside to `.xel-aside-<name>`, then the staged tree (`.xel-staging-<name>`,
copied there first when the temp directory is on another filesystem) is renamed
in. `begin_swap` claims it; `end_swap` marks the renames done, and the old
version is deleted after that, outside the uncancellable part. A package folder
is therefore always either the old version or the new one. Dot-prefixed names
are skipped by `LocalPackageStore::list`.

**Shutdown.** Dropping the queue cancels everything, lets a running swap
finish, and joins both threads. `Canceller` (from `canceller()`) is a cloneable
handle for signal handlers: `cancel_all` cancels every job in one critical
section, and `abandon` additionally stops waiters from waiting for anything but
a running swap and stops the drop from joining, because there may be no time for
cleanup. A live `Canceller` keeps the queue's lock held.

**Lock and sweep.** Starting a queue takes an exclusive `flock` on
`{temp_dir}/.queue.lock` for the queue's life (`ManagerError::QueueBusy` if
another process holds it); the kernel drops it however the process exits. Under
that lock, `sweep` deletes leftover `install_*` temp directories,
`.xel-staging-*` folders and `.xel-removed-*` folders, and renames a
`.xel-aside-*` folder back when its package folder is missing (the process died
between a swap's two renames, so the aside is the only copy), deleting it
otherwise. A remove never uses the aside name: it renames the package straight
to `.xel-removed-<name>` and deletes it there. That delete can take minutes, and
a tree it was part way through must never become a package again, so a removed
folder is always deleted and never renamed back.

**Observer.** `QueueObserver::on_event(id, &JobEvent)` receives `Queued`,
`Preparing`, `WaitingForSpace`, `Downloading`, `Downloaded`, `Installing`,
`Removing` and `Finished(&JobOutcome)`. Events are borrowed, so errors need not
be `Clone`. The observer is `Send + Sync` and is called from the submitter, the
downloader and the installer **while the queue's state lock is held**: it must
not block and must never call back into the queue. A panicking observer is
logged and ignored. The CLI's `ConsoleQueueObserver` shows one line per
unfinished job above the part bars of the download.

**CLI signals.** `xearthlayer-cli/src/signals.rs` routes SIGINT, SIGTERM and
SIGHUP to the running queue's `Canceller`. A first SIGINT cancels and waits for
cleanup; a second SIGINT, SIGTERM or SIGHUP abandons. The stop request is
remembered across queues, so a command that runs one queue per package (remove)
does not start the next after the user asked to stop. After SIGHUP nothing more
is written to the terminal. The exit code is 128 plus the signal number of the
first signal received (130, 143, 129). With no queue running, a signal gets its
default action.

### Overlays

The CLI plans the whole batch before it starts: each selected package, followed
by its region's overlay when `packages.auto_install_overlays` is on. Packages
succeed or fail independently, so an overlay installs even if its ortho package
fails. After a batch that installed any overlay, the CLI rebuilds the
consolidated `yzXEL_overlay/` folder once.

### Symlink Considerations

- Check filesystem supports symlinks (warn on FAT32/exFAT)
- Handle existing directory/symlink at target
- Provide manual copy fallback if symlinks unavailable

## Mount Coordination

For ortho packages, coordinate with XEarthLayer FUSE system.

### Multi-Mount Strategy

Each installed ortho package gets its own FUSE mount:

```
Custom Scenery/
├── zzXEL_afr_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_afr_ortho/
├── zzXEL_eur_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_eur_ortho/
└── zzXEL_na_ortho/   → FUSE mount → ~/.xearthlayer/packages/zzXEL_na_ortho/
```

### Mount State Tracking

Track active mounts for clean shutdown:

```rust
pub struct MountState {
    pub package: String,
    pub source_path: PathBuf,
    pub mount_point: PathBuf,
    pub session: BackgroundSession,
}
```

### Lifecycle

1. **Start**: Iterate installed ortho packages, mount each
2. **Runtime**: Handle mount failures gracefully (log, continue with others)
3. **Shutdown**: Unmount all in reverse order, wait for clean unmount

## Package Removal

Removal is a `Job::Remove` in the same queue, so it takes the same lock as an
install. The folder is renamed to `.xel-removed-<name>` before it is deleted,
so a remove interrupted mid-delete leaves nothing the sweep would restore.

### Ortho Packages

1. Check if mounted - unmount first if so
2. Remove mount point directory from Custom Scenery
3. Delete package directory from `install_location`

### Overlay Packages

1. Remove symlink from Custom Scenery
2. Delete package directory from `install_location`

### Partial Removal

If removal fails partway:
- Log what was removed
- Attempt to restore consistent state
- Report to user what manual cleanup may be needed

## CLI Interface

### Commands

```bash
# List available and installed packages
xearthlayer packages list

# Check for updates
xearthlayer packages check

# Install a package
xearthlayer packages install <region> [--type ortho|overlay|all]

# Update packages
xearthlayer packages update [<region>] [--all]

# Remove a package
xearthlayer packages remove <region> [--type ortho|overlay|all]

# Show package details
xearthlayer packages info <region>
```

### Output Examples

```
$ xearthlayer packages list

Available Packages:
  AFRICA        ortho: 1.0.0    overlay: 1.0.0
  ANTARCTICA    ortho: 1.0.0    overlay: 1.0.0
  ASIA          ortho: 1.0.0    overlay: 1.0.0
  AUSTRALIA     ortho: 1.0.0    overlay: 1.0.0
  EUROPE        ortho: 1.2.0    overlay: 1.1.0
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0
  SOUTH AMERICA ortho: 1.0.0    overlay: 1.0.0

Installed:
  EUROPE        ortho: 1.0.0 [update available: 1.2.0]
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0

$ xearthlayer packages install europe --type ortho

Downloading EUROPE ortho package (v1.2.0)...
  Part 1/5: zzXEL_eur-1.2.0.tar.gz.aa [################] 100% (1.0 GB)
  Part 2/5: zzXEL_eur-1.2.0.tar.gz.ab [########        ]  50% (512 MB)
  ...

Verifying checksums... OK
Extracting... OK
Registering for FUSE mount... OK

EUROPE ortho package installed successfully.
Run 'xearthlayer start' to begin serving scenery.
```

## Error Handling

### Network Errors

- Retry transient failures with exponential backoff
- Resume from last successful byte on connection drop
- Clear error message if server unreachable

### Disk Space

- The `DiskLedger` admits each download against the free space (see Install queue)
- Estimate required space (archive + extracted) as `SPACE_BUFFER_MULTIPLIER` times the download
- A package that cannot fit fails before downloading, with a clear message; the rest of the batch continues

### Checksum Failures

- Re-download failed part (up to 3 attempts)
- If persistent, report corruption and suggest re-download
- Never proceed with failed checksum

### Mount Failures

- Log detailed error
- Continue mounting other packages
- Report which packages failed at end

## Library Interface

The Package Manager exposes a library API for use by CLI and future GUI:

```rust
pub trait PackageManager {
    /// Scan local packages and return installed state
    fn scan_installed(&self) -> Result<Vec<InstalledPackage>, PackageError>;

    /// Fetch and parse remote library
    fn fetch_library(&self) -> Result<PackageLibrary, PackageError>;

    /// Check for available updates
    fn check_updates(&self) -> Result<Vec<UpdateAvailable>, PackageError>;

    /// Download and install a package
    fn install(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Update a package to latest version
    fn update(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Remove an installed package
    fn remove(&self, region: &str, package_type: PackageType) -> Result<(), PackageError>;
}
```

## Security Considerations

- **HTTPS only**: All downloads over TLS
- **Checksum verification**: SHA-256 for all parts
- **No arbitrary code execution**: Packages contain only data files
- **Symlink safety**: Validate symlink targets are within expected paths
