# Changelog

All notable changes to XEarthLayer will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/samsoir/xearthlayer/compare/v0.2.9...HEAD
[0.2.9]: https://github.com/samsoir/xearthlayer/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/samsoir/xearthlayer/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/samsoir/xearthlayer/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/samsoir/xearthlayer/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/samsoir/xearthlayer/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/samsoir/xearthlayer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/samsoir/xearthlayer/releases/tag/v0.1.0
