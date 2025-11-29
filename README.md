# XEarthLayer

A high-performance Rust implementation for streaming satellite imagery to X-Plane flight simulator via FUSE virtual filesystem.

## What It Does

XEarthLayer mounts a virtual filesystem that intercepts X-Plane's texture file requests and generates satellite imagery on-demand. Instead of downloading massive scenery packages (100+ GB), it dynamically fetches only the imagery you need as you fly.

**Key Features:**
- On-demand satellite imagery from Bing Maps or Google Maps (GO2 - same as Ortho4XP)
- FUSE passthrough filesystem - real scenery files pass through, DDS textures generated on-demand
- Parallel tile generation with request coalescing for fast scene loading
- Two-tier caching (configurable memory + disk, default 2GB + 20GB)
- BC1/BC3 DDS compression with 5-level mipmap chains
- Graceful shutdown with automatic FUSE unmount on Ctrl+C/SIGTERM
- Automatic X-Plane 12 installation detection
- INI-based configuration at `~/.xearthlayer/config.ini`

## Quick Start

```bash
# Build release binary
make release

# Start with a scenery pack (auto-detects X-Plane Custom Scenery folder)
./target/release/xearthlayer start --source /path/to/z_ortho_scenery

# Or specify mountpoint explicitly
./target/release/xearthlayer start \
  --source /path/to/z_ortho_scenery \
  --mountpoint "/path/to/X-Plane 12/Custom Scenery/z_ortho_scenery"
```

Press Ctrl+C to unmount when done.

## Using the Makefile

All project operations are available through `make`. Run `make help` for full list.

### Common Commands

| Command | Description |
|---------|-------------|
| `make init` | Initialize development environment |
| `make build` | Build debug version |
| `make release` | Build optimized release (runs verify first) |
| `make test` | Run all tests |
| `make verify` | Run format check, lint, and tests |
| `make clean` | Remove build artifacts |

### Development Workflow

```bash
# First time setup
make init

# Development cycle
make check          # Fast compilation check
make test           # Run tests
make verify         # Full verification (format + lint + test)

# Before committing
make pre-commit     # Format code and run all checks
```

### Code Quality

```bash
make format         # Format code
make lint           # Run clippy linter
make coverage       # Generate test coverage report
make doc-open       # Generate and open documentation
```

## Contributing

### Development Setup

1. Install Rust via [rustup](https://rustup.rs/)
2. Clone the repository
3. Run `make init` to set up the development environment
4. Run `make verify` to ensure everything works

### Code Guidelines

- Follow TDD (Test-Driven Development) - write tests first
- Follow SOLID principles - see `docs/DESIGN_PRINCIPLES.md`
- Run `make pre-commit` before committing
- Maintain test coverage above 80%

### Pull Request Process

1. Create a feature branch
2. Write tests for new functionality
3. Implement the feature
4. Run `make verify` to ensure all checks pass
5. Submit PR with clear description

## CLI Commands

```bash
# Initialize configuration file
xearthlayer init

# Start XEarthLayer with a scenery pack (passthrough filesystem)
xearthlayer start --source <scenery_dir> [--mountpoint <path>]

# Download a single tile (for testing)
xearthlayer download --lat <lat> --lon <lon> --zoom <zoom> --output <file.dds>
```

Run `xearthlayer --help` for all options.

## Documentation

| Document | Description |
|----------|-------------|
| [Configuration](docs/CONFIGURATION.md) | All configuration settings and INI file reference |
| [FUSE Filesystem](docs/FUSE_FILESYSTEM.md) | Virtual filesystem architecture and passthrough implementation |
| [Parallel Processing](docs/PARALLEL_PROCESSING.md) | Thread pool architecture and request coalescing |
| [Cache Design](docs/CACHE_DESIGN.md) | Two-tier caching strategy (memory + disk) |
| [Network Statistics](docs/NETWORK_STATS.md) | Download metrics, bandwidth tracking, and periodic logging |
| [DDS Implementation](docs/DDS_IMPLEMENTATION.md) | BC1/BC3 texture compression and mipmap generation |
| [Coordinate System](docs/COORDINATE_SYSTEM.md) | Web Mercator projection and tile coordinate system |
| [Design Principles](docs/DESIGN_PRINCIPLES.md) | SOLID principles and TDD guidelines |

### Scenery Package System (In Development)

| Document | Description |
|----------|-------------|
| [Scenery Overview](docs/SCENERY_OVERVIEW.md) | High-level system architecture and concepts |
| [Package Specification](docs/SCENERY_PACKAGES.md) | File formats and naming conventions |
| [Package Manager](docs/PACKAGE_MANAGER.md) | Downloading and installing packages |
| [Package Publisher](docs/PACKAGE_PUBLISHER.md) | Creating and publishing packages |
| [Implementation Plan](docs/SCENERY_PACKAGE_PLAN.md) | Development roadmap and progress |

## Known Issues

See [TODO.md](TODO.md) for current bugs and planned enhancements.

## Credits

This project is architecturally influenced by [AutoOrtho](https://github.com/kubilus1/autoortho) by [kubilus1](https://github.com/kubilus1). XEarthLayer is an independent Rust implementation focused on performance, memory safety, and cross-platform compatibility.

Developed with assistance from [Claude](https://claude.ai) by Anthropic, using [Claude Code](https://claude.ai/code).

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for details.
