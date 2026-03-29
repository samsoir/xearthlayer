# Multi-threaded FUSE Implementation Proposal

> **Status**: Implementation Complete - Ready for Testing
> **Decision**: Approved - Proceeding with fuse3
> **Started**: 2025-12-13
> **Code Complete**: 2025-12-13

## Executive Summary

XEarthLayer's current FUSE implementation uses `fuser`, which processes filesystem operations on a **single thread**. When X-Plane requests hundreds of DDS textures simultaneously during scene load, each request blocks the FUSE thread until completion. This serializes what should be parallel operations, resulting in:

- ~6% CPU utilization despite 296 available threads
- 50+ minute load times for cached scenes
- Poor user experience during scene transitions

This document evaluates multi-threaded FUSE alternatives and recommends a migration path.

## Problem Analysis

### Current Architecture

```
X-Plane                    FUSE Handler (single thread)           Tokio Runtime
   │                              │                                    │
   ├── read(file1.dds) ──────────►│                                    │
   │                              ├── block_on(generate) ─────────────►│
   │   [X-Plane blocked]          │   [FUSE thread blocked]            │
   │                              │◄── response ──────────────────────┤
   │◄── data ─────────────────────┤                                    │
   │                              │                                    │
   ├── read(file2.dds) ──────────►│  ← Can't start until file1 done   │
   ...                           ...                                  ...
```

### Root Cause

The `fuser` crate implements FUSE protocol handling on a single thread. Our `block_on()` call in `request_dds()` blocks this thread while waiting for tile generation. Even though our pipeline is fully async and multi-threaded, the FUSE serialization point negates all parallelism benefits.

### Impact

| Metric | Current | Expected (MT FUSE) |
|--------|---------|-------------------|
| CPU Utilization | ~6% | 60-80% |
| Concurrent DDS Generation | 1 | 50-100+ |
| Scene Load Time (cached) | 50+ min | 2-5 min |
| Scene Load Time (cold) | hours | 10-20 min |

## Options Evaluated

### Option 1: fuse-mt

**Repository**: https://github.com/wfraser/fuse-mt
**Crates.io**: https://crates.io/crates/fuse_mt

#### Overview

`fuse-mt` is a multi-threaded wrapper around the older `fuse-rs` library. It dispatches I/O operations (read, write, flush, fsync) to a thread pool while running other operations on the main thread.

#### Threading Model

```rust
// Operations dispatched to thread pool:
- read()
- write()
- flush()
- fsync()

// Operations run on main thread:
- lookup()
- getattr()
- readdir()
- all others
```

#### API Pattern

```rust
trait FilesystemMT {
    fn read(
        &self,
        _req: RequestInfo,
        path: &Path,
        fh: u64,
        offset: u64,
        size: u32,
        callback: impl FnOnce(ResultSlice<'_>) -> CallbackResult,
    ) -> ResultEmpty {
        // Callback-based, synchronous within thread
        Err(libc::ENOSYS)
    }

    fn lookup(&self, _req: RequestInfo, parent: &Path, name: &OsStr) -> ResultEntry {
        Err(libc::ENOENT)
    }

    fn getattr(&self, _req: RequestInfo, path: &Path, fh: Option<u64>) -> ResultGetattr {
        Err(libc::ENOENT)
    }
}
```

#### Advantages

1. **Path-based API**: Translates inodes to paths automatically, simplifying implementation
2. **Proven design**: Used in production systems
3. **Lower migration effort**: Synchronous callbacks align better with our current blocking approach
4. **Thread pool for I/O**: read() calls run in parallel

#### Disadvantages

1. **Inactive maintenance**: Last significant update in 2023
2. **Based on deprecated fuse-rs**: Not the modern `fuser` crate
3. **No async support**: Must use `block_on()` or `spawn_blocking` within callbacks
4. **lookup/getattr still serialized**: Only I/O operations are threaded
5. **Unknown thread pool limits**: May need tuning for high concurrency

#### Migration Complexity: **Medium**

- Reimplement `FilesystemMT` trait instead of `Filesystem`
- Change from inode-based to path-based API
- Callbacks instead of reply objects
- ~500-700 lines of code changes

---

### Option 2: fuse3 (Async)

**Repository**: https://github.com/Sherlock-Holo/fuse3
**Crates.io**: https://crates.io/crates/fuse3

#### Overview

`fuse3` is a modern async FUSE implementation that integrates directly with Tokio or async-io runtimes. All filesystem operations are async functions, allowing true concurrent execution.

#### Threading Model

```
Tokio Runtime (multi-threaded)
├── FUSE Protocol Handler
│   ├── read(file1) ──► spawned task
│   ├── read(file2) ──► spawned task  } All run concurrently
│   ├── read(file3) ──► spawned task
│   └── lookup(dir) ──► spawned task
└── Worker Threads handle all tasks
```

#### API Pattern

```rust
trait Filesystem {
    async fn read(
        &self,
        req: Request,
        inode: Inode,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        // Native async - no blocking!
        let data = self.generate_dds(inode).await?;
        Ok(ReplyData { data })
    }

    async fn lookup(
        &self,
        req: Request,
        parent: Inode,
        name: &OsStr,
    ) -> Result<ReplyEntry> {
        // Also async
    }

    async fn getattr(
        &self,
        req: Request,
        inode: Inode,
        fh: Option<u64>,
        flags: u32,
    ) -> Result<ReplyAttr> {
        // Also async
    }
}
```

#### Advantages

1. **Native async**: Perfect integration with our Tokio-based pipeline
2. **All operations concurrent**: Not just I/O - lookup, getattr also parallel
3. **Modern maintenance**: Active development, last release September 2024
4. **No libfuse dependency**: Pure Rust implementation (except for unprivileged mount)
5. **Tokio integration**: Uses same runtime as our HTTP client and pipeline
6. **No blocking calls needed**: Eliminates `block_on()` entirely

#### Disadvantages

1. **Higher migration effort**: Different API paradigm (async traits)
2. **Inode-based**: Must maintain inode mapping ourselves (like current impl)
3. **Less mature**: Fewer production deployments than fuse-mt
4. **API still evolving**: Some features marked unstable

#### Migration Complexity: **Medium-High**

- Reimplement `fuse3::raw::Filesystem` trait
- Convert all methods to async
- Remove `block_on()` calls - direct async/await
- Update mount/unmount logic for fuse3 API
- ~600-900 lines of code changes

---

### Option 3: fuser with spawn_blocking (Status Quo Optimization)

Keep `fuser` but optimize by spawning all DDS work to background threads.

#### Approach

```rust
fn read(&mut self, ..., reply: ReplyData) {
    let handler = self.handler.clone();
    let coords = self.get_coords(ino);

    // Spawn work to thread pool, reply later
    std::thread::spawn(move || {
        let data = handler.generate_blocking(&coords);
        reply.data(&data);
    });
}
```

#### Why This Won't Work

The `fuser` crate's `ReplyData` is **not Send** - it cannot be moved across threads. The reply must happen on the FUSE handler thread. This is a fundamental limitation of the `fuser` architecture.

**Verdict**: Not viable.

---

### Option 4: easy_fuser (Parallel Mode)

**Repository**: https://github.com/alogani/easy_fuser

#### Overview

A wrapper around `fuser` that adds parallel operation support via a thread pool.

#### Assessment

- Very new (limited production use)
- Documentation is sparse
- Based on `fuser` which has the Reply limitation
- Parallel mode implementation details unclear

**Verdict**: Insufficient maturity for production use.

---

## Recommendation: fuse3

### Rationale

| Criterion | fuse-mt | fuse3 | Winner |
|-----------|---------|-------|--------|
| Concurrency Model | Thread pool (I/O only) | Full async (all ops) | fuse3 |
| Maintenance | Inactive (2023) | Active (2024) | fuse3 |
| Tokio Integration | None | Native | fuse3 |
| All Operations Parallel | No (lookup/getattr serial) | Yes | fuse3 |
| API Complexity | Medium | Medium-High | fuse-mt |
| Blocking Calls Needed | Yes | No | fuse3 |
| Future-Proof | No | Yes | fuse3 |

**fuse3 is recommended** because:

1. **Complete solution**: All operations run async, not just read/write
2. **Native Tokio**: Our entire pipeline is async - fuse3 fits naturally
3. **No blocking**: Eliminates `block_on()` which is our current bottleneck
4. **Active development**: Bug fixes and improvements ongoing
5. **Modern Rust**: Uses async traits, proper error handling

### Migration Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| API instability | Medium | Medium | Pin to specific version, test thoroughly |
| Performance regression | Low | High | Benchmark before/after |
| Tokio version conflicts | Low | Medium | Use workspace-level Tokio dependency |
| Unknown bugs in fuse3 | Medium | Medium | Extensive testing, fallback plan |

## Implementation Plan

### Phase 1: Proof of Concept (1-2 days)

1. Create new module `fuse/fuse3_passthrough.rs`
2. Implement minimal `fuse3::raw::Filesystem`:
   - `lookup()` - path resolution
   - `getattr()` - file attributes
   - `read()` - async DDS generation
   - `readdir()` - directory listing
3. Add `fuse3` dependency with `tokio-runtime` feature
4. Test with single package mount

### Phase 2: Feature Parity (2-3 days)

1. Implement all required filesystem operations
2. Wire up to existing `DdsHandler` pipeline
3. Add proper error handling and logging
4. Test with multi-package mounts
5. Benchmark against current implementation

### Phase 3: Integration (1-2 days)

1. Update `XEarthLayerService` to use fuse3
2. Update CLI mount commands
3. Remove old `fuser`-based implementation
4. Update documentation

### Phase 4: Optimization (ongoing)

1. Tune concurrency limits
2. Profile and optimize hot paths
3. Consider caching strategies at FUSE level

## API Mapping

### Current (fuser) → Proposed (fuse3)

```rust
// Current: fuser::Filesystem
impl Filesystem for AsyncPassthroughFS {
    fn read(&mut self, _req: &Request, ino: u64, ..., reply: ReplyData) {
        let data = self.request_dds(ino, &coords);  // BLOCKS!
        reply.data(&data[offset..end]);
    }
}

// Proposed: fuse3::raw::Filesystem
impl fuse3::raw::Filesystem for AsyncPassthroughFS {
    async fn read(&self, req: Request, ino: Inode, ...) -> Result<ReplyData> {
        let data = self.request_dds(ino, &coords).await;  // Non-blocking!
        Ok(ReplyData { data: data[offset..end].to_vec() })
    }
}
```

### Key Differences

| Aspect | fuser | fuse3 |
|--------|-------|-------|
| Self reference | `&mut self` | `&self` |
| Response | `reply.data()` callback | Return `Result<ReplyData>` |
| Async | No (must block) | Native async |
| Concurrency | Single-threaded | Tokio executor |

## Dependencies

### Cargo.toml Changes

```toml
[dependencies]
# Remove
fuser = "0.14"

# Add
fuse3 = { version = "0.8", features = ["tokio-runtime"] }
```

### Feature Flags

- `tokio-runtime`: Required for Tokio integration
- `unprivileged`: Optional, for non-root mounting (requires fusermount3)

## Rollback Plan

If fuse3 migration encounters blocking issues:

1. Keep `fuser` implementation in `fuse/legacy/`
2. Add feature flag to select implementation
3. Default to fuse3, fallback to fuser if needed

## Success Criteria

1. **CPU utilization**: >50% during scene load (vs current 6%)
2. **Concurrent DDS generation**: 50+ simultaneous (vs current 1)
3. **Scene load time**: <5 min for cached scenes (vs current 50+ min)
4. **No regressions**: All existing functionality preserved
5. **Stability**: No crashes or hangs under load

## References

- [fuse3 GitHub](https://github.com/Sherlock-Holo/fuse3)
- [fuse3 Documentation](https://docs.rs/fuse3)
- [fuse-mt GitHub](https://github.com/wfraser/fuse-mt)
- [Building an Async FUSE Filesystem in Rust](https://r2cn.dev/blog/building-an-asynchronous-fuse-filesystem-in-rust)
- [fuser GitHub](https://github.com/cberner/fuser)

## Appendix: Current Implementation Files

Files requiring modification:

| File | Changes |
|------|---------|
| `xearthlayer/Cargo.toml` | Replace fuser with fuse3 |
| `xearthlayer/src/fuse/mod.rs` | Update exports |
| `xearthlayer/src/fuse/async_passthrough/` | Rewrite for fuse3 API |
| `xearthlayer/src/service/facade.rs` | Update mount calls |
| `xearthlayer-cli/src/commands/run.rs` | Update session handling |

Files that can remain unchanged:

| File | Reason |
|------|--------|
| `xearthlayer/src/pipeline/` | Pipeline is already async |
| `xearthlayer/src/provider/` | HTTP client unchanged |
| `xearthlayer/src/cache/` | Cache system unchanged |
| `xearthlayer/src/dds/` | Encoder unchanged |

---

## Implementation Progress

### Phase 1: Proof of Concept ✅ COMPLETE

| Task | Status | Notes |
|------|--------|-------|
| Add fuse3 dependency | ✅ Done | With `tokio-runtime` and `unprivileged` features |
| Create fuse3_passthrough module | ✅ Done | `xearthlayer/src/fuse/fuse3/` |
| Implement lookup() | ✅ Done | With virtual DDS inode support |
| Implement getattr() | ✅ Done | Real files + virtual DDS attrs |
| Implement read() | ✅ Done | Async DDS generation via pipeline |
| Implement readdir() | ✅ Done | With . and .. entries |
| Test single package mount | ✅ Done | CLI `--fuse3` flag added |

### Phase 2: Feature Parity ✅ COMPLETE

| Task | Status | Notes |
|------|--------|-------|
| Implement flush/fsync | ✅ Done | No-op for read-only FS |
| Implement access | ✅ Done | Allow all (read-only FS) |
| Wire up DdsHandler | ✅ Done | Integrated in Phase 1 |
| Multi-package mount test | ⏳ Pending | Requires manual testing |
| Performance benchmark | ⏳ Pending | Requires X-Plane testing |

### Phase 3: Integration ✅ COMPLETE

| Task | Status | Notes |
|------|--------|-------|
| Update XEarthLayerService | ✅ Done | `serve_passthrough_fuse3()` and `serve_passthrough_fuse3_blocking()` |
| Update CLI commands | ✅ Done | `--fuse3` flag on `start` and `run` commands |
| Update MountManager | ✅ Done | `with_fuse3()` builder, `MountSession` enum for dual backend |
| Remove fuser implementation | ⏳ Later | Keep as fallback for now |
| Update documentation | ⏳ Pending | After validation |

### Files Created/Modified

**New Files:**
- `xearthlayer/src/fuse/fuse3/mod.rs` - Module declaration
- `xearthlayer/src/fuse/fuse3/types.rs` - Error types, MountHandle wrapper with Future impl
- `xearthlayer/src/fuse/fuse3/inode.rs` - Re-export InodeManager
- `xearthlayer/src/fuse/fuse3/filesystem.rs` - Core Filesystem trait impl (~600 lines)

**Modified Files:**
- `xearthlayer/Cargo.toml` - Added fuse3, futures, bytes dependencies
- `xearthlayer/src/fuse/mod.rs` - Export fuse3 module
- `xearthlayer/src/fuse/async_passthrough/mod.rs` - Make inode module public
- `xearthlayer/src/service/facade.rs` - Add fuse3 mount methods
- `xearthlayer/src/service/error.rs` - Add FuseError variant
- `xearthlayer/src/manager/mounts.rs` - Add MountSession enum, with_fuse3() builder
- `xearthlayer-cli/src/main.rs` - Add --fuse3 flag to start and run commands
- `xearthlayer-cli/src/commands/start.rs` - Handle --fuse3 flag
- `xearthlayer-cli/src/commands/run.rs` - Handle --fuse3 flag, pass to MountManager

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2025-12-13 | Proceed with fuse3 over fuse-mt | Full async support, active maintenance, native Tokio integration |
| 2025-12-13 | Keep fuser as fallback initially | Risk mitigation during migration |
| 2025-12-13 | Implementation complete | All core functionality implemented, ready for testing |

## Testing

To test the fuse3 implementation:

```bash
# Single package mount with fuse3 backend
xearthlayer start --source /path/to/scenery --mountpoint /path/to/mount --fuse3

# Single package mount with legacy fuser backend (default)
xearthlayer start --source /path/to/scenery --mountpoint /path/to/mount

# Multi-package run with fuse3 backend
xearthlayer run --fuse3

# Multi-package run with legacy fuser backend (default)
xearthlayer run
```

Key differences from fuser:
- All FUSE operations run async on Tokio runtime
- Multiple concurrent DDS reads can be processed in parallel
- No `block_on()` calls - direct async/await throughout
