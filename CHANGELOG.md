# Changelog

All notable changes to XEarthLayer will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/samsoir/xearthlayer/compare/v0.2.12...HEAD
[0.2.12]: https://github.com/samsoir/xearthlayer/compare/v0.2.10...v0.2.12
[0.2.10]: https://github.com/samsoir/xearthlayer/compare/v0.2.9...v0.2.10
[0.2.9]: https://github.com/samsoir/xearthlayer/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/samsoir/xearthlayer/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/samsoir/xearthlayer/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/samsoir/xearthlayer/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/samsoir/xearthlayer/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/samsoir/xearthlayer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/samsoir/xearthlayer/releases/tag/v0.1.0
