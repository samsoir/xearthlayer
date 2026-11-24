# XEarthLayer Cache Design

## Overview

Generating an ortho tile costs 256 chunk downloads, a 4096×4096 assembly and a
BC1 encode. The cache keeps each of those results so the work is done once.
There are three tiers for ortho imagery, plus a separate disk tier for night
lighting:

| Tier | Holds | Key | Where | Bounded by | Eviction |
|------|-------|-----|-------|------------|----------|
| Memory | Encoded DDS tiles | `tile:{zoom}:{row}:{col}` | Process memory | `cache.memory_size` | moka, TinyLFU |
| DDS disk | Encoded DDS tiles | `tile:{zoom}:{row}:{col}` | `{cache_dir}/{provider}/dds/` | share of `cache.disk_size` | LRU, GC job |
| Chunk disk | Raw imagery chunks as downloaded | `chunk:{zoom}:{tile_row}:{tile_col}:{chunk_row}:{chunk_col}` | `{cache_dir}/{provider}/chunks/` | share of `cache.disk_size` | LRU, GC job |
| Lit disk (night lighting only) | Generated lit payloads and road extractions | `lit:{zoom}:{row}:{col}`, `roads:{lat}:{lon}:{hex16}` | `{cache_dir}/night/` | 5% of `cache.disk_size` | LRU, GC job |

The memory tier is a staging buffer for active FUSE reads. The DDS disk tier is
the retention layer: a hit there serves a tile without re-encoding it. The chunk
disk tier saves the downloads when a tile has to be encoded again. A tile that is
in none of them is downloaded.

`{zoom}`, `{row}` and `{col}` in `tile:` and `chunk:` keys are tile coordinates:
X-Plane's file `25312_10528_BI16.dds` names chunk coordinates at zoom 16, and
`fuse::fuse3::chunk_to_tile_coords` turns it into tile (1582, 658) at zoom 12
(`CHUNK_ZOOM_OFFSET` is 4). `lit:` keys use the file's own coordinates.

Every disk tier is the same type, `DiskCacheProvider`, with a validator chosen by
its `DiskTier`. The memory tier is `MemoryCacheProvider`. Both implement the
`Cache` trait (string keys, `Bytes` values) and run inside a `CacheService`.

## Read Path

FUSE sends a demand request through `DdsClient` to the demand lane's
`ExecutorDaemon`. Prefetch and prewarm submit to the prefetch lane's daemon. Both
daemons check the same memory and DDS disk tiers before they create a job
(`executor/daemon.rs`):

```text
DDS request (FUSE, prefetch or prewarm)
  |
  +-- memory tier hit ----------------------------> serve
  |
  +-- DDS disk hit -------------------------------> serve, and promote to memory
  |                                                 (spawned, fire-and-forget)
  |
  +-- same tile already generating on this lane --> wait for that job (coalescer)
  |
  +-- new DdsGenerateJob
        DownloadChunksTask, per chunk:
          chunk disk hit -> use it
          miss           -> download, then write to chunk disk (spawned)
        BuildAndCacheDdsTask:
          assemble, encode, then write memory + DDS disk (spawned)
          if every chunk was obtained (#180)
        -> serve the encoded bytes from the job result
```

The job returns the encoded tile in its output. The requester never reads it back
from the memory tier, because the memory write is spawned and may not have landed.

Prefetch-lane tile jobs start with `CachedTileProbeTask`
(`tasks/cached_tile_probe.rs`). It checks memory and the DDS disk index when the
job is admitted, and on a hit the download and build tasks return at once (#296).
A job can wait in the prefetch queue while FUSE generates the same tile, so this
check turns the queued copy into a no-op.

The prefetch coordinator checks the memory tier and the DDS disk tier before it
submits a tile; prewarm checks the memory tier. The DDS disk check,
`DdsDiskCacheBridge::tile_exists_blocking`, answers from the LRU index without
touching the filesystem.

## Write Path

Each tier has its own completeness rule.

**Chunk disk.** `DownloadChunksTask` writes a chunk after a successful HTTP
download, in a spawned task (`spawn_cache_write`). A chunk that failed every
attempt is never written, so the next request for that tile retries it. Chunks
already downloaded when a job is killed keep their writes (#300).

**Memory and DDS disk.** `BuildAndCacheDdsTask` writes both, in two spawned tasks,
only when `ChunkResults::is_complete()` is true: 256 chunks obtained and no
failures. A tile with any failed chunk has magenta regions. It is served to
X-Plane so the sim does not stall, and never persisted (#180). Completeness counts
chunks obtained, not chunks decoded: a chunk that is present but fails to decode
is drawn magenta in a tile that is still written.

**Promotion.** A DDS disk hit in the daemon is written back to the memory tier.

**Lit disk.** Written by `NightLitSource` and the road service, not by the ortho
executor. See [night-lighting-design.md](night-lighting-design.md) for when a lit
payload is kept.

Nothing waits for a spawned write. The writes do not go through the executor's
resource pools. `tokio::fs` runs them on the blocking pool, whose size is capped
(#227).

## On-Disk Layout

A disk entry's file name is its key with `:` replaced by `_` and `.cache`
appended (`LruIndex::key_to_filename`). The file lives in a 1°×1° region
directory named like an X-Plane DSF tile (`lru_index::key_to_region`):

- `tile:`, `chunk:` and `lit:` keys: the region holding the centre of the tile
  the key names. A chunk lands in the same region as its tile.
- `roads:{lat}:{lon}:...` keys: the DSF tile the key names.

`cache::layout` (`cache/layout.rs`) is the one statement of where each tier
lives; the code that creates the tiers and the code that finds them again
(`cache clear`, `cache stats`, the legacy cleanup) both use it. For the tile in
the example above, with Bing:

```text
{cache_dir}/
  Bing Maps/
    dds/
      +37-123/
        tile_12_1582_658.cache            DDS disk tier, 11,184,952 bytes
    chunks/
      +37-123/
        chunk_12_1582_658_0_0.cache       chunk disk tier, one file per chunk
        ...
        chunk_12_1582_658_15_15.cache
  night/                                  lit disk tier, night.enabled only
    +37-123/
      lit_16_25312_10528.cache
```

`{cache_dir}` is `cache.directory`. Its default is the platform cache directory
(`paths::tile_cache_dir()`; see [directory-layout.md](../directory-layout.md)).

`{provider}` is the provider's display name, as returned by `name()`: `Bing Maps`,
`Google GO2`, `Google Maps`, `Apple Maps`, `ArcGIS`, `MapBox` or `USGS`. It is not
the `provider.type` value.

Each provider has its own DDS and chunk tiers; chunk keys carry no provider, the
directory does (#398). Before 0.5.1 the chunk tier was `{cache_dir}` itself:
region directories directly in the cache root, shared by every provider. Those
are no longer indexed, so `CacheLayer::spawn_legacy_chunk_cleanup` deletes them
(`cache::legacy::remove_legacy_chunk_regions`) on a blocking thread at each start
with disk caching on, without delaying startup. It removes only directories in
the root whose names are region names (`layout::is_region_dir_name`, the shape
`coord::format_dsf_name` writes) and that hold nothing but `*.cache` and `*.tmp`
files (`layout::holds_only_cache_entries`), never follows a symbolic link, logs
"Removed legacy chunk cache regions from the cache root" at info, and treats a
failed deletion as a warning. A region-named directory holding anything else is
left alone: `cache.directory` may also hold the user's own folders, scenery
among them.

A DDS tile is 11,184,952 bytes: 4096×4096 BC1 with the full 13-level mipmap chain
(`integrity::EXPECTED_DDS_SIZE`). BC1 is the only format (#395). A chunk is the
provider's 256×256 image as downloaded, on the order of 14 KB.

The same directory also holds files that are not cache tiers: the scenery index
cache, the ortho union index cache and, by default, the package download staging
directory `tmp/` (`paths::temp_dir()`). These paths come from the platform cache
directory, not from `cache.directory`, so they share it only when
`cache.directory` is left at its default.

## Budgets

`service/cache_layer.rs` `disk_budgets` splits `cache.disk_size`:

1. With `night.enabled`, the lit tier takes `LIT_DISK_SHARE` (5%) first.
2. `cache.dds_disk_ratio` (default 0.6) of the rest goes to the DDS disk tier.
3. The remainder goes to the chunk disk tier.

With the defaults (`disk_size` 20 GiB, `night.enabled` true) that is 1 GiB lit,
about 11.4 GiB DDS (about 1,094 tiles) and about 7.6 GiB chunks. With night
lighting off it is 12 GiB DDS and 8 GiB chunks. The lit share is taken whenever
`night.enabled` is set, whether or not lit textures end up being served.

The memory tier is `cache.memory_size`, default 512 MiB (`DEFAULT_MEMORY_CACHE_SIZE`),
weighted by entry size: about 47 tiles. Sizes in the config file use binary
units (`GB` is 2^30 bytes).

Night lighting keeps its own in-memory caches (lit payloads, road tiles) outside
`cache.memory_size`; see [night-lighting-design.md](night-lighting-design.md).

## Eviction

### Memory tier

`MemoryCacheProvider` wraps `moka::future::Cache` with a weigher that charges
each entry its byte length and `max_capacity` set to the budget. No eviction
policy is configured, so moka uses its default, TinyLFU: LRU eviction combined
with an admission filter based on each key's estimated access frequency. When
the cache is full a new entry can be refused in favour of a more popular one.
A refused tile is still written to the DDS disk tier.

### Disk tiers

Each `DiskCacheProvider` keeps an in-memory `LruIndex` (`cache/lru_index.rs`): a
`DashMap` from key to size and last access time, plus atomic totals. `set`
records an entry, `get` touches it, and `delete`, a validator rejection or a
`NotFound` read removes it. `contains` answers from the index alone, with no
filesystem call (#172); a stale entry is corrected by the next `get`.

The index is rebuilt at every start with each file's mtime as its last access
time. Accesses are not written to disk, so across restarts the order is by write
time.

Garbage collection runs on the executor, not inside the provider:

- `XEarthLayerService::start` (`service/facade.rs`) spawns one
  `GcSchedulerDaemon` (`cache/gc_scheduler.rs`) per disk tier: chunk, DDS, and
  lit when it exists.
- Every 60 seconds (`DEFAULT_CHECK_INTERVAL_SECS`; the first check comes one
  interval after start) the daemon compares the index total with the tier's
  budget. Above 95% (`DEFAULT_TRIGGER_THRESHOLD`) it submits a `CacheGcJob` with
  a target of 80% (`DEFAULT_TARGET_RATIO`).
- `CacheGcJob` (`jobs/cache_gc.rs`) takes the least recently used entries that
  were last accessed at least 60 seconds ago (`DEFAULT_MIN_AGE_SECS`) until it has
  enough bytes, and splits them into `CacheGcBatchTask`s of 25
  (`DEFAULT_BATCH_SIZE`). Each batch is a `DiskIO` task that deletes its files,
  removes them from the index and checks for cancellation between files. The
  job's priority is `HOUSEKEEPING` and its error policy `ContinueOnError`.
- The job is submitted with `try_submit` to the prefetch lane's local executor
  (`LocalPrefetchLane::job_submitter`). GC never uses the demand lane's pools, and
  a GC job waits for admission while demand holds the prefetch lane's admission
  gate closed. If the queue is full, the round is skipped and the next check
  tries again.

GC deletes files only. Empty region directories stay.

A cache over budget at start is not trimmed before the first check.

## Lifecycle

`XEarthLayerService::start` builds the metrics system first, then
`CacheLayer::new` (`service/cache_layer.rs`), which:

1. starts the memory `CacheService`;
2. computes the disk budgets;
3. with disk caching on, starts the DDS disk service at
   `{cache_dir}/{provider}/dds/`, the chunk disk service at
   `{cache_dir}/{provider}/chunks/`, and with night lighting the lit service at
   `{cache_dir}/night/`;
4. wraps the memory, DDS disk and chunk disk caches in the bridge adapters
   `MemoryCacheBridge`, `DdsDiskCacheBridge` and `DiskCacheBridge`
   (`cache/adapters/`). They implement the executor's `MemoryCache`,
   `DdsDiskCache` and `DiskCache` traits, translating coordinates into keys
   through `TileCacheClient` and `ChunkCacheClient` (`cache/clients/`).

Starting a disk service creates its directory and runs
`LruIndex::populate_from_disk`: the subdirectories of the tier's directory are
listed, each taken to be a region, then scanned in parallel with rayon inside
`spawn_blocking`. Only regular files directly inside one of them and named
`*.cache` are indexed. A file anywhere else is not counted against the budget
and never evicted.

`CacheLayer::scan_disk_cache_size` then reads the DDS and chunk totals from
their indexes (no second scan) for the dashboard's initial disk size. The service
builds the runtime with the three bridges, shared by both lanes
(`CacheLayer::executor_disk_caches`, `RuntimeBuilder::build_with_cache_service`),
spawns the GC scheduler daemons and starts the legacy chunk cleanup. The lit cache reaches `NightLitSource` and its
`RoadService` through `XEarthLayerService::lit_disk_cache` when the mount is set
up (`manager/mounts.rs`).

`CacheLayer::shutdown` stops the services in reverse order (lit, chunk, DDS,
memory). It does not wait for spawned writes.

## Integrity

Every disk tier treats a bad entry as a miss (#253). `DiskCacheProvider::get`
validates what it reads:

- DDS: magic `DDS ` and exactly `EXPECTED_DDS_SIZE` bytes;
- chunk: not empty;
- lit: magic `XELL`. The night code also checks the fingerprint and the payload
  length (`night/disk.rs`).

A rejected entry is deleted (`integrity::discard`), removed from the index and
reported as a miss, so the request regenerates it. A read error other than
`NotFound` is logged and also treated as a miss.

`DiskCacheProvider::set` writes `tokio::fs::write` to a sibling `.tmp` file and
renames it over the entry. A reader never sees a half-written file at the live
path, but the data is not fsynced before the rename, and a failed write leaves
its `.tmp` file. `.tmp` files are not indexed. The disk tiers do not use
`integrity::write_atomic`.

[cache-integrity-design.md](cache-integrity-design.md) has the full model.

## Memory Notes

- Values are `bytes::Bytes` end to end (#237). Encoding hands its `Vec<u8>` to
  `Bytes::from` without a copy, and every cache write, promotion and FUSE read
  after that shares the allocation.
- An open virtual DDS handle memoises its tile (#234). That tile stays in memory
  while the handle is open, even after moka evicts it. The bytes pinned this way
  are bounded separately by `PinnedTileBudget` (`pinned_budget.rs`, default
  ceiling 512 MiB, #236). Process memory for tiles is the memory tier plus the
  pinned handles.
- [memory-telemetry.md](memory-telemetry.md) explains how to read memory in the
  field.

## Coherency

**Provider change.** The memory tier lives only for one run, with one provider.
The DDS and chunk disk tiers are per provider, so tiles and chunks from
different providers never mix, and a tile is never assembled from another
provider's chunks (#398).

**Format.** XEarthLayer encodes BC1 only and has no format setting (#395).
Neither the memory key nor the DDS disk path carries a format. A second format
would need it in both, and a format-aware size in the DDS validator and the FUSE
size gate (see [dds-implementation.md](dds-implementation.md), "Supported
formats").

**Imagery updates.** Entries have no age limit. A tile leaves the cache only by
GC or by deletion.

**Night lighting inputs.** Every lit entry carries a SHA-256 fingerprint of its
inputs, so a changed input makes the stored entry a miss.

## Configuration

`[cache]` in `config.ini`:

| Key | Default | Meaning |
|-----|---------|---------|
| `directory` | platform cache directory | Root of every disk tier |
| `memory_size` | `512MB` | Memory tier budget |
| `disk_size` | `20GB` | Budget shared by the disk tiers |
| `dds_disk_ratio` | `0.6` | DDS share of the disk budget left after the lit share (0.0 to 1.0; anything else, including `inf` and `NaN`, is refused when the file is read, #401) |

`night.enabled` (default true) decides whether the lit tier exists. Read and
change the keys with `xearthlayer config get|set cache.<key>`. There are no
environment variables for the cache.

`run --no-cache` turns off the disk tiers for one session (#400). It sets
`ServiceConfig::disk_cache_enabled` to false, and then:

- `CacheLayer` starts only the memory tier: no DDS, chunk or lit disk service,
  and no tier under `cache.directory` is created, read, written or deleted
  (no startup scan, no legacy cleanup). The scenery and ortho union index
  caches are not tiers: they load and save as usual, and on a default
  installation they sit in the same directory;
- `executor_disk_caches()` returns `ExecutorDiskCaches::Off`, so both lanes get
  `NullDdsDiskCache` and `NullDiskCache`, which always miss and drop writes;
  demand, prefetch, `CachedTileProbeTask` and prewarm see every tile as not on
  disk, and the prefetch coordinator's DDS disk filter is not installed;
- no GC scheduler daemon is spawned;
- night lighting runs without its disk tier;
- prefetch does not run (`run.rs` `prefetch_off_reason`, which also applies
  `--no-prefetch`), because its tiles would reach only the memory tier and be
  evicted long before they are flown over, and clap refuses `--airport` with
  `--no-cache` for the same reason.

The memory tier works as usual, because FUSE serving, the coalescer and the
open-handle memo depend on it. The startup banner says
`Cache: <memory> memory, disk caching off (--no-cache)` and the dashboard shows
the DDS and chunk tiers as off.

The GC check interval, thresholds and batch size are constants, not settings.

## Maintenance

`cache::maintenance` (#399) finds the tier directories through `cache::layout`
(`find_tiers`): each provider directory's `dds/` and `chunks/`, `night/`, and any
pre-0.5.1 region directories still in the root. It trusts content, not names: a
directory is a tier only if it holds nothing but region directories of `*.cache`
and `*.tmp` files (`layout::looks_like_tier_dir`), so a user's own `dds` or
`chunks` folder is never taken for one. Symbolic links are never followed. Everything else in `cache.directory` is outside the tiers: the package
staging directory `tmp/` (with `.queue.lock`), `scenery_index.cache`,
`ortho_union_index.cache`, and any other file or directory, including other
contents of a provider directory.

- `xearthlayer cache stats` counts the tier directories only, one line per tier
  (DDS and chunks per provider, old shared chunks if any, night lighting) and a
  total. It only reads.
- `xearthlayer cache clear` deletes the tier directories only and keeps the
  provider directories themselves. It refuses, deletes nothing and exits 1 while
  `xearthlayer run` is active (the instance lock, as the package commands check
  it) or while a package install, update or removal holds the queue lock in the
  package temp directory (`manager::queue_in_use`). The check is made when the
  command starts.

By hand, with XEarthLayer stopped, any region directory or `.cache` file in a
tier can be deleted; the indexes are rebuilt from disk at the next start. For
example:

```bash
# DDS tiles of one provider
rm -rf "$CACHE_DIR/Bing Maps/dds"

# Imagery chunks of one provider
rm -rf "$CACHE_DIR/Bing Maps/chunks"

# Night lighting tier
rm -rf "$CACHE_DIR/night"

# Everything cached for one 1x1 degree region
rm -rf "$CACHE_DIR"/*/dds/+37-123 "$CACHE_DIR"/*/chunks/+37-123 "$CACHE_DIR/night/+37-123"

# Leftover temporary files from interrupted writes
find "$CACHE_DIR" -name '*.tmp' -delete
```

`$CACHE_DIR` stands for `cache.directory` (`xearthlayer config get
cache.directory`).

## Code Not on the Production Path

These remain in `xearthlayer/src/cache/` and `executor/` but are not used by
`XEarthLayerService`:

- `cache::path::cache_path`, `row_directory` and `provider_directory`: the old
  `{provider}/{format}/{zoom}/{row}/` layout.
- `cache::MemoryCache` and `CacheKey` (`cache/memory.rs`, `cache/types.rs`), with
  `ExecutorCacheAdapter`, `executor::adapters::DiskCacheAdapter` and
  `RuntimeBuilder::build`. Production builds the runtime with
  `RuntimeBuilder::build_with_cache_service`.
- `MemoryCacheConfig` and `DiskCacheConfig` (including `max_age_days`).
- `DiskProviderConfig::gc_interval`, filled from
  `ServiceConfig::disk_gc_interval_secs`: logged, but the GC daemons use
  `DEFAULT_CHECK_INTERVAL_SECS`.
- `DiskCacheProvider::gc`, which returns an empty result.

## Related Documents

- [cache-integrity-design.md](cache-integrity-design.md): validation, discard,
  bounded reads, atomic writes.
- [cache-service-design.md](cache-service-design.md): the `Cache` trait and the
  domain clients. Its sections on garbage collection describe a GC daemon inside
  `DiskCacheProvider`, which `GcSchedulerDaemon` and `CacheGcJob` replaced.
- [night-lighting-design.md](night-lighting-design.md): the lit tier, its
  fingerprint and the road extraction entries.
- [job-executor-design.md](job-executor-design.md): lanes, priorities and the
  admission gate the GC job runs under.
- [memory-telemetry.md](memory-telemetry.md): memory sampling in the field.
