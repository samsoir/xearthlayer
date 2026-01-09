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
| [Async Pipeline Architecture](async-pipeline-architecture.md) | **Primary design doc**: multi-stage async processing, request coalescing, thread pool exhaustion fix |
| [FUSE Filesystem](fuse-filesystem.md) | Virtual filesystem, consolidated mounting, passthrough implementation |
| [Coordinate System](coordinate-system.md) | Web Mercator projection, tile math, zoom levels |
| [DDS Implementation](dds-implementation.md) | BC1/BC3 texture compression, mipmap generation |
| [Cache Design](cache-design.md) | Two-tier caching (memory + disk), LRU eviction |
| [Parallel Processing](parallel-processing.md) | Thread pools, legacy coalescing (see async pipeline for current) |
| [Network Stats](network-stats.md) | Download metrics, bandwidth tracking |

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
| [Tile Patches](tile-patches.md) | Custom mesh/elevation from airport addons |
| [Predictive Caching](predictive-caching.md) | Radial prefetcher design, telemetry integration |
| [Heading-Aware Prefetch](heading-aware-prefetch-design.md) | Flight-path prediction algorithm (reference) |

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
            ├── pipeline (async tile processing)
            │       ├── coalesce (request coalescing)
            │       ├── stages (download, assembly, encode, cache)
            │       └── adapters (bridges to providers, cache)
            ├── texture (DDS encoding)
            ├── cache (memory + disk)
            ├── fuse/fuse3 (async multi-threaded filesystem)
            │       ├── passthrough_fs (single source)
            │       ├── union_fs (multi-source patches)
            │       └── ortho_union_fs (consolidated mount)
            ├── ortho_union (tile → source mapping)
            ├── patches (patch discovery, validation)
            ├── prefetch (radial prefetcher, telemetry)
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
