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
| [Job Executor Design](job-executor-design.md) | **Primary design doc**: job/task framework, daemon architecture, resource pools (v0.3.0+) |
| [FUSE Filesystem](fuse-filesystem.md) | Virtual filesystem, consolidated mounting, passthrough implementation |
| [Coordinate System](coordinate-system.md) | Web Mercator projection, tile math, zoom levels |
| [DDS Implementation](dds-implementation.md) | BC1/BC3 texture compression, mipmap generation |
| [GeoIndex Design](geo-index-design.md) | Geospatial reference database, patch region ownership |
| [Cache Design](cache-design.md) | Two-tier caching (memory + disk), LRU eviction |
| [Parallel Processing](parallel-processing.md) | Thread pools, legacy coalescing |
| [Network Stats](network-stats.md) | Download metrics, bandwidth tracking |
| ~~[Async Pipeline Architecture](async-pipeline-architecture.md)~~ | **(DEPRECATED)** Superseded by Job Executor Framework in v0.3.0 |

## Package System

| Document | Description |
|----------|-------------|
| [Scenery Packages](scenery-packages.md) | File formats, naming conventions, metadata specs |
| [Package Manager Design](package-manager-design.md) | Download, install, update architecture |
| [Package Publisher Design](package-publisher-design.md) | Build, archive, release pipeline |
| [GitHub Releases Publishing](github-releases-publishing.md) | Multi-part upload workflow |
| [Implementation Plan](scenery-package-plan.md) | Development roadmap and phase tracking |
| [Zoom Level Overlap](zoom-level-overlap-design.md) | Dedupe and gap analysis tools |

## Patches & Prefetch

| Document | Description |
|----------|-------------|
| [Scenery Patches](scenery-patches.md) | Bring-your-own-scenery patches with GeoIndex region ownership (current) |
| [Airport Scenery Integration](airport-scenery-integration.md) | **Planned (0.3.x)**: Dynamic texture generation for patches (previously implemented, temporarily removed) |
| [Adaptive Prefetch Design](adaptive-prefetch-design.md) | **Primary design doc**: Self-calibrating prefetch with flight phase detection (v0.3.0+) |
| [X-Plane Scenery Loading Whitepaper](xplane-scenery-loading-whitepaper.md) | Research on X-Plane 12's scenery loading behavior |
| [Prefetch Flight Test Plan](prefetch-flight-test-plan.md) | Flight test data informing prefetch design |
| [Aircraft Telemetry Architecture](aircraft-telemetry-architecture.md) | ForeFlight UDP telemetry integration |
| ~~[Predictive Caching](predictive-caching.md)~~ | **(SUPERSEDED)** Original radial prefetcher design |
| ~~[Heading-Aware Prefetch](heading-aware-prefetch-design.md)~~ | **(SUPERSEDED)** Cone-based prefetch algorithm |
| ~~[Tile-Based Prefetch Design](tile-based-prefetch-design.md)~~ | **(SUPERSEDED)** DSF tile-based approach |

## Root Cause Analysis

| Document | Description |
|----------|-------------|
| [Memory Cache Deadlock](rca-memory-cache-deadlock.md) | Investigation of cache deadlock issue |

## Deprecated Documents

Historical documents that describe superseded designs or completed implementations are archived in [deprecated/](deprecated/).

| Document | Description |
|----------|-------------|
| [Concurrency Hardening](deprecated/concurrency-hardening.md) | Historical: semaphore-based limiting proposal |
| [Multithreaded FUSE Proposal](deprecated/multithreaded-fuse-proposal.md) | Historical: led to fuse3 implementation |
| [Setup Wizard Plan](deprecated/plan-setup-wizard.md) | Historical: now implemented |
| [Refactoring Strategy](deprecated/refactoring-strategy.md) | Historical: async migration strategy |

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
            ├── tasks (download, assemble, encode, cache)
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
            ├── prefetch (adaptive prefetch, flight phase detection)
            ├── package (metadata, library parsing)
            ├── manager (mounts, symlinks, install/update/remove)
            └── publisher (scan, build, release)
```

## Getting Started

```bash
# Clone and build
git clone https://github.com/samsoir/xearthlayer.git
cd xearthlayer
make init
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
- **Pre-commit**: Run `make pre-commit` before pushing
