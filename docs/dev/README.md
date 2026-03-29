# Developer Documentation

Technical documentation for XEarthLayer developers and contributors.

## Architecture

| Document | Description |
|----------|-------------|
| [Design Principles](design-principles.md) | SOLID principles, TDD approach, code guidelines |
| [Scenery Overview](scenery-overview.md) | High-level system architecture |
| [Consolidated Mounting](consolidated-mounting-design.md) | Single FUSE mount for all ortho sources (v0.2.11+) |

## Core Systems

| Document | Description |
|----------|-------------|
| [Job Executor Design](job-executor-design.md) | Job/task framework, daemon architecture, resource pools (v0.3.0+) |
| [FUSE Filesystem](fuse-filesystem.md) | Virtual filesystem, consolidated mounting, Direct I/O for DDS |
| [Coordinate System](coordinate-system.md) | Web Mercator projection, tile math, zoom levels |
| [DDS Implementation](dds-implementation.md) | BC1/BC3 texture compression, mipmap generation |
| [GPU Encoding](gpu-encoding-design.md) | wgpu compute shader encoding, channel worker, memory optimization |
| [GeoIndex Design](geo-index-design.md) | Geospatial reference database, patch region ownership |
| [Cache Design](cache-design.md) | Two-tier caching (memory + disk), LRU eviction |
| [Cache Service Design](cache-service-design.md) | CacheLayer lifecycle, GC daemons, bridge adapters |
| [Cache Load Transition Ramp](cache-load-transition-ramp-design.md) | TransitionThrottle design for takeoff phase management |
| [Index Building Optimization](index-building-optimization.md) | OrthoUnionIndex caching and parallel scanning |
| [Network Stats](network-stats.md) | Download metrics, bandwidth tracking |

## Package System

| Document | Description |
|----------|-------------|
| [Scenery Packages](scenery-packages.md) | File formats, naming conventions, metadata specs |
| [Package Manager Design](package-manager-design.md) | Download, install, update architecture (parallel downloads, retry) |
| [Package Publisher Design](package-publisher-design.md) | Build, archive, release pipeline |
| [GitHub Releases Publishing](github-releases-publishing.md) | Multi-part upload workflow |
| [Zoom Level Overlap](zoom-level-overlap-design.md) | Dedupe and gap analysis tools |

## Patches & Prefetch

| Document | Description |
|----------|-------------|
| [Scenery Patches](scenery-patches.md) | Bring-your-own-scenery patches with GeoIndex region ownership |
| [Airport Scenery Integration](airport-scenery-integration.md) | **Planned (0.3.x)**: Dynamic texture generation for patches |
| [Adaptive Prefetch Design](adaptive-prefetch-design.md) | Sliding prefetch box, boundary monitors, flight phase detection |
| [X-Plane Scenery Loading Whitepaper](xplane-scenery-loading-whitepaper.md) | Research on X-Plane 12's scenery loading behaviour |
| [Prefetch Flight Test Plan](prefetch-flight-test-plan.md) | Flight test data and empirical findings |
| [Aircraft Telemetry Architecture](aircraft-telemetry-architecture.md) | X-Plane Web API telemetry integration (v0.4.0) |

## Debugging & Operations

| Document | Description |
|----------|-------------|
| [Debug Map](debug-map.md) | Live browser map for prefetch observability (`--features debug-map`) |
| [Memory Profiling](memory-profiling.md) | Heaptrack profiling guide for memory optimization |
| [Application Release Runbook](app-release-runbook.md) | Release workflow, versioning, and troubleshooting |

## Root Cause Analysis

| Document | Description |
|----------|-------------|
| [Memory Cache Deadlock](rca-memory-cache-deadlock.md) | Investigation of async Mutex cache deadlock (2025-12-25) |

## Archive

Historical documents including superseded designs, completed implementation plans, and implemented specs are archived in [archive/](archive/).

## Module Dependencies

```
xearthlayer-cli
    └── xearthlayer (library)
            ├── provider (Bing, Go2, Google - async variants)
            ├── executor (job executor daemon, tasks)
            │       ├── daemon (channel-based job processing)
            │       ├── client (DdsClient trait, request submission)
            │       ├── resource_pool (concurrency limiting)
            │       └── adapters (cache, provider bridges)
            ├── runtime (orchestration, health monitoring)
            ├── jobs (DdsGenerateJob, TilePrefetchJob)
            ├── tasks (DownloadChunks, BuildAndCacheDds)
            ├── texture (DDS encoding)
            ├── cache (memory + disk)
            ├── fuse/fuse3 (async multi-threaded filesystem)
            │       ├── passthrough_fs (single source)
            │       ├── union_fs (multi-source patches)
            │       ├── ortho_union_fs (consolidated mount)
            │       └── coalesce (request coalescing)
            ├── geo_index (geospatial reference database)
            ├── ortho_union (tile → source mapping)
            ├── patches (patch discovery, validation)
            ├── prefetch (boundary-driven adaptive prefetch, flight phase detection)
            ├── package (metadata, library parsing)
            ├── manager (mounts, symlinks, install/update/remove)
            └── publisher (scan, build, release)
```

## Getting Started

```bash
# Clone and build
git clone https://github.com/samsoir/xearthlayer.git
cd xearthlayer
make init      # Installs toolchain components + git hooks
make verify

# Run tests
make test

# Generate docs
make doc-open
```

## Code Standards

- **TDD**: Write tests first
- **SOLID**: Use traits for abstraction, dependency injection
- **Coverage**: Maintain 80%+ test coverage
- **Formatting**: Run `cargo fmt` before committing
- **Linting**: Run `cargo clippy` with no warnings
- **Pre-commit hook**: Installed by `make init` (or `make setup-hooks`), runs `make pre-commit` automatically before each commit. Skips doc-only changes. Bypass with `git commit --no-verify`.
