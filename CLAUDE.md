# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**XEarthLayer** is a Rust implementation inspired by AutoOrtho, providing on-demand satellite imagery streaming for X-Plane via a FUSE virtual filesystem. It downloads satellite imagery tiles as X-Plane requests them, eliminating the need for massive pre-downloaded scenery packages.

**Core Concept**: Creates a passthrough virtual filesystem that overlays real scenery files while generating DDS textures on-demand when X-Plane requests them.

**Current Status**: Functional and tested with X-Plane 12. Scene load times reduced from 4-5 minutes to ~30 seconds with parallel tile generation.

## Design Principles

**IMPORTANT**: This project follows SOLID principles and Test-Driven Development (TDD). See `docs/dev/design-principles.md` for details.

**All code must**:
- Be developed using TDD (write tests first)
- Follow SOLID principles (traits for abstraction, dependency injection)
- Be testable in isolation with mock support
- Maintain minimum 80% test coverage (target 90%+)

## Architecture

### Implemented Components

1. **FUSE Layer** (`xearthlayer/src/fuse/fuse3/`)
   - `Fuse3OrthoUnionFS` - The single FUSE mount: all ortho sources (patches + packages)
     merged into one union filesystem. The earlier `Fuse3PassthroughFS` and
     `Fuse3UnionFS` implementations were deleted in #233 once they became unreachable.
   - All FUSE operations run asynchronously on the Tokio runtime
   - Shared traits: `FileAttrBuilder`, `DdsRequestor` (SOLID: Interface Segregation)
   - Pattern matching for DDS files: `{row}_{col}_ZL{zoom}.dds`
   - Virtual DDS files open with `FOPEN_DIRECT_IO` on Linux so every read reaches the handler. macOS serves them through the kernel page cache (direct I/O crashes X-Plane's `mmap` texture loader under macFUSE), so the FUSE read counters under-report there; nothing functional depends on per-read visibility (#204). See `docs/dev/fuse-filesystem.md`

2. **Tile Generation** (`xearthlayer/src/tile/`)
   - `TileGenerator` trait with dependency injection
   - `DefaultTileGenerator` - Download + encode pipeline
   - `ParallelTileGenerator` - Thread pool with request coalescing

3. **Download Orchestrator** (`xearthlayer/src/orchestrator/`)
   - Parallel download of 256 chunks (16×16) per tile
   - Configurable concurrency, timeout, and retry logic
   - Failed chunks are not cached (retry on next request); see Cache System for tile-level rules
   - Retry classification lives in `executor/adapters/provider.rs` `map_provider_error`: transport failures and non-404 statuses are retryable, HTTP 404 is permanent (one attempt) because every shipped provider answers 404 only for "no tile here" (#301). The HTTP clients emit `ProviderError::HttpStatus { status, url }` for non-success responses, so callers match on the code, never on message text
   - A killed job (FUSE timeout, error policy, shutdown) aborts its in-flight chunk downloads and returns their permits before reporting `Cancelled`; chunks already downloaded keep their cache writes (#300)

4. **DDS Compression** (`xearthlayer/src/dds/`)
   - BC1/BC3 (DXT1/DXT5) compression
   - 5-level mipmap chain generation
   - `ImageCompressor` trait for single-level backends:
     - `SoftwareCompressor` — Pure-Rust fallback
     - `IspcCompressor` — SIMD-optimized via Intel ISPC (default)
   - `MipmapCompressor` trait for full-pipeline backends (GPU):
     - `GpuEncoderChannel` — Worker-side mipmap streaming, zero-clone channel transfer
   - `MipmapStream` — Memory-efficient iterator yielding one mipmap level at a time (no clones)
   - `system::encoder_policy` (#203): `EncoderPolicy` chooses the backend from a `HardwareProfile` (the adapters wgpu enumerated). `DefaultEncoderPolicy` honours an explicit `texture.compressor`, warns (log and an amber Status-pane line) when `gpu` is honoured on a host whose only GPU is the one X-Plane renders with (every Apple Silicon Mac, any single-GPU PC), and picks ISPC with no preference; GPU is never auto-selected. The runtime encoder and the wizard's GPU step both go through it, and the profile comes from the one enumeration the GPU path already does (`GpuSelection.host`), never a second one
   - GPU encoding architecture: `mpsc` channel → dedicated pipeline worker → `GpuBlockCompressor`
   - `WgpuCompressor` wraps `block_compression` crate (ISPC kernels ported to WGSL compute shaders)
   - Worker-side tile processing: one channel round-trip per tile, GPU buffer reuse across mipmap levels
   - `create_gpu_resources()` — shared factory for device/queue/compressor creation (DRY); returns the selected `GpuAdapter`, which reaches the dashboard's Status pane via `EncoderComponents::gpu_adapter` and `ServiceOrchestrator::gpu_adapter()` (never re-enumerate adapters for display: enumeration can take ~30s)
   - GPU pipeline hardening:
     - `map_async` errors propagated via `std::sync::mpsc` (not silently ignored)
     - Worker panic recovery via `catch_unwind` (logs error, sends failure, drains queue, breaks loop)
     - `device.on_uncaptured_error()` registered for device loss logging
     - Structured tracing on buffer lifecycle and error paths

5. **Cache System** (`xearthlayer/src/cache/`, `xearthlayer/src/service/cache_layer.rs`)
   - Three-tier cache hierarchy: Memory → DDS Disk → Chunk Disk → Network, plus a separate lit disk tier when night lighting is on
   - `CacheLayer` - Service-owned cache lifecycle with budget management: the three tiers, plus the lit disk tier when night lighting is on
   - `CacheService` - Self-contained cache with internal GC daemons
   - `MemoryCacheProvider`: moka-based LRU eviction (default 512MB, staging buffer)
   - `DiskCacheProvider`: Two instances (DDS tiles and raw chunks), three with night lighting on (plus lit payloads), each with own GC
   - DDS disk cache: Encoded DDS tiles on disk, avoids re-encoding on memory eviction (~3.5ms NVMe read vs ~50-200ms re-encode)
   - Lit tier (night lighting only, `night.enabled`): generated lit payloads on disk at `{cache_dir}/night/`, read path memory → disk → generate (`night/disk.rs`, `lit_payload_validator`), 5% of `cache.disk_size` taken first with `dds_disk_ratio` splitting the other 95%, own GC daemon on the prefetch lane; entries carry a SHA-256 fingerprint (generator version, brightness, Laminar art and recipe file size+mtime, and the package region each recipe was read from) so a mismatch regenerates. Off means no tier, directory or daemon
   - Shared disk budget: `cache.disk_size` split by `cache.dds_disk_ratio` (default 0.6 = 60% DDS, 40% chunks)
   - DDS disk directory: `{cache_dir}/{provider}/dds/`, chunks keep existing provider directory
   - Region-based disk layout: files in 1°×1° DSF subdirs (e.g., `+33-119/tile_15_12754_5279.cache`)
   - Read path: memory hit → DDS disk hit (+ promote to memory) → chunk disk (assemble+encode) → network
   - Write path (per-tier completeness rules):
     - Chunk disk: per-chunk, only successful HTTP downloads (failed/magenta chunks never written)
     - Memory + DDS disk: per-tile, fire-and-forget after encode, **only when all 256 chunks succeeded** (`ChunkResults::is_complete()`). Tiles with any failed chunks are served to X-Plane (so the sim doesn't stall) but never persisted; see issue #180
   - Parallel startup scanning via rayon over region directories
   - `migrate_cache()` for migrating flat-layout caches to region layout
   - Bridge adapters for executor integration (MemoryCacheBridge, DdsDiskCacheBridge, DiskCacheBridge)
   - Per-provider cache directories
   - `cache/integrity`: canonical corruption-handling model shared by every on-disk cache (index cache, scenery index cache, DDS/chunk/lit disk tiers): bounded reads (`length_ceiling`), atomic durable writes (`write_atomic`, which also creates the parent directory), and invariant-3 satisfied per cache: explicit `discard()` on the disk tiers, unconditional overwrite-on-rebuild for the index caches; see `docs/dev/cache-integrity-design.md`
   - `XEarthLayerService::start()` creates `CacheLayer` with proper metrics ordering

6. **Configuration** (`xearthlayer/src/config/`)
   - INI file at `~/.config/xearthlayer/config.ini` (Linux, honours `$XDG_CONFIG_HOME`) or `~/Library/Application Support/XEarthLayer/config.ini` (macOS). **Never construct this path**: use `paths::config_file()`
   - Auto-detection of X-Plane installation
   - See `docs/configuration.md` for all settings

7. **Providers** (`xearthlayer/src/provider/`)
   - Bing Maps (free, no API key)
   - Google Maps GO2 (free, no API key)
   - Google Maps (paid, requires API key)
   - Async variants (`AsyncBingMapsProvider`, `AsyncGo2Provider`, `AsyncGoogleMapsProvider`) for non-blocking I/O

8. **Job Executor Framework** (`xearthlayer/src/executor/`, `xearthlayer/src/jobs/`, `xearthlayer/src/tasks/`)
   - Trait-based job/task architecture with hierarchical job spawning
   - `Job` trait - High-level work unit (e.g., generate a DDS tile, prefetch a region)
   - `Task` trait - Atomic unit of work within a job (e.g., download chunks, encode DDS)
   - `ExecutorDaemon` - Background daemon that receives jobs via channel and executes them
   - `DdsClient` trait - Abstraction for submitting FUSE demand tile requests (`request_dds`); demand only since #296
   - `ChannelDdsClient` - Implementation that sends requests via mpsc channel
   - Priority scheduling inside a lane: `ON_DEMAND` > `PREFETCH` > `HOUSEKEEPING`. FUSE and prefetch no longer share a queue, so priority does not mediate between them
   - **Two lanes (#296)**: FUSE demand and prefetch each run on their own `ExecutorDaemon` + `JobExecutor` (own coalescer, channels, pools, `tile_generation` group, HTTP semaphore), sharing only the memory, DDS disk and chunk disk caches, plus the GPU encoder worker when `texture.compressor = gpu`. Coalescing is per lane and FUSE always starts its own job (always claim)
   - **Admission gate**: `ExecutorConfig::admission_gate` (`watch<bool>`). While `true` the executor admits no queued job; admitted jobs run every task. The prefetch lane is gated by demand, the demand lane has no gate. Child jobs bypass admission, and no production job spawns children (`TilePrefetchJob` was deleted)
   - Error policies: `FailFast`, `ContinueOnError`, `PartialSuccess`
   - Resource pools for concurrency limiting by resource type (Network, CPU, DiskIO)
   - See `docs/dev/job-executor-design.md` for full architecture

9. **Runtime Orchestrator** (`xearthlayer/src/runtime/`)
    - `XEarthLayerRuntime` - Central daemon orchestrator, owns the job channel
    - `RuntimeConfig` - Configuration for the runtime and executor daemon
    - `JobRequest` - Request message with tile, priority, cancellation, and response channel
    - `DdsResponse` - Response with generated DDS data and metadata
    - `RequestOrigin` - Tracks request source (FUSE, Prefetch, Prewarm, Manual)
    - Peer daemon architecture: FUSE, Prefetch, and the two executor daemons are independent
    - `PrefetchLane` trait (`runtime/lane.rs`) - Where prefetch work runs: `submit`, `submit_with_receipt` (outcome only, never tile bytes) and `load()`. The result of a submission is a complete tile in the primary's DDS disk cache, so a lane could run elsewhere
    - `LocalPrefetchLane` - The in-process lane (second daemon + executor). Also carries cache GC jobs via the local-only `job_submitter()`, so GC pauses with prefetch
    - `DemandTracker` (`executor/demand.rs`) - Counts demand jobs in flight with an RAII `DemandGuard` and publishes the pause state the prefetch lane's admission gate reads
    - `XEarthLayerRuntime` hands each lane to its consumers at construction (FUSE gets the demand client, the coordinator and prewarm get the lane), so FUSE cannot reach the prefetch lane by type
    - Channel as mediator pattern for maximum decoupling

10. **Package Publisher** (`xearthlayer-publisher/`, its own crate behind the `xearthlayer-publish` binary since #284; the runtime does not link it, and the shared format lives in `xearthlayer-package/`)
   - Creates distributable scenery packages from Ortho4XP output (ortho tiles and overlays)
   - Repository management with package versioning
   - Archive building with configurable part sizes, in process: the `tar.gz` streams straight into `split`-named parts, each hashed as written (#274)
   - Library index management for package discovery
   - `delete` retires a package: library index entry, working directory and built archives. Assets already uploaded to a hosting provider are deliberately never touched, because they must outlive the index entry while users are still installing from them
   - `release` also records `size.<type>.download_bytes` and `size.<type>.installed_bytes` into `region_metadata.json`; a missing file or missing region entry is a warning, not a failed release, because updating the library index is the release
   - `Ortho4XPProcessor` for ortho tiles, `OverlayProcessor` for overlays
   - DSF files are written as single-entry 7z containers (Laminar profile: LZMA, 16 MiB dictionary) by `DsfCompressor`; X-Plane decodes them natively, XEL never parses them (#124)
   - `spec_version` on both package formats is enforced: a MAJOR above `SUPPORTED_SPEC_MAJOR` (`package/spec.rs`) is refused with an upgrade message

11. **Predictive Tile Caching** (`xearthlayer/src/prefetch/`)
    - `AdaptivePrefetchCoordinator` - Prefetch with flight phase detection
    - `GroundStrategy` - Ring-based prefetching for ground operations (GS < 40kt)
    - `PrefetchBox` - Heading-biased sliding box (speed-proportional extent, proportional 80/20 bias, cruise phase)
    - `ExtentCalculator` - Speed-proportional box extent: clamped linear ramp 3.5° at 40kt to `box_extent` (default 7.0°) at 450kt in cruise; the ground box is the full `box_extent`, unbiased
    - `SceneryWindow` - Window config and dimensions holder; dead code, nothing outside its tests constructs it
    - `BoundaryStrategy` - Region lifecycle management (InProgress/Prefetched/NoCoverage/Deferred)
    - `PhaseDetector` - Ground/Cruise flight phase state machine; reset via `on_ground` from AircraftState on telemetry resume
    - `SimState` - Direct sim state detection via X-Plane Web API (paused, on_ground, scenery_loading, replay)
    - `TransitionThrottle` - Grace period + ramp-up after Ground-to-Cruise transition
    - `LoopState` - Per-cycle runner state including `telemetry_paused` flag for stale telemetry safe mode
    - `PrefetchStateObserver` - FUSE-side divergence detector; demotes a `Prefetched`/`NoCoverage` region back to *absent* when FUSE has to generate a tile on demand there, rate-limited to one demotion per region per 120s window
    - Submits tiles to the prefetch lane via the `PrefetchLane` trait; the lane admits nothing while FUSE is waiting on generation (no-harm contract, see `docs/dev/adaptive-prefetch-design.md`)
    - Prefetch-lane tile jobs start with `CachedTileProbeTask` (`tasks/cached_tile_probe.rs`), which skips tiles already cached when the job is admitted
    - `PrefetchState` (`prefetch/state.rs`) - the single dashboard state resolved by precedence (Disabled, Paused (demand), Holding (takeoff), Deferred (load), Ramping, Active, Idle)
    - Mode is `prefetch.mode` alone: `opportunistic` (default), `aggressive` or `disabled`. The throughput calibrator that was meant to choose it had no input and no reader, and was removed in #277 along with the `auto` value; `PerformanceCalibration::default_opportunistic()` survives only as the fixed estimate `PrefetchPlan` budgets with
    - **Four-tier filtering**: Memory cache → Patched region exclusion (via `GeoIndex`) → Installed package disk (via `OrthoUnionIndex`) → XEL DDS disk cache (the write-only `cached_tiles` local-tracking shadow tier was deleted in #176 after it was confirmed to never be read)
    - **Backpressure takes the higher of two signals**: `PrefetchLane::load()` (jobs waiting for admission on the prefetch lane, against its `tile_generation` group size) and `fuse_pressure` (memory pinned by open virtual DDS handles, as a fraction of the ceiling in `pinned_budget::PinnedTileBudget`). They diverge at a boundary crossing, where most reads are cache-served and so invisible to the lanes; that collision sets the memory floor per #227. Open handles rather than a read rate because `open`/`release` reach FUSE on every platform and reads do not (macOS page-cache serving). See #246
    - **Two-phase region commit**: Regions marked `InProgress` in GeoIndex during prefetch, promoted to `Prefetched` on completion
    - **Stale telemetry safe mode**: pauses tile submissions when telemetry stale >5s; on resume, reads `on_ground` from AircraftState to reset PhaseDetector before next cycle
    - See `docs/dev/adaptive-prefetch-design.md` for design details

12. **Aircraft Position & Telemetry** (`xearthlayer/src/aircraft_position/`)
    - `StateAggregator` - Combines multiple position sources with accuracy-based selection
    - `PositionModel` - Persistent position model refined by best available data
    - `WebApiAdapter` - Connects to X-Plane Web API (REST + WebSocket) for position and sim state
    - `FlightPathHistory` - Position history for track derivation
    - Position sources: Web API (10m), OnlineNetwork (10m), ManualReference (100m). Position inference from FUSE access patterns (`InferenceAdapter`, `SceneInference`) and the `scene_tracker` module that fed it were deleted in #204: neither had a production caller on any platform
    - Higher accuracy wins; stale high-accuracy can be beaten by fresh lower-accuracy
    - `AircraftState` includes `on_ground: bool` propagated from SimState → WebApiAdapter → aircraft_position → prefetch

13. **Ortho Union Index** (`xearthlayer/src/ortho_union/`)
    - `OrthoUnionIndex` - Merged index of all ortho sources (patches + packages)
    - `OrthoUnionIndexBuilder` - Builder pattern for index construction
    - Parallel scanning via rayon for performance
    - Lazy resolution for `terrain/` and `textures/` directories (skips pre-scanning)
    - Index caching with mtime-based invalidation
    - `IndexBuildProgress` - Progress reporting with factory methods
    - Priority resolution: patches (`_patches/*`) sort before regional packages
    - `dds_tile_exists(row, col, zoom)` - Checks if DDS tile exists on disk (used by prefetch)
    - `resolve_lazy_filtered(path, predicate)` - Source-filtered lazy resolution (used by FUSE with GeoIndex)

14. **Package Install Queue** (`xearthlayer/src/manager/queue/`)
    - `InstallQueue`: a downloader thread and an installer thread; downloads run ahead of installs, installs run one at a time in order. `PackageInstaller` does the work as its `Stages`. A panicking stage fails only its own job
    - `DiskLedger` admits a download only if `4*s[j] + sum(s after j)` fits for every admitted j (`SPACE_BUFFER_MULTIPLIER`), so the installer never waits and the threads cannot deadlock. Free space is read once per busy period, never mid-install
    - Removes (`packages remove` too) skip the downloader; `submit` refuses a second job for the same package
    - `CancelToken` per job, checked per buffer and per tar entry; the new tree is first staged beside its destination (`.xel-staging-*`, copied when temp is on another filesystem), then the swap (`manager/aside.rs`: rename old aside, rename staged in, delete old after `end_swap`) is the only step that cannot be cancelled
    - `flock` on `{temp_dir}/.queue.lock` allows one package operation at a time; `sweep` at start deletes `install_*` and `.xel-staging-*` leftovers, and renames a `.xel-aside-*` back when its package folder is missing (else deletes it)
    - CLI signals (`xearthlayer-cli/src/signals.rs`): first SIGINT cancels, a second one or SIGTERM/SIGHUP abandons (exit 130, 143, 129 by the first signal); a stop request is remembered across the per-package queues of a multi-package remove; with no queue running, the default action. `run` is not on it yet (#314)

15. **Version Update Checker** (`xearthlayer/src/update/`)
    - `VersionManifest` - Serde model for remote `version.json`
    - `UpdateInfo` - Update notification data (current version, latest version, homepage)
    - `UpdateChecker` trait - Abstraction for version checking (dependency injection)
    - `RemoteUpdateChecker` - Production implementation with HTTP fetch and 24h disk cache
    - Non-blocking: spawned on Tokio runtime, never delays startup
    - Cache at `paths::version_check_file()`, in the state directory (mtime-based 24h expiry)
    - TUI dashboard shows persistent footer when update available
    - Configurable via `general.update_check` (default: true)

16. **Path Resolution** (`xearthlayer/src/paths/`)
    - `BaseDirectories` trait with `XdgDirectories`, `AppleDirectories`, `LegacyDirectories` and a test double
    - Four roles: config, cache, data, state. **macOS collapses config and data into one directory** and has no `dirs::state_dir()`, so the platforms differ in shape and not only in values
    - Role accessors (`config_file()`, `packages_dir()`, `log_file()`, ...) are the only supported way to get a path. **Never construct one from `home_dir()`**
    - Platform selected with `cfg!`, not `#[cfg]`, so both implementations are compiled and tested everywhere
    - `LAYOUT_VERSION` and `general.layout_version` record which layout a config conforms to; **0 means pre-0.5.0**, and `ConfigFile::default()` uses 0 deliberately because the parser overlays a file onto the defaults
    - One site intentionally still names `~/.xearthlayer`: `preflight/mod.rs` `legacy_default_packages_dir()`, which detects a pre-XDG install
    - See `docs/directory-layout.md` and `docs/dev/preflight-design.md`

17. **Preflight Checks** (`xearthlayer/src/preflight/`, `xearthlayer-cli/src/preflight/`)
    - `Preflight<C>` trait and `Runner<C>` in the library, generic over context; the concrete checks live in the CLI
    - Executed in `main()` **before command dispatch**, so `run` and `setup` get the same answer to "does this installation exist"
    - `BootstrapContext` accumulates validated inputs (config, install location, Custom Scenery path); carries **inputs, not constructed services**
    - Registration order is execution order, asserted by test. No dependency graph
    - `inspect` is pure and `remediate` acts, so the registry can also be run as a report; remediation is verified by a second inspection
    - Checks never call one another. A check reading an absent contributed value returns `Unsatisfied` **naming** it rather than unwrapping
    - Platform exclusion uses `cfg!` in `applies()`, not `#[cfg]`, so macOS-only checks are still compiled and tested on Linux
    - X-World Pro (#213): `xworld-pro-detect` contributes `xearthlayer::xworld_pro::Detection` when `packages.xworld_pro` is on (hint only when off), `xworld-pro-order` stops or continues on a bad `scenery_packs.ini` order via an injected `ConfirmOrder`, `xworld-pro-setup` warns; overlay consolidation leaves active continents out via `OrchestratorConfig.xworld_pro`. See `docs/x-world-pro.md`
    - See `docs/dev/preflight-design.md`

18. **GeoIndex** (`xearthlayer/src/geo_index/`)
    - `GeoIndex` - Type-keyed, region-indexed geospatial reference database (thread-safe, ACID)
    - `DsfRegion` - 1°×1° DSF region coordinate type
    - `GeoLayer` trait - Marker trait for storable layer types (`Clone + Send + Sync + 'static`)
    - `PatchCoverage` - Layer type indicating a region is owned by a scenery patch
    - `RetainedRegion` - Layer type tracking regions in the SceneryWindow's retained set
    - `PrefetchedRegion` - Layer type tracking prefetch state per region (InProgress, Prefetched, NoCoverage, Deferred)
    - **Region retirement (#226)**: `NoCoverage` is reachable *only* when the scenery index attributes zero tiles to a region. A region with indexed tiles that is merely slow or stuck is `Deferred` — a non-terminal state that expires on a 20/30/40/60s ladder — so its tiles stay retryable
    - Locking: `RwLock<HashMap<TypeId, DashMap>>` — rare write locks, concurrent reads via DashMap shards
    - `populate()` provides atomic bulk loading (builds outside lock, swaps in)
    - Used by FUSE (`is_geo_filtered`), prewarm, and prefetch for patch region exclusion and prefetch state tracking
    - See `docs/dev/geo-index-design.md` for design details

19. **Night Lighting** (`xearthlayer/src/night/`)
    - Recipe reader (`recipe.rs`): OXO `.oxn` format 1
    - `RecipeStore` (`store.rs`): recipes found via each ortho source path, `night/<block>/<tile>.oxn`
    - `LitTextureSource` / `BlackLitSource` (`service.rs`) and the 152-byte minimal black DDS (`layout.rs`)
    - FUSE routes `_LIT.dds` names (`TextureKind::Lit`, `LIT_INODE_FLAG`) to the lit source, never the executor
    - Phase 2a: `NightLitSource` (`service.rs`) answers lit `stat`/`open` with the real file size for the player's texture setting; `LaminarArt` (`laminar/`) reads the installed lit art, `StartupTextureSetting` (`prefs.rs`) reads the setting once at start, and `footprint.rs` gives a texture's ground footprint (whether it has light is `RecipeField::has_light`, phase 2b) and `layout.rs` (`LitLayout`) sizes the file. Wired by `MountManager::set_night_lighting` from `night.enabled` (default true); when the X-Plane installation is not found the black default stays. The outcome is recorded as `night::NightLighting` (Off, Lit, Dark) and read by the dashboard's Status pane through `ServiceOrchestrator::night_lighting()`
    - Generator (`generator/`): `LitGenerator::generate(&LitRequest) -> RgbaImage` is infallible and `encode_visible` BC1-encodes with the default ISPC encoder, dimming the first visible level to 2/3 of the light (`FADE_RAMP`, sRGB-corrected) so X-Plane's level-by-level page-in fades a texture out in two steps; the first visible level stays fixed in metres at every texture setting because the blank level's texel size sets the cut-out distance; every lit read is logged at debug (`Night lighting: lit read`) with its offset. `CpuLitGenerator` (`cpu.rs`) computes each texel from its ground position alone: `RecipeField` (`field.rs`) gives bilinear brightness and class weights from globally indexed cells (warped by value noise, `noise.rs`, so class edges are soft and cross 1 degree tiles), `gain.rs` maps VIIRS radiance and `night.brightness` (0.5 to 1.5) to a gain, and `tiling.rs` samples the Laminar pattern decoded by `laminar/pattern.rs` with hex stochastic tiling in Web Mercator metres; the LED stage (`led.rs`) turns neighbourhood patches LED white by class, VIIRS brightness and package region, keeping luminance times `LedModel::energy`, and sodium texels keep their bytes. `NightLitSource` serves the result after the blank levels via `LitLayout::read_lit`: payloads cached in memory (256 MB) keyed `(row, col, zoom)` and on disk (the lit tier, see Cache System), concurrent reads of one footprint coalesced (moka `optionally_get_with`), generation permits `(cores / 8).clamp(2, 4)`, 2 probe permits and 2 lit disk permits held inside `BLOCKING_POOL_HEADROOM` by a compile-time assertion, recipe reads retried twice then given up on per tile for the session (`SessionRecipes`), a generation missing a recipe served full-size black and not cached in memory or on disk, first committed probe answer wins. `has_light` is `RecipeField::has_light`, which follows the generator's sampling model (class reach and bilinear brightness reach, gain above zero) so it never answers dark where the generator lights. Tuning continues by flight
    - Publisher copies `yOrtho4XP_Night/` recipes into the package's `night/` folder
    - See `docs/dev/night-lighting-design.md`

### Module Dependencies

```
xearthlayer-cli
    └─→ service/facade (XEarthLayerService::start())
            ├─→ service/cache_layer (CacheLayer) ───────────────────────┐
            │       ├─→ cache/service (CacheService)                    │
            │       └─→ cache/providers (Memory, Disk with internal GC) │
            │                                                            │
            ├─→ runtime (XEarthLayerRuntime orchestrator) ──────────────┤
            │       └─→ executor (2x ExecutorDaemon: demand + prefetch) │
            │               └─→ jobs (DdsGenerateJob)                   │
            │                       └─→ tasks (DownloadChunks, BuildAndCacheDds) │
            │                                                            │
            ├─→ fuse/fuse3 (async multi-threaded FUSE) ─────────────────┤
            │       ├─→ uses DdsClient (demand lane) for tile requests  │
            │       └─→ uses GeoIndex for patch region filtering        │
            │                                                            │
            ├─→ prefetch (AdaptivePrefetchCoordinator) ────────────────┘
            │       └─→ uses PrefetchLane + GeoIndex for background prefetch
            │
            ├─→ geo_index (geospatial reference database)
            ├─→ provider (Bing, Go2, Google - async variants)
            └─→ texture (DDS encoding)
```

### Coordinate System

Uses **Web Mercator** (Slippy Map) projection:
- Tile coordinates: (row, col, zoom)
- Each tile = 16×16 chunks of 256×256 pixels = 4096×4096 total
- Chunk zoom = tile zoom + 4 (e.g., tile zoom 12 → chunk zoom 16)
- Named constants: `CHUNKS_PER_TILE_SIDE` (16), `CHUNK_ZOOM_OFFSET` (4) in `coord/types.rs`
- Canonical conversion: `TileCoord::chunk_origin()` → (chunk_row, chunk_col, chunk_zoom)

### Threading Model

- Main thread: CLI and signal handling
- Tokio runtime: Multi-threaded async runtime for all I/O
  - Worker threads: tokio default (one per core)
  - Blocking pool: capped at `LaneSizing::blocking_threads_required()`
    (demand `cpu + disk_io` + prefetch `cpu + disk_io` + `BLOCKING_POOL_HEADROOM`), built by
    `runtime::build_service_runtime()` — **not** tokio's 512 default
  - FUSE operations via fuse3 (native async)
  - HTTP downloads via async reqwest
  - DDS encoding via spawn_blocking (CPU-bound)
- Cache cleanup daemon: Background memory/disk eviction

All FUSE operations run asynchronously via fuse3, enabling true parallel
processing of X-Plane's concurrent DDS requests. CPU utilization improved
from ~6% to ~45% during scene loading.

### Process Memory Management (#227)

Two process-level settings bound memory growth on long flights. Both are
applied in code, not by environment variable, and both have flight evidence:

- **`mallopt(M_MMAP_THRESHOLD, 1 MiB)`** — called by `configure_allocator()` as
  the first statement in `main()`, before anything allocates. glibc's threshold
  otherwise adapts *upward* past the 10.66 MiB tile size after a single
  alloc/free cycle, after which tiles come from arenas and are never returned
  on free. Pinning it cut committed memory 37% at matched work. Override with
  `MALLOC_MMAP_THRESHOLD_`.
- **Blocking pool cap** — tokio's 512 default let the pool reach 581 threads,
  and each excursion ratcheted glibc's arena high-water mark by ~420 MB. The
  arena reached 6,166 MB while holding 572 MB of live data.
- **`mallopt(M_ARENA_MAX, 4)`** — glibc's default of `8 x ncores` (256 here)
  lets every burst recruit fresh arenas, each keeping its own high-water mark
  for the life of the process, so the total is the sum of every peak ever
  seen and never converges. Capping the count removes the supply: the sum
  saturates once each arena has seen its worst burst. On the reference leg the
  arena settled at 3,916 MB within six minutes and held to the byte — peak
  concurrent demand (3,093 MB) plus ~27% headroom — against 6,091 MB still
  climbing at hour 10.75 uncapped. Override with `MALLOC_ARENA_MAX`.

Both pins defer to an explicit `MALLOC_*` variable or `GLIBC_TUNABLES` entry:
`mallopt` silently overrides what glibc read at startup, so without that check
a user's own setting would vanish with no diagnostic. **Both are `cfg`-gated to
`linux-gnu`** — macOS uses libmalloc and currently has neither these fixes nor
the allocator telemetry.

DDS payloads are `bytes::Bytes` end to end (#237): a tile is 10.66 MiB and a
FUSE read wants ~4% of it, so cloning a payload costs more than delivering it.
`Bytes` clones are refcount bumps, and the FUSE read path slices rather than
copies.

Diagnosing memory in the field: `docs/dev/memory-telemetry.md`.

### Concurrency Limiting

All job executor operations are protected by resource pools with semaphore-based
limiting to prevent resource exhaustion under heavy load. Each resource type has
tuned concurrency limits. The figures below are the **demand lane's**. The prefetch
lane has its own pools, sized by `LaneSizing` (`executor/lane_sizing.rs`) as
`ceil(demand x prefetch.capacity_share)` per pool (default 0.35, range 0.05 to 0.50,
the only user-facing sizing setting), `tile_generation = 2 x (prefetch cpu + prefetch network)`
and its own unscaled 1024 HTTP permits (connections cost X-Plane no CPU, and prefetch
downloads are bound by connections in flight). The old shared-pool prefetch quota
(`max_prefetch_fraction`, `prefetch_in_flight`) was removed in #296:

- **Network pool (download tasks)**: `clamp(ceil(cpu_capacity * 1.5), 8, 64)` — pipeline-balanced with CPU pool
- **HTTP semaphore (chunk connections)**: 1024 shared across the demand lane's download tasks
- **CPU-bound work** (assemble + encode): `max(num_cpus * 1.25, num_cpus + 2)` shared limiter
- **Disk I/O pool**: `DEFAULT_DISK_IO_CAPACITY` = 64, flat
- **Tokio blocking pool**: `LaneSizing::blocking_threads_required()` = demand
  `cpu + disk_io` + prefetch `cpu + disk_io` + `BLOCKING_POOL_HEADROOM`. The CPU
  and disk I/O pools of both lanes dispatch through `spawn_blocking`, so their
  sum is the worst-case demand for blocking threads (#227)

The `ResourcePool` struct (`executor/resource_pool.rs`) manages concurrency by
resource type. Each task declares its `ResourceType` and the executor acquires
permits before execution.

**Design rationale**: Tasks declare their resource requirements (Network, CPU, DiskIO)
and the executor manages permits automatically. This prevents deadlocks that occurred
when separate limiters were used for sequential stages.

## CLI Commands

```bash
# Core commands
xearthlayer                         # Defaults to 'run' (mount all packages)
xearthlayer setup                   # Interactive setup wizard (first-time configuration)
xearthlayer init                    # Create config file with defaults
xearthlayer run                     # Mount all packages and start streaming (primary command)
xearthlayer run --debug             # Enable debug-level logging for xearthlayer crate
xearthlayer run --profile           # Enable Chrome Trace profiling (requires --features profiling)
xearthlayer run --no-prefetch       # Disable prefetch system
xearthlayer run --airport ICAO      # Pre-warm tiles around airport before starting
xearthlayer download --lat --lon    # Download single tile
xearthlayer diagnostics             # Show system info, config, and health status
xearthlayer cache clear|stats       # Cache management
xearthlayer migrate [layout|status]  # Move between directory layouts (--dry-run supported)

# Scenery index cache
xearthlayer scenery-index status    # Show cache status and tile counts
xearthlayer scenery-index update    # Rebuild index from installed packages
xearthlayer scenery-index clear     # Delete the cache file

# Package management (<reg> may be the start of a code: NA, NA-CANADA; #293)
xearthlayer packages list           # List installed/available packages
xearthlayer packages install <reg>  # Install every package <reg> selects
xearthlayer packages update [reg]   # Update packages
xearthlayer packages remove <reg>   # Remove packages (whole set validated, one prompt)
xearthlayer packages info <reg>     # Show installed package details
xearthlayer packages check          # Check for updates

# Configuration management
xearthlayer config path             # Show config file path
xearthlayer config list             # List all settings
xearthlayer config get <key>        # Get a setting (e.g., packages.library_url)
xearthlayer config set <key> <val>  # Set a setting with validation
xearthlayer config upgrade          # Add missing settings with defaults (backup created)

# Publisher commands: the separate xearthlayer-publish binary (#284)
xearthlayer-publish init [<path>]           # Initialize package repository
xearthlayer-publish scan --source <path> [--type <ortho|overlay>]  # Scan Ortho4XP output
xearthlayer-publish add --source <path> --region <code> --type <ortho|overlay>  # Process tiles/overlays
xearthlayer-publish list                    # List packages
xearthlayer-publish build --region <code>   # Create archives
xearthlayer-publish urls --region <code> --base-url <url>  # Configure URLs
xearthlayer-publish version --region <code> --bump <type>  # Manage versions
xearthlayer-publish release --region <code> --metadata-url <url>  # Release
xearthlayer-publish status                  # Show release status
xearthlayer-publish validate                # Validate repository
xearthlayer-publish coverage [--dark] [--geojson] [-o <file>] [--metadata <path>]  # Generate coverage map
xearthlayer-publish dedupe --region <code> [--priority <mode>] [--tile <lat,lon>] [--dry-run]  # Remove overlapping ZL tiles
xearthlayer-publish gaps --region <code> [--tile <lat,lon>] [--format <fmt>] [-o <file>]  # Analyze coverage gaps
xearthlayer-publish delete --region <code> [--type <ortho|overlay>] [--dry-run] [--yes]  # Remove a package
```

## Key Files

| File | Purpose |
|------|---------|
| `xearthlayer-cli/src/main.rs` | CLI entry point |
| `xearthlayer-publish/src/` | Publisher CLI binary (Command Pattern) |
| `xearthlayer-cli/src/commands/config.rs` | Config CLI commands |
| `xearthlayer/src/service/facade.rs` | Main service API, wires up runtime |
| `xearthlayer/src/runtime/orchestrator.rs` | XEarthLayerRuntime - daemon orchestrator |
| `xearthlayer/src/runtime/request.rs` | JobRequest, DdsResponse, RequestOrigin types |
| `xearthlayer/src/runtime/lane.rs` | PrefetchLane trait, LocalPrefetchLane, LaneReceipt |
| `xearthlayer/src/executor/demand.rs` | DemandTracker and DemandGuard (demand jobs in flight, pause state) |
| `xearthlayer/src/executor/lane_sizing.rs` | LaneSizing - prefetch lane pools from `prefetch.capacity_share`, blocking-pool cap |
| `xearthlayer/src/tasks/cached_tile_probe.rs` | CachedTileProbeTask - prefetch-lane admission cache re-check |
| `xearthlayer/src/metrics/latency.rs` | LatencyWindow - per-sample FUSE latency median, p95, max |
| `xearthlayer/src/prefetch/state.rs` | PrefetchState and resolve_state - dashboard state by precedence |
| `xearthlayer/src/executor/daemon.rs` | ExecutorDaemon - background job processor |
| `xearthlayer/src/executor/client.rs` | DdsClient trait and ChannelDdsClient |
| `xearthlayer/src/executor/job.rs` | Job trait and JobId, JobResult types |
| `xearthlayer/src/executor/task.rs` | Task trait and TaskContext, TaskOutput |
| `xearthlayer/src/jobs/dds_generate.rs` | DdsGenerateJob - tile generation job |
| `xearthlayer/src/jobs/factory.rs` | DdsJobFactory trait for job creation |
| `xearthlayer/src/tasks/` | Task implementations (DownloadChunks, BuildAndCacheDds; legacy: assemble, encode, cache) |
| `xearthlayer/src/provider/factory.rs` | Provider factories (sync + async) |
| `xearthlayer/src/fuse/fuse3/` | Fuse3 async multi-threaded filesystem |
| `xearthlayer/src/fuse/fuse3/shared.rs` | Shared FUSE traits (FileAttrBuilder, DdsRequestor) |
| `xearthlayer/src/fuse/fuse3/ortho_union_fs.rs` | Consolidated ortho FUSE mount |
| `xearthlayer/src/paths/` | Path resolution: BaseDirectories, platform layouts, role accessors |
| `xearthlayer-cli/src/preflight/migrate.rs` | Layout migration check |
| `xearthlayer-cli/src/preflight/proposal.rs` | Migration decision table and guidance (pure) |
| `xearthlayer/src/preflight/` | Preflight framework: trait, Runner, BootstrapContext |
| `xearthlayer/src/xworld_pro/` | SimHeaven X-World Pro detection, continent coverage, order rule, setup findings |
| `xearthlayer-cli/src/preflight/xworld.rs` | The three `xworld-pro-*` checks and `ConfirmOrder` |
| `xearthlayer-cli/src/preflight/` | The concrete startup checks and the registry |
| `xearthlayer/src/geo_index/` | GeoIndex geospatial reference database |
| `xearthlayer/src/ortho_union/` | Ortho union index module |
| `xearthlayer/src/night/recipe.rs` | OXO `.oxn` recipe reader and validation |
| `xearthlayer/src/night/store.rs` | RecipeStore - finds and caches recipes per ortho source |
| `xearthlayer/src/night/service.rs` | LitTextureSource trait, BlackLitSource and NightLitSource |
| `xearthlayer/src/night/layout.rs` | Lit DDS layout, `minimal_black_dds()` |
| `xearthlayer/src/night/laminar/` | Laminar lit art reader (`art.rs`, `library.rs`, `ter.rs`) |
| `xearthlayer/src/night/prefs.rs` | StartupTextureSetting - texture setting read once from `X-Plane.prf` |
| `xearthlayer/src/night/footprint.rs` | Footprint geometry (`RecipeField::has_light` decides full-size vs minimal) |
| `xearthlayer/src/night/generator/cpu.rs` | CpuLitGenerator: per-texel lit generation from ground position |
| `xearthlayer/src/night/generator/tiling.rs` | Hex stochastic tiling of the Laminar pattern in Web Mercator metres |
| `xearthlayer/src/night/generator/led.rs` | LED lighting model: regions, patches, colour temperature, recolour |
| `xearthlayer/src/ortho_union/index.rs` | OrthoUnionIndex implementation |
| `xearthlayer/src/ortho_union/builder.rs` | OrthoUnionIndexBuilder (with progress + caching) |
| `xearthlayer/src/update/mod.rs` | Version update checking (VersionManifest, UpdateInfo) |
| `xearthlayer/src/update/checker.rs` | UpdateChecker trait and RemoteUpdateChecker |
| `xearthlayer/src/config/file.rs` | Configuration loading |
| `xearthlayer/src/config/keys.rs` | ConfigKey enum with validation (Specification Pattern) |
| `xearthlayer/src/manager/queue/` | InstallQueue, DiskLedger, Stages, lock and sweep |
| `xearthlayer-package/src/` | Scenery package format: metadata, library index, naming, spec version gate (shared by runtime and publisher) |
| `xearthlayer-publisher/src/` | Package publisher library |
| `xearthlayer-publisher/src/dedupe/` | Zoom level overlap detection and gap analysis |
| `xearthlayer/src/prefetch/strategy.rs` | Prefetcher trait (strategy pattern) |
| `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` | AdaptivePrefetchCoordinator (sliding box prefetch) |
| `xearthlayer/src/prefetch/adaptive/prefetch_box.rs` | PrefetchBox (heading-biased sliding prefetch region) |
| `xearthlayer/src/prefetch/adaptive/scenery_window.rs` | SceneryWindow (window config and dimensions) |
| `xearthlayer/src/prefetch/adaptive/boundary_strategy.rs` | BoundaryStrategy (region lifecycle management) |
| `xearthlayer/src/prefetch/adaptive/ground_strategy.rs` | GroundStrategy (ring-based for ground ops) |
| `xearthlayer/src/aircraft_position/web_api/adapter.rs` | WebApiAdapter - X-Plane Web API position source |
| `xearthlayer/src/aircraft_position/web_api/client.rs` | Web API HTTP/WebSocket client |
| `xearthlayer/src/aircraft_position/web_api/datarefs.rs` | X-Plane dataref definitions |
| `xearthlayer/src/aircraft_position/web_api/sim_state.rs` | SimState - sim state detection (paused, loading, etc.) |
| `xearthlayer/src/aircraft_position/web_api/config.rs` | Web API connection configuration |
| `xearthlayer/src/service/cache_layer.rs` | CacheLayer - cache lifecycle (memory + DDS disk + chunk disk, plus lit disk with night lighting on) |
| `xearthlayer/src/cache/providers/disk.rs` | DiskCacheProvider with internal GC daemon |
| `xearthlayer/src/cache/adapters/` | Bridge adapters for backward compatibility |
| `xearthlayer/src/cache/integrity/` | Canonical cache integrity model (`length_ceiling`, `write_atomic`, `discard`, `CacheEntryValidator`) |

## Configuration

Default config location: `~/.config/xearthlayer/config.ini` (Linux) or `~/Library/Application Support/XEarthLayer/config.ini` (macOS). Resolve it with `xearthlayer config path`, never by construction.

Key sections:
- `[general]` - General settings (update_check)
- `[provider]` - Imagery source (bing/google)
- `[cache]` - Memory/disk sizes, directory, DDS disk ratio
- `[generation]` - Thread count, timeout
- `[texture]` - DDS format (bc1/bc3), compressor backend (software/ispc/gpu), GPU device selection
- `[prefetch]` - Boundary-driven prefetch, mode, web_api_port (default 8086), transition ramp
- `[prewarm]` - Cold-start cache warming (grid_rows/grid_cols for DSF tile grid around airport)
- `[executor]` - Per-chunk download behaviour only: `request_timeout_secs` and `max_retries`, which reach the executor through `ServiceConfig::chunk_download_config()`. Concurrency is NOT configurable: pool capacities, task dispatch limits and channel capacities are derived from logical core count by `ResourcePoolConfig::default()`. `max_concurrent_jobs` and `retry_base_delay_ms` were removed in #249; the backoff base is a literal in `tasks/download_chunks.rs`
- `[fuse]` - FUSE kernel limits (max_background, congestion_threshold)
- `[night]` - Night lighting (enabled, default true)
- `[tui]` - Dashboard display (symbols: auto/emoji/plain; `auto` detection in `xearthlayer-cli/src/ui/symbols.rs`)
- `[packages]` - Package manager settings (concurrent_downloads: parallel part downloads 1-10)

See `docs/configuration.md` for full reference.

**IMPORTANT - Deprecating Config Settings**: When removing a configuration key from `ConfigKey` enum in `config/keys.rs`, you MUST add it to `DEPRECATED_KEYS` in `config/upgrade.rs`. This ensures `xearthlayer config upgrade` properly detects and removes the obsolete setting from user config files.

## Development

### Running Tests

```bash
make test           # Run all tests
make verify         # Format + lint + test
cargo test          # Direct cargo test
```

On macOS, `make verify-macos` additionally runs the live macFUSE smoke tests that CI
cannot execute (macFUSE is a kext; hosted runners cannot load it).

### Building

```bash
make build          # Debug build
make release        # Release build (runs verify first)
```

### Before Pushing

**IMPORTANT**: Always run `make pre-commit` before pushing changes:

```bash
make pre-commit     # Format + lint + test (REQUIRED before push)
```

This ensures:
- Code is formatted with `cargo fmt`
- Clippy linting passes with no warnings
- All tests pass

CI will fail if pre-commit checks were not run.

**Exception**: For documentation-only changes (`.md`, `.txt`, or other static files that don't affect compilation), `make pre-commit` is not required.

**Platform CI**: PRs run `Verify` (Linux, `ubuntu-latest`) and `Verify (macOS)`
(Apple Silicon, `macos-15`). Both are blocking. Do not convert the `verify` job to a
matrix — `main`'s branch protection requires the exact status context `Verify`, and
matrix jobs are renamed.

### Key Crates Used

- `fuse3` - Async FUSE filesystem (multi-threaded)
- `reqwest` - HTTP client with connection pooling
- `image` - JPEG decoding and image manipulation
- `clap` - CLI argument parsing
- `ini` - Configuration file parsing
- `tracing` - Structured logging
- `tracing-chrome` - Chrome Trace profiling (optional, `profiling` feature)
- `tokio` - Async runtime
- `rlimit` - File descriptor limit management (raises soft limit at startup)
- `wgpu` - GPU compute shaders for DDS encoding
- `block_compression` - BCn GPU compression via WGSL
- `indicatif` - Progress bar rendering for package downloads
- `sevenz-rust2` - Pure-Rust 7z writer for DSF compression (`compress` feature only)
- `tar` + `flate2` - In-process `tar.gz` handling (#274): the client extracts packages and the publisher builds them, no `tar` or `split` binary on either side. Declared once in `[workspace.dependencies]`. flate2 runs on the `zlib-rs` backend, which picks AVX2/SSE/NEON at run time: never use `target-cpu` flags, the packaged binary must run on every CPU

## Performance Notes

- **Parallel generation**: 8 threads default, configurable
- **Request coalescing**: Prevents duplicate work for same tile
- **Timeout handling**: Returns magenta placeholder after 10s
- **Cache hits**: <10ms response time
- **Cold downloads**: ~1-2s per tile with good network

## Platform Support

- **Linux**: ✅ Fully working (native FUSE)
- **macOS**: ✅ Working (requires [macFUSE](https://macfuse.io); Apple Silicon tested; fuse3 pinned to `samsoir/fuse3` `master` (upstream + `configurable-background` plus the macOS reply-resilience patch — upstream PR [Sherlock-Holo/fuse3#137](https://github.com/Sherlock-Holo/fuse3/pull/137) — comma-joined mount options, and reply-drop log demotion); install/approval guide in `docs/macos.md`)
- **Windows**: ⏳ Planned (requires Dokan/WinFSP)

## References

- Original inspiration: [AutoOrtho](https://github.com/kubilus1/autoortho)
- Configuration reference: `docs/configuration.md`
- Developer documentation: `docs/dev/`
- **Job Executor Framework**: `docs/dev/job-executor-design.md` (daemon architecture, job/task traits)
- **Fuse3 implementation**: `xearthlayer/src/fuse/fuse3/mod.rs` (async multi-threaded FUSE)
- **GPU Encoding**: `docs/dev/gpu-encoding-design.md` (wgpu compute shaders, channel worker, memory optimization)
- Package publisher design: `docs/dev/package-publisher-design.md`
- **World rebuild runbook**: `docs/dev/world-rebuild-runbook.md` (the campaign to rebuild every regional package: naming, staging channel, promotion, retirement)
- Zoom level overlap management: `docs/dev/zoom-level-overlap-design.md` (dedupe, gap analysis)
- **Consolidated FUSE mounting**: `docs/dev/consolidated-mounting-design.md` (single ortho mount, patches + packages)
- **Directory layout**: `docs/directory-layout.md` (where files live, upgrading from pre-0.5.0, cleaning up `~/.xearthlayer`)
- **Preflight checks**: `docs/dev/preflight-design.md` (startup prerequisite registry, bootstrap context, operating principles, how to add a check)
- **GeoIndex design**: `docs/dev/geo-index-design.md` (geospatial reference database, patch region ownership)
- **Memory telemetry**: `docs/dev/memory-telemetry.md` (periodic memory sampling, trace interpretation, confounders)
- **Cache integrity model**: `docs/dev/cache-integrity-design.md` (bounded reads, atomic durable writes, discard-on-reject, shared by every on-disk cache)
- **Night lighting**: `docs/dev/night-lighting-design.md` (phases 1, 2a and 2b implemented (packaging, black serving, real lit sizes, generated lit textures), plus the lit disk cache and LED lighting: lit textures served through FUSE, generated from OXO `.oxn` recipes and the user's Laminar lit art; a separate lit path beside the ortho executor)
- **CI/CD pipeline**: `docs/dev/cicd.md` (branch model, release job graph, platform tiers, version propagation)
- **Publisher separation**: `docs/dev/publisher-separation-design.md` (#284: `xearthlayer-package` is the format contract both sides depend on, `xearthlayer-publisher` holds the publishing domain, and `xearthlayer-publish` is its binary; the runtime links none of the publisher's dependencies)
- memorize review allow(dead_code) macros at major checkpoints. Refactor aggresively to remove them when appropriate.
- memorize ensure to update the projects documentation to reflect the current state of the project before committing changes