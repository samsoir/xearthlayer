# Developer Documentation

Technical documentation for XEarthLayer developers and contributors.

## Architecture

| Document | Description |
|----------|-------------|
| [Design Principles](design-principles.md) | SOLID principles, TDD approach, code guidelines |
| [Scenery Overview](scenery-overview.md) | High-level system architecture |
| [Module Status](module-status.md) | Implementation status of all modules |

## Core Systems

| Document | Description |
|----------|-------------|
| [Async Pipeline Architecture](async-pipeline-architecture.md) | **Primary design doc**: multi-stage async processing, request coalescing, thread pool exhaustion fix |
| [Coordinate System](coordinate-system.md) | Web Mercator projection, tile math, zoom levels |
| [DDS Implementation](dds-implementation.md) | BC1/BC3 texture compression, mipmap generation |
| [FUSE Filesystem](fuse-filesystem.md) | Virtual filesystem, passthrough implementation |
| [Cache Design](cache-design.md) | Two-tier caching (memory + disk), LRU eviction |
| [Parallel Processing](parallel-processing.md) | Thread pools, legacy coalescing (see async pipeline for current) |
| [Network Stats](network-stats.md) | Download metrics, bandwidth tracking |

## Package System

| Document | Description |
|----------|-------------|
| [Scenery Packages](scenery-packages.md) | File formats, naming conventions, metadata specs |
| [Package Manager Design](package-manager-design.md) | Download, install, update architecture |
| [Package Publisher Design](package-publisher-design.md) | Build, archive, release pipeline |
| [Implementation Plan](scenery-package-plan.md) | Development roadmap and phase tracking |

## Planning & History

| Document | Description |
|----------|-------------|
| [Refactoring Strategy](refactoring-strategy.md) | Code improvement approaches |
| [Phase 5 Test Plan](phase5-test-plan.md) | Archive building test cases |

## Module Dependencies

```
xearthlayer-cli
    └── xearthlayer (library)
            ├── provider (Bing, Go2, Google - sync + async variants)
            ├── pipeline (async tile processing)
            │       ├── coalesce (request coalescing)
            │       ├── stages (download, assembly, encode, cache)
            │       └── adapters (bridges to providers, cache)
            ├── texture (DDS encoding)
            ├── cache (memory + disk)
            ├── fuse/async_passthrough (virtual filesystem)
            ├── package (metadata, library parsing)
            ├── manager (install, update, remove)
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
