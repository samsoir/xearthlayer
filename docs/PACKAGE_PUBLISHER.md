# XEarthLayer Package Publisher Design

This document describes the design of the XEarthLayer Package Publisher component.

## Overview

The Package Publisher creates distributable XEarthLayer Scenery Packages from Ortho4XP output, manages versioning, and maintains the package library index. It enables anyone to create and host their own scenery libraries.

## Responsibilities

1. **Initialize** a new package repository
2. **Process** Ortho4XP output into regional packages
3. **Create** package archives with proper structure
4. **Split** large archives into downloadable parts
5. **Generate** checksums and metadata files
6. **Manage** library index (add, update, remove packages)
7. **Version** packages with semantic versioning

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Package Publisher                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │  Repository  │  │   Ortho4XP   │  │      Archive         │  │
│  │  Manager     │  │   Processor  │  │      Builder         │  │
│  │              │  │              │  │                      │  │
│  │ Init repo    │  │ Parse output │  │ Create tar.gz        │  │
│  │ Track state  │  │ Filter files │  │ Split into parts     │  │
│  │ Publish ver  │  │ Organize     │  │ Generate checksums   │  │
│  └──────────────┘  └──────────────┘  └──────────────────────┘  │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │   Metadata   │  │   Library    │  │      Version         │  │
│  │   Generator  │  │   Index      │  │      Manager         │  │
│  │              │  │              │  │                      │  │
│  │ Package meta │  │ Add/remove   │  │ Semver handling      │  │
│  │ Checksums    │  │ Update refs  │  │ Sequence numbers     │  │
│  │ URLs         │  │ Publish      │  │ Changelog            │  │
│  └──────────────┘  └──────────────┘  └──────────────────────┘  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Repository Structure

A Publisher repository is a local directory containing:

```
xearthlayer-packages/
├── .xearthlayer-repo                    # Repository marker file
├── xearthlayer_package_library.txt      # Library index
├── packages/                            # Package working directories
│   ├── zzXEL_na_ortho/
│   │   ├── xearthlayer_scenery_package.txt
│   │   ├── Earth nav data/
│   │   ├── terrain/
│   │   └── textures/
│   └── yzXEL_na_overlay/
│       └── ...
├── dist/                                # Published archives
│   ├── zzXEL_na-1.0.0.tar.gz.aa
│   ├── zzXEL_na-1.0.0.tar.gz.ab
│   └── ...
└── staging/                             # Work in progress
    └── ...
```

### Repository Marker

`.xearthlayer-repo` contains repository metadata:

```
XEARTHLAYER PACKAGE REPOSITORY
1.0.0
2025-12-20T10:00:00Z
```

## Workflow

### 1. Initialize Repository

Create a new package repository:

```bash
xearthlayer publish init /path/to/repo

# Or in current directory
xearthlayer publish init .
```

Creates:
- `.xearthlayer-repo` marker
- Empty `xearthlayer_package_library.txt`
- `packages/`, `dist/`, `staging/` directories

### 2. Process Ortho4XP Output

Import Ortho4XP tiles into a regional package:

```bash
xearthlayer publish add \
  --source /path/to/Ortho4XP/Tiles \
  --region na \
  --type ortho \
  --version 1.0.0
```

Processing steps:

1. **Scan source**: Find all tile directories
2. **Validate structure**: Ensure proper DSF/ter/texture layout
3. **Filter files**:
   - Keep: DSF files, .ter files, water mask PNGs
   - Remove: DDS textures (XEarthLayer generates these)
4. **Organize**: Place files in correct package structure
5. **Generate metadata**: Create `xearthlayer_scenery_package.txt` (without URLs yet)

### 3. Build Archives

Create distributable archives:

```bash
xearthlayer publish build --region na --type ortho
```

Steps:

1. Create tar.gz archive of package directory
2. Split into parts (configurable size, default 1GB)
3. Generate SHA-256 checksum for each part
4. Store in `dist/` directory

### 4. Configure URLs

Set download URLs for the package:

```bash
xearthlayer publish urls \
  --region na \
  --type ortho \
  --base-url https://dl.example.com/packages/na/ortho/
```

Updates metadata file with actual download URLs.

### 5. Publish

Finalize and update library index:

```bash
xearthlayer publish release
```

Steps:

1. Validate all packages have URLs configured
2. Update `xearthlayer_package_library.txt`:
   - Increment sequence number
   - Update timestamp
   - Add/update package entries
3. Generate checksums for metadata files
4. Mark repository as published

### 6. Upload (Manual)

The Publisher creates files but doesn't upload. User uploads:

```bash
# Example using rclone
rclone sync dist/ remote:bucket/packages/

# Or rsync
rsync -avz dist/ user@server:/var/www/packages/

# Or any other method
```

## Ortho4XP Processing Details

### Input Structure (Ortho4XP Output)

```
Tiles/
├── +37-118/
│   ├── Earth nav data/
│   │   └── +37-118.dsf
│   ├── terrain/
│   │   ├── 25264_10912_BI16.ter
│   │   └── ...
│   └── textures/
│       ├── 25264_10912_BI16.dds      # REMOVED
│       ├── 25264_10912_BI16_sea.png  # KEPT
│       └── ...
└── +37-119/
    └── ...
```

### Output Structure (XEarthLayer Package)

```
zzXEL_na_ortho/
├── xearthlayer_scenery_package.txt
├── Earth nav data/
│   └── +30-120/
│       ├── +37-118.dsf
│       └── +37-119.dsf
├── terrain/
│   ├── 25264_10912_BI16.ter
│   └── ...
└── textures/
    ├── 25264_10912_BI16_sea.png
    └── ...
```

### File Filtering Rules

| File Type | Action | Reason |
|-----------|--------|--------|
| `*.dsf` | Keep | Terrain mesh data |
| `*.ter` | Keep | Terrain definitions |
| `*_sea.png`, `*_mask.png` | Keep | Water masks |
| `*.dds` | Remove | Generated on-demand |
| `*.pol` | Keep if present | Polygon definitions |
| `*.net` | Keep if present | Network definitions |
| `*.obj` | Keep if present | Object definitions |

### Region Assignment

Tiles are assigned to regions based on latitude/longitude:

| Region | Latitude Range | Longitude Range |
|--------|---------------|-----------------|
| Africa | -35 to 37 | -18 to 52 |
| Antarctica | -90 to -60 | -180 to 180 |
| Asia | 0 to 80 | 52 to 180, -180 to -170 |
| Australia | -50 to 0 | 110 to 180 |
| Europe | 35 to 72 | -25 to 52 |
| North America | 15 to 85 | -170 to -50 |
| South America | -60 to 15 | -90 to -30 |

Note: Boundaries are approximate and may overlap. Publisher should warn if a tile doesn't clearly fit one region.

## Version Management

### Package Versioning

Each package has independent semantic version:

- **Major**: Breaking changes (rare for scenery)
- **Minor**: New tiles added to region
- **Patch**: Tile corrections, metadata fixes

```bash
# Bump version when adding tiles
xearthlayer publish version --region na --type ortho --bump minor

# Or set explicitly
xearthlayer publish version --region na --type ortho --set 2.0.0
```

### Library Sequence

The library index has a sequence number that increments with every publish:

```
Sequence: 1 → 2 → 3 → ...
```

Clients can quickly check `sequence > cached_sequence` to know if updates exist.

## CLI Interface

### Commands

```bash
# Initialize repository
xearthlayer publish init [<path>]

# Add Ortho4XP output to a package
xearthlayer publish add \
  --source <ortho4xp_tiles_path> \
  --region <region_code> \
  --type <ortho|overlay> \
  [--version <semver>]

# Build archives for a package
xearthlayer publish build \
  --region <region_code> \
  --type <ortho|overlay> \
  [--part-size <bytes>]

# Set download URLs
xearthlayer publish urls \
  --region <region_code> \
  --type <ortho|overlay> \
  --base-url <url>

# Bump or set version
xearthlayer publish version \
  --region <region_code> \
  --type <ortho|overlay> \
  <--bump major|minor|patch | --set <version>>

# Finalize and update library index
xearthlayer publish release

# List packages in repository
xearthlayer publish list

# Remove a package from repository
xearthlayer publish remove \
  --region <region_code> \
  --type <ortho|overlay>

# Validate repository integrity
xearthlayer publish validate
```

### Output Examples

```
$ xearthlayer publish init ~/scenery-repo
Initialized XEarthLayer package repository at /home/user/scenery-repo

$ xearthlayer publish add --source ~/Ortho4XP/Tiles --region na --type ortho --version 1.0.0
Scanning Ortho4XP output...
Found 45 tiles in /home/user/Ortho4XP/Tiles

Processing tiles:
  +37-118: OK (DSF: 1, TER: 256, masks: 128)
  +37-119: OK (DSF: 1, TER: 312, masks: 156)
  ...

Removed 12,456 DDS files (saving 45.2 GB in package)
Created package: zzXEL_na_ortho v1.0.0

$ xearthlayer publish build --region na --type ortho
Creating archive...
  Compressing: 2.3 GB → 1.8 GB (22% reduction)
  Splitting into 2 parts (1 GB each)

Generated:
  dist/zzXEL_na-1.0.0.tar.gz.aa (1.0 GB) SHA256: 55e772c1...
  dist/zzXEL_na-1.0.0.tar.gz.ab (0.8 GB) SHA256: b91f75f9...

$ xearthlayer publish urls --region na --type ortho --base-url https://dl.example.com/na/
Updated URLs in zzXEL_na_ortho metadata

$ xearthlayer publish release
Updating library index...
  Sequence: 0 → 1
  Packages: 1
  Published: 2025-12-20T15:30:00Z

Repository published successfully.
Upload dist/ contents to your hosting provider.
```

## Representational State

The Publisher maintains working state in the repository:

### Uncommitted Changes

After `add` or `version` but before `release`:
- Package directories are updated
- Metadata files reflect new state
- Library index is NOT updated yet

### Published State

After `release`:
- Library index updated with all changes
- Sequence number incremented
- Ready for upload

### Workflow Example

```
1. add --region na      → packages/zzXEL_na_ortho/ created
2. add --region eur     → packages/zzXEL_eur_ortho/ created
3. build --region na    → dist/zzXEL_na-*.tar.gz.* created
4. build --region eur   → dist/zzXEL_eur-*.tar.gz.* created
5. urls --region na     → metadata updated
6. urls --region eur    → metadata updated
7. release              → library index updated, seq++
8. (manual upload)
```

## Library Interface

```rust
pub trait PackagePublisher {
    /// Initialize a new repository
    fn init_repository(&self, path: &Path) -> Result<(), PublishError>;

    /// Add Ortho4XP output to a package
    fn add_package(
        &self,
        source: &Path,
        region: Region,
        package_type: PackageType,
        version: Version,
    ) -> Result<PackageSummary, PublishError>;

    /// Build archives for a package
    fn build_archives(
        &self,
        region: Region,
        package_type: PackageType,
        part_size: usize,
    ) -> Result<Vec<ArchivePart>, PublishError>;

    /// Set download URLs for a package
    fn set_urls(
        &self,
        region: Region,
        package_type: PackageType,
        base_url: &str,
    ) -> Result<(), PublishError>;

    /// Update version for a package
    fn set_version(
        &self,
        region: Region,
        package_type: PackageType,
        version: Version,
    ) -> Result<(), PublishError>;

    /// Publish all pending changes
    fn release(&self) -> Result<ReleaseInfo, PublishError>;

    /// List packages in repository
    fn list_packages(&self) -> Result<Vec<PackageInfo>, PublishError>;

    /// Remove a package
    fn remove_package(
        &self,
        region: Region,
        package_type: PackageType,
    ) -> Result<(), PublishError>;

    /// Validate repository integrity
    fn validate(&self) -> Result<ValidationReport, PublishError>;
}
```

## Error Handling

### Source Validation

- Warn if Ortho4XP structure not recognized
- Error if no valid tiles found
- Warn if tiles don't match expected region

### Archive Building

- Check disk space before building
- Handle compression failures gracefully
- Validate checksums after split

### URL Configuration

- Validate URL format
- Warn if URLs not HTTPS (but allow for local testing)

### Release Validation

- Ensure all packages have URLs
- Ensure all archives exist
- Validate all checksums

## Version Control Integration

While not required, Git integration is recommended:

```bash
cd ~/scenery-repo
git init
git add .
git commit -m "Initial repository"

# After changes
xearthlayer publish release
git add .
git commit -m "Release: NA ortho 1.0.0"
git push
```

Benefits:
- Track changes over time
- Collaborate with multiple contributors
- Rollback if needed
- History of what changed when

## Security Considerations

- Validate all input paths (no directory traversal)
- Generate checksums for integrity, not security
- HTTPS recommended for download URLs
- No secrets stored in repository (URLs are public)
