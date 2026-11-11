# Filesystem Backend Split

**Technical Design Document**

**Project**: XEarthLayer
**Date**: November 2026
**Version**: 1.0
**Status**: Design Phase

---

## Table of Contents

1. [Overview](#overview)
2. [Goals and Non-Goals](#goals-and-non-goals)
3. [System Architecture](#system-architecture)
4. [Acceptance](#acceptance)

---

## Overview

### Purpose

The filesystem backend split is an architectural reorganization of the (virtual)
filesystem backend implementation to facilitate the implementation of multiple
backends that are specific to various platforms (e.g., Linux, Windows, macOS).
The current implementation has a single backend that supports Linux and macOS
via fuse3.
This "works" for now, but there still is no clean boundary between them to handle
their minor differences.

The planned addition of Windows support (via WinFSP) and other approaches such as
FSKit for macOS will introduce far greater differences that would have to be handled
with clumsy or `cfg`-driven modifications throughout the code base.
A more modular backend architecture addresses this concern.

### Key Observation

This is largely a code reorganization with no user-visible changes.

---

## Goals and Non-Goals

### Goals

1. **No changes in overall functionality and behavior** - The app should still run as before.
2. **All tests continue to pass** - Green CI required
3. **Update developer/internal documentation** - The internals change substantially in this project.

### Non-Goals

1. **Implementation of new backends** - This project is viewed as a prerequisite before any new backend implementation.
2. **Changes to existing Linux and macOS backends** - These operate as they did before with only the code changes needed to adapt to the new architecture.  That is, we keep one shared fuse3 translation and add only thin per-OS adapters and a FusePlatform policy.

---

## System Architecture

### Module Structure

#### Present Structure

```
xearthlayer/src/fuse
|── fuse3/
|  |── inode.rs
|  |── mod.rs
|  |── ortho_union_fs.rs
|  |── shared.rs
|  |── types.rs
|── support/
|  |── inode.rs
|  |── mod.rs
|  |── types.rs
|── coalesce.rs
|── filename.rs
|── mod.rs
|── placeholder.rs

```

#### Future Structure

The module layout is shown for illustrative purposes, the implementation may be different.

New items indicated by the addition of a comment.

The new items marked with "(refactor)" are added solely as part of a feature-preserving refactoring.

```
xearthlayer/src/vfs
|── fuse3/
|  |── adapters.rs              # New adapters for LinuxFuse and MacFuse translations
|  |── fuse_platform.rs         # Per-platform FUSE behavior/policy within fuse3 backend
|  |── inode.rs
|  |── mod.rs
|  |── ortho_union_fs.rs
|  |── shared.rs
|  |── types.rs
|── common/
|  |── attrs.rs                 # Neutral file attribute types (refactor)
|  |── conformance.rs           # Neutral one contract test suite
|  |── core.rs                  # Neutral core of union filesystem (refactor)
|  |── error.rs                 # Neutral error handling (refactor)
|  |── mod.rs
|  |── node.rs                  # Neutral filesystem-node identity (refactor)
|  |── requestor.rs             # Neutral DDS-generation request pipeline (refactor)
|  |── types.rs
|  |── union.rs                 # Union filesystem trait
|── backend.rs                  # MountBackend returns MountSession
|── coalesce.rs
|── filename.rs
|── mod.rs
|── placeholder.rs

```

### Core state and metrics

The read-path state in `ortho_union_fs.rs` is backend neutral
and moves to `common/core.rs` unchanged:

- the `DdsHandle` memo (`dds_handles`)
- the `PinnedTileBudget` (`pinned`)
- the metrics client
- the `PrefetchStateObserver`
- the geo filter (`resolve_lazy_geo`)
- the water-mask guard in `lookup` (#68): return `ENOENT` when the `.png`
  exists so X-Plane falls back to it instead of a generated virtual DDS

The core is the sole emitter of the metrics the dashboard reads. In
particular, per-read latency (`MetricEvent::FuseRead`, from which the daemon
derives the latency window) and `fuse_handle_budget_exhausted` are both
emitted on the core's read/open path. The adapter emits no dashboard metrics;
it performs only the platform translation (open flags, mount API, `FileAttr`
fields). Keeping these two core-emitted is what guarantees they are reported
identically on Linux and macOS: if they drifted into the adapter, the macOS
page-cache path (reads served by the kernel, never reaching the handler) would
silently drop them.

Note that the per-read metric already under-reports on macOS by design (#204):
the cold first read reaches the core, repeat reads are kernel-served. That is
a known, accepted property, not a regression this move should change.

### Components

#### `fuse/` becomes `vfs/`

This is a rename ("move-only") operation to better represent the underlying
structure. "`fuse`" or "FUSE" is sometimes used to describe more generally a
"Filesystem in Userspace" software interface. Because there are other solutions
that are "FUSE-compatible" and not strictly "FUSE", such as WinFSP, it is better
to describe these as a more generic virtual filesystem (vfs).

`fuse3/` remains `fuse3/` under `vfs/` because it is a backend that explicitly
uses "fuse 3" on Linux and macOS. Future backends will be added as siblings to
`fuse3/`.

This operation is an initial straightforward renaming. However, further
refactoring is required to move code that is backend neutral to the common
`common/` area. This is to avoid this code being duplicated by a copy-and-paste
operation when starting a new backend.

This refactoring became more complex than expected after driving the design to a
more detailed level. Therefore, a prototype of this step was implemented to
mitigate any unseen risks. The completed prototype returns a green cargo test
and can be leveraged for the final implementation.

### Neutral Conformance Suite

The neutral conformance suite runs the core contract suite once, against
`OrthoUnionCore`. The suite drives the `UnionFilesystem` operations, testing
only the neutral types. There should not be any instances of backend-specific
symbols (`fuse3::`) or references to inode, kernel flags, caching policy.
This suite should not test adapter-specific translations.

The adapter-specific tests need to be in a live mount test that reads through a
real mount to exercise filesystem operations (lookup, attributes, virtual DDS read,
directory listing).  These tests run for each backend.

For implementation, `tests/macfuse_smoke.rs` is turned into a live mount test
parameterized by backend. The test is **unignored on Linux** (CI installs
`libfuse3-dev` on `ubuntu-latest`, so `mount_with_unprivileged` works) and
**`#[ignore]` on macOS** (macFUSE is a kext; hosted runners cannot load it —
run via `make verify-macos`). This means green CI exercises the `LinuxFuse`
adapter; the `MacFuse` adapter is covered by `verify-macos` and the regression
flight.


### MountBackend ("Live" Mounting)

The manager builds the neutral OrthoUnionCore and mounts it via
select_backend(), holding the live mount as Box<dyn MountSession> instead of a
concrete SpawnedMountHandle.

```text
             UnionFilesystem (implemented by OrthoUnionCore)
                               ▲
                         MountBackend
        ┌───────────────┬──────┴───────┬─────────────────┐
    LinuxFuse        MacFuse         WinFsp            FsKit, <...>, <...>
        └───────┬───────┘
    shared fuse3 translation
    + a small FusePlatform policy per OS
```


```rust
/// A live mount of the union filesystem.
///
/// Callers do not need to know which backend produced the session; they only
/// need the mount point, an unmount operation, and a liveness check.
/// When a MountSession is dropped, the filesystem must be unmounted by the backend.
pub trait MountSession: Send {
    /// The directory the filesystem is mounted at.
    fn mountpoint(&self) -> &Path;

    /// Unmount the filesystem, consuming the session.
    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

    /// Whether the mount point currently reports a live mount.
    fn is_mounted(&self) -> bool;
}
```
Each backend should have a test to make sure the unmount occurs on a `MountSession` drop.

```rust 
/// The future returned by [`MountBackend::mount`]: resolves to a live mount
/// session or a [`MountError`].
pub type MountFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn MountSession>, MountError>> + Send + 'a>>;

/// Failure to establish or tear down a live mount.
#[derive(Debug)]
pub enum MountError {
    /// The backend could not mount (e.g. `mount_with_unprivileged` failed).
    Mount(std::io::Error),
    /// The backend could not unmount.
    Unmount(std::io::Error),
}

/// A platform backend that can mount the union filesystem.
pub trait MountBackend: Send + Sync {
    /// Mount `fs` according to `req`, returning a handle to the live mount.
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, req: MountRequest) -> MountFuture<'_>;
    /// Whether a stale mount at `mountpoint` was recovered. Synchronous, needs
    /// no runtime, and must not panic (see the panic-hook contract below).
    fn recover_stale(&self, mountpoint: &Path) -> bool;
    /// Force-detach the mount at `path`. Returns `true` if the mountpoint
    /// ended up unmounted (including the benign already-unmounted case), matching
    /// `unmount_fuse`. Synchronous, needs no runtime, and must not
    /// panic.
    fn force_unmount(&self, path: &Path) -> bool;
}
```

The crash path is the panic hook (`panic.rs`), not the `MountManager`. It
calls `unmount_fuse(path, false)` and, if that fails, `unmount_fuse(path, true)`,
logging which one worked. Under the split the hook gets a backend with its own
`select_backend()` call (cheap enough) and calls `force_unmount` the same way:
graceful first, then forced, reporting the `bool` outcome. `force_unmount`
returns `bool` so the hook can still say which attempt succeeded.

Both `recover_stale` and `force_unmount` are synchronous, need no runtime, and
must not panic. A panic inside a panic hook aborts the process with the mount
still up, so these two are the one place in the backend where that guarantee
matters.
```rust
/// The mount backend for the OS this binary runs on.
///
/// This is the one place in the crate that names a concrete backend. Callers
/// ask for a backend here and then work only with [`MountBackend`] and
/// [`MountSession`].
pub fn select_backend() -> Box<dyn MountBackend> {
    if cfg!(target_os = "macos") {
        Box::new(crate::vfs::fuse3::MacFuse)
    } else {
        Box::new(crate::vfs::fuse3::LinuxFuse)
    }
}
```

```rust
pub struct MountRequest {
    /// The directory the union filesystem is mounted at.
    pub mountpoint: PathBuf,
    /// Kernel background-request limits (fuse3 `max_background` /
    /// `congestion_threshold`); backends that have no equivalent ignore them.
    pub fuse_limits: FuseLimits,
}

/// FUSE kernel limits, from the `[fuse]` config section.
pub struct FuseLimits {
    pub max_background: u16,
    pub congestion_threshold: u16,
}
```
    

The mount manager then mounts with something like this:

```rust
use std::sync::Arc;
use crate::vfs::{select_backend, UnionFilesystem, MountRequest, MountSession};
...
        let backend = select_backend();
        let fs: Arc<dyn UnionFilesystem> = Arc::new(core);

        let mount_result = runtime_handle.block_on(backend.mount(fs, request));

```

This keeps the `cfg` that decides the mount backend in one place.
Backend specific parameters (fuse3's max_background and congestion_threshold)
travel in `MountRequest::fuse_limits`; the fuse3 backend reads them and any
backend without an equivalent ignores them.

### FusePlatform Policy

`fuse_platform.rs` holds the per-platform FUSE behavior and policy,
consolidating the `cfg(target_os)` sites that are scattered inside `fuse/`. Its complete contents
are the four decisions below; the shared fuse3 translation in `adapters.rs`
stays platform-neutral.

| Policy decision | Linux | macOS |
|---|---|---|
| Open flags reported for virtual DDS files | `FOPEN_DIRECT_IO` — every `read()` reaches the handler, so the FUSE read counters are exact and the page cache never holds a second copy of a tile | `0` — X-Plane `mmap`s texture files and macFUSE faults with `EXC_BAD_ACCESS` when a `direct_io` file is mapped, so virtual DDS is served through the page cache and the read counters under-report (#204) |
| Mount API call | `Session::mount_with_unprivileged` | `Session::mount` |
| `volname` mount option | not set | set, so Finder shows a meaningful volume name |
| `FileAttr` fields | — | `crtime` (creation time) and `flags` (BSD file flags) exist only in macFUSE's `FileAttr`; the policy supplies their values |

Because the contract's `open` returns a bare `u64` file handle, the open flags
are the adapter's decision, keyed on the `FsNode` variant: a `VirtualDds` node
gets the platform's virtual-DDS flags from the policy, and `RealFile` and
`Directory` nodes get none.

`select_backend()` keeps its `cfg` — it is the one place in the crate that
names a concrete backend.

### Union Filesystem

The backend-neutral contract shared by every mount backend. The asynchronous
operations are object-safety-boxed (`Pin<Box<dyn Future + Send + '_>>`, done by
hand, with no `async-trait` dependency) so the trait can be used as
`Arc<dyn UnionFilesystem>`; the synchronous ones (`open`, `release`, `statfs`)
stay plain.

```rust
pub struct Entry {
    pub node: FsNode,
    pub attrs: FileAttributes,
}

pub trait UnionFilesystem: Send + Sync {
    fn lookup<'a>(
        &'a self,
        node: &'a FsNode,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Entry, FsError>> + Send + 'a>>;

    fn attrs<'a>(
        &'a self,
        node: &'a FsNode,
    ) -> Pin<Box<dyn Future<Output = Result<FileAttributes, FsError>> + Send + 'a>>;

    fn read<'a>(
        &'a self,
        node: &'a FsNode,
        fh: u64,
        offset: u64,
        size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Bytes, FsError>> + Send + 'a>>;

    fn list_directory<'a>(
        &'a self,
        node: &'a FsNode,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DirEntry>, FsError>> + Send + 'a>>;

    /// open returns a file handle, or 0 when no handle is allocated. 0 is a
    /// valid return for four cases: real files (no handle state), lit
    /// textures (served by range, pin nothing), unknown virtual inodes (the
    /// gap surfaces as ENOENT in read, not a failed open), and the exhausted
    /// pinned-tile budget. A 0 handle means read resolves the tile per call.
    fn open(&self, node: &FsNode) -> Result<u64, FsError>;
    /// release(0) is a no-op: there is no handle to drop. A backend must not
    /// treat 0 as a real handle or error on release(0).
    fn release(&self, fh: u64) -> Result<(), FsError>;
    fn statfs(&self) -> StatFsInfo;
}
```
Conformance suite should cover all four 0-handle cases and that `release(0)`
is a no-op.

`open` returns a bare file handle; the open flags reported to the kernel are
the adapter's decision, keyed on the `FsNode` variant — a `VirtualDds` node
gets the platform's virtual-DDS flags from the `FusePlatform` policy, and
`RealFile` and `Directory` nodes get none.

Types and errno mapping.
```rust
pub enum FsNode {
    Directory(PathBuf),      // root = empty path
    RealFile(PathBuf),       // a source file, addressed by its virtual path
    VirtualDds(DdsFilename), // generated texture: ortho or night-lit (kind)
}

pub enum FsError {
    /// The requested node does not exist in the union view.
    #[error("not found")]
    NotFound,

    /// The requested node is not a directory (e.g. a directory listing was
    /// asked for a file).
    #[error("not a directory")]
    NotADirectory,

    /// An I/O failure while reading a real file from its source.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// File attributes in the neutral form shared by all backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttributes {
    /// File size in bytes.
    pub size: u64,
    /// Number of 512-byte blocks allocated (POSIX convention).
    pub blocks: u64,
    /// Last access time.
    pub atime: SystemTime,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Last metadata or status change time.
    pub ctime: SystemTime,
    /// Creation time.
    pub crtime: SystemTime,
    /// Node kind.
    pub kind: NodeKind,
    /// Permission bits (e.g. `0o444`).
    pub perm: u16,
    /// Link count.
    pub nlink: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Device number (0 for non-device files).
    pub rdev: u32,
    /// Filesystem block size.
    pub blksize: u32,
}

pub struct DirEntry {
    pub name: OsString,
    pub node: FsNode,
    pub attrs: FileAttributes,
}

/// Volume statistics in neutral form (FUSE `statfs` / WinFSP `GetVolumeInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFsInfo {
    /// Total data blocks.
    pub blocks: u64,
    /// Free blocks.
    pub bfree: u64,
    /// Free blocks available to unprivileged users.
    pub bavail: u64,
    /// Total inodes.
    pub files: u64,
    /// Free inodes.
    pub ffree: u64,
    /// Block size.
    pub bsize: u32,
    /// Maximum filename length.
    pub namelen: u32,
    /// Fundamental block size.
    pub frsize: u32,
}

/// Map the core's neutral error onto FUSE's.
fn errno_from(err: FsError) -> Errno {
    match err {
        FsError::NotFound => Errno::from(libc::ENOENT),
        FsError::NotADirectory => Errno::from(libc::ENOTDIR),
        FsError::Io(_) => Errno::from(libc::EIO),
    }
}

```

---

## Acceptance

Since there is no new functionality, all existing checks are required and expected to pass.
Green CI all the way.

In addition:
- passing `make verify-macos` run
- regression flying on macOS and Linux before merge
- structural checks
  - `manager/mounts.rs` contains no `cfg` and names no backend
  - `common` module contains no `fuse3::`, inode or kernel-flag symbols
  - no `cfg(target_os)` remains in `vfs/` outside `fuse_platform.rs` (and `select_backend`)
  - no `crate::vfs::fuse3::` import outside `vfs/`
  - `system/unmount.rs` moves into the fuse3 backend, so `panic.rs` and `mounts.rs` reach it only through `MountBackend`

The design should be further validated by the addition of any new backends,
which are expected to follow shortly after this lands.
Actually adding a new backend is a true test.

Landing this at the start of a new development branch will also provide a lot of exercise.