# FOPEN_DIRECT_IO for Virtual DDS Files

**Issue:** #65
**Status:** Design approved

## Problem

`Fuse3OrthoUnionFS` does not implement `open()`. The kernel uses default page caching for all files, including virtual DDS files generated on-demand. This causes:

1. **Invisible reads** — kernel-cached reads bypass the FUSE `read()` handler, hiding X-Plane's access patterns from `FuseLoadMonitor`, `SceneTracker`, and `DdsAccessEvent`
2. **Stale data** — page cache can serve outdated DDS data after provider changes or cache clears
3. **Redundant memory** — kernel duplicates data already cached in the moka memory cache

## Solution

Implement `open()` on the `Filesystem` trait for `Fuse3OrthoUnionFS`:

- **Virtual DDS inodes** (`InodeManager::is_virtual_inode()`): return `FOPEN_DIRECT_IO` flag — kernel bypasses page cache
- **Real passthrough files**: return default flags — kernel uses default caching behavior

## Design

### open() Implementation

```rust
async fn open(&self, _req: Request, inode: Inode, flags: u32) -> Result<ReplyOpen> {
    if InodeManager::is_virtual_inode(inode) {
        Ok(ReplyOpen {
            fh: 0,
            flags: FOPEN_DIRECT_IO,
        })
    } else {
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }
}
```

### What changes

- `Fuse3OrthoUnionFS` gains an `open()` method (~15 lines)
- Virtual DDS reads always go through the FUSE handler (full observability)

### What doesn't change

- `read()` logic is unchanged
- No file handle state (`fh: 0` everywhere, matching existing `opendir()` pattern)
- `release()` not needed (no state to clean up)
- `Fuse3PassthroughFS` and `Fuse3UnionFS` are not modified
- Mount options unchanged
- Passthrough file behavior unchanged (default kernel caching)

### File to modify

- `xearthlayer/src/fuse/fuse3/ortho_union_fs.rs` — add `open()` to the `Filesystem` impl

### Testing

- Unit test: virtual DDS inodes receive `FOPEN_DIRECT_IO` flag
- Unit test: real passthrough inodes receive default flags (0)
- Existing FUSE integration tests serve as regression guards

### Expected outcome

- `FuseLoadMonitor`, `SceneTracker`, and `DdsAccessEvent` see every X-Plane DDS read
- No stale DDS data after provider/cache changes
- Reduced kernel memory usage (no page cache duplication)
- No performance regression — moka cache serves hits in <1ms
