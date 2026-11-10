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
This "works" today, but there still is no clean boundary between them to handle
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
`OrhtoUnionCore`. The suite drives the `UnionFilesystem` operations, testing
only the neutral types. There should not be any instances of backend-specific
symbols (`fuse3::`) or references to inode, kernel flags, caching policy.
This suite should not test adapter-specific translations.

The adapter-specific tests need to be in a live mount test that reads through a
real mount to exercise filesystem operations (lookup, attributes, virtual DDS read,
directory listing).  These tests run for each backend.

For implementation, `tests/macfuse_smoke.rs` can be turned into a live mount test
parameterized by backend.


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

/// A platform backend that can mount the union filesystem.
pub trait MountBackend: Send + Sync {
    /// Mount `fs` according to `req`, returning a handle to the live mount.
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, req: MountRequest) -> MountFuture<'_>;
    fn recover_stale(&self, mountpoint: &Path) -> bool;
    fn force_unmount(&self, path: &Path);
}
```
The calls in `MountManager` that handle crash and stale mount recovery need to be migrated to
the new calls in the `MountBackend.`
```rust
/// The mount backend for the OS this binary runs on.
///
/// This is the one place in the crate that names a concrete backend. Callers
/// ask for a backend here and then work only with [`MountBackend`] and
/// [`MountSession`].
pub fn select_backend() -> Box<dyn MountBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(crate::vfs::fuse3::MacFuse)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(crate::vfs::fuse3::LinuxFuse)
    }
}
```
Backend specific parameters are passed in a `MountRequest` where each backend can
choose to use or ignore them.

```rust
pub struct MountRequest {
    /// The directory the union filesystem is mounted at.
    pub mountpoint: PathBuf,
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
Backend specific parameters (fuse3's max_background and congestion_threshold) are
also handled by `select_backend` by passing to the backends`constructors.

### Union Filesystem

The backend-neutral contract shared by every mount backend. The asynchronous
operations are object-safety-boxed (`Pin<Box<dyn Future + Send + '_>>`, done by
hand, with no `async-trait` dependency) so the trait can be used as
`Arc<dyn UnionFilesystem>`; the synchronous ones (`open`, `release`, `statfs`)
stay plain.

```rust
pub trait UnionFilesystem: Send + Sync {
    fn lookup<'a>(
        &'a self,
        node: &'a FsNode,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<FsNode, FsError>> + Send + 'a>>;

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

    /// open can return a fh of 0 when the pinned-tile budget is exhausted.
    fn open(&self, node: &FsNode) -> Result<u64, FsError>;
    fn release(&self, fh: u64) -> Result<(), FsError>;
    fn statfs(&self) -> StatFsInfo;
}
```
Conformance suite should cover the budget-exhausted case.

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
///
/// `uid`/`gid` are carried for the backends that report them: the FUSE
/// mapper fills them from the file metadata (real files) or the process
/// (virtual files), so `libc`/`MetadataExt` calls stay out of core logic.
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
    /// File or directory name (stored as String for serialization).
    #[serde(
        serialize_with = "serialize_os_string",
        deserialize_with = "deserialize_os_string"
    )]
    pub name: OsString,

    /// Whether this is a directory.
    pub is_dir: bool,

    /// File size (0 for directories).
    pub size: u64,

    /// Modification time (stored as secs since UNIX_EPOCH for serialization).
    #[serde(
        serialize_with = "serialize_system_time",
        deserialize_with = "deserialize_system_time"
    )]
    pub mtime: SystemTime,
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
  - `common` module contains no `fuse3::`, inode or kernal-flag symbols
  - nothing outside of `vfs/` refers to FUSE

The design should be further validated by the addition of any new backends,
which are expected to follow shortly after this lands.
Actually adding a new backend is a true test.

Landing this at the start of a new development branch will also provide a lot of exercise.