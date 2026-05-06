# Changelog

All notable changes to XEarthLayer will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.5] - 2026-05-05

### Fixed

- **Tiles assembled from failed chunk downloads are no longer cached** ([#180](https://github.com/samsoir/xearthlayer/issues/180)): Network failures during tile generation produced magenta-filled DDS tiles that were structurally valid (correct size + DDS magic) and persisted indefinitely in the memory and DDS disk caches, surviving even X-Plane's "Reload Scenery". On reconnect, re-requests would short-circuit to the poisoned cache instead of re-fetching. Cache writes are now gated on `ChunkResults::is_complete()`: tiles with any failed chunks are still served to X-Plane (so the sim doesn't stall during outages) but never persisted to memory or DDS disk. The chunk-disk tier was already correct (only successful HTTP downloads were cached), so it now functions as a natural retry-resume buffer on re-request — only the chunks that previously failed get re-fetched. Users who saw magenta tiles persist across an outage on v0.4.4 or earlier should run `xearthlayer cache clear` to purge any poisoned tiles.

## [0.4.4] - 2026-04-16

### Changed

- **Unified ground/cruise prefetch under single PrefetchBox** ([#172](https://github.com/samsoir/xearthlayer/issues/172)): Replaced the ring-based `GroundStrategy` (746 lines) with symmetric `PrefetchBox` (bias 0.5). Ground and cruise phases now share the same code path — ground uses a fixed extent with symmetric bias, cruise uses speed-proportional extent with heading bias (0.8). Eliminated dead `scene_tracker.loaded_bounds()` wiring that was never called in production.

- **Debug map renders actual prefetch box (SSOT)** ([#172](https://github.com/samsoir/xearthlayer/issues/172)): The coordinator now computes bounds once per cycle and publishes them as `BoxBoundsSnapshot` via `SharedPrefetchStatus`. The debug map reads these verbatim — no recomputation, no drift between display and reality. Region colours are now GeoIndex-authoritative: regions stay yellow (InProgress) until every tile is verified on disk, then flip green (Prefetched). New orange (Mixed) state shows when FUSE has served tiles from a prefetched region.

- **Three-tier cache metrics in TUI** ([#166](https://github.com/samsoir/xearthlayer/issues/166)): Split the combined "Disk" cache display into separate DDS Disk and Chunks tiers with independent hit rates. The TUI now shows a 3-column layout (Memory, DDS Disk, Chunks) with per-tier progress bars and hit/miss counters. DDS disk hit rate (93%+ in flight tests) is no longer hidden by chunk-level event volume.

- **Default `executor.max_concurrent_jobs` tuned to `num_cpus / 2`** ([#172](https://github.com/samsoir/xearthlayer/issues/172)): Reduced from `ceil(num_cpus × 0.75)` to halve CPU pressure on X-Plane during heavy prefetch. Flight-tested as the best compromise between prefetch throughput and simulator frame rate.

### Fixed

- **Prefetch coverage degrades on long-haul flights** ([#172](https://github.com/samsoir/xearthlayer/issues/172)): Multiple interacting defects caused prefetched regions to enter permanent dead states on flights over ~2 hours. Regions were marked `InProgress` before submission results were known, the `cached_tiles` shadow set tracked only ~6% of actually-cached tiles, and DDS disk cache lookups used chunk coordinates instead of tile coordinates (silently returning `false` every time). Fixed by deferring `mark_in_progress` until after successful submission, replacing the shadow set with authoritative DDS disk cache queries, and correcting the coordinate mismatch. Verified against a 9-hour LOWW flight log.

- **TUI cache hit rates reflect X-Plane experience, not prefetch traffic** ([#171](https://github.com/samsoir/xearthlayer/issues/171)): The Memory and DDS Disk tiers in the cache widget now render FUSE-only hit rates and counts, so the percentages reflect how well the cache is serving X-Plane's actual requests. Previously, aggregate counters included prefetch and prewarm traffic — a long-haul flight with a 100%-full memory cache would show ~47% hit rate because prefetch misses dominated the denominator. Added `is_fuse` discriminator to `DdsDiskCacheHit`/`DdsDiskCacheMiss` events, paired FUSE-only counters in state and snapshot, and wired the executor daemon to tag events based on `RequestOrigin`. The Chunks tier continues to use aggregate (there's only one chunk-read path, so aggregate equals FUSE there). Also fixed a latent bug in `manager/mounts.rs` where `fuse_memory_cache_hit_rate` was never recalculated during multi-service aggregation.

- **TUI queue display order reversed** ([#165](https://github.com/samsoir/xearthlayer/issues/165)): Queue column now shows oldest (completing) jobs at the top with new jobs appended at the bottom, matching the natural reading order for a processing pipeline.

## [0.4.3] - 2026-04-07

### Changed

- **Memory cache size reporting moved to provider** ([#156](https://github.com/samsoir/xearthlayer/issues/156)): `MemoryCacheProvider` now self-reports its size to metrics internally after each write/delete/GC, replacing the push-based pattern where callers emitted size after each operation. Eliminates cache domain logic leaking into tasks and adapters.

- **Configuration audit and consolidation** ([#159](https://github.com/samsoir/xearthlayer/issues/159), [#160](https://github.com/samsoir/xearthlayer/issues/160)): Removed 15 unused or redundant config keys (`[pipeline]`, `[download]`, `[control_plane]` sections and executor internals). Wired previously ignored executor settings (`cpu_concurrent`, `max_concurrent_jobs`, `request_timeout_secs`, `max_retries`). Default CPU concurrency reduced to 50% of logical cores for X-Plane coexistence.

### Fixed

- **Prefetch InProgress dead state** ([#157](https://github.com/samsoir/xearthlayer/issues/157)): Regions marked `InProgress` by the prefetch boundary strategy could enter a permanent dead state — never promoted to `Prefetched` and never retried. Added three-layer protection: DDS disk cache verification (promotes regions with tiles on disk), retry with attempt counter (up to 3 attempts), and `NoCoverage` fallback for persistently failing regions.

- **DDS disk cache metrics not displayed in TUI** ([#156](https://github.com/samsoir/xearthlayer/issues/156)): DDS disk cache hits were not emitting `disk_cache_hit` or `fuse_tile_served` metric events, causing the TUI to show 0% disk hit rate despite the cache working correctly. Added metric emissions on the DDS disk → memory promotion path.

- **Memory cache size showing 0 bytes in TUI** ([#156](https://github.com/samsoir/xearthlayer/issues/156)): Memory cache size metric was only emitted from external callers, missing the DDS disk → memory promotion path. Provider self-reporting ensures the metric is always current.

- **Session summary not printed on shutdown** ([#156](https://github.com/samsoir/xearthlayer/issues/156)): The session summary at exit was gated on `jobs_completed > 0`, which skipped it when all tiles were served from cache. Gate removed.

## [0.4.2] - 2026-04-06

### Added

- **DDS disk cache tier** ([#132](https://github.com/samsoir/xearthlayer/issues/132)): Three-tier cache hierarchy (memory → DDS disk → chunk disk → network). Encoded DDS tiles persist to disk, avoiding re-encoding on memory eviction (~3.5ms NVMe read vs ~50-200ms re-encode). Configurable disk budget split via `cache.dds_disk_ratio` (default 60% DDS, 40% chunks).

- **Speed-proportional prefetch box** ([#125](https://github.com/samsoir/xearthlayer/issues/125)): Prefetch box extent scales linearly with ground speed — 3.5° at 40kt to 6.5° at 450kt+. Reduces over-fetching during approach by ~45%, cutting burst sizes that contributed to swap-in storms on memory-constrained systems.

- **Stale telemetry safe mode** ([#125](https://github.com/samsoir/xearthlayer/issues/125)): When X-Plane position telemetry goes stale (5s), prefetch pauses entirely. On resume, `on_ground` from SimState determines phase reset (Ground or Cruise) before normal cycling restarts.

- **Version update check** ([#128](https://github.com/samsoir/xearthlayer/issues/128)): Non-blocking startup check against remote `version.json` with 24h disk cache. TUI dashboard shows persistent footer when an update is available. Configurable via `general.update_check` (default: true).

### Changed

- **GPU encoding is now built-in** ([#139](https://github.com/samsoir/xearthlayer/issues/139)): The `gpu-encode` Cargo feature flag has been removed. GPU encoding via wgpu compute shaders is compiled unconditionally into every binary. Select it at runtime via `texture.compressor = gpu` in config — no special build flags required. CI/CD pipeline simplified to a single binary variant.

- **fuse3 updated to 0.9.0** ([#134](https://github.com/samsoir/xearthlayer/issues/134)): Upstream switched serialization from bincode to zerocopy. `ReplyInit` adapted to new `#[non_exhaustive]` constructor pattern. Contributed by [@mmaechtel](https://github.com/mmaechtel).

- **Removed `prefetch.strategy` config setting** ([#136](https://github.com/samsoir/xearthlayer/issues/136)): Only the adaptive strategy exists; setting was redundant. Added to deprecated keys for automatic removal via `config upgrade`.

### Fixed

- **Debug map**: DDS disk cache hits now recorded in tile activity tracker — boundary tile loads were previously invisible to the debug map ([#125](https://github.com/samsoir/xearthlayer/issues/125))
- **Debug map**: Prefetch box extent propagated in no-plan status updates — live speed-proportional box now renders correctly during steady cruise ([#125](https://github.com/samsoir/xearthlayer/issues/125))
- **Download progress**: Completed download bars now clear so progress scrolls correctly for large packages ([#129](https://github.com/samsoir/xearthlayer/issues/129))
- **CI**: Website sync now triggers on release branch merge instead of during release workflow ([#127](https://github.com/samsoir/xearthlayer/pull/127))

## [0.4.1] - 2026-03-29

### Added

- **DDS Encoder Memory Optimization** ([#117](https://github.com/samsoir/xearthlayer/issues/117), [#120](https://github.com/samsoir/xearthlayer/issues/120)): Streaming mipmap architecture reduces peak memory across both CPU and GPU encoding paths
  - `MipmapStream` iterator yields one mipmap level at a time, eliminating intermediate clones
  - `DdsEncoder::encode()` fused pipeline — compress and drop each level incrementally
  - Owned `RgbaImage` signatures propagated through full trait chain
  - `ImageCompressor` trait (renamed from `BlockCompressor`) for single-level backends
  - `MipmapCompressor` trait for full-pipeline GPU backend — source image moved (not cloned), one channel round-trip per tile instead of 5
  - GPU worker-side mipmap iteration with buffer reuse across levels
  - ISPC path: peak heap -29% (8.55 GB → 6.06 GB), peak RSS -21%
  - GPU path: peak heap -21% (7.06 GB → 5.61 GB), peak RSS -44%, memory leaked -71%
  - Profiling and flight testing by [@mmaechtel](https://github.com/mmaechtel)

- **Parallel Package Downloads** ([#115](https://github.com/samsoir/xearthlayer/issues/115)): Configurable concurrent part downloads with per-part progress UI
  - `packages.concurrent_downloads` config setting (1-10, default 5)
  - Sliding-window execution model replacing batch-based parallelism
  - Per-part progress bars via `indicatif::MultiProgress` (queued, downloading, done, failed, retrying)
  - `RetryDownloader` with exponential backoff (3 retries, 2s/4s/8s)
  - Animated spinner for indeterminate stages (reassembling, extracting, installing)

- **Memory Profiling Guide** — `docs/dev/memory-profiling.md` for heaptrack installation, profiling, and A/B comparison

### Changed

- **Documentation restructured** ([#123](https://github.com/samsoir/xearthlayer/pull/123)): Consolidated planning artifacts into proper design docs
  - New `docs/dev/gpu-encoding-design.md` (from 6 scattered files)
  - Enriched adaptive-prefetch, FUSE, and package manager design docs with spec content
  - Archived 19 completed plans, implemented specs, and superseded docs to `docs/dev/archive/`
  - Removed `docs/plans/`, `docs/dev/plans/`, `docs/dev/specs/` directories

### Fixed

- **Default temp directory** now uses `~/.xearthlayer/tmp` instead of system `/tmp` ([#118](https://github.com/samsoir/xearthlayer/issues/118)). On Linux distros where `/tmp` is a `tmpfs` (RAM-backed), downloading multi-GB scenery packages caused OOM errors — reported by [@r0adrunner](https://github.com/r0adrunner)
- **CI**: GitHub Actions upgraded to Node.js 24-compatible versions ([#107](https://github.com/samsoir/xearthlayer/issues/107))
- **Release workflow**: `version.json` now updated on release branch instead of post-release push ([#109](https://github.com/samsoir/xearthlayer/pull/109))

## [0.4.0] - 2026-03-21

> **Breaking**: Replaces XGPS2/ForeFlight UDP telemetry with X-Plane's built-in Web API. The `[online_network]` config section and `udp_port` setting are removed. X-Plane 12.1+ is required for position telemetry.

### Added

- **X-Plane Web API Telemetry** ([#79](https://github.com/samsoir/xearthlayer/issues/79)): Real-time position and sim state via X-Plane's built-in Web API
  - `WebApiAdapter` — REST dataref lookup + WebSocket subscription at 10Hz
  - `SimState` — direct sim state detection (paused, on_ground, scenery_loading, replay, sim_speed)
  - Automatic reconnection with configurable interval
  - `prefetch.web_api_port` config setting (default: 8086)

- **Sliding Prefetch Box** ([#86](https://github.com/samsoir/xearthlayer/issues/86), [#98](https://github.com/samsoir/xearthlayer/issues/98)): Heading-biased prefetch region that slides with the aircraft
  - `PrefetchBox` — 6.5° extent with proportional 80/20 heading bias
  - Replaces boundary-monitor edge detection approach
  - `box_extent` and `box_max_bias` config settings
  - Retention tracking integrated with PrefetchBox bounds

- **Debug Map** ([#97](https://github.com/samsoir/xearthlayer/issues/97)): Live browser-based map for prefetch observability (feature-gated behind `debug-map`)
  - Leaflet.js map showing aircraft, prefetch box, DSF regions, and DDS tiles
  - Colour-coded region states: green (prefetched), yellow (in progress), red (FUSE loaded), grey (no coverage), purple (patched)
  - Per-tile activity tracking — FUSE on-demand vs prefetch origin
  - Stats panel with sim state, position, and region counts
  - `make debug-build` / `make debug-run` targets

- **GPU Pipeline Overlap** ([#80](https://github.com/samsoir/xearthlayer/issues/80)): Pipelined GPU encoding for improved throughput
  - While GPU compresses tile A, CPU uploads tile B
  - Panic recovery via `catch_unwind`, `map_async` error propagation
  - `device.on_uncaptured_error()` for device loss logging

- **FOPEN_DIRECT_IO** ([#65](https://github.com/samsoir/xearthlayer/issues/65)): Return `FOPEN_DIRECT_IO` for virtual DDS files, bypassing kernel page cache for reduced memory pressure

### Changed

- **Prefetch box resized** from 4°×4° to 6.5°×6.5° to match X-Plane's observed ~6×6 DSF loading area ([#98](https://github.com/samsoir/xearthlayer/issues/98))
- **Proportional heading bias** replaces binary forward/behind margins — smooth interpolation from 50/50 (perpendicular) to 80/20 (aligned) ([#98](https://github.com/samsoir/xearthlayer/issues/98))
- **TUI queue display** now shows one row per DSF region with aggregate tile progress instead of individual tiles ([#104](https://github.com/samsoir/xearthlayer/issues/104))
- **XEarthLayerService** consolidated to single `start()` constructor — removed 4 unused legacy constructors and private `build()` method ([#91](https://github.com/samsoir/xearthlayer/issues/91))
- **mod.rs files** sanitized — implementations extracted to focused modules across 5 modules ([#95](https://github.com/samsoir/xearthlayer/issues/95))
- **CI**: GitHub Actions upgraded to v5 for Node.js 24 compatibility ([#78](https://github.com/samsoir/xearthlayer/pull/82))

### Fixed

- **Prefetch executor flooding** — enforce `max_tiles_per_cycle` cap on sliding box strategy, preventing executor saturation on first cruise tick ([#87](https://github.com/samsoir/xearthlayer/pull/87)) — thanks [@mmaechtel](https://github.com/mmaechtel)
- **FUSE shutdown hang** when GPU pipeline and FOPEN_DIRECT_IO are both enabled ([#90](https://github.com/samsoir/xearthlayer/issues/90))
- **Pre-commit hook** now fails on formatting issues instead of silently fixing them ([#101](https://github.com/samsoir/xearthlayer/issues/101))
- **version.json** atomic updates via GitHub Contents API, eliminating push race condition

### Removed

- **XGPS2/ForeFlight UDP telemetry** — replaced by X-Plane Web API ([#79](https://github.com/samsoir/xearthlayer/issues/79))
- **Online network position sources** (VATSIM, IVAO, PilotEdge) — redundant with Web API ([#79](https://github.com/samsoir/xearthlayer/issues/79))
- **Circuit breaker and FUSE load monitor** — replaced by SimState from Web API ([#79](https://github.com/samsoir/xearthlayer/issues/79))
- **Boundary monitors** — replaced by sliding prefetch box ([#94](https://github.com/samsoir/xearthlayer/issues/94))
- **Config keys**: `udp_port`, `online_network.*`, `circuit_breaker_*`, `trigger_distance`, `load_depth_lat`, `load_depth_lon`, `forward_margin`, `behind_margin` ([#79](https://github.com/samsoir/xearthlayer/issues/79), [#94](https://github.com/samsoir/xearthlayer/issues/94))
- **Legacy service constructors**: `new()`, `with_disk_profile()`, `with_runtime()`, `with_cache_bridges()` ([#91](https://github.com/samsoir/xearthlayer/issues/91))

## [0.3.1] - 2026-03-13

> **Note**: Major release featuring GPU-accelerated DDS encoding, a complete rewrite of the prefetch system to a boundary-driven model, and extensive stability and performance improvements across the pipeline.

### Added

- **GPU-Accelerated DDS Encoding** ([#67](https://github.com/samsoir/xearthlayer/issues/67)): Optional GPU compute shader pipeline for DDS texture compression
  - `GpuEncoderChannel` — channel-based GPU encoding architecture eliminating Mutex contention
  - `WgpuCompressor` — wgpu/WGSL compute shaders via `block_compression` crate (ISPC kernels ported to GPU)
  - Dedicated GPU worker task receives encode requests via `mpsc` channel
  - GPU device selection via `texture.gpu_device` config setting
  - Compressor backend selection: `software`, `ispc`, or `gpu` via `texture.compressor` config setting
  - Queue depth diagnostic logging for GPU worker monitoring
  - Requires `--features gpu-encode` build flag

- **ISPC SIMD Block Compressor** ([#58](https://github.com/samsoir/xearthlayer/issues/58)): High-performance CPU-based DDS compression
  - Intel ISPC-optimized BC1/BC3 compression via `intel_tex_2` crate
  - Significantly faster than pure-Rust software compressor
  - Now the default compressor backend

- **Boundary-Driven Prefetch System** ([#58](https://github.com/samsoir/xearthlayer/issues/58)): Complete rewrite of the predictive tile caching model
  - `SceneryWindow` — three-window model (XP Window, XEL Window, Retained) derived from empirical X-Plane measurements
  - `BoundaryMonitor` — position-based DSF boundary edge detection (row and column axes)
  - `BoundaryStrategy` — converts boundary crossings to target DSF region lists for prefetch
  - Asymmetric loading depth: 3 rows × 3-4 cols (lat crossings), 2 cols × 3-4 rows (lon crossings)
  - Dynamic longitude column computation via `lon_tiles_for_latitude()` utility
  - Region completion tracking with staleness timeout
  - World rebuild detection in SceneryWindow
  - Retention inference and eviction of stale PrefetchedRegion and cached_tiles outside retained window

- **Transition Throttle** ([#62](https://github.com/samsoir/xearthlayer/issues/62)): Smooth prefetch ramp-up after ground-to-cruise transition
  - Phase-driven fraction throttle with configurable grace period and ramp duration
  - MSL-based phase release (transition completes at altitude)
  - `IntegerRangeSpec` and `FloatRangeSpec` config validators

- **GeoIndex Geospatial Reference Database** ([#51](https://github.com/samsoir/xearthlayer/issues/51)): Type-keyed, region-indexed spatial lookup
  - `PatchCoverage` layer — tracks which 1°×1° DSF regions are owned by scenery patches
  - `RetainedRegion` layer — tracks regions in SceneryWindow's retained set
  - `PrefetchedRegion` layer — four-state model (InProgress, Prefetched, NoCoverage, absent)
  - Atomic bulk loading via `populate()` with RwLock + DashMap sharding
  - Used by FUSE (`is_geo_filtered`), prewarm, and prefetch for patch region exclusion

- **Online Network Position Source** ([#52](https://github.com/samsoir/xearthlayer/issues/52)): Aircraft position from online ATC networks
  - VATSIM, IVAO, and PilotEdge support via REST API polling
  - Configurable pilot ID and poll interval
  - Debug logging for data feed health monitoring
  - Integrates with `StateAggregator` for accuracy-based source selection

- **Chrome Trace Profiling**: Performance analysis via `--profile` flag
  - Non-blocking writer with fuse3 trace noise filtering
  - Dedicated target for surgical trace filtering
  - Instrument coverage on `BuildAndCacheDdsTask`
  - `make` targets for profiling builds (`--features profiling`)

- **Prefetch Diagnostics** ([#58](https://github.com/samsoir/xearthlayer/issues/58), [#59](https://github.com/samsoir/xearthlayer/issues/59)):
  - Boundary crossing and region targeting diagnostic logging
  - Per-origin FUSE cache hit rate tracking with double-counting fix
  - Thousand separators in telemetry display for readability

- **CI/CD Improvements**:
  - GPU feature compilation check in CI pipeline ([#74](https://github.com/samsoir/xearthlayer/pull/74))
  - `xearthlayer-gpu` artifact in release pipeline
  - `chore/*` branch pattern added to CI triggers

- **Community Standards** ([#55](https://github.com/samsoir/xearthlayer/pull/55)):
  - Contributing guide (`CONTRIBUTING.md`)
  - Security policy (`SECURITY.md`)
  - Pull request template
  - Code of conduct

### Fixed

- **Priority Inversion in Job Executor** ([#56](https://github.com/samsoir/xearthlayer/issues/56)): Pipeline-balanced concurrency and resource-aware backpressure
  - Network pool rebalanced: `clamp(ceil(cpu*1.5), 8, 64)` — pipeline-balanced with CPU pool
  - Backpressure uses actual resource pool utilization (`executor_load()`) instead of channel pressure
  - Prefetch quota and FUSE resilience improvements
  - Scene load time reduced from 8m30s to 3m20s with 100+ tiles/sec prefetch throughput

- **Circuit Breaker Self-Tripping** ([#59](https://github.com/samsoir/xearthlayer/issues/59)): Circuit breaker no longer blocks prefetch during cruise
  - Switched from FUSE request counting (which included harmless cache hits) to resource pool utilization
  - Removed dead `record_request()` from FUSE read path
  - Deduplicated TileCoords in `get_tiles_for_region`

- **Prewarm Cache Miss** ([#63](https://github.com/samsoir/xearthlayer/issues/63)): Progress bar rebased on tiles needing generation
  - Previously counted all tiles including those already cached, showing misleading progress

- **Water Mask DDS Guard** ([#68](https://github.com/samsoir/xearthlayer/issues/68)): Prevent virtual DDS generation for water mask textures
  - FUSE no longer attempts to generate DDS for water mask files

- **Cache Count Formatting** ([#70](https://github.com/samsoir/xearthlayer/issues/70)): Wired `format_count` into cache hit/miss display

- **Disk Cache Startup Performance** ([#46](https://github.com/samsoir/xearthlayer/issues/46)): Restructured disk cache into region subdirectories
  - Region-based layout (1°×1° DSF subdirs, e.g., `+33-119/`) replaces flat directory
  - Parallel scanning via rayon over region directories for fast startup

- **Patch Region Handling** ([#51](https://github.com/samsoir/xearthlayer/issues/51)):
  - Fixed geo-filter region calculation and added package fallthrough
  - Removed DDS gate in patched regions
  - Fixed GO2 zoom parsing
  - Prewarm now checks disk for existing DDS tiles before generating
  - Region-level ownership for FUSE, prewarm, and prefetch

- **Overlay Union Behavior** ([#45](https://github.com/samsoir/xearthlayer/issues/45)): Use consolidated overlay for install/remove/update

- **Packages List Hang** ([#47](https://github.com/samsoir/xearthlayer/pull/47)): Fixed hang in patches list command (contributed by [@mmaechtel](https://github.com/mmaechtel))

- **HTTP Concurrency**: Raised FD limit at startup, restored 1024 HTTP permits after FD exhaustion fix

- **CLI Improvements**: Moved `raise_fd_limit` after logging init; `--debug` and `--profile` flags now combine correctly

### Changed

- **Prefetch Architecture Rewrite** ([#58](https://github.com/samsoir/xearthlayer/issues/58)): Boundary-driven model replaces band/cone-based approach
  - SceneryWindow locked to configured dimensions based on empirical measurements (3° lat × 3°/cos(lat) lon)
  - Configurable: `load_depth_lat=3`, `load_depth_lon=2`, `trigger_distance=1.5`, `default_window_rows=3`, `window_lon_extent=3.0`
  - Prewarm grid changed from square to rectangular: `grid_rows=3`, `grid_cols=4`

- **Coordinator Core Refactored** ([#73](https://github.com/samsoir/xearthlayer/pull/73)): Extracted into focused modules
  - Filtering, execution, and status extracted to separate modules
  - Test mocks moved to `test_support` module

- **Network Pool Sizing** ([#56](https://github.com/samsoir/xearthlayer/issues/56)): Cap raised from 48 to 64 concurrent tiles

- **Log Verbosity** ([#50](https://github.com/samsoir/xearthlayer/pull/50)): Per-request log messages demoted from INFO to DEBUG

- **Config Split**: `config/file.rs` and `service/orchestrator.rs` split into submodules for maintainability

### Removed

- **Dead Code Audit** ([#75](https://github.com/samsoir/xearthlayer/pull/75)): Removed dead code and unused `allow(dead_code)` annotations
- **Legacy Prefetch Components** ([#58](https://github.com/samsoir/xearthlayer/issues/58)): Removed `CruiseStrategy`, `BandCalculator`, `BoundaryPrioritizer`, `TurnDetector` (superseded by boundary-driven model)
- **Dead FUSE Code** ([#59](https://github.com/samsoir/xearthlayer/issues/59)): Removed unused `record_request()` from FUSE read path

### Deprecated

- **Configuration Keys**: The following keys are deprecated (run `xearthlayer config upgrade` to migrate):
  - `prefetch.load_depth` — replaced by `prefetch.load_depth_lat` and `prefetch.load_depth_lon`
  - `prefetch.default_window_cols` — now computed dynamically from latitude
  - `prewarm.grid_size` — replaced by `prewarm.grid_rows` and `prewarm.grid_cols`
  - Legacy band-based prefetch settings replaced by boundary-driven settings

### Documentation

- GPU acceleration documentation and build options
- Updated adaptive prefetch design to v2.0 — boundary-driven model
- X-Plane scenery loading whitepaper updated to v1.1 and v1.2
- GPU channel architecture design and implementation plan
- GeoIndex design documentation
- Transition ramp design speclet
- Boundary-driven prefetch implementation plan
- Compressed flight test logs for scenery loading research
- Updated CLAUDE.md with current architecture and concurrency limits
- Updated README with GPU encoding feature

### Upgrade Notes

**New `config.ini` settings for v0.3.1:**

```ini
[texture]
# Compressor backend: software, ispc, or gpu (requires --features gpu-encode)
compressor = ispc
# GPU device selection (optional, for multi-GPU systems)
# gpu_device = 0

[prefetch]
# Boundary-driven prefetch (replaces band-based settings)
load_depth_lat = 3        ; DSF rows to load on latitude boundary crossing
load_depth_lon = 2        ; DSF cols to load on longitude boundary crossing
trigger_distance = 1.5    ; Degrees from window edge to trigger prefetch
default_window_rows = 3   ; Scenery window height in DSF rows
window_lon_extent = 3.0   ; Scenery window longitude extent in degrees

[prewarm]
# Rectangular grid (replaces square grid_size)
grid_rows = 3             ; DSF rows to prewarm around airport
grid_cols = 4             ; DSF cols to prewarm around airport

[online_network]
# Online ATC network position (VATSIM/IVAO/PilotEdge)
enabled = false
# pilot_id =              ; Your CID/VID on the network
# poll_interval_secs = 15
```

**Optional GPU encoding (requires build flag):**

```bash
# Build with GPU encoding support
cargo build --release --features gpu-encode

# Configure in config.ini
[texture]
compressor = gpu
```

Run `xearthlayer config upgrade` to automatically add new settings with defaults.

### Acknowledgements

- Thanks to [@mmaechtel](https://github.com/mmaechtel) for fixing the packages list hang ([#47](https://github.com/samsoir/xearthlayer/pull/47)) and for extensive testing and verification throughout this release

## [0.3.0] - 2026-02-04

> **Note**: This is a major release introducing the Adaptive Prefetch System and a complete rewrite of the execution core.

### Added

- **Adaptive Prefetch System**: Self-calibrating tile prediction that eliminates scenery loading stutters
  - Predicts which tiles X-Plane will request based on position, heading, and empirically-observed loading patterns
  - Self-calibrates during initial scene load by measuring tile generation throughput
  - **Flight phase detection**: Ground (< 40kt) vs Cruise mode with different strategies
  - **GroundStrategy**: Ring-based prefetching around aircraft position
  - **CruiseStrategy**: Track-based band prefetching ahead of heading, matching X-Plane's actual band-loading behavior
  - **Mode selection**: Aggressive (>30 tiles/sec), Opportunistic (10-30), or Disabled based on system performance
  - Pauses during scene loading via circuit breaker to avoid interfering with on-demand requests
  - See `docs/dev/adaptive-prefetch-design.md` for full design

- **Job Executor Framework**: Complete rewrite of the execution core enabling adaptive prefetch
  - Async, non-blocking execution replaces legacy `spawn_blocking` pipeline
  - **Priority scheduling**: `ON_DEMAND` (100) > `PREFETCH` (0) > `HOUSEKEEPING` (-50)
  - **Resource pools**: Semaphore-based concurrency limits by type (Network, CPU, DiskIO)
  - **Job concurrency groups**: Limits concurrent DDS generation jobs to balance pipeline stages
  - **Cancellation support**: Via `CancellationToken` for clean shutdown and job control
  - **Child job spawning**: Prefetch spawns individual DDS generation jobs
  - **Stall detection watchdog**: Warns when executor appears stuck with pending work
  - Modular design decomposed into 8 focused modules
  - See `docs/dev/job-executor-design.md` for architecture

- **Aircraft Position & Telemetry (APT) Module**: Real-time flight data integration
  - Receives UDP telemetry from X-Plane via ForeFlight protocol (port 49002)
  - Flight path tracking with heading calculation from position history
  - Aggregates telemetry from multiple sources with authoritative track selection
  - Dashboard integration showing GPS status and telemetry source

- **Scene Tracker**: Empirical X-Plane request monitoring
  - Tracks aggregate FUSE request load for circuit breaker
  - Detects scene loading events to pause prefetch
  - Shared load counter across all mounted services

- **Self-Contained Cache Services**: Improved cache lifecycle management
  - `CacheLayer` encapsulates memory + disk caches with internal lifecycle
  - In-memory LRU index for O(1) cache tracking without filesystem scanning
  - Garbage collection runs as executor job - async, cancellable, yields to high-priority work
  - Cache bridges for executor integration (`MemoryCacheBridge`, `DiskCacheBridge`)

- **Integration Tests**: Comprehensive executor test coverage
  - Tests for job execution, sequential tasks, concurrent jobs
  - Tests for kill signal cancellation, error policies, graceful shutdown
  - Tests for zero-task edge case

### Fixed

- **Directory Listing (#38)**: `ls` now works in FUSE mounted directories
  - Implemented `opendir` and `readdirplus` operations
  - Proper directory handle management

- **Skip Installed Ortho Tiles (#39)**: Prefetch no longer downloads tiles already on disk
  - Checks `OrthoUnionIndex` before submitting tiles for download
  - Covers both user-installed ortho packages and patches

- **Disk Cache Size Reporting**: Fixed metrics showing inflated cache size
  - Root cause: `populate_from_disk()` double-counted when called twice during startup
  - Root cause: Reporter formula `initial + written - evicted` accumulated drift
  - Replaced with authoritative absolute value from LRU index via `DiskCacheSizeUpdate` events
  - Cache size now matches OS-reported disk usage across restarts

- **Request Coalescing**: Prevents duplicate work for same tile
  - Concurrent requests for the same tile share a single generation job

- **Panic with Zero Tasks**: Jobs with 0 tasks no longer panic
  - Fixed `drain(..1)` on empty vector in CacheGcJob

- **APT Debug Logging Panic**: Resolved crash when debug logging enabled
  - Fixed format string issue in airport coordinate logging

### Changed

- **Executor Architecture**: Decomposed monolithic executor into focused modules
  - `core.rs`: JobExecutor struct and main event loop
  - `config.rs`: ExecutorConfig and public constants
  - `submitter.rs`: JobSubmitter and SubmittedJob
  - `active_job.rs`: ActiveJob state and TaskCompletion
  - `lifecycle.rs`: Job/task lifecycle management
  - `dispatch.rs`: Task dispatching to workers
  - `signals.rs`: Signal processing (pause, resume, stop, kill)
  - `watchdog.rs`: StallWatchdog for stall detection

- **Metrics Architecture**: 3-layer event-based telemetry
  - Fire-and-forget emission pattern
  - Disk read tracking for cache diagnostics
  - Disk cache size tracked via authoritative `DiskCacheSizeUpdate` events from LRU index
  - Sparkline consolidation across widgets

- **TUI Dashboard**: Improved queue display and layout
  - Active Tiles consolidated into Scenery System as QUEUE column
  - Tiles coalesced by 1-degree coordinate with progress averaging
  - Queue sorted by progress descending (tiles about to complete shown first)
  - Consistent 3-char padding across all panels

- **Service Initialization**: Consolidated startup via `ServiceOrchestrator`
  - Eliminated duplicate initialization paths
  - Proper metrics ordering with cache lifecycle

- **Prefetch System**: Removed legacy prefetchers
  - Heading-aware and radial prefetchers replaced by adaptive system
  - Single `AdaptivePrefetchCoordinator` handles all flight phases

### Removed

- **Legacy Sync Download Pipeline**: ~500 lines of code removed
  - Removed `tile/` module (TileOrchestrator, ParallelTileGenerator)
  - Removed `orchestrator/` module (DownloadOrchestrator)
  - Replaced by async Job Executor Framework

- **Deprecated MountManager Methods**: Cleaned up unused mount operations

- **Deprecated UI Widgets**: Removed obsolete dashboard components

- **Dead Code**: Removed unused `retry_counts` field from ActiveJob

- **Legacy Disk Eviction Daemon**: Superseded by job-based GC via `CacheGcJob`

### Deprecated

- **Configuration Keys**: The following `pipeline.*` keys are deprecated
  - Use `executor.*` equivalents instead
  - Run `xearthlayer config upgrade` to migrate

### Documentation

- `docs/dev/adaptive-prefetch-design.md` - Prefetch system design with empirical findings
- `docs/dev/xplane-scenery-loading-whitepaper.md` - X-Plane loading behavior research
- `docs/dev/job-executor-design.md` - Executor framework architecture
- `CLAUDE.md` - Project guidelines for AI-assisted development

### Upgrade Notes

**New `config.ini` settings for v0.3.0:**

```ini
[executor]
# Resource pool configuration
max_concurrent_tasks = 128
job_channel_capacity = 256

# Job concurrency limits (balances pipeline stages)
tile_generation_limit = 40
```

**Breaking Changes:**

- `pipeline.*` configuration keys no longer used (replaced by `executor.*`)
- Legacy `TileGenerator` API removed (use `DdsClient` trait)
- `run_eviction_daemon` deprecated (GC now internal to `DiskCacheProvider`)

Run `xearthlayer config upgrade` to automatically migrate configuration.

### Notes for Testers

The new Job Executor Framework is aggressive with system resources by design — it
maximizes CPU, network, and disk I/O utilization to minimize scene loading times.
If you experience instability in X-Plane or XEarthLayer:

1. Run `xearthlayer diagnostics` and include the system report
2. Collect the X-Plane `Log.txt` and the XEarthLayer log output
3. Report the issue at https://github.com/samsoir/xearthlayer/issues
4. Running with `xearthlayer run --debug` provides additional diagnostic output

## [0.2.12] - 2026-01-10

> **Note**: v0.2.11 was skipped due to a release infrastructure issue.

### Added

- **Consolidated FUSE Mounting**: Single mount point for all ortho sources
  - All ortho packages and patches merged into one FUSE mount (`zzXEL_ortho`)
  - `OrthoUnionIndex` provides efficient merged index with priority-based collision resolution
  - Patches (`_patches/*`) always take priority over regional packages
  - Parallel source scanning via rayon for fast index building
  - Index caching with automatic mtime-based invalidation

- **Tile Patches Support**: Custom mesh/elevation from airport add-ons
  - Place scenery folders in `~/.xearthlayer/patches/`
  - Patches with `Earth nav data/` containing DSF files are automatically discovered
  - Patches override regional package tiles for the same coordinates
  - See `docs/dev/index-building-optimization.md` for details

- **Circuit Breaker for Prefetch**: Intelligent prefetch throttling during scene loading
  - Detects X-Plane scene loading via FUSE request rate spikes
  - Pauses prefetch when load exceeds threshold to reduce resource contention
  - Three states: Closed (normal), Open (paused), Half-Open (testing recovery)
  - Configurable threshold and timing via new config settings

- **Ring-Based Radial Prefetching**: Improved prefetch tile selection
  - Configurable tile radius for prefetch zone
  - Shared FUSE load counter across all mounted services

### Fixed

- **DDS Zoom Level Bug**: Fixed incorrect texture generation at wrong zoom levels
  - Previously hardcoded zoom level 18 for all DDS requests
  - Now correctly extracts zoom from DDS filename (`{row}_{col}_ZL{zoom}.dds`)
  - Fixes terrain texture mismatches and visual artifacts

- **Index Building Performance**: Lazy resolution for terrain/textures directories
  - Skips pre-scanning of large `terrain/` and `textures/` folders
  - Resolves file paths on-demand during FUSE operations
  - Dramatically reduces startup time for large scenery packages

### Changed

- **FUSE3 System Architecture**: Major refactoring for maintainability
  - New shared traits module (`fuse3/shared.rs`)
  - `FileAttrBuilder` trait for unified metadata conversion
  - `DdsRequestor` trait for common DDS request handling
  - Eliminated ~650 lines of duplicated code across FUSE filesystems

- **Prefetch Configuration**: New circuit breaker settings
  - `prefetch.circuit_breaker_threshold` - FUSE requests/sec to trigger (default: 50)
  - `prefetch.circuit_breaker_open_ms` - Pause duration in milliseconds (default: 500)
  - `prefetch.circuit_breaker_half_open_secs` - Recovery test interval (default: 2)
  - `prefetch.radial_radius` - Tile radius for prefetch zone (default: 120)

- **Log Verbosity**: Moved high-volume tile request logs from INFO to DEBUG
  - Reduces log noise during normal operation
  - Use `--debug` flag to see detailed tile request logging

### Removed

- **Deprecated Prefetch Setting**: `prefetch.circuit_breaker_open_secs`
  - Replaced by `prefetch.circuit_breaker_open_ms` for finer control

### Upgrade Notes

**New `config.ini` settings for v0.2.12:**

```ini
[prefetch]
# Circuit breaker settings (pause prefetch during X-Plane scene loading)
circuit_breaker_threshold = 50     ; FUSE requests/sec threshold
circuit_breaker_open_ms = 500      ; Pause duration in milliseconds
circuit_breaker_half_open_secs = 2 ; Recovery test interval

# Radial prefetch tile radius
radial_radius = 120                ; Tiles in each direction (max: 255)
```

**Patches directory setup:**

To use custom mesh/elevation from airport add-ons:

1. Create the patches directory: `mkdir -p ~/.xearthlayer/patches/`
2. Copy your scenery folders into it (e.g., `KDEN_Mesh/`)
3. Each patch must contain `Earth nav data/` with DSF files
4. Patches are automatically discovered on next `xearthlayer run`

Run `xearthlayer config upgrade` to automatically add new settings with defaults.

## [0.2.10] - 2026-01-02

### Added

- **Setup Wizard**: Interactive first-time configuration (`xearthlayer setup`)
  - Detects X-Plane 12 Custom Scenery folder automatically
  - Auto-detects system hardware (CPU, memory, storage type)
  - Recommends optimal cache settings based on hardware
  - Configures package and cache directories

- **Default-to-Run Behavior**: Running `xearthlayer` without arguments starts the service
  - Same as `xearthlayer run`
  - Streamlined onboarding for new users

- **Coverage Map Generator**: Visualize package tile coverage (`xearthlayer publish coverage`)
  - Static PNG maps with light/dark themes (`--dark`)
  - Interactive GeoJSON maps for GitHub rendering (`--geojson`)
  - Configurable dimensions for PNG output

- **Zoom Level Deduplication**: Remove overlapping tiles to eliminate Z-fighting
  - `xearthlayer publish dedupe` command
  - Priority modes: highest, lowest, or specific zoom level
  - Dry-run and tile filtering support
  - Gap protection prevents creating coverage holes

- **Coverage Gap Analysis**: Identify missing tiles in packages
  - `xearthlayer publish gaps` command
  - JSON and text output formats
  - Filter by specific tile coordinates

- **GitHub Releases Publishing Runbook**: Step-by-step documentation
  - Complete workflow for package publishing via GitHub Releases
  - Website sync integration with automatic update triggers

### Changed

- **Default Library URL**: Now points to `https://xearthlayer.app/packages/`
  - No configuration required for new installations
  - Existing configs can use `xearthlayer config upgrade` to update

- **System Detection**: Moved from CLI to core library
  - Hardware detection now reusable by future GTK4 UI
  - Same SystemInfo struct for all frontends

### Documentation

- Updated getting started guide with setup wizard instructions
- Added configuration reference for new settings
- Improved command documentation with examples
- Added website sync automation documentation

## [0.2.9] - 2025-12-28

### Added

- **Disk Cache Eviction Daemon**: Background task enforcing disk cache size limits
  - Runs every 60 seconds via async tokio task
  - Immediate eviction check on startup
  - LRU eviction based on file modification time (mtime)
  - Evicts to 90% of limit to leave headroom for new writes
  - Diagnostic logging for troubleshooting cache issues

- **SceneryIndex Persistent Cache**: Dramatically faster startup times
  - Caches scenery tile index to `~/.cache/xearthlayer/scenery_index.bin`
  - Cache validated against package list on each startup
  - Rebuilds automatically when packages change
  - Reduces startup from 30+ seconds to <1 second on subsequent launches

- **SceneryIndex CLI Commands**: Manage the scenery index cache
  - `xearthlayer scenery-index status` - Show cache status and tile counts
  - `xearthlayer scenery-index update` - Force rebuild from installed packages
  - `xearthlayer scenery-index clear` - Delete cache file

- **Cold-Start Prewarm**: Pre-populate cache before X-Plane starts loading
  - New `--airport <ICAO>` flag for `xearthlayer run`
  - Loads tiles within configurable radius (default 100nm) of departure airport
  - Runs in background after scenery index loads
  - Press 'c' to cancel prewarm and proceed to flight
  - Configure radius via `prewarm.radius_nm` in config.ini

- **Dashboard Loading State**: Visual feedback during startup
  - Shows scenery index building progress (packages scanned, tiles indexed)
  - Displays "Cache loaded" message when using cached index
  - Smooth transition to running state when ready

### Fixed

- **Disk Cache Size Enforcement**: Cache now respects configured limits
  - Previously, disk cache would grow unbounded (design existed but wasn't implemented)
  - Eviction daemon now actively removes oldest tiles when over limit

- **Config Upgrade Detection**: Properly identifies deprecated and unknown keys
  - `xearthlayer config upgrade` now correctly detects all config issues
  - Fixed false negatives where deprecated keys were not flagged

- **Airport Coordinate Parsing**: Fixed prewarm searching wrong locations
  - Corrected lat/lon parsing from apt.dat files
  - Fixed potential deadlock during prewarm initialization

### Changed

- **Dual-Zone Prefetch Architecture**: Configurable inner and outer boundaries
  - `prefetch.inner_radius_nm` (default 85nm) - Inner boundary of prefetch zone
  - `prefetch.outer_radius_nm` (default 95nm) - Outer boundary of prefetch zone
  - Replaced single distance setting with explicit zone boundaries
  - See updated docs for zone configuration guidance

- **Dashboard Modularization**: Refactored into focused submodules
  - Improved maintainability and testability
  - No user-visible changes

- **Concurrency Limiter Naming**: Clearer internal naming for debugging
  - HTTP limiter, CPU limiter, Disk I/O limiter now consistently named
  - Helps identify bottlenecks in logs

- **Prewarm Radius**: Reverted from 300nm to 100nm default
  - 100nm matches X-Plane's standard DSF loading radius
  - Larger radius wastes bandwidth on tiles X-Plane won't request

### Removed

- **Deprecated Prefetch Settings**: Cleaned up unused configuration
  - Removed `prefetch.prediction_distance_nm` (replaced by inner/outer radius)
  - Removed `prefetch.time_horizon_secs` (no longer used)

### Upgrade Notes

**Recommended `config.ini` changes for v0.2.9:**

```ini
[prefetch]
# New dual-zone settings (replace prediction_distance_nm if present)
inner_radius_nm = 85
outer_radius_nm = 95

[prewarm]
# Optional: adjust prewarm radius for Extended DSFs
radius_nm = 100    ; Default covers standard DSF loading
; radius_nm = 150  ; Use for Extended DSFs setting
```

Run `xearthlayer config upgrade` to automatically add new settings with defaults.

## [0.2.8] - 2025-12-27

### Added

- **Heading-Aware Prefetch**: Intelligent tile prefetching based on aircraft heading and flight path
  - Prediction cone prefetches tiles ahead of aircraft in direction of travel
  - Graceful degradation to radial prefetch when telemetry unavailable
  - Configurable cone angle (default 45°) and prefetch zone (85-95nm from aircraft)
  - Strategy selection via `prefetch.strategy` setting (auto/heading-aware/radial)

- **Multi-Zoom Prefetch (ZL12)**: Prefetch distant terrain at lower resolution
  - ZL12 tiles reduce stutters at the ~90nm scenery boundary
  - Separate configurable zone (88-100nm) for distant terrain
  - Toggle via `prefetch.enable_zl12` setting

- **Configuration Auto-Upgrade**: Safely update config files when new settings are added
  - New `xearthlayer config upgrade` command with `--dry-run` preview mode
  - Creates timestamped backup before modifying configuration
  - Startup warning when config is missing new settings
  - Syncs all 43 ConfigKey entries with ConfigFile settings

- **Pipeline Control Plane**: Improved job management and health monitoring
  - Semaphore-based concurrency limiting prevents resource exhaustion
  - Stall detection and automatic job recovery (default 60s threshold)
  - Health monitoring with configurable check intervals
  - Dashboard shows control plane status (Healthy/Degraded/Critical)

- **Quit Confirmation**: Prevents accidental X-Plane crashes from premature exit
  - Press 'q' twice or Ctrl+C twice to confirm quit
  - Warning message explains potential impact on X-Plane

- **Dashboard Improvements**: Enhanced real-time monitoring
  - GPS status indicator shows telemetry source (UDP/FUSE/None)
  - Prefetch mode display (Heading-Aware/Radial)
  - Grid layouts with section titles for better organization
  - On-demand tile request instrumentation

- **Predictive Tile Caching**: X-Plane 12 telemetry integration
  - ForeFlight protocol UDP listener (port 49002)
  - FUSE-based position inference as fallback
  - TTL tracking prevents re-requesting failed tiles

### Fixed

- **Apple Maps Authentication**: Token race condition and missing headers
  - Added Origin and Referer headers required by Apple's tile server
  - Fixed token refresh race condition during concurrent requests
  - Improved token refresh logging and status tracking

- **Memory Cache Overflow**: Shared cache across multiple packages
  - Cache size now correctly tracked across all mounted scenery packages

- **Coordinate Calculation Bug**: Scenery-aware prefetch tile positioning
  - Fixed incorrect tile coordinates causing cache misses

- **FUSE Unmount Race Condition**: Orphaned mounts on shutdown
  - Proper synchronization prevents mount table corruption

- **First-Cycle Rate Limiting**: Prefetch now respects rate limits from startup

### Changed

- **Async Cache Implementation**: Replaced blocking synchronization primitives
  - `MemoryCache` now uses moka async cache instead of std::sync::Mutex
  - `InodeManager` uses DashMap instead of blocking Mutex
  - Eliminates potential deadlocks under high concurrency

- **HTTP Concurrency Ceiling**: Hard limit of 256 concurrent requests
  - Prevents provider rate limiting (HTTP 429) and cascade failures
  - Configurable via `pipeline.max_http_concurrent` (64-256 range)

- **Configurable Radial Radius**: `prefetch.radial_radius` setting
  - Default 3 (7×7 = 49 tiles), adjustable for bandwidth/coverage tradeoff

### Performance

- **Permit-Bounded Downloads**: Semaphore-based download limiting with panic recovery
- **Request Coalescing**: Control plane deduplicates concurrent requests for same tile
- **Prefetch Concurrency Limiting**: Dedicated limiter prevents prefetch from starving on-demand requests

## [0.2.7] - 2025-12-21

### Fixed

- **Deadlock with Cached Chunks**: Fixed system freeze when loading scenery with partially cached tiles
  - Root cause: Unlimited assembly tasks monopolized blocking thread pool, starving disk I/O and encode stages
  - Solution: Added concurrency limiter to assembly stage and merged with encode into shared CPU limiter

### Added

- **Storage-Aware Disk I/O Profiles**: Automatic detection and tuning of disk I/O concurrency based on storage type
  - Auto-detection via `/sys/block/<device>/queue/rotational` on Linux
  - HDD profile: Conservative concurrency (1-4 ops) for seek-bound devices
  - SSD profile: Moderate concurrency (32-64 ops) - default fallback
  - NVMe profile: Aggressive concurrency (128-256 ops) for high-performance drives
  - Configurable via `cache.disk_io_profile` setting (auto/hdd/ssd/nvme)

- **Shared CPU Limiter with Over-Subscription**: Improved CPU utilization during tile generation
  - Merged assemble and encode stages into single shared limiter
  - Formula: `max(num_cpus * 1.25, num_cpus + 2)` keeps cores busy during brief I/O waits
  - Prevents blocking thread pool exhaustion while maximizing throughput

### Changed

- Disk I/O concurrency now tuned per storage type instead of fixed formula
- Assembly and encode stages share concurrency limit instead of separate limiters

## [0.2.6] - 2025-12-19

### Added

- **Additional Imagery Providers**: Four new satellite imagery sources
  - Apple Maps - High quality imagery with automatic token acquisition via DuckDuckGo MapKit
  - ArcGIS World Imagery - ESRI's global satellite imagery service (no API key required)
  - MapBox Satellite - High resolution imagery (requires free access token)
  - USGS Orthoimagery - Excellent quality imagery for United States coverage

- **Disk I/O Concurrency Limiting**: Prevents file descriptor exhaustion under heavy load
  - Semaphore-based rate limiting for FUSE file reads and directory operations
  - Configurable scaling formula: `min(num_cpus * 16, 256)` concurrent operations
  - Addresses potential crashes during X-Plane scene loading with warm cache

- **Debug Logging Flag**: New `--debug` flag for `xearthlayer run` command
  - Enables debug-level logging for troubleshooting
  - Useful for diagnosing issues in the field without modifying config

### Changed

- HTTP client now supports Bearer token authentication (required for Apple Maps)
- Improved provider validation in configuration system

### Fixed

- Apple Maps token extraction updated for new DuckDuckGo JWT format
- Apple Maps authentication now uses proper Bearer token header

### Acknowledgements

- Thanks to [xjs36uk](https://forums.x-plane.org/profile/1171657-xjs36uk/) from the X-Plane.org forums for testing v0.2.5 and reporting the disk I/O concurrency issue

## [0.2.5] - 2025-12-15

### Changed

- Simplified CI workflow to single `make verify` job
- Redesigned release workflow with atomic publish process:
  - Creates draft release first
  - Uploads all artifacts (Linux binary, .deb, .rpm, AUR files)
  - Publishes release only after all assets uploaded successfully
- Uses GitHub CLI for release operations instead of third-party actions

### Fixed

- Release workflow race condition where release was published before assets uploaded
- CI/CD pipeline reliability issues with parallel async steps

## [0.2.0] - 2025-12-15

### Added

- **Async Pipeline Architecture**: Complete rewrite of the tile generation pipeline using async/await for dramatically improved performance and resource efficiency
  - Async FUSE filesystem implementation using fuse3 with multi-threaded I/O
  - Request coalescing to deduplicate concurrent requests for the same tile
  - HTTP concurrency limiter to prevent network stack exhaustion
  - Cooperative cancellation support for FUSE timeout handling
  - Two-tier caching with memory and disk layers

- **TUI Dashboard**: Real-time monitoring interface showing:
  - Pipeline metrics (requests, cache hits, downloads, errors)
  - Active job tracking with status indicators
  - Log output panel for debugging

- **Linux Distribution Packages**:
  - Debian/Ubuntu packages via cargo-deb
  - RPM packages for Fedora/RHEL/openSUSE
  - AUR package (PKGBUILD) for Arch Linux
  - Static musl binary for universal Linux compatibility

- **CI/CD Pipeline**:
  - GitHub Actions workflow for automated testing on PRs
  - Release workflow for building and publishing packages
  - Branch protection for main branch

### Changed

- Minimum supported Rust version is now 1.70
- Improved error handling throughout the pipeline
- Reduced log verbosity for routine operations (moved to DEBUG level)
- Refactored facade into smaller, testable units following Single Responsibility Principle

### Fixed

- Pipeline freeze when FUSE operations timeout ([#5](https://github.com/samsoir/xearthlayer/issues/5))
- TUI corruption from log output during DDS errors
- TUI dashboard display issues with metrics and job counters
- Memory cache sharing between pipeline stages
- DDS validation for malformed texture files

### Performance

- 10x+ improvement in tile generation throughput
- Eliminated thread pool exhaustion under heavy load
- Reduced memory usage through request coalescing
- Faster cache lookups with async I/O

## [0.1.0] - 2025-01-01

### Added

- Initial release of XEarthLayer
- Regional scenery package system for on-demand satellite imagery
- Package manager with download, install, and library management
- FUSE-based virtual filesystem for X-Plane integration
- Support for Bing Maps and Google Maps imagery providers
- DDS texture generation with BC1/BC3 compression and mipmaps
- Two-tier caching (memory + disk) for performance
- CLI interface with package and config management
- Ortho4XP scenery package compatibility

### Notes

- Linux support only (Windows and macOS planned for future releases)
- Requires FUSE3 for filesystem mounting

[Unreleased]: https://github.com/samsoir/xearthlayer/compare/v0.4.5...HEAD
[0.4.5]: https://github.com/samsoir/xearthlayer/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/samsoir/xearthlayer/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/samsoir/xearthlayer/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/samsoir/xearthlayer/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/samsoir/xearthlayer/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/samsoir/xearthlayer/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/samsoir/xearthlayer/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/samsoir/xearthlayer/compare/v0.2.12...v0.3.0
[0.2.12]: https://github.com/samsoir/xearthlayer/compare/v0.2.10...v0.2.12
[0.2.10]: https://github.com/samsoir/xearthlayer/compare/v0.2.9...v0.2.10
[0.2.9]: https://github.com/samsoir/xearthlayer/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/samsoir/xearthlayer/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/samsoir/xearthlayer/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/samsoir/xearthlayer/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/samsoir/xearthlayer/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/samsoir/xearthlayer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/samsoir/xearthlayer/releases/tag/v0.1.0
