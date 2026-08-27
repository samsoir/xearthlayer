# OrthoUnionIndex Building Optimization

## Problem Statement

Building the `OrthoUnionIndex` at startup is extremely slow for large scenery installations:

- **NA ortho alone**: ~963,000 terrain files
- **4 regions installed**: Potentially 3-4 million files total
- **Current behavior**: Sequential scanning, ~10+ minutes startup time
- **No progress feedback**: Users see no output, appears hung

## Goals

1. **Reduce startup time** from minutes to seconds (via caching)
2. **Show real-time progress** during index building (TUI feedback)
3. **Parallelize scanning** when cache is stale (multi-core utilization)
4. **Skip unnecessary scanning** (terrain files don't need indexing)

## Design

### 1. Index Caching

Cache the built index to disk. Only rebuild when sources change.

#### Cache Location

```
~/.xearthlayer/ortho_union_index.cache
```

#### Cache Key (Fingerprint)

The cache is valid if all of these match:

```rust
pub struct IndexCacheKey {
    /// XEL version (cache format may change between versions)
    version: String,

    /// Sorted list of source paths
    source_paths: Vec<PathBuf>,

    /// Modification time of each source's root directory
    source_mtimes: Vec<SystemTime>,

    /// Hash of patches config (enabled, directory)
    patches_config_hash: u64,
}
```

#### Cache Format

Use `bincode` for fast serialization:

```rust
pub struct IndexCache {
    /// Cache key for validation
    key: IndexCacheKey,

    /// The cached index data
    index: OrthoUnionIndex,

    /// When the cache was created
    created_at: SystemTime,
}
```

##### Corruption handling

The cache is a performance optimization, never a source of truth: any file we
cannot read is discarded and the index is rebuilt from sources. Reads are
bounded by the file's own length and writes go through `write_atomic`, both
from the shared model in `cache/integrity`. See
[Cache Integrity Design](cache-integrity-design.md).

#### Cache Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                         Startup                                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                   ┌─────────────────────┐
                   │  Load cache file    │
                   │  (if exists)        │
                   └─────────────────────┘
                              │
                              ▼
                   ┌─────────────────────┐
                   │  Compute current    │
                   │  cache key          │
                   └─────────────────────┘
                              │
                              ▼
                   ┌─────────────────────┐
              No   │  Cache key matches? │   Yes
            ┌──────┤                     ├──────┐
            │      └─────────────────────┘      │
            ▼                                   ▼
   ┌─────────────────┐               ┌─────────────────────┐
   │  Build index    │               │  Use cached index   │
   │  (parallel +    │               │  (instant startup)  │
   │  progress)      │               └─────────────────────┘
   └─────────────────┘
            │
            ▼
   ┌─────────────────┐
   │  Save to cache  │
   └─────────────────┘
```

### 2. Parallel Index Building

When cache is stale, build in parallel using `rayon`.

#### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                  OrthoUnionIndexBuilder                          │
├─────────────────────────────────────────────────────────────────┤
│  build_with_progress(callback) → Result<OrthoUnionIndex>        │
│                                                                  │
│  1. Discover sources (patches + packages)                       │
│  2. Sort sources by priority                                    │
│  3. Parallel scan each source with rayon::par_iter              │
│  4. Report progress via callback                                │
│  5. Merge partial indexes respecting priority                   │
└─────────────────────────────────────────────────────────────────┘
```

#### Partial Index

Each source scanned independently produces a `PartialIndex`:

```rust
pub struct PartialIndex {
    /// Source index (for priority ordering)
    source_idx: usize,

    /// Files found in this source
    files: HashMap<PathBuf, FileInfo>,

    /// Directories found in this source
    directories: HashMap<PathBuf, Vec<DirEntry>>,

    /// Total files scanned
    file_count: usize,
}
```

#### Parallel Scanning

```rust
use rayon::prelude::*;

let partial_indexes: Vec<PartialIndex> = sources
    .par_iter()
    .enumerate()
    .map(|(idx, source)| {
        // Report progress
        if let Some(ref cb) = progress_callback {
            cb(IndexBuildProgress {
                phase: IndexBuildPhase::Scanning,
                current_source: Some(source.name.clone()),
                sources_complete: 0, // Updated atomically
                sources_total: sources.len(),
                files_scanned: 0,
            });
        }

        scan_source_optimized(idx, source)
    })
    .collect();

// Merge in priority order (first source wins)
merge_partial_indexes(partial_indexes, sources.len())
```

### 3. Terrain/Textures Skip Optimization

The key insight: **terrain files don't need priority resolution**.

- Terrain files have unique `{row}_{col}` coordinates
- Two packages covering different regions won't have conflicting terrain files
- Only DSF files need priority resolution (patches vs packages)

#### Optimized Scanning

```rust
fn scan_source_optimized(source_idx: usize, source: &OrthoSource) -> PartialIndex {
    let mut partial = PartialIndex::new(source_idx);

    // Scan root directory
    for entry in std::fs::read_dir(&source.source_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if entry.path().is_dir() {
            match name_str.as_ref() {
                "Earth nav data" => {
                    // FULL SCAN: DSF files need priority resolution
                    partial.scan_directory_recursive(&entry.path(), Path::new("Earth nav data"))?;
                }
                "terrain" | "textures" => {
                    // LAZY: Just record directory exists, don't scan contents
                    partial.record_directory_exists(Path::new(&*name_str));
                }
                _ => {
                    // Other directories: scan normally
                    partial.scan_directory_recursive(&entry.path(), Path::new(&*name_str))?;
                }
            }
        } else {
            // Files at root level
            partial.add_file(Path::new(&*name_str), &entry.path())?;
        }
    }

    partial
}
```

#### Lazy Directory Handling

For `terrain/` and `textures/`:

```rust
/// Record that a directory exists without scanning contents.
fn record_directory_exists(&mut self, virtual_path: &Path) {
    // Add to directories map with empty entries
    // FUSE readdir will read the real directory on access
    self.directories.insert(virtual_path.to_path_buf(), LazyDir);
}
```

The FUSE filesystem handles lazy directories:

```rust
impl Fuse3OrthoUnionFS {
    fn readdir(&self, virtual_path: &Path) -> Vec<DirEntry> {
        // Check if directory is marked as lazy
        if self.index.is_lazy_directory(virtual_path) {
            // Read directly from first matching source
            if let Some(source) = self.index.resolve_directory(virtual_path) {
                return read_real_directory(&source.real_path);
            }
        }

        // Normal indexed directory
        self.index.list_directory(virtual_path)
    }
}
```

### 4. Progress Reporting

#### Progress Types

```rust
/// Progress callback for index building.
pub type IndexBuildProgressCallback = Arc<dyn Fn(IndexBuildProgress) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct IndexBuildProgress {
    /// Current phase
    pub phase: IndexBuildPhase,

    /// Source being processed (if in scanning phase)
    pub current_source: Option<String>,

    /// Sources completed so far
    pub sources_complete: usize,

    /// Total sources to process
    pub sources_total: usize,

    /// Files scanned so far (across all sources)
    pub files_scanned: usize,

    /// Whether using cached index
    pub using_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBuildPhase {
    /// Checking cache validity
    CheckingCache,

    /// Discovering patches and packages
    Discovering,

    /// Scanning source directories (parallel)
    Scanning,

    /// Merging partial indexes
    Merging,

    /// Saving to cache
    SavingCache,

    /// Complete
    Complete,
}
```

#### TUI Integration

Update `DashboardState`:

```rust
pub enum DashboardState {
    /// Building the ortho union index
    BuildingIndex(IndexBuildProgress),

    /// Loading the SceneryIndex (existing)
    Loading(LoadingProgress),

    /// Normal operation
    Running,
}
```

TUI display during index building:

```
╔════════════════════════════════════════════════════════════════╗
║  XEarthLayer v0.2.11                                           ║
╠════════════════════════════════════════════════════════════════╣
║                                                                 ║
║  Building scenery index...                                      ║
║                                                                 ║
║  Sources:                                                       ║
║    ✓ _patches/KLAX_Mesh        234 files    0.1s               ║
║    ✓ _patches/KDEN_Mesh        189 files    0.1s               ║
║    ◐ eu                        scanning...                      ║
║    ○ na                        pending                          ║
║    ○ oc                        pending                          ║
║    ○ sa                        pending                          ║
║                                                                 ║
║  [████████░░░░░░░░░░░░░░░░░░░░░░]  33%                         ║
║                                                                 ║
║  Files: 45,231  |  Elapsed: 2.3s                               ║
║                                                                 ║
╚════════════════════════════════════════════════════════════════╝
```

When cache hit:

```
╔════════════════════════════════════════════════════════════════╗
║  XEarthLayer v0.2.11                                           ║
╠════════════════════════════════════════════════════════════════╣
║                                                                 ║
║  Loading cached scenery index...                                ║
║                                                                 ║
║  ✓ Cache valid (created 2h ago)                                ║
║  ✓ 6 sources, 1,234,567 files                                  ║
║                                                                 ║
║  Ready in 0.3s                                                  ║
║                                                                 ║
╚════════════════════════════════════════════════════════════════╝
```

### 5. API Changes

#### Builder Updates

```rust
impl OrthoUnionIndexBuilder {
    /// Build with progress reporting and optional caching.
    pub fn build_with_progress(
        self,
        progress: Option<IndexBuildProgressCallback>,
        cache_path: Option<&Path>,
    ) -> std::io::Result<OrthoUnionIndex> {
        // 1. Check cache
        if let Some(path) = cache_path {
            if let Some(cached) = self.try_load_cache(path, &progress)? {
                return Ok(cached);
            }
        }

        // 2. Build index (parallel)
        let index = self.build_parallel(&progress)?;

        // 3. Save to cache
        if let Some(path) = cache_path {
            self.save_cache(path, &index)?;
        }

        Ok(index)
    }

    /// Legacy build (no progress, no cache).
    pub fn build(self) -> std::io::Result<OrthoUnionIndex> {
        self.build_with_progress(None, None)
    }
}
```

#### MountManager Updates

```rust
impl MountManager {
    pub fn mount_consolidated_ortho_with_progress(
        &mut self,
        patches_dir: &Path,
        store: &LocalPackageStore,
        service_builder: &ServiceBuilder,
        progress: Option<IndexBuildProgressCallback>,
    ) -> ConsolidatedOrthoMountResult {
        // ... existing setup ...

        let cache_path = dirs::home_dir()
            .map(|h| h.join(".xearthlayer").join("ortho_union_index.cache"));

        let index = match builder.build_with_progress(progress, cache_path.as_deref()) {
            Ok(i) => i,
            Err(e) => return ConsolidatedOrthoMountResult::failure(...),
        };

        // ... rest of mounting ...
    }
}
```

## Implementation Plan

### Phase 1: Add Dependencies and Types

1. Add `rayon` and `bincode` to `Cargo.toml`
2. Add `IndexBuildProgress`, `IndexBuildPhase` types
3. Add `PartialIndex` struct
4. Add `IndexCache`, `IndexCacheKey` types

### Phase 2: Implement Optimized Scanning

1. Implement `scan_source_optimized()` with terrain skip
2. Add `LazyDirectory` marker for deferred scanning
3. Update FUSE to handle lazy directories

### Phase 3: Implement Parallel Building

1. Implement `build_parallel()` using rayon
2. Implement `merge_partial_indexes()`
3. Add atomic progress counters

### Phase 4: Implement Caching

1. Implement `try_load_cache()`
2. Implement `save_cache()`
3. Implement cache key computation
4. Add cache path to config

### Phase 5: CLI/TUI Integration

1. Add progress channel from library to CLI
2. Update `DashboardState` with `BuildingIndex` variant
3. Implement TUI rendering for index building progress
4. Wire up progress callback in `run.rs`

### Phase 6: Testing

1. Unit tests for PartialIndex
2. Unit tests for cache key computation
3. Integration tests for parallel building
4. Performance benchmarks

## Expected Improvements

| Scenario | Before | After |
|----------|--------|-------|
| Cold start (no cache) | ~10+ minutes | ~30 seconds |
| Warm start (cache hit) | ~10+ minutes | ~0.5 seconds |
| Progress visibility | None | Real-time TUI |
| CPU utilization during build | ~3% (single-core) | ~80% (multi-core) |

## Files to Modify

### New Files

| File | Purpose |
|------|---------|
| `ortho_union/progress.rs` | Progress types and callbacks |
| `ortho_union/partial.rs` | PartialIndex for parallel scanning |
| `ortho_union/cache.rs` | Index caching logic |

### Modified Files

| File | Changes |
|------|---------|
| `ortho_union/index.rs` | Add lazy directory support |
| `ortho_union/builder.rs` | Add `build_with_progress()`, parallel scanning |
| `ortho_union/mod.rs` | Export new types |
| `manager/mounts.rs` | Use new builder API with progress |
| `fuse/fuse3/ortho_union_fs.rs` | Handle lazy directories in readdir |
| `xearthlayer/Cargo.toml` | Add rayon, bincode dependencies |
| `xearthlayer-cli/src/ui/dashboard/state.rs` | Add BuildingIndex state |
| `xearthlayer-cli/src/ui/dashboard/render.rs` | Render index building progress |
| `xearthlayer-cli/src/commands/run.rs` | Wire up progress callback |

## Dependencies to Add

```toml
[dependencies]
rayon = "1.10"
bincode = "1.3"
```

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Cache corruption | Validate cache key before use; delete and rebuild on error |
| Cache disk space | Use bincode (compact); add config option for cache location |
| Stale cache | Include XEL version in cache key; invalidate on upgrade |
| Parallel race conditions | Use atomic counters for progress; rayon handles parallelism safely |
