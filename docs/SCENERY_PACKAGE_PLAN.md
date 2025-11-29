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

### Dependencies Added

- `semver = "1.0"` - Semantic versioning
- `chrono = { version = "0.4", default-features = false, features = ["std"] }` - UTC timestamps

---

## Phase 2: Publisher - Core Functionality

Create packages from Ortho4XP output.

### Repository Management

- [ ] Repository initialization (`.xearthlayer-repo`, directories)
- [ ] Repository detection and validation
- [ ] Repository state tracking

### Ortho4XP Processing

- [ ] Scan Ortho4XP tile directories
- [ ] Validate tile structure (DSF, ter, textures)
- [ ] Filter files (keep DSF/ter/masks, remove DDS)
- [ ] Organize into package structure
- [ ] Region assignment from lat/lon

### Metadata Generation

- [ ] Generate `xearthlayer_scenery_package.txt`
- [ ] SHA-256 checksum calculation
- [ ] Version management (bump, set)

### Files

- [ ] `xearthlayer/src/publisher/mod.rs`
- [ ] `xearthlayer/src/publisher/repository.rs`
- [ ] `xearthlayer/src/publisher/processor.rs`
- [ ] `xearthlayer/src/publisher/metadata.rs`

---

## Phase 3: Publisher - Archive & Release

Build distributable archives and manage library.

### Archive Building

- [ ] Create tar.gz archives
- [ ] Split into configurable part sizes
- [ ] Generate per-part checksums
- [ ] Store in `dist/` directory

### URL Configuration

- [ ] Set base URL for package
- [ ] Update metadata with full URLs
- [ ] Validate URL format

### Library Index Management

- [ ] Add package to library index
- [ ] Update existing package entry
- [ ] Remove package from library
- [ ] Increment sequence number
- [ ] Update timestamps

### Release Process

- [ ] Validate all packages ready
- [ ] Update library index
- [ ] Generate final checksums

### Files

- [ ] `xearthlayer/src/publisher/archive.rs`
- [ ] `xearthlayer/src/publisher/library.rs`
- [ ] `xearthlayer/src/publisher/release.rs`

---

## Phase 4: Publisher CLI

Command-line interface for Publisher.

### Commands

- [ ] `xearthlayer publish init [<path>]`
- [ ] `xearthlayer publish add --source --region --type [--version]`
- [ ] `xearthlayer publish build --region --type [--part-size]`
- [ ] `xearthlayer publish urls --region --type --base-url`
- [ ] `xearthlayer publish version --region --type <--bump|--set>`
- [ ] `xearthlayer publish release`
- [ ] `xearthlayer publish list`
- [ ] `xearthlayer publish remove --region --type`
- [ ] `xearthlayer publish validate`

### Files

- [ ] `xearthlayer-cli/src/commands/publish.rs`

---

## Phase 5: Create Test Package

Use Publisher to create real North America package from California Ortho4XP data.

### Tasks

- [ ] Initialize test repository
- [ ] Process California tiles into NA ortho package
- [ ] Build archives
- [ ] Configure test URLs (local filesystem or test server)
- [ ] Generate library index
- [ ] Verify package structure

### Deliverable

- [ ] Working NA ortho package for Manager testing

---

## Phase 6: Package Manager - Read Operations

Discover and compare packages.

### Local Package Discovery

- [ ] Scan install location for packages
- [ ] Parse package metadata files
- [ ] Build installed packages state
- [ ] Track mount status for ortho packages

### Remote Library Fetching

- [ ] Fetch library index from `library_root`
- [ ] Parse library index
- [ ] Cache with TTL
- [ ] Force refresh option

### Update Detection

- [ ] Compare local vs remote versions
- [ ] Identify available updates
- [ ] Sequence number comparison for quick check

### Files

- [ ] `xearthlayer/src/manager/mod.rs`
- [ ] `xearthlayer/src/manager/scanner.rs`
- [ ] `xearthlayer/src/manager/library_client.rs`
- [ ] `xearthlayer/src/manager/updates.rs`

---

## Phase 7: Package Manager - Download & Install

Download and install packages.

### Download Manager

- [ ] HTTP client with Range request support
- [ ] Parallel part downloads
- [ ] Resume interrupted downloads
- [ ] Download state persistence
- [ ] Progress callback support
- [ ] SHA-256 verification per part

### Installation

- [ ] Reassemble split archives
- [ ] Extract tar.gz to install location
- [ ] Verify extracted contents
- [ ] Create symlinks for overlay packages
- [ ] Register ortho packages for mounting

### Removal

- [ ] Unmount if necessary (ortho)
- [ ] Remove symlink (overlay)
- [ ] Delete package directory
- [ ] Handle partial removal gracefully

### Files

- [ ] `xearthlayer/src/manager/download.rs`
- [ ] `xearthlayer/src/manager/installer.rs`
- [ ] `xearthlayer/src/manager/remover.rs`

---

## Phase 8: Package Manager CLI

Command-line interface for Manager.

### Commands

- [ ] `xearthlayer packages list`
- [ ] `xearthlayer packages check`
- [ ] `xearthlayer packages install <region> [--type]`
- [ ] `xearthlayer packages update [<region>] [--all]`
- [ ] `xearthlayer packages remove <region> [--type]`
- [ ] `xearthlayer packages info <region>`

### Files

- [ ] `xearthlayer-cli/src/commands/packages.rs`

---

## Phase 9: Multi-Mount Support

Modify start command to mount all installed ortho packages.

### Tasks

- [ ] Enumerate installed ortho packages
- [ ] Create FUSE mount for each package
- [ ] Track mount state (BackgroundSession per mount)
- [ ] Handle individual mount failures gracefully
- [ ] Clean shutdown of all mounts
- [ ] Update `start` command

### Files

- [ ] `xearthlayer/src/manager/mounts.rs`
- [ ] Update `xearthlayer-cli/src/commands/start.rs`

---

## Phase 10: Configuration & Polish

Configuration options and UX improvements.

### Configuration

- [ ] Add `[packages]` section to config.ini
- [ ] `library_root` setting
- [ ] `install_location` setting
- [ ] `download_threads` setting

### Update Notifications

- [ ] Check for updates on first run
- [ ] Display notification (don't repeat)
- [ ] Respect quiet mode

### Error Messages

- [ ] User-friendly error messages
- [ ] Disk space warnings
- [ ] Network error guidance

### Documentation

- [ ] Update README with package commands
- [ ] Add examples to CLI help text

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
Phase 1 (Core)
    ↓
Phase 2 (Publisher Core) ────→ Phase 3 (Publisher Archive)
                                        ↓
                               Phase 4 (Publisher CLI)
                                        ↓
                               Phase 5 (Test Package)
                                        ↓
Phase 6 (Manager Read) ←───────────────┘
    ↓
Phase 7 (Manager Install)
    ↓
Phase 8 (Manager CLI)
    ↓
Phase 9 (Multi-Mount)
    ↓
Phase 10 (Config/Polish)
    ↓
Phase 11 (Integration Tests)
```

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

---

## Estimated Scope

- **Phase 1**: Core types and parsing - foundational
- **Phases 2-4**: Publisher - moderate complexity
- **Phase 5**: Test package - straightforward
- **Phases 6-8**: Manager - moderate complexity
- **Phase 9**: Multi-mount - integration with existing FUSE code
- **Phases 10-11**: Polish and testing

Total: Significant feature, multi-session implementation.
