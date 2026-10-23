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
6. **Coordinate** with a running `xearthlayer run` (see Coordination with run)

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
│  │   Installer  │  │   Run        │  │      Update          │   │
│  │              │  │   Check      │  │      Checker         │   │
│  │ Extract pkg  │  │              │  │                      │   │
│  │ Create links │  │ Read the lock│  │ Compare local/remote │   │
│  │ Verify files │  │ Refuse edits │  │ Notify user          │   │
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

- **Retry with resume**: Track progress, and resume an interrupted part via HTTP Range headers within the same run; nothing survives a restart, since each run clears the temp folder
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
3. **Install** (disk bound). Extract the parts straight into
   `.xel-staging-<name>` beside the install path, reading them as one stream in
   process with the `tar` crate over flate2's `zlib-rs` backend (#274);
   `PartsReader` deletes each part once read (#260). Then swap the staged tree
   in by renaming any old version aside first (`manager/aside.rs`). Dropping the
   guard removes the temp directory on every exit path.

### Install queue

**Threads.** One downloader thread takes install jobs in submission order and
downloads one package at a time (its parts stay parallel). One installer thread
runs the ready jobs one at a time, in the order they became ready. Packages
never install in parallel: they would only compete for the same disk. A
failure at any stage is that job's result alone, and a stage that panics fails
only its own job (`ManagerError::Panicked`).

**Stages.** The `Stages` trait is the seam between the queue and the work:
`prepare`, `download_size`, `download`, `install`, `remove`, `capacity`,
plus the temp directory and packages root. `PackageInstaller` implements it for
production; tests use a gated fake. `install` receives an `InstallContext`
with `begin_swap` and `end_swap`.

**Disk ledger.** A package downloads its parts into the temp folder, then
extracts them straight into a staging folder on the packages filesystem,
deleting each part once read. So `DiskLedger` charges each package against two
budgets, with `s` its download size and `TREE_MULTIPLIER` (3) the estimated
expansion of its extracted tree:

- its parts, `s`, on the temp filesystem until it installs, fails or is
  cancelled;
- its tree, `3*s`, on the packages filesystem. An installed tree stays charged
  for the rest of the busy period, so a batch counts every tree it has already
  extracted; a failed or cancelled package releases both charges.

`Capacity` says whether the two folders share a device. With `Split { temp,
packages }` the budgets are independent: a package is refused when
`installed + sum(3*s of unfinished) + 3*s` exceeds `packages` or `s` alone
exceeds `temp`, and waits while `sum(s of unfinished) + s` exceeds `temp`. With
`Shared { free }`, for the unfinished packages in install order plus the new one,

```
max over j of  installed + sum(3*s[i] for i before j) + 3*s[j] + sum(s[i] for i from j)  <=  free
```

must hold: while `j` extracts, the trees ahead of it and its own are on disk with
its parts and the complete parts queued behind it. The ledger answers one of
three ways:

| Answer | Meaning | Job |
|---|---|---|
| `Admitted` | It fits | Downloads |
| `Wait { needed, available }` | It fits once packages ahead have installed | Emits `WaitingForSpace`, waits on a condvar, retried on every finish |
| `Never { filesystem, required, available }` | It cannot fit even with every package ahead installed | Fails with `InsufficientDiskSpace` naming the temp folder or the packages folder; the downloader moves on |

The check is made at admission, so the installer never waits for space and the
two threads cannot deadlock. Removes never touch the ledger: the space they free
is seen at the next busy period.

**Capacity** comes from `Stages::capacity`: the free space and device of the temp
and packages filesystems, read once per busy period (when the first job asks to
download after the queue was empty) and never mid-install, because it would then
count a half-extracted tree as used. It is read outside the state lock, since
`statvfs` can block on a slow filesystem. Writers other than the queue are unseen
until the next busy period. If one fills the disk, extraction fails cleanly with
ENOSPC: the job fails, its guards delete its parts and staging folder, and the
old version is untouched. Devices are compared by `st_dev`, so two btrfs
subvolumes of one pool count as two filesystems and their shared free space is
counted twice; the failure mode is that same clean extraction failure.

**Removes** skip the downloader and go straight to the install queue, so a
remove can overtake a download that is waiting for space. `submit` refuses a
second unfinished job for the same region and package type
(`ManagerError::AlreadyQueued`). The comparison uses the package folder name,
as the store does, so `eu` and `EU` are the same package. The refusal is what
makes overtaking safe, and it also guards a UI against double selection. It also refuses once the queue is shutting down or
abandoned (`QueueClosed`). `submit_batch` submits many installs under one lock
and `finish_batch` waits for them, so a cancel sees all of a batch or none of it.

**Cancellation.** Every job has a `CancelToken`, checked per buffer by the HTTP
download loop and per tar entry by the extractor.

| Cancelled while | Result |
|---|---|
| Queued, or downloaded and waiting | Removed from the queue immediately. A downloaded package is marked `Cancelling`, its temp directory is deleted outside the state lock, and only then is the job finished and its ledger entry released, so a waiter that returns finds the directory gone. `abandon` finishes it at once instead |
| Preparing, waiting for space, downloading | The token is raised; the owning thread notices, aborts every part worker, and the temp guard deletes the parts before the job finishes |
| Extracting | Checked per tar entry; the guard deletes the staging folder and the temp directory; the old version is untouched |
| The swap | `TooLate`; the swap runs to completion |
| A remove that has started | `TooLate` |

The swap is two renames in the destination's parent directory: the old version
is set aside to `.xel-aside-<name>`, then the staged tree (`.xel-staging-<name>`,
already beside it) is renamed in. `begin_swap` claims it; `end_swap` marks the renames done, and the old
version is deleted after that, outside the uncancellable part. A package folder
is therefore always either the old version or the new one. Dot-prefixed names
are skipped by `LocalPackageStore::list`.

**Shutdown.** Dropping the queue cancels everything, lets a running swap
finish, and joins both threads, unless the startup sweep is still running: a
sweep cannot be cut short, so that queue's threads are left to finish on their
own and the lock is held until they do. `Canceller` (from `canceller()`) is a cloneable
handle for signal handlers: `cancel_all` cancels every job in one critical
section, and `abandon` additionally stops waiters from waiting for anything but
a running swap and stops the drop from joining, because there may be no time for
cleanup. A live `Canceller` keeps the queue's lock held.

**Lock and sweep.** Starting a queue takes an exclusive `flock` on
`{temp_dir}/.queue.lock` for the queue's life (`ManagerError::QueueBusy` if
another process holds it); the kernel drops it however the process exits. The lock is per temp folder: two commands with different temp folders and the same packages root do not exclude each other, and either one's startup sweep can delete the other's in-progress staging folder, which then fails cleanly. Under
that lock, `sweep` runs on the queue's downloader thread, so `start` returns
at once: neither thread takes a job, install or remove, until the sweep is done,
and a sweep that panics is logged and counts as done. It deletes leftover `install_*` temp directories,
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

## Coordination with run

`xearthlayer run` serves every installed ortho package, and the patches, through
one union filesystem mounted at `Custom Scenery/zzXEL_ortho` (see
[consolidated-mounting-design.md](consolidated-mounting-design.md)). Packages
are no longer mounted one by one, so the package manager keeps no mount state
and cannot unmount anything.

What it has instead is the instance lock. `run` records its process ID in
`paths::lock_file()`. `system::process::running_instance(lock_path)` reads it
(`recorded_pid`) and answers with the PID only when that process is alive and is
not the caller, so a stale or corrupt lock reads as "not running".
`PackageManagerService::running_instance` wraps it for the CLI layer, and the
`actions` module (`commands/packages/actions.rs`) turns it into decisions:

- `check_removal`: a remove is refused while `run` is alive.
- `check_replacement`: an update, or an install over an installed package, is
  refused for the same reason. A package that is not installed is not a
  replacement, so it installs, and `RESTART_NOTE` tells the user that `run`
  picks it up on its next start.

Both refuse with `RunActive`, whose message names the PID. An update or remove
batch is refused whole before any work starts; an install batch refuses
per package and continues with the rest.

The reason is that the union filesystem holds the files of every installed
package open and indexed. A package replaced or deleted under it leaves the
index and the open handles pointing at files that are gone.

### Why the old mount check never fired

The first version asked whether each package folder was itself a mount point
(`MountStatus`, read from the platform mount table: `/proc/mounts` on Linux,
the system mount table on macOS). That matched the per-package mounts
of the design this section used to describe. After consolidated mounting the
only mount is `zzXEL_ortho`, so the answer for every package folder was always
"not mounted" and `update` and `remove` ran freely against a live filesystem.
`MountStatus`, `check_mount_status` and the Mount lines of `list -v` and `info`
were removed with the fix; the lock is the signal because it describes the
thing that matters, a running `run`, rather than a mount layout.

## Package Removal

Removal is a `Job::Remove` in the same queue, so it takes the same lock as an
install. The folder is renamed to `.xel-removed-<name>` before it is deleted,
so a remove interrupted mid-delete leaves nothing the sweep would restore. A
stale `.xel-aside-<name>` left by an earlier swap is deleted first, while the
package is still in place; if it cannot be deleted the remove fails and the
package stays installed, because once the package folder is gone the sweep
would rename that aside back as the package.

### Ortho Packages

1. Refuse while `xearthlayer run` is alive (the instance lock names its pid)
2. Delete package directory from `install_location`

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

## Package manager TUI

`xearthlayer packages` with no subcommand and a terminal on stdin and stdout
opens a three-tab screen (Local, Library, Progress) over the same machinery as
the commands. Without a terminal it prints the command help, as before. The
setup wizard offers to open it at the end (`offer_manager`, which asks only
when there is a terminal). The code is `xearthlayer-cli/src/ui/packages/`.

### State and effects

`AppState` (`app.rs`) is pure. `AppState::update(Input, Instant)` takes a key's
`Action` or a `Msg` and returns a list of `Effect`s. It touches no terminal,
network or disk, so every rule (marks, what `u`, `d` and `i` do, the
confirmations, the quit sequence, the idle step) is tested by calling `update`.
The state keeps what is on screen: rows, marks, selection, the open modal, the
status line and the job rows.

The status line is replaced by the answer to each key, and by each message
that reports on the queue, but the notes on why some of a request will not
happen (a package left out while `run` is active, a failed metadata fetch, a
package the queue refused, one already up to date) are kept with the request.
None of those packages gets a Progress row, so the notes stay beside the queued
note and end up in the idle queue's "Done" line, and that line stays beside the
overlay rebuild's. The "Nothing installed yet" hint only fills an empty status
line. A line wider than the terminal wraps to a second row.

Everything else is an `Effect` carried out by `Effects` (`effects.rs`): load the
installed packages, load the library, check for `run`, fetch package metadata,
submit installs and removes, cancel, abandon, rebuild the overlay folder,
release the queue, exit. The answers come back as `Msg`s on one channel.

`runner.rs` is the loop around them. Each turn it acts on a signal, draws, waits
up to 50 ms for keys, hands the channel's messages to the state, once a second
sends a `Tick` (which expires the quit prompt), and applies the effects that
came back. The terminal is restored on every exit, a panic on the runner thread
included.

### The worker and the queue

Slow reads (the installed list, the library over HTTP, the `run` check, package
metadata, the overlay rebuild) run on one worker thread, so the runner never
blocks on them. A load of one kind is shared while one of its kind is waiting
to run; a load asked for while one is running runs again after it.

The `InstallQueue` is started on demand. The first submitted job calls the
factory, which takes `{temp_dir}/.queue.lock` and starts the downloader and
installer threads, and the queue's startup sweep runs on the downloader thread,
so starting never blocks the runner. When the last job finishes and nothing is
in flight, the state emits `RebuildOverlays` (once, only if an overlay changed)
and, once no rebuild is waiting for its answer, `ReleaseQueue`, which drops the
queue and with it the lock. The rebuild therefore runs under the lock, and a job
queued meanwhile holds the queue until its own idle step. Quitting while jobs
are cancelled waits for the rebuild too; a second `q` leaves at once, and
dropping `Effects` still finishes the rebuild, saying "Finishing the overlay
folder…", before it drops the queue. An idle package
manager therefore holds no lock, and a package command in another terminal is
refused only while a job is actually running. If the lock is held,
`QueueRefused` carries the reason to the status line and the marks that the
confirmed action cleared are put back.

`ChannelObserver` implements `QueueObserver` by sending each `JobEvent` as a
`Msg` on that channel. The queue calls observers while it holds its own lock, so
`on_event` only does an unbounded `send` and ignores a closed channel. It never
blocks, allocates beyond the message, or calls back into the queue.

Cancelling deletes the temp trees of downloaded jobs, which takes time, so
`Effects::stop` runs it on a short-lived thread and the screen keeps drawing.

### Signals

The runner starts `SignalWatch::start_handling_all`, so a signal never kills the
process with the terminal in raw mode. In raw mode Ctrl+C is a key (it quits),
so a SIGINT is one sent from outside. The first acts as a confirmed quit:
cancel the jobs, then leave. A later SIGINT, and SIGTERM at any time, abandon
the queue and leave. SIGHUP abandons and stops drawing. The exit status is 130,
143 or 129 by the first signal (`CliError::Interrupted`), after the one-line
summary on stderr (not after SIGHUP, which took the terminal). A dropped, abandoned
queue is still waited for while it runs its final swap, because the process
must not end between the two renames.

### Shared with the commands

The `actions` module (`commands/packages/actions.rs`) holds the decisions the
commands and the TUI must agree on: `library_status` (not installed, up to date,
or update, and a failed update check is an error rather than "not installed"),
`plan_install` (a selected ortho followed by its overlay when
`auto_install_overlays` is on), `check_removal` and `check_replacement` (the
`run` refusals), `RESTART_NOTE`, `after_removal` (an overlay's stale symlink)
and `rebuild_overlays`, which returns an `OverlayReport` for the caller to show
where the commands print it. The commands and the TUI call the same functions, so
a rule is changed in one place.

### running_instance replaces the mount check

See [Coordination with run](#coordination-with-run). The TUI reads
`running_instance` through the worker (`Effect::CheckRun`) on start, on refresh
and each time finished jobs reload the lists, and keeps the answer as
`run_active`. That answer drives the top banner and refuses `u`, `d` and an
install over an installed package at the keypress, with `RunActive::message`.

It can be stale: a `run` started while the screen is open is not seen until the
next check. So `Effects` asks again at the moment it carries out
`Effect::Remove` or `Effect::Submit`, on the runner's thread (one lock file
read), through the same `check_removal` and `check_replacement` the commands
call. A remove batch is refused whole. A `Submit` names in `replacing` the
packages that are already installed, which the state knows from the library
statuses. Those rows may be stale too, so when the re-check finds `run`,
`Effects` also lists what is installed on disk and leaves out those packages
as well; the rest install, and the status line adds `RESTART_NOTE`. A batch
with nothing left is refused, and so is one whose installed list cannot be read.
A refusal is answered as `QueueRefused` is (the marks come back and the status
line gives the reason, after the left-out note if there is one) after a
`RunActive` that brings up the banner. The refusal names the instance lock, so
a user can delete one a `run` that exited left behind. As in the commands, a
`run` started after that check is not seen. The queue lock does not cover this
either: it serialises package operations, and the instance lock is a different
file.

Job ids come from one counter for the whole process, not one per queue. The
state keeps its job rows after `ReleaseQueue`, so a job in the next queue must
never share an id with a finished row.

## Error Handling

### Network Errors

- Retry transient failures with exponential backoff
- Resume from last successful byte on connection drop
- Clear error message if server unreachable

### Disk Space

- The `DiskLedger` admits each download against the free space (see Install queue)
- Estimate a package's extracted tree as `TREE_MULTIPLIER` (3) times its download, charged to the packages filesystem, with its parts charged to the temp filesystem until it installs
- A package that cannot fit fails before downloading, with a clear message; the rest of the batch continues

### Checksum Failures

- Re-download failed part (up to 3 attempts)
- If persistent, report corruption and suggest re-download
- Never proceed with failed checksum

### Running Instance

- Refuse updates and removes while `run` is alive, naming its PID
- Install new packages, and say that `run` needs a restart to use them

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
