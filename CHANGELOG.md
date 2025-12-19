# Changelog

All notable changes to XEarthLayer will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/samsoir/xearthlayer/compare/v0.2.6...HEAD
[0.2.6]: https://github.com/samsoir/xearthlayer/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/samsoir/xearthlayer/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/samsoir/xearthlayer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/samsoir/xearthlayer/releases/tag/v0.1.0
