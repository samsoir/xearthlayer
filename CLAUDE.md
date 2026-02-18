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
   - `Fuse3PassthroughFS` - Async multi-threaded passthrough for single scenery packs
   - `Fuse3UnionFS` - Union filesystem for patch folders (priority-based merge)
   - `Fuse3OrthoUnionFS` - Consolidated FUSE mount for all ortho sources (patches + packages)
   - All FUSE operations run asynchronously on the Tokio runtime
   - Shared traits: `FileAttrBuilder`, `DdsRequestor` (SOLID: Interface Segregation)
   - Pattern matching for DDS files: `{row}_{col}_ZL{zoom}.dds`

2. **Tile Generation** (`xearthlayer/src/tile/`)
   - `TileGenerator` trait with dependency injection
   - `DefaultTileGenerator` - Download + encode pipeline
   - `ParallelTileGenerator` - Thread pool with request coalescing

3. **Download Orchestrator** (`xearthlayer/src/orchestrator/`)
   - Parallel download of 256 chunks (16×16) per tile
   - Configurable concurrency, timeout, and retry logic
   - 80% minimum success rate requirement

4. **DDS Compression** (`xearthlayer/src/dds/`)
   - BC1/BC3 (DXT1/DXT5) compression
   - 5-level mipmap chain generation
   - ~0.2s encoding for 4096×4096 tiles

5. **Cache System** (`xearthlayer/src/cache/`, `xearthlayer/src/service/cache_layer.rs`)
   - `CacheLayer` - Service-owned cache lifecycle (encapsulates memory + disk)
   - `CacheService` - Self-contained cache with internal GC daemons
   - `MemoryCacheProvider`: moka-based LRU eviction (default 2GB)
   - `DiskCacheProvider`: Owns GC daemon internally (default 20GB)
   - Region-based disk layout: files in 1°×1° DSF subdirs (e.g., `+33-119/tile_15_12754_5279.cache`)
   - Parallel startup scanning via rayon over region directories
   - `migrate_cache()` for migrating flat-layout caches to region layout
   - Bridge adapters for executor integration (MemoryCacheBridge, DiskCacheBridge)
   - Per-provider cache directories
   - `XEarthLayerService::start()` creates `CacheLayer` with proper metrics ordering

6. **Configuration** (`xearthlayer/src/config/`)
   - INI file at `~/.xearthlayer/config.ini`
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
   - `DdsClient` trait - Abstraction for submitting tile requests (used by FUSE, Prefetch)
   - `ChannelDdsClient` - Implementation that sends requests via mpsc channel
   - Priority scheduling: `ON_DEMAND` > `PREFETCH` > `HOUSEKEEPING`
   - Error policies: `FailFast`, `ContinueOnError`, `PartialSuccess`
   - Resource pools for concurrency limiting by resource type (Network, CPU, DiskIO)
   - See `docs/dev/job-executor-design.md` for full architecture

9. **Runtime Orchestrator** (`xearthlayer/src/runtime/`)
    - `XEarthLayerRuntime` - Central daemon orchestrator, owns the job channel
    - `RuntimeConfig` - Configuration for the runtime and executor daemon
    - `JobRequest` - Request message with tile, priority, cancellation, and response channel
    - `DdsResponse` - Response with generated DDS data and metadata
    - `RequestOrigin` - Tracks request source (FUSE, Prefetch, Prewarm, Manual)
    - Peer daemon architecture: FUSE, Prefetch, and Executor are independent daemons
    - Channel as mediator pattern for maximum decoupling

10. **Package Publisher** (`xearthlayer/src/publisher/`)
   - Creates distributable scenery packages from Ortho4XP output (ortho tiles and overlays)
   - Repository management with package versioning
   - Archive building with configurable part sizes
   - Library index management for package discovery
   - `Ortho4XPProcessor` for ortho tiles, `OverlayProcessor` for overlays

11. **Predictive Tile Caching** (`xearthlayer/src/prefetch/`)
    - `AdaptivePrefetchCoordinator` - Self-calibrating prefetch with flight phase detection
    - `GroundStrategy` - Ring-based prefetching for ground operations (GS < 40kt)
    - `CruiseStrategy` - Track-based band prefetching for cruise (band ahead of aircraft)
    - `PhaseDetector` - Ground/Cruise flight phase state machine
    - `PerformanceCalibrator` - Measures throughput during initial load for mode selection
    - `TelemetryListener` - Receives X-Plane/ForeFlight UDP telemetry (position, heading, speed)
    - `FuseLoadMonitor` trait - Abstraction for FUSE request tracking (ISP)
    - `CircuitBreaker` - Pauses prefetch during X-Plane scene loading
    - Submits jobs to shared job executor daemon via `DdsClient` trait
    - Mode selection: Aggressive (>30 tiles/sec), Opportunistic (10-30), or Disabled
    - **Four-tier filtering**: Local tracking → Memory cache → Patched region exclusion (via `GeoIndex`) → Disk existence (via `OrthoUnionIndex`)
    - See `docs/dev/adaptive-prefetch-design.md` for design details

12. **Ortho Union Index** (`xearthlayer/src/ortho_union/`)
    - `OrthoUnionIndex` - Merged index of all ortho sources (patches + packages)
    - `OrthoUnionIndexBuilder` - Builder pattern for index construction
    - Parallel scanning via rayon for performance
    - Lazy resolution for `terrain/` and `textures/` directories (skips pre-scanning)
    - Index caching with mtime-based invalidation
    - `IndexBuildProgress` - Progress reporting with factory methods
    - Priority resolution: patches (`_patches/*`) sort before regional packages
    - `dds_tile_exists(row, col, zoom)` - Checks if DDS tile exists on disk (used by prefetch)
    - `resolve_lazy_filtered(path, predicate)` - Source-filtered lazy resolution (used by FUSE with GeoIndex)

13. **GeoIndex** (`xearthlayer/src/geo_index/`)
    - `GeoIndex` - Type-keyed, region-indexed geospatial reference database (thread-safe, ACID)
    - `DsfRegion` - 1°×1° DSF region coordinate type
    - `GeoLayer` trait - Marker trait for storable layer types (`Clone + Send + Sync + 'static`)
    - `PatchCoverage` - Layer type indicating a region is owned by a scenery patch
    - Locking: `RwLock<HashMap<TypeId, DashMap>>` — rare write locks, concurrent reads via DashMap shards
    - `populate()` provides atomic bulk loading (builds outside lock, swaps in)
    - Used by FUSE (`is_geo_filtered`), prewarm, and prefetch for patch region exclusion
    - See `docs/dev/geo-index-design.md` for design details

### Module Dependencies

```
xearthlayer-cli
    └─→ service/facade (XEarthLayerService::start())
            ├─→ service/cache_layer (CacheLayer) ───────────────────────┐
            │       ├─→ cache/service (CacheService)                    │
            │       └─→ cache/providers (Memory, Disk with internal GC) │
            │                                                            │
            ├─→ runtime (XEarthLayerRuntime orchestrator) ──────────────┤
            │       └─→ executor (ExecutorDaemon, DdsClient)            │
            │               └─→ jobs (DdsGenerateJob, TilePrefetchJob)  │
            │                       └─→ tasks (Download, Assemble, ...)  │
            │                                                            │
            ├─→ fuse/fuse3 (async multi-threaded FUSE) ─────────────────┤
            │       ├─→ uses DdsClient for tile requests                │
            │       └─→ uses GeoIndex for patch region filtering        │
            │                                                            │
            ├─→ prefetch (AdaptivePrefetchCoordinator) ────────────────┘
            │       └─→ uses DdsClient + GeoIndex for background prefetch
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
  - FUSE operations via fuse3 (native async)
  - HTTP downloads via async reqwest
  - DDS encoding via spawn_blocking (CPU-bound)
- Cache cleanup daemon: Background memory/disk eviction

All FUSE operations run asynchronously via fuse3, enabling true parallel
processing of X-Plane's concurrent DDS requests. CPU utilization improved
from ~6% (single-threaded fuser) to ~45% during scene loading.

### Concurrency Limiting

All job executor operations are protected by resource pools with semaphore-based
limiting to prevent resource exhaustion under heavy load. Each resource type has
tuned concurrency limits:

- **HTTP concurrency**: `min(num_cpus * 16, 256)` concurrent requests
- **CPU-bound work** (assemble + encode): `max(num_cpus * 1.25, num_cpus + 2)` shared limiter
- **Disk I/O concurrency**: Profile-based, auto-detected from storage type:
  - HDD: `min(num_cpus * 1, 4)` - seek-bound, low concurrency
  - SSD: `min(num_cpus * 4, 64)` - moderate concurrency (default)
  - NVMe: `min(num_cpus * 8, 256)` - aggressive concurrency

The `ResourcePool` struct (`executor/resource_pool.rs`) manages concurrency by
resource type. Each task declares its `ResourceType` and the executor acquires
permits before execution. Storage type detection can be overridden via
`cache.disk_io_profile` config.

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
xearthlayer download --lat --lon    # Download single tile
xearthlayer cache clear|stats|migrate # Cache management

# Scenery index cache
xearthlayer scenery-index status    # Show cache status and tile counts
xearthlayer scenery-index update    # Rebuild index from installed packages
xearthlayer scenery-index clear     # Delete the cache file

# Package management
xearthlayer packages list           # List installed/available packages
xearthlayer packages install <reg>  # Install a regional package
xearthlayer packages update [reg]   # Update packages
xearthlayer packages remove <reg>   # Remove a package
xearthlayer packages check          # Check for updates

# Configuration management
xearthlayer config path             # Show config file path
xearthlayer config list             # List all settings
xearthlayer config get <key>        # Get a setting (e.g., packages.library_url)
xearthlayer config set <key> <val>  # Set a setting with validation
xearthlayer config upgrade          # Add missing settings with defaults (backup created)

# Publisher commands (create scenery packages)
xearthlayer publish init [<path>]           # Initialize package repository
xearthlayer publish scan --source <path> [--type <ortho|overlay>]  # Scan Ortho4XP output
xearthlayer publish add --source <path> --region <code> --type <ortho|overlay>  # Process tiles/overlays
xearthlayer publish list                    # List packages
xearthlayer publish build --region <code>   # Create archives
xearthlayer publish urls --region <code> --base-url <url>  # Configure URLs
xearthlayer publish version --region <code> --bump <type>  # Manage versions
xearthlayer publish release --region <code> --metadata-url <url>  # Release
xearthlayer publish status                  # Show release status
xearthlayer publish validate                # Validate repository
xearthlayer publish coverage [--dark] [--geojson] [-o <file>]  # Generate coverage map
xearthlayer publish dedupe --region <code> [--priority <mode>] [--tile <lat,lon>] [--dry-run]  # Remove overlapping ZL tiles
xearthlayer publish gaps --region <code> [--tile <lat,lon>] [--format <fmt>] [-o <file>]  # Analyze coverage gaps
```

## Key Files

| File | Purpose |
|------|---------|
| `xearthlayer-cli/src/main.rs` | CLI entry point |
| `xearthlayer-cli/src/commands/publish/` | Publisher CLI (Command Pattern) |
| `xearthlayer-cli/src/commands/config.rs` | Config CLI commands |
| `xearthlayer/src/service/facade.rs` | Main service API, wires up runtime |
| `xearthlayer/src/service/fuse_mount.rs` | FuseMountService for FUSE mounting |
| `xearthlayer/src/runtime/orchestrator.rs` | XEarthLayerRuntime - daemon orchestrator |
| `xearthlayer/src/runtime/request.rs` | JobRequest, DdsResponse, RequestOrigin types |
| `xearthlayer/src/executor/daemon.rs` | ExecutorDaemon - background job processor |
| `xearthlayer/src/executor/client.rs` | DdsClient trait and ChannelDdsClient |
| `xearthlayer/src/executor/job.rs` | Job trait and JobId, JobResult types |
| `xearthlayer/src/executor/task.rs` | Task trait and TaskContext, TaskOutput |
| `xearthlayer/src/jobs/dds_generate.rs` | DdsGenerateJob - tile generation job |
| `xearthlayer/src/jobs/factory.rs` | DdsJobFactory trait for job creation |
| `xearthlayer/src/tasks/` | Task implementations (download, assemble, encode, cache) |
| `xearthlayer/src/provider/factory.rs` | Provider factories (sync + async) |
| `xearthlayer/src/fuse/fuse3/` | Fuse3 async multi-threaded filesystem |
| `xearthlayer/src/fuse/fuse3/shared.rs` | Shared FUSE traits (FileAttrBuilder, DdsRequestor) |
| `xearthlayer/src/fuse/fuse3/ortho_union_fs.rs` | Consolidated ortho FUSE mount |
| `xearthlayer/src/geo_index/` | GeoIndex geospatial reference database |
| `xearthlayer/src/ortho_union/` | Ortho union index module |
| `xearthlayer/src/ortho_union/index.rs` | OrthoUnionIndex implementation |
| `xearthlayer/src/ortho_union/builder.rs` | OrthoUnionIndexBuilder (with progress + caching) |
| `xearthlayer/src/config/file.rs` | Configuration loading |
| `xearthlayer/src/config/keys.rs` | ConfigKey enum with validation (Specification Pattern) |
| `xearthlayer/src/publisher/` | Package publisher library |
| `xearthlayer/src/publisher/dedupe/` | Zoom level overlap detection and gap analysis |
| `xearthlayer/src/prefetch/strategy.rs` | Prefetcher trait (strategy pattern) |
| `xearthlayer/src/prefetch/adaptive/coordinator.rs` | AdaptivePrefetchCoordinator (self-calibrating prefetch) |
| `xearthlayer/src/prefetch/adaptive/cruise_strategy.rs` | CruiseStrategy (track-based band prefetch) |
| `xearthlayer/src/prefetch/adaptive/ground_strategy.rs` | GroundStrategy (ring-based for ground ops) |
| `xearthlayer/src/prefetch/load_monitor.rs` | FuseLoadMonitor trait + SharedFuseLoadMonitor |
| `xearthlayer/src/prefetch/circuit_breaker.rs` | CircuitBreaker for X-Plane load detection |
| `xearthlayer/src/service/cache_layer.rs` | CacheLayer - service-owned cache lifecycle (memory + disk) |
| `xearthlayer/src/cache/providers/disk.rs` | DiskCacheProvider with internal GC daemon |
| `xearthlayer/src/cache/adapters/` | Bridge adapters for backward compatibility |

## Configuration

Default config location: `~/.xearthlayer/config.ini`

Key sections:
- `[provider]` - Imagery source (bing/google)
- `[cache]` - Memory/disk sizes, directory, disk I/O profile (auto/hdd/ssd/nvme)
- `[generation]` - Thread count, timeout
- `[texture]` - DDS format (bc1/bc3)

See `docs/configuration.md` for full reference.

**IMPORTANT - Deprecating Config Settings**: When removing a configuration key from `ConfigKey` enum in `config/keys.rs`, you MUST add it to `DEPRECATED_KEYS` in `config/upgrade.rs`. This ensures `xearthlayer config upgrade` properly detects and removes the obsolete setting from user config files.

## Development

### Running Tests

```bash
make test           # Run all tests
make verify         # Format + lint + test
cargo test          # Direct cargo test
```

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

### Key Crates Used

- `fuse3` - Async FUSE filesystem (multi-threaded)
- `reqwest` - HTTP client with connection pooling
- `image` - JPEG decoding and image manipulation
- `clap` - CLI argument parsing
- `ini` - Configuration file parsing
- `tracing` - Structured logging
- `tokio` - Async runtime

## Performance Notes

- **Parallel generation**: 8 threads default, configurable
- **Request coalescing**: Prevents duplicate work for same tile
- **Timeout handling**: Returns magenta placeholder after 10s
- **Cache hits**: <10ms response time
- **Cold downloads**: ~1-2s per tile with good network

## Platform Support

- **Linux**: ✅ Fully working (native FUSE)
- **Windows**: ⏳ Planned (requires Dokan/WinFSP)
- **macOS**: ⏳ Planned (requires macFUSE)

## References

- Original inspiration: [AutoOrtho](https://github.com/kubilus1/autoortho)
- Configuration reference: `docs/configuration.md`
- Developer documentation: `docs/dev/`
- **Job Executor Framework**: `docs/dev/job-executor-design.md` (daemon architecture, job/task traits)
- **Fuse3 implementation**: `xearthlayer/src/fuse/fuse3/mod.rs` (async multi-threaded FUSE)
- Parallel processing: `docs/dev/parallel-processing.md`
- Package publisher design: `docs/dev/package-publisher-design.md`
- Zoom level overlap management: `docs/dev/zoom-level-overlap-design.md` (dedupe, gap analysis)
- Scenery package plan: `docs/dev/scenery-package-plan.md`
- **Consolidated FUSE mounting**: Plan file describes single ortho mount architecture (patches + packages)
- **GeoIndex design**: `docs/dev/geo-index-design.md` (geospatial reference database, patch region ownership)
- memorize review allow(dead_code) macros at major checkpoints. Refactor aggresively to remove them when appropriate.
- memorize ensure to update the projects documentation to reflect the current state of the project before committing changes