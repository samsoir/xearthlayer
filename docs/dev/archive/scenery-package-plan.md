# XEarthLayer Scenery Package Implementation Plan

This document tracks the implementation progress for the XEarthLayer Scenery Package system.

## Overview

The implementation is divided into phases, with the Publisher built first to generate real test data for the Manager.

## Status Legend

- [ ] Not started
- [~] In progress
- [x] Complete
- [!] Blocked

---

## Phase 1: Core Data Structures & Parsing ✓

Foundation types and parsers used by both Publisher and Manager.

### Data Types

- [x] `PackageType` enum (Ortho, Overlay) - type code, sort prefix, folder suffix
- [x] `ArchivePart` struct (checksum, local filename, remote URL)
- [x] `PackageMetadata` struct (parsed from `xearthlayer_scenery_package.txt`)
- [x] `PackageLibrary` struct (parsed from `xearthlayer_package_library.txt`)
- [x] `LibraryEntry` struct (checksum, type, title, version, metadata URL)
- [x] Re-export `semver::Version` for package versioning

**Design Decision**: Region is a flexible `String` field (not enum) to allow publishers to define their own regional groupings.

### Parsers

- [x] `parse_package_metadata()` - Parse `xearthlayer_scenery_package.txt`
- [x] `serialize_package_metadata()` - Write `xearthlayer_scenery_package.txt`
- [x] `parse_package_library()` - Parse `xearthlayer_package_library.txt`
- [x] `serialize_package_library()` - Write `xearthlayer_package_library.txt`

### Tests

- [x] Unit tests for all parsers with sample data
- [x] Round-trip tests (parse → serialize → parse)
- [x] Error handling tests (malformed input)

### Files

- [x] `xearthlayer/src/package/mod.rs`
- [x] `xearthlayer/src/package/types.rs`
- [x] `xearthlayer/src/package/metadata.rs`
- [x] `xearthlayer/src/package/library.rs`
- [x] `xearthlayer/src/package/naming.rs` - Centralized naming (SRP)

### Dependencies Added

- `semver = "1.0"` - Semantic versioning
- `chrono = { version = "0.4", default-features = false, features = ["std"] }` - UTC timestamps

---

## Phase 2: Publisher - Core Functionality ✓

Create packages from Ortho4XP output.

### Repository Management

- [x] Repository initialization (`.xearthlayer-repo`, directories)
- [x] Repository detection and validation
- [x] Package directory structure (`packages/<region>/<type>/`)

### Ortho4XP Processing

- [x] Scan Ortho4XP tile directories (`zOrtho4XP_+NN-NNN/` format)
- [x] Validate tile structure (DSF in grid subdirs, ter, textures)
- [x] Filter files (keep DSF/ter/PNG masks, skip DDS)
- [x] Organize DSF into 10° × 10° grid structure
- [x] Coalesce terrain and texture files with deduplication
- [x] Region suggestion from lat/lon coordinates

**Design Decision**: `SceneryProcessor` trait abstraction allows future support for other scenery formats (e.g., X-Plane 13 when released).

### Metadata Generation

- [x] Generate `xearthlayer_scenery_package.txt`
- [x] SHA-256 checksum calculation (streaming, 8KB buffer)
- [x] Version management (bump major/minor/patch, set specific version)

### Files

- [x] `xearthlayer/src/publisher/mod.rs`
- [x] `xearthlayer/src/publisher/error.rs`
- [x] `xearthlayer/src/publisher/repository.rs`
- [x] `xearthlayer/src/publisher/processor.rs` (trait + module)
- [x] `xearthlayer/src/publisher/processor/tile.rs`
- [x] `xearthlayer/src/publisher/processor/ortho4xp.rs`
- [x] `xearthlayer/src/publisher/metadata.rs`
- [x] `xearthlayer/src/publisher/region.rs`

### Dependencies Added

- `sha2 = "0.10"` - SHA-256 checksums
- `chrono` clock feature enabled - UTC timestamps

---

## Phase 3: Publisher - Archive & Release (Ortho Only) ✓

Build distributable archives and manage library. Focus on ortho packages first; overlay support added in Phase 3b.

### Repository Configuration

- [x] Extend `.xearthlayer-repo` to store settings (part size, URLs)
- [x] Load/save repository configuration
- [x] Validate configuration on repository open

### Archive Building

- [x] Create tar.gz archives (shell out to `tar`)
- [x] Split into configurable part sizes (shell out to `split`)
- [x] Detect missing tools and provide helpful error messages
- [x] Generate SHA-256 checksums for each part
- [x] Store archives in `dist/<region>/<type>/` directory

### URL Configuration

- [x] Validate URL format (http/https required)
- [x] Optional URL accessibility verification (HEAD request)
- [x] Optional checksum verification (download and verify SHA-256)
- [x] Update package metadata with verified URLs
- [x] Support batch URL entry with base URL + auto-generated filenames

### Library Index Management

- [x] Create library index in repository root
- [x] Add package to library index
- [x] Update existing package entry (by title + type)
- [x] Remove package from library
- [x] Increment sequence number on save
- [x] Update timestamps on save
- [x] Sort entries by title then type

### Release Workflow

Multi-phase workflow to handle the "genesis paradox" (can't have URLs before files are hosted):

```
1. build   → Create archives in dist/, generate metadata (no URLs yet)
2. [user]  → Upload archives to hosting (GitHub releases, S3, etc.)
3. urls    → Provide URLs, optionally verify accessibility & checksums, update metadata
4. [user]  → Upload updated metadata files
5. release → Update library index, validate consistency
6. [user]  → Upload library index to CDN
```

### Context-Aware Validation

- [x] `ValidationContext` enum (Initial, AwaitingUrls, Release)
- [x] Lenient parsing (0 parts allowed, empty URLs allowed)
- [x] Context-specific validation (parts/URLs requirements vary by stage)
- [x] `ReleaseStatus` enum tracks package position in workflow

**Design Decision**: Separated parsing from validation to support the multi-phase workflow. Parser is lenient; validation is context-aware.

### Files

- [x] `xearthlayer/src/publisher/config.rs` - Repository configuration
- [x] `xearthlayer/src/publisher/archive.rs` - Archive creation & splitting
- [x] `xearthlayer/src/publisher/urls.rs` - URL verification & configuration
- [x] `xearthlayer/src/publisher/library.rs` - Library index management
- [x] `xearthlayer/src/publisher/release.rs` - Release orchestration

### Dependencies Added

- `rand = "0.9"` (dev-dependency) - Random data for compression tests

---

## Phase 3b: Publisher - Overlay Support ✓

Add overlay package support to the publisher.

### Overlay Processing

- [x] Create `OverlayProcessor` implementing `SceneryProcessor` trait
- [x] Scan `yOrtho4XP_Overlays/` directory structure (`Earth nav data/+NN+NNN/`)
- [x] Handle consolidated overlay format (only DSF files, not per-tile like ortho)
- [x] Package type `Y` with `yzXEL_` folder prefix

### CLI Integration

- [x] Add `scan_overlay()` method to `PublisherService` trait
- [x] Update `AddHandler` to dispatch to correct scanner based on package type
- [x] Update `process_tiles()` to use correct processor based on package type
- [x] Add test for overlay add command

### Files Added

- [x] `xearthlayer/src/publisher/processor/overlay.rs`

### Notes

Ortho4XP overlay structure differs from ortho tiles:
- Overlays are in a single `yOrtho4XP_Overlays/` directory
- Only contains `Earth nav data/+NN+NNN/+NN+NNN.dsf` files
- No terrain files (`.ter`) or textures (PNG/DDS)
- DSFs organized into 10° × 10° grid subdirectories

---

## Phase 4: Publisher CLI ✓

Command-line interface for Publisher.

### Commands

- [x] `xearthlayer publish init [<path>]` - Initialize repository
- [x] `xearthlayer publish scan --source <path>` - Scan and report tile info
- [x] `xearthlayer publish add --source <path> --region <code> --type <ortho|overlay> [--version <ver>]`
- [x] `xearthlayer publish list` - List packages in repository
- [x] `xearthlayer publish build [--region <code>] [--type <type>]` - Build archives
- [x] `xearthlayer publish urls --region <code> --type <type> --base-url <url>` - URL configuration
- [x] `xearthlayer publish version --region <code> --type <type> <--bump <major|minor|patch>|--set <version>>`
- [x] `xearthlayer publish release --region <code> --type <type> --metadata-url <url>` - Update library index
- [x] `xearthlayer publish status [--region <code>] [--type <type>]` - Show release status
- [x] `xearthlayer publish validate` - Validate repository state

**Design Decision**: Command Pattern with trait-based dependency injection enables testable handlers that depend only on interfaces.

### Architecture

```
xearthlayer-cli/src/commands/publish/
├── mod.rs        # Module exports and command dispatch
├── traits.rs     # Core interfaces (Output, PublisherService, CommandHandler)
├── services.rs   # Concrete implementations wrapping xearthlayer publisher
├── args.rs       # CLI argument types and parsing (clap-derived)
├── handlers.rs   # Command handlers implementing business logic
└── output.rs     # Shared output formatting utilities
```

### Files

- [x] `xearthlayer-cli/src/commands/publish/mod.rs`
- [x] `xearthlayer-cli/src/commands/publish/traits.rs`
- [x] `xearthlayer-cli/src/commands/publish/services.rs`
- [x] `xearthlayer-cli/src/commands/publish/args.rs`
- [x] `xearthlayer-cli/src/commands/publish/handlers.rs`
- [x] `xearthlayer-cli/src/commands/publish/output.rs`

---

## Phase 5: Publisher Testing & Verification ✓

Manual verification of Publisher CLI with real Ortho4XP data, plus integration tests.

### Stage 1: Manual Verification ✓

Tested complete workflow with real California Ortho4XP data:

- [x] Initialize test repository (`xearthlayer publish init`)
- [x] Scan tiles (62 tiles discovered)
- [x] Add package with region/type/version
- [x] Build archives (split into 3 parts)
- [x] Configure URLs (local HTTP server for testing)
- [x] Check status (workflow state machine)
- [x] Release to library index
- [x] Validate repository structure

### Stage 2: Integration Tests ✓

- [x] Create integration test suite (`xearthlayer-cli/tests/publish_workflow.rs`)
- [x] 13 integration tests covering full Publisher CLI workflow
- [x] MockTileBuilder for creating Ortho4XP-like tile structures
- [x] Tests excluded from regular `make test` via `#[ignore]` attribute
- [x] Run integration tests with `make integration-tests`

### Files Added

- [x] `xearthlayer-cli/tests/publish_workflow.rs` - Integration tests

### Bug Fixes

- [x] Fixed archive filename mismatch (`zzXEL_na-0.1.0.tar.gz` → `zzXEL_na_ortho-0.1.0.tar.gz`)
- [x] Created centralized naming module for SRP compliance

---

## Phase 6: Package Manager - Read Operations ✓

Discover and compare packages.

### Module Groundwork ✓

- [x] Create manager module structure
- [x] Define error types (`ManagerError`, `ManagerResult`)
- [x] Define configuration (`ManagerConfig` with builder pattern)
- [x] Define traits for dependency injection:
  - [x] `LibraryClient` - Fetch library indexes and metadata
  - [x] `PackageDownloader` - Download archive parts
  - [x] `ArchiveExtractor` - Extract archives
- [x] Local package store (`LocalPackageStore`, `InstalledPackage`)

### Files Added

- [x] `xearthlayer/src/manager/mod.rs`
- [x] `xearthlayer/src/manager/error.rs`
- [x] `xearthlayer/src/manager/config.rs`
- [x] `xearthlayer/src/manager/traits.rs`
- [x] `xearthlayer/src/manager/local.rs`
- [x] `xearthlayer/src/manager/client.rs` - HTTP library client
- [x] `xearthlayer/src/manager/updates.rs` - Update detection
- [x] `xearthlayer/src/manager/cache.rs` - Library caching with TTL

### Local Package Discovery ✓

- [x] Scan install location for packages (`LocalPackageStore`)
- [x] Parse package metadata files
- [x] Build installed packages state (`InstalledPackage`)
- [x] Track mount status for ortho packages (`MountStatus` enum)

### Remote Library Fetching ✓

- [x] Implement `LibraryClient` trait (`HttpLibraryClient`)
- [x] Fetch library index from URL
- [x] Parse library index
- [x] Cache with TTL (`CachedLibraryClient`)
- [x] Force refresh option (`fetch_fresh()`, `invalidate()`, `clear_cache()`)

### Update Detection ✓

- [x] Compare local vs remote versions (`UpdateChecker`)
- [x] Identify available updates (`list_updates()`)
- [x] `PackageStatus` enum (UpToDate, UpdateAvailable, NotInstalled, Orphaned)
- [x] `PackageInfo` struct with version info
- [x] Merge multiple library sources

---

## Phase 7: Package Manager - Download & Install ✓

Download and install packages.

### Download Manager

- [x] HTTP client with Range request support (`HttpDownloader`)
- [x] Parallel part downloads (`MultiPartDownloader`)
- [x] Resume interrupted downloads (Range header support)
- [x] Download state tracking (`DownloadState`)
- [x] Progress callback support (`MultiPartProgressCallback`)
- [x] SHA-256 verification per part (`calculate_file_checksum`)

### Archive Extraction

- [x] Reassemble split archives (`ShellExtractor::reassemble`)
- [x] Extract tar.gz to install location (`ShellExtractor::extract`)
- [x] List archive contents without extracting (`ShellExtractor::list_contents`)
- [x] Check for required tools (`check_required_tools`)

### Installation

- [x] `PackageInstaller` orchestrates full workflow
- [x] Install from metadata URL or pre-fetched metadata
- [x] Stage-based progress reporting (`InstallStage`)
- [x] Automatic cleanup of temporary files
- [x] Handle existing package removal during update

### Removal

- [x] Delete package directory via `LocalPackageStore::remove`
- [x] Handle partial removal gracefully

### Files Added

- [x] `xearthlayer/src/manager/download.rs` - HTTP download with Range support
- [x] `xearthlayer/src/manager/extractor.rs` - Archive reassembly and extraction
- [x] `xearthlayer/src/manager/installer.rs` - Full installation orchestration

### Notes

- Removal via `LocalPackageStore::remove()` was already implemented in Phase 6
- Symlink creation for overlays and mount registration for ortho packages
  will be handled in Phase 9 (Multi-Mount Support)
- Download uses blocking HTTP client (reqwest::blocking) for simplicity

---

## Phase 8: Package Manager CLI ✓

Command-line interface for Manager.

### Commands

- [x] `xearthlayer packages list [--verbose]`
- [x] `xearthlayer packages check --library-url <url>`
- [x] `xearthlayer packages install <region> --type <ortho|overlay> --library-url <url>`
- [x] `xearthlayer packages update [--region <region>] [--type <type>] [--all] --library-url <url>`
- [x] `xearthlayer packages remove <region> --type <ortho|overlay> [--force]`
- [x] `xearthlayer packages info <region> --type <ortho|overlay>`

### Architecture

```
xearthlayer-cli/src/commands/packages/
├── mod.rs        # Module exports and command dispatch
├── traits.rs     # Core interfaces (Output, PackageManagerService, UserInteraction)
├── services.rs   # Concrete implementations wrapping xearthlayer manager
├── args.rs       # CLI argument types and parsing (clap-derived)
└── handlers.rs   # Command handlers implementing business logic
```

### Files

- [x] `xearthlayer-cli/src/commands/packages/mod.rs`
- [x] `xearthlayer-cli/src/commands/packages/traits.rs`
- [x] `xearthlayer-cli/src/commands/packages/services.rs`
- [x] `xearthlayer-cli/src/commands/packages/args.rs`
- [x] `xearthlayer-cli/src/commands/packages/handlers.rs`

### Notes

Follows same Command Pattern architecture as Publisher CLI with trait-based dependency injection for testability.

---

## Phase 9: Multi-Mount Support ✓

Mount all installed ortho packages for X-Plane with a single command.

### Tasks

- [x] Enumerate installed ortho packages via `LocalPackageStore`
- [x] Create FUSE mount for each package via `MountManager`
- [x] Track mount state (`BackgroundSession` per mount in HashMap)
- [x] Handle individual mount failures gracefully (continue with others)
- [x] Clean shutdown of all mounts (unmount in reverse order)
- [x] Add `xearthlayer serve` command

### Implementation

The `serve` command discovers all installed ortho packages from the configured scenery directory and mounts each one using FUSE. Each mount overlays the package directory with on-demand DDS texture generation.

```
xearthlayer serve [--scenery-dir <path>] [--provider <bing|go2|google>] [--no-cache]
```

### Files

- [x] `xearthlayer/src/manager/mounts.rs` - MountManager, MountResult, ActiveMount, ServiceBuilder
- [x] Update `xearthlayer-cli/src/main.rs` - Add Serve command and run_serve function

### Design Notes

- Uses existing `start` command for single-package mounting (backward compatible)
- New `serve` command for multi-mount (production use case)
- Each ortho package gets its own service instance with shared config
- `MountManager` tracks `BackgroundSession` per region in HashMap
- Graceful shutdown via Ctrl+C unmounts all packages
- Failed mounts don't block successful ones (reports failures at end)

---

## Phase 10: Configuration & Polish ✓

Configuration options and UX improvements.

### Configuration

- [x] Add `[packages]` section to config.ini
- [x] `library_url` setting - URL to package library index
- [x] `temp_dir` setting - Temporary directory for downloads
- [x] Uses existing `[xplane].scenery_dir` as install location

### CLI Improvements

- [x] All packages commands use config defaults
- [x] `--library-url` optional if configured in config.ini
- [x] User-friendly error messages for missing config

### Implementation

The `[packages]` section in config.ini:
```ini
[packages]
; URL to the XEarthLayer package library index
library_url = https://example.com/library.txt
; Temporary directory for package downloads (default: system temp dir)
temp_dir = /tmp/xearthlayer
```

CLI commands now use these defaults:
- `xearthlayer packages check` - no `--library-url` required if configured
- `xearthlayer packages install` - no `--library-url` required if configured
- `xearthlayer packages update` - no `--library-url` required if configured

### Files

- [x] `xearthlayer/src/config/file.rs` - Add PackagesSettings struct
- [x] `xearthlayer/src/config/mod.rs` - Export PackagesSettings
- [x] `xearthlayer-cli/src/commands/packages/args.rs` - Make library_url optional
- [x] `xearthlayer-cli/src/commands/packages/mod.rs` - Use config defaults

### Deferred Items

Update notifications, disk space warnings, and additional documentation deferred to future work.

---

## Phase 11: Integration Testing

End-to-end testing with real packages.

### Tests

- [ ] Publisher: Create package from test tiles
- [ ] Publisher: Build and split archives
- [ ] Manager: Fetch library from local server
- [ ] Manager: Download and install package
- [ ] Manager: Mount and verify FUSE serves tiles
- [ ] Manager: Update package to new version
- [ ] Manager: Remove package cleanly

---

## Dependencies

```
Phase 1 (Core) ✓
    ↓
Phase 2 (Publisher Core) ✓
    ↓
Phase 3 (Publisher Archive - Ortho) ✓
    ↓
Phase 4 (Publisher CLI) ✓
    ↓
Phase 5 (Publisher Testing) ✓
    ↓
Phase 6 (Manager Read) ✓
    ↓
Phase 3b (Publisher - Overlay Support) ✓
    ↓
Phase 7 (Manager Install) ✓
    ↓
Phase 8 (Manager CLI) ✓
    ↓
Phase 9 (Multi-Mount) ✓
    ↓
Phase 10 (Config/Polish) ✓
    ↓
Phase 11 (Integration Tests) ←── Next
```

**Note:** Phases 1-10 completed. The system now supports full end-to-end workflow with configuration:
- Create packages (Publisher)
- Install/update packages (Manager with config defaults)
- Serve with multi-mount (Serve command)

---

## Notes

### California Test Data

Location: User's Ortho4XP output for California
- Will become first version of North America ortho package
- Provides real-world test data for Manager development

### Session Continuity

This plan spans multiple development sessions. Update status markers as work progresses. Each session should:

1. Review current status
2. Pick up next incomplete phase/task
3. Update markers when complete
4. Commit progress

### Design Decisions Log

Record significant decisions made during implementation:

| Date | Decision | Rationale |
|------|----------|-----------|
| 2025-11-28 | Publisher before Manager | Real test data for integration tests |
| 2025-11-28 | Symlinks for overlays | No FUSE overhead for static content |
| 2025-11-28 | Scan-based state (no index) | Self-healing, single source of truth |
| 2025-11-28 | Multi-mount (one per region) | Simpler than merged filesystem |
| 2025-11-28 | Region as String not enum | Allows publishers to define custom regional groupings |
| 2025-11-28 | Use semver crate | Less code to maintain, standard implementation |
| 2025-11-28 | SceneryProcessor trait | SOLID principles - abstracts scenery format, supports future versions of X-Plane |
| 2025-11-28 | 10° × 10° DSF grid | Matches X-Plane/Ortho4XP grid organization for Earth nav data |
| 2025-11-28 | Coalesce tiles into single package | Ortho4XP creates per-tile dirs; we consolidate into regional packages |
| 2025-11-28 | Skip DDS by default | XEarthLayer generates DDS on-demand via FUSE |
| 2025-11-28 | PNG mask detection (not `*_sea.png`) | Ortho4XP uses `*_ZL16.png` naming for water masks |
| 2025-11-28 | Store config in `.xearthlayer-repo` | Makes repositories portable and self-contained |
| 2025-11-28 | Global part size setting | Curator decides once; applies to all packages in repo |
| 2025-11-28 | Library index in repo root | Simple location, easy to find |
| 2025-11-28 | Multi-phase release workflow | Avoids genesis paradox - can't have URLs before files are hosted |
| 2025-11-28 | Interactive URL verification | Fetch and verify SHA-256 before accepting URLs |
| 2025-11-28 | Shell out to tar/split | Common Linux utils; provide guidance if missing |
| 2025-11-28 | Ortho before overlay | Complete ortho pipeline end-to-end before adding overlay complexity |
| 2025-11-28 | Separate library/package hosting | Library on CDN, packages on GitHub releases (cost optimization) |
| 2025-11-28 | Context-aware validation | Lenient parsing, strict validation per lifecycle stage (Initial/AwaitingUrls/Release) |
| 2025-11-28 | ReleaseStatus state machine | Track package position in publish workflow (NotBuilt→AwaitingUrls→Ready→Released) |
| 2025-11-28 | Archive parts may have empty URLs | Supports "genesis paradox" - metadata exists before files are uploaded |
| 2025-11-28 | Command Pattern for CLI | Separates concerns - handlers own logic, services abstract library, traits enable testing |
| 2025-11-28 | Trait-based dependency injection | Handlers depend on Output/PublisherService interfaces, not implementations |
| 2025-11-28 | CommandContext bundles dependencies | Single injection point for handler dependencies |
| 2025-11-29 | Centralized naming module | SRP compliance - `naming.rs` is single source for mountpoint/archive filenames |
| 2025-11-29 | `_ortho`/`_overlay` suffix in all names | Consistent naming: `zzXEL_na_ortho`, `zzXEL_na_ortho-1.0.0.tar.gz` |
| 2025-11-29 | Integration tests use `#[ignore]` | Run with `make integration-tests`, excluded from regular `make test` |
| 2025-11-29 | Manager traits for DI | `LibraryClient`, `PackageDownloader`, `ArchiveExtractor` enable testing |
| 2025-12-03 | CachedLibraryClient decorator | Wraps any LibraryClient with TTL-based caching; thread-safe |
| 2025-12-03 | MountStatus enum | Tracks ortho package mount state (Mounted/NotMounted/Unknown) via /proc/mounts |
| 2025-12-03 | PackageStatus enum | UpToDate, UpdateAvailable, NotInstalled, Orphaned for version comparison |
| 2025-12-03 | OverlayProcessor for overlays | Separate processor for overlays; scans `Earth nav data/` for DSF files only |
| 2025-12-03 | Overlay DSF-only structure | Overlays have no ter/texture files; only DSF in 10° grid subdirectories |
| 2025-12-03 | Blocking HTTP client for downloads | Using reqwest::blocking for simplicity; async could be added later if needed |
| 2025-12-03 | Range request support | HttpDownloader checks Accept-Ranges header and resumes partial downloads |
| 2025-12-03 | Shell-based extraction | Using `tar` command for extraction matches Publisher's use of `tar` for creation |
| 2025-12-03 | Stage-based progress reporting | InstallStage enum provides clear progress tracking through installation workflow |
| 2025-12-03 | MountManager with HashMap<region, session> | Track multiple BackgroundSession instances by region; auto-unmount on drop |
| 2025-12-03 | Separate serve command | Keep `start` for single-pack, add `serve` for multi-mount production use |
| 2025-12-03 | Graceful failure handling | Failed mounts don't block successful ones; report all at end |
| 2025-12-03 | Service factory pattern for mounts | Create service instance per package with shared config via closure |
| 2025-12-03 | PackagesSettings in ConfigFile | New `[packages]` section for library_url and temp_dir settings |
| 2025-12-03 | Optional library_url in CLI | Falls back to config; user-friendly error if neither specified |
| 2025-12-03 | Reuse scenery_dir for installs | No separate install_location - use existing `[xplane].scenery_dir` |

---

## Estimated Scope

- **Phase 1**: Core types and parsing - foundational
- **Phases 2-4**: Publisher - moderate complexity
- **Phase 5**: Test package - straightforward
- **Phases 6-8**: Manager - moderate complexity
- **Phase 9**: Multi-mount - integration with existing FUSE code
- **Phases 10-11**: Polish and testing

Total: Significant feature, multi-session implementation.
