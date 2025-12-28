# XEarthLayer Cache Design

## Overview

XEarthLayer implements a two-tier caching system to minimize expensive satellite imagery downloads and DDS encoding operations. The cache architecture balances performance, memory constraints, and persistent storage across flight simulator sessions.

## Architecture

### Two-Tier Cache System

```
┌─────────────────────────────────────────────────────────────┐
│                     FUSE Read Request                       │
│                  (e.g., +37-122_BI15.dds)                   │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
         ┌─────────────────────────────┐
         │   Inode Cache Lookup        │
         │   (inode → coordinates)     │
         └─────────────┬───────────────┘
                       │
                       ▼
         ┌─────────────────────────────┐
         │   Memory Cache Check        │
         │   (HashMap<u64, Vec<u8>>)   │
         └─────────────┬───────────────┘
                       │
            ┌──────────┴──────────┐
            │                     │
            ▼                     ▼
    ┌───────────────┐     ┌──────────────────┐
    │  MEMORY HIT   │     │   MEMORY MISS    │
    │               │     │                  │
    │ Return data   │     │ Check disk cache │
    │ Update stats  │     └────────┬─────────┘
    │               │              │
    │ ✓ <1ms        │    ┌─────────┴──────────┐
    └───────────────┘    │                    │
                         ▼                    ▼
                 ┌──────────────┐     ┌─────────────────┐
                 │   DISK HIT   │     │   DISK MISS     │
                 │              │     │                 │
                 │ 1. Read file │     │ 1. Convert to   │
                 │ 2. Add to    │     │    tile coords  │
                 │    memory    │     │ 2. Download     │
                 │ 3. Return    │     │    (256 chunks) │
                 │              │     │ 3. Encode DDS   │
                 │ ✓ ~10-50ms   │     │ 4. Cache (both) │
                 └──────────────┘     │ 5. Return data  │
                                      │                 │
                                      │ ⏱ ~2 seconds   │
                                      └─────────────────┘
```

## Design Decisions

### Provider-Specific Caching

**Decision**: Cache tiles separately for each imagery provider (Bing, Google, etc.)

**Rationale**:
1. **Correctness**: Same geographic coordinates from different providers yield different imagery
   - Bing Maps: Aerial photography (may be older, different seasons, different resolution)
   - Google Maps: Satellite imagery (different processing, quality, coverage)
2. **No cache poisoning**: Switching providers doesn't serve stale imagery from the wrong provider
3. **Quality comparison**: Users can maintain caches for multiple providers simultaneously
4. **Clear semantics**: Easy to manage and understand which provider generated which tiles

**Trade-off**: Uses more disk space if user experiments with multiple providers, but disk space is cheap and correctness is critical for visual consistency in flight simulation.

### Cache Key Strategy

**Decision**: Hierarchical directory structure based on tile coordinates

**Structure**:
```
<cache_dir>/               # Default: ~/.cache/xearthlayer
  <provider>/              # "bing", "google", etc.
    <format>/              # "BC1" or "BC3"
      <zoom>/              # Zoom level (e.g., "15")
        <row>/             # Tile row (e.g., "12754")
          <format>_<zoom>_<row>_<col>.dds
```

**Example**:
```
~/.cache/xearthlayer/
  bing/
    BC1/
      15/
        12754/
          BC1_15_12754_5279.dds
          BC1_15_12754_5280.dds
  google/
    BC1/
      15/
        12754/
          BC1_15_12754_5279.dds
```

**Rationale**:
1. **Filesystem-friendly**: Distributes files across directories (~360 files per directory max)
2. **Self-documenting**: Filenames include format, zoom, and coordinates
3. **Easy management**: Clear specific regions (`rm -rf cache/bing/BC1/15/12754/`)
4. **Natural hierarchy**: Matches internal TileCoord structure
5. **Portable**: Files can be moved/shared with full context preserved in filename

## Cache Tier 1: Memory Cache

### Purpose
Fast, in-memory storage of recently accessed DDS tiles to serve repeated requests without disk I/O.

### Implementation
```rust
struct MemoryCache {
    /// Cache mapping inode to complete DDS file data
    cache: Arc<Mutex<HashMap<u64, CacheEntry>>>,
    /// Configuration
    config: MemoryCacheConfig,
    /// Statistics
    stats: Arc<Mutex<CacheStats>>,
}

struct CacheEntry {
    data: Vec<u8>,
    last_accessed: Instant,
    access_count: u64,
}

struct MemoryCacheConfig {
    max_size_bytes: usize,        // Default: 2GB (2_147_483_648)
    max_entries: usize,            // Calculated: ~180 tiles @ 11MB each
    eviction_policy: EvictionPolicy,
}

enum EvictionPolicy {
    LRU,  // Least Recently Used (default)
}
```

### Characteristics

| Metric | Value | Notes |
|--------|-------|-------|
| **Default Size** | 2 GB | Configurable via `--memory-cache-size` |
| **Max Tiles (BC1)** | ~180 tiles | 11 MB per tile |
| **Max Tiles (BC3)** | ~90 tiles | 22 MB per tile |
| **Eviction** | LRU | Evict oldest on size limit |
| **Hit Time** | <1 ms | Direct memory access |
| **Miss Penalty** | 10-2000 ms | Depends on disk/network |

### Eviction Strategy

**When**: Memory cache exceeds `max_size_bytes`

**How**: LRU (Least Recently Used) on `put()`
1. When adding a new entry would exceed the limit
2. Sort entries by `last_accessed` timestamp
3. Evict oldest entries until space is available
4. Log eviction statistics

**Trigger**: Eviction happens synchronously during `put()` operations. This is efficient because:
- Memory operations are fast (<1ms to sort and evict)
- No separate daemon thread needed for memory cache
- Cache size is always maintained under limit
- Stale entries remain valid until space is needed

## Cache Tier 2: Disk Cache

### Purpose
Persistent storage of DDS tiles across sessions to avoid re-downloading and re-encoding.

### Directory Structure

```
<cache_dir>/               # Default: ~/.cache/xearthlayer
  bing/                    # Provider name
    BC1/                   # DDS format (BC1 or BC3)
      15/                  # Zoom level
        12754/             # Tile row
          BC1_15_12754_5279.dds    # Format_Zoom_Row_Col.dds
          BC1_15_12754_5280.dds
          BC1_15_12754_5281.dds
        12755/
          BC1_15_12755_5279.dds
          ...
      16/
        25508/
          BC1_16_25508_10376.dds
          ...
    BC3/                   # Separate tree for BC3 format
      15/
        12754/
          BC3_15_12754_5279.dds
          ...
  google/                  # Separate tree for Google provider
    BC1/
      15/
        12754/
          BC1_15_12754_5279.dds
          ...
```

### Filename Format

**Pattern**: `<FORMAT>_<ZOOM>_<ROW>_<COL>.dds`

**Examples**:
- `BC1_15_12754_5279.dds` - BC1, zoom 15, row 12754, col 5279
- `BC3_16_25508_10376.dds` - BC3, zoom 16, row 25508, col 10376

**Benefits**:
- Self-documenting (format, zoom, coordinates visible in filename)
- Portable (files can be moved/shared with context preserved)
- Debuggable (easy to identify what each file represents)
- Provider implicit from directory path (cleaner filenames)

### Path Construction

```rust
fn cache_path(
    cache_dir: &Path,
    provider: &str,
    tile: &TileCoord,
    format: DdsFormat,
) -> PathBuf {
    let format_str = match format {
        DdsFormat::BC1 => "BC1",
        DdsFormat::BC3 => "BC3",
    };

    cache_dir
        .join(provider)
        .join(format_str)
        .join(tile.zoom.to_string())
        .join(tile.row.to_string())
        .join(format!(
            "{}_{}_{}_{}.dds",
            format_str, tile.zoom, tile.row, tile.col
        ))
}

// Example: ~/.cache/xearthlayer/bing/BC1/15/12754/BC1_15_12754_5279.dds
```

### Implementation

```rust
struct DiskCache {
    /// Cache directory root
    cache_dir: PathBuf,
    /// Active provider
    provider: String,
    /// Index of cached tiles (inode → path)
    index: Arc<Mutex<HashMap<u64, PathBuf>>>,
    /// Configuration
    config: DiskCacheConfig,
    /// Channel for async writes
    write_tx: mpsc::Sender<DiskWriteRequest>,
    /// Statistics
    stats: Arc<Mutex<CacheStats>>,
}

struct DiskCacheConfig {
    max_size_bytes: usize,        // Default: 20GB (21_474_836_480)
    cache_dir: PathBuf,            // Default: ~/.cache/xearthlayer
    max_age_days: Option<u32>,     // Optional: clear tiles older than N days
}

struct DiskWriteRequest {
    inode: u64,
    provider: String,
    tile: TileCoord,
    format: DdsFormat,
    data: Vec<u8>,
}
```

### Characteristics

| Metric | Value | Notes |
|--------|-------|-------|
| **Default Size** | 20 GB | Configurable via `--disk-cache-size` |
| **Max Tiles (BC1)** | ~1,800 tiles/provider | 11 MB per tile |
| **Max Tiles (BC3)** | ~900 tiles/provider | 22 MB per tile |
| **Eviction** | LRU + Age | Oldest files first |
| **Hit Time** | 10-50 ms | SSD: ~10ms, HDD: ~50ms |
| **Miss Penalty** | ~2 seconds | Network + encode |
| **Persistence** | Across sessions | Survives FUSE unmount |

### Write Strategy

**Approach**: Write-through (immediate, asynchronous)

**Flow**:
1. Tile is generated (downloaded + encoded)
2. Added to memory cache
3. Sent to disk write queue (non-blocking channel)
4. Background thread writes to disk
5. Index updated on successful write

**Benefits**:
- FUSE read remains responsive (write is async)
- No data loss on crash (written immediately)
- No "dirty" tracking needed (always consistent)
- DDS files are expensive to regenerate (~2s), worth preserving immediately

### Eviction Strategy

**When**: Disk cache exceeds `max_size_bytes`

**How**:
1. Sort cached files by modification time (oldest first)
2. Delete oldest files until cache is at 90% of limit
3. Update index and statistics
4. Log eviction activity

**Trigger**: Two mechanisms ensure disk cache stays under limit:

1. **Startup Eviction**: When `DiskCache` is created, it scans the cache directory and immediately evicts if over limit. This handles cleanup from previous sessions before the simulator starts requesting tiles.

2. **Background Daemon**: A `DiskCacheDaemon` thread runs every 60 seconds (configurable via `daemon_interval_secs`), checking the cache size and evicting LRU entries if needed. This keeps eviction off the critical path of serving tiles to X-Plane.

**Rationale for Background GC**:
- Disk I/O for eviction is slow (scanning directories, deleting files)
- Eviction during `put()` would block tile delivery to X-Plane
- Background thread allows eviction to happen asynchronously
- 60-second interval is sufficient since tiles are ~11MB each

## Async Architecture

### Design Decision

**Decision**: Memory cache uses moka's automatic LRU eviction; disk cache uses an async tokio daemon.

**Rationale**:
- Memory cache: `moka::future::Cache` provides efficient, lock-free LRU eviction
- Disk eviction is slow (filesystem I/O) - must not block tile delivery
- Uses tokio async tasks instead of OS threads for better integration with async pipeline
- Clean shutdown via `CancellationToken` for graceful termination

### FUSE Async Operations
- Handles FUSE operations asynchronously via fuse3
- Queries memory cache (moka provides sub-millisecond access)
- Triggers disk reads via async I/O with concurrency limiting
- Writes to disk asynchronously (write-through strategy)

### Disk Cache Eviction Daemon
- Spawned as tokio async task via `runtime_handle.spawn()`
- Runs eviction check every 60 seconds (configurable via `daemon_interval_secs`)
- Uses `CancellationToken` for clean shutdown signaling
- Filesystem operations wrapped in `spawn_blocking` to avoid blocking the async runtime
- LRU eviction based on file modification time (mtime)
- Evicts to 90% of limit to provide headroom for new writes

### Implementation

```rust
/// Disk cache eviction daemon configuration.
pub struct DiskCacheConfig {
    pub cache_dir: PathBuf,
    pub max_size_bytes: usize,
    pub daemon_interval_secs: u64,  // Default: 60
    pub max_age_days: Option<u32>,
}

/// Run the disk cache eviction daemon.
pub async fn run_eviction_daemon(
    config: DiskCacheConfig,
    cancellation: CancellationToken,
) {
    // Initial eviction check on startup
    if let Some(result) = evict_if_over_limit(&config).await {
        log_eviction_result(&result);
    }

    // Periodic eviction loop
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(config.daemon_interval_secs)) => {
                if let Some(result) = evict_if_over_limit(&config).await {
                    log_eviction_result(&result);
                }
            }
        }
    }
}
```

### Lifecycle

1. **Startup**: `run.rs` spawns eviction daemon with `runtime_handle.spawn()`
2. **Initial Check**: Immediately evicts if over limit on startup
3. **Running**: Daemon wakes every 60s, checks disk size, evicts if needed
4. **Shutdown**: `CancellationToken` signals daemon to stop gracefully

## Cache Statistics

### Tracked Metrics

```rust
struct CacheStats {
    // Memory cache
    memory_hits: u64,
    memory_misses: u64,
    memory_size_bytes: usize,
    memory_entry_count: usize,
    memory_evictions: u64,

    // Disk cache
    disk_hits: u64,
    disk_misses: u64,
    disk_size_bytes: usize,
    disk_entry_count: usize,
    disk_evictions: u64,
    disk_writes: u64,
    disk_write_failures: u64,

    // Downloads
    downloads: u64,
    download_failures: u64,
    bytes_downloaded: u64,

    // Timing
    last_updated: Instant,
}

impl CacheStats {
    fn memory_hit_rate(&self) -> f64 {
        let total = self.memory_hits + self.memory_misses;
        if total == 0 { 0.0 } else { self.memory_hits as f64 / total as f64 }
    }

    fn overall_hit_rate(&self) -> f64 {
        let hits = self.memory_hits + self.disk_hits;
        let total = hits + self.disk_misses;
        if total == 0 { 0.0 } else { hits as f64 / total as f64 }
    }
}
```

### Statistics Reporting

**1. Startup Banner**
```
XEarthLayer FUSE Server v0.1.0
=======================

Mountpoint: /tmp/xearthlayer-mount
DDS Format: BC1
Mipmap Levels: 5

Cache Configuration:
  Memory: 2.0 GB (max ~180 tiles)
  Disk:   20.0 GB (max ~1,800 tiles) at ~/.cache/xearthlayer

Provider: Bing Maps (no API key required)

Mounting filesystem...
```

**2. Periodic Logging (every 60s)**
```
[STATS] Memory: 45 tiles (512 MB), hits: 1,234 (89.2%), evictions: 12
[STATS] Disk: 156 tiles (1.7 GB), hits: 89 (12.1%), writes: 67, evictions: 3
[STATS] Downloads: 23 (252 MB), failures: 2
[STATS] Overall hit rate: 98.3%
```

**3. Virtual Stats File**

**Decision**: Provide `/stats` virtual file in FUSE filesystem for real-time monitoring

```bash
# User can read at any time
cat /tmp/xearthlayer-mount/stats

XEarthLayer Cache Statistics
Generated: 2025-11-25 18:45:32
Provider: bing

MEMORY CACHE
  Entries:     45 / 180
  Size:        512 MB / 2.0 GB (25.6%)
  Hits:        1,234
  Misses:      149
  Hit Rate:    89.2%
  Evictions:   12

DISK CACHE
  Entries:     156 / 1,800
  Size:        1.7 GB / 20.0 GB (8.5%)
  Hits:        89
  Misses:      23
  Hit Rate:    79.5%
  Writes:      67
  Failures:    0
  Evictions:   3

DOWNLOADS
  Total:       23
  Failures:    2
  Bytes:       252 MB

OVERALL
  Hit Rate:    98.3%
  Uptime:      2h 15m
```

## Configuration

### CLI Arguments

```bash
xearthlayer start \
  --source /path/to/scenery \
  --provider bing \                      # Imagery provider
  --memory-cache-size 2G \               # Memory cache size (default: 2G)
  --disk-cache-size 20G \                # Disk cache size (default: 20G)
  --cache-dir ~/.cache/xearthlayer \     # Cache directory (default: ~/.cache/xearthlayer)
  --disk-cache-max-age 30                # Optional: evict files older than 30 days
```

### Environment Variables

```bash
XEARTHLAYER_MEMORY_CACHE_SIZE=2G
XEARTHLAYER_DISK_CACHE_SIZE=20G
XEARTHLAYER_CACHE_DIR=~/.cache/xearthlayer
```

## Cache Coherency

### Scenario: Format Change

**Problem**: User switches from BC1 to BC3

**Solution**: Separate directory trees
- BC1 tiles cached in `cache/<provider>/BC1/`
- BC3 tiles cached in `cache/<provider>/BC3/`
- Both can coexist
- No conflicts

### Scenario: Provider Change

**Problem**: User switches from Bing to Google

**Solution**: Provider-specific directories
- Bing tiles in `cache/bing/`
- Google tiles in `cache/google/`
- Both preserved simultaneously
- No cache clearing needed
- Can switch back and forth instantly
- Enables quality comparison

### Scenario: Tile Update

**Problem**: Satellite imagery updated by provider

**Solution**: Age-based eviction (optional)
- `--disk-cache-max-age 30` clears tiles older than 30 days
- User can manually clear: `rm -rf ~/.cache/xearthlayer/bing/BC1/15/`
- Future: Could add tile versioning

### Scenario: Corruption

**Problem**: Partial write or corrupted file

**Solution**:
- Verify file size matches expected size on read
- On read error, delete corrupted file and re-download
- Log corruption events

## Performance Expectations

### Cold Start (Empty Cache)
```
Request: +37-122_BI15.dds
├─ Memory: MISS
├─ Disk:   MISS
└─ Download + Encode: ~2 seconds
   ├─ Download: ~1.5s (256 chunks)
   └─ Encode:   ~0.3s (BC1)
Result: 2 seconds
```

### Warm Cache (In Memory)
```
Request: +37-122_BI15.dds
└─ Memory: HIT
Result: <1 ms
```

### Disk Cache Hit
```
Request: +37-122_BI15.dds
├─ Memory: MISS
└─ Disk:   HIT (~10-50ms read)
   └─ Add to memory cache
Result: ~10-50 ms
```

### Typical Session (Flying over San Francisco)
```
Tiles in view: ~9 tiles (3×3 grid at zoom 15)
First load:    ~18 seconds (9 × 2s)
Memory hits:   ~100+ requests (camera panning, zoom changes)
Performance:   <1ms per request after initial load
```

## Provider Comparison Use Case

A key design goal is enabling users to compare imagery quality between providers:

```bash
# Day 1: Try Bing Maps (free)
xearthlayer start --source /path/to/scenery --provider bing
# Flies around Bay Area, caches 100 tiles (~1.1 GB)

# Day 2: Compare with Google Maps
xearthlayer start --source /path/to/scenery --provider google
# Bing cache preserved at cache/bing/
# Google tiles cached to cache/google/
# Can instantly switch back to Bing without re-downloading

# Cleanup: Prefer Google, remove Bing cache
rm -rf ~/.cache/xearthlayer/bing/
# Frees 1.1 GB
```

## Implementation Status

### Phase 1: Basic Memory Cache ✅
- ✅ In-memory cache using moka::future::Cache
- ✅ CacheKey with provider/format/tile coordinates

### Phase 2: Memory Cache with LRU ✅
- ✅ Memory size limits (2GB default, configurable)
- ✅ Automatic LRU eviction via moka weigher
- ✅ Statistics tracking (hits, misses, evictions)

### Phase 3: Disk Cache ✅
- ✅ Hierarchical disk cache structure (provider/format/zoom/row)
- ✅ Async disk writes with concurrency limiting
- ✅ Disk cache with size limits (20GB default, configurable)
- ✅ LRU eviction based on file mtime
- ✅ Startup eviction (immediate check before simulator starts)
- ✅ Background eviction daemon (60-second interval, async tokio task)
- ✅ Clean daemon shutdown via CancellationToken
- ✅ Eviction to 90% of limit (10% headroom)

### Phase 4: Advanced Features (Partial)
- ❌ Virtual `/stats` file (planned)
- ✅ Configurable cache sizes via config.ini
- ❌ Age-based eviction (config exists, not implemented)
- ❌ Corruption detection/recovery (not implemented)
- ✅ Cache pre-warming via `--airport` flag

## Directory Size Estimates

### Example: Regional Flying (San Francisco Bay Area)

**Coverage**: 1° latitude × 1° longitude at zoom 15

**Calculation**:
- At zoom 15, 1° ≈ 512 tiles (equator)
- Bay Area: ~500 tiles needed
- BC1 size: 500 × 11 MB = 5.5 GB
- BC3 size: 500 × 22 MB = 11 GB

**Cache Requirements**:
- Memory (2GB): Holds ~180 tiles (enough for active flying area)
- Disk (20GB): Holds entire region + additional areas
- Can cache both Bing and Google (40GB total) if desired

### Example: Cross-Country Flight (Coast to Coast)

**Coverage**: 40° longitude × 10° latitude at zoom 15

**Calculation**:
- ~20,000 tiles
- BC1 size: 20,000 × 11 MB = 220 GB
- BC3 size: 20,000 × 22 MB = 440 GB

**Cache Requirements**:
- Needs larger disk cache (configure with `--disk-cache-size 250G`)
- Or use selective caching (only cache tiles actually viewed)
- Memory cache (2GB) still sufficient for active area

## Maintenance

### Manual Cache Management

```bash
# View total cache size
du -sh ~/.cache/xearthlayer

# View per-provider size
du -sh ~/.cache/xearthlayer/*/

# Clear all cache
rm -rf ~/.cache/xearthlayer

# Clear specific provider
rm -rf ~/.cache/xearthlayer/bing

# Clear specific zoom level (e.g., Bing BC1 zoom 15)
rm -rf ~/.cache/xearthlayer/bing/BC1/15

# Clear specific region (e.g., row 12754)
rm -rf ~/.cache/xearthlayer/bing/BC1/15/12754

# Find largest directories
du -sh ~/.cache/xearthlayer/*/* | sort -h
```

### Automated Cleanup

The disk cache eviction daemon (`cache/disk_eviction.rs`) runs automatically:

**On Startup**:
- Daemon spawned via `runtime_handle.spawn()` in `run.rs`
- Immediate eviction check before the 60-second loop begins
- Evicts to 90% of limit if over, leaving headroom for new writes
- Logs scan results: files collected, collectable size, target size

**Background Daemon** (every 60 seconds):
- Checks if cache exceeds `max_size_bytes`
- Collects all cache files and sorts by mtime (oldest first)
- Deletes oldest files until at 90% of limit
- Cleans up empty directories after eviction
- Logs eviction statistics: files deleted, bytes freed, duration
- Uses minimal CPU when cache is under limit (just stat calls)

**On Shutdown**:
- `CancellationToken` signals daemon to stop
- Daemon completes current operation and exits cleanly
- No orphaned tasks or resources

## Future Enhancements

1. **Compression**: Store DDS files compressed on disk (zstd) to reduce size
2. **Chunk caching**: Cache JPEG chunks instead of DDS (like AutoOrtho) for format flexibility
3. **Predictive loading**: Pre-fetch tiles based on flight path and velocity
4. **Network sync**: Share cache across machines on local network
5. **Smart eviction**: Weight eviction by access frequency, not just recency
6. **Tile versioning**: Track provider updates and invalidate old imagery
7. **HTTP cache server**: Serve cached tiles to other XEarthLayer instances
8. **Cache import/export**: Share caches between users (scenery pack style)

## References

- AutoOrtho cache implementation: `getortho.py` (TileCacher class)
- Web Mercator projection: https://en.wikipedia.org/wiki/Web_Mercator_projection
- LRU cache algorithms: https://en.wikipedia.org/wiki/Cache_replacement_policies#LRU
- Filesystem performance: https://en.wikipedia.org/wiki/Comparison_of_file_systems
