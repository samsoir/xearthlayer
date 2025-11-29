# XEarthLayer Package Manager Design

This document describes the design of the XEarthLayer Package Manager component.

## Overview

The Package Manager is responsible for discovering, downloading, installing, updating, and removing XEarthLayer Scenery Packages on the user's system. It provides both library and CLI interfaces.

## Responsibilities

1. **Discover** available packages from remote library
2. **Download** packages with resume and parallel support
3. **Install** packages to local storage
4. **Update** packages when new versions are available
5. **Remove** packages from the system
6. **Mount** ortho packages via FUSE (coordinate with existing mount system)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Package Manager                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │   Library    │  │   Package    │  │      Download        │  │
│  │   Client     │  │   Scanner    │  │      Manager         │  │
│  │              │  │              │  │                      │  │
│  │ Fetch remote │  │ Scan local   │  │ Parallel downloads   │  │
│  │ library idx  │  │ packages     │  │ Resume support       │  │
│  │ Parse meta   │  │ Build state  │  │ Checksum verify      │  │
│  └──────────────┘  └──────────────┘  └──────────────────────┘  │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │   Installer  │  │   Mount      │  │      Update          │  │
│  │              │  │   Coordinator│  │      Checker         │  │
│  │ Extract pkg  │  │              │  │                      │  │
│  │ Create links │  │ Track mounts │  │ Compare local/remote │  │
│  │ Verify files │  │ Start/stop   │  │ Notify user          │  │
│  └──────────────┘  └──────────────┘  └──────────────────────┘  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Configuration

Settings in `~/.xearthlayer/config.ini`:

```ini
[packages]
# URL to the package library index
library_root = https://packages.xearthlayer.org

# Local directory for package storage
install_location = ~/.xearthlayer/packages

# Number of parallel download connections
download_threads = 4

# Download chunk size for resume support (bytes)
download_chunk_size = 10485760  # 10MB
```

## Local Package Discovery

The Manager discovers installed packages by scanning the `install_location` directory for `xearthlayer_scenery_package.txt` files.

### Scan Process

1. List directories in `install_location`
2. For each directory, check for `xearthlayer_scenery_package.txt`
3. Parse metadata file to extract version and package info
4. Build in-memory state of installed packages

### Package State

```rust
pub struct InstalledPackage {
    /// Region identifier (e.g., "EUROPE")
    pub region: String,
    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,
    /// Installed version
    pub version: Version,
    /// Path to package directory
    pub path: PathBuf,
    /// Whether currently mounted (ortho only)
    pub mounted: bool,
}
```

No persistent index file - state is always derived from scanning. This ensures:
- Single source of truth (the actual files)
- Self-healing if user manually modifies packages
- No sync issues between index and reality

## Remote Library Fetching

### Process

1. Fetch `xearthlayer_package_library.txt` from `library_root`
2. Parse library index
3. Compare sequence number with cached value for quick change detection
4. Build available packages list

### Caching

- Cache library index locally with timestamp
- Re-fetch if older than configurable TTL (default: 1 hour)
- Force refresh available via CLI flag

## Update Detection

Compare installed packages against remote library:

```rust
pub struct UpdateAvailable {
    pub region: String,
    pub package_type: PackageType,
    pub installed_version: Version,
    pub available_version: Version,
}
```

User is notified of available updates on first run of XEarthLayer (not repeatedly).

## Download Manager

### Features

- **Resumable downloads**: Track progress, resume interrupted downloads
- **Parallel parts**: Download multiple parts simultaneously
- **Verification**: SHA-256 checksum each part before proceeding
- **Progress reporting**: Callback for UI progress updates

### Download State

For interrupted downloads, maintain state file:

```
~/.xearthlayer/packages/.downloads/
├── zzXEL_na-1.0.0.download_state
└── zzXEL_na-1.0.0.tar.gz.aa  (partial)
```

State file format (JSON for simplicity):
```json
{
  "package": "zzXEL_na_ortho",
  "version": "1.0.0",
  "total_parts": 5,
  "completed_parts": ["aa", "ab"],
  "current_part": "ac",
  "current_part_bytes": 524288000,
  "started_at": "2025-12-20T10:30:00Z"
}
```

### HTTP Range Requests

For resume support, use HTTP Range header:
```
Range: bytes=524288000-
```

Most CDNs support this. Fall back to full re-download if not supported.

## Installation Process

### Ortho Packages

1. Download all parts
2. Verify checksums
3. Reassemble: `cat parts > archive.tar.gz`
4. Extract to `install_location/<package_name>/`
5. Verify `xearthlayer_scenery_package.txt` exists
6. Register package for mounting

### Overlay Packages

1. Download all parts
2. Verify checksums
3. Reassemble and extract to `install_location/<package_name>/`
4. Create symlink: `Custom Scenery/<package_name>` → `install_location/<package_name>`
5. Verify symlink resolves correctly

### Symlink Considerations

- Check filesystem supports symlinks (warn on FAT32/exFAT)
- Handle existing directory/symlink at target
- Provide manual copy fallback if symlinks unavailable

## Mount Coordination

For ortho packages, coordinate with XEarthLayer FUSE system.

### Multi-Mount Strategy

Each installed ortho package gets its own FUSE mount:

```
Custom Scenery/
├── zzXEL_afr_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_afr_ortho/
├── zzXEL_eur_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_eur_ortho/
└── zzXEL_na_ortho/   → FUSE mount → ~/.xearthlayer/packages/zzXEL_na_ortho/
```

### Mount State Tracking

Track active mounts for clean shutdown:

```rust
pub struct MountState {
    pub package: String,
    pub source_path: PathBuf,
    pub mount_point: PathBuf,
    pub session: BackgroundSession,
}
```

### Lifecycle

1. **Start**: Iterate installed ortho packages, mount each
2. **Runtime**: Handle mount failures gracefully (log, continue with others)
3. **Shutdown**: Unmount all in reverse order, wait for clean unmount

## Package Removal

### Ortho Packages

1. Check if mounted - unmount first if so
2. Remove mount point directory from Custom Scenery
3. Delete package directory from `install_location`

### Overlay Packages

1. Remove symlink from Custom Scenery
2. Delete package directory from `install_location`

### Partial Removal

If removal fails partway:
- Log what was removed
- Attempt to restore consistent state
- Report to user what manual cleanup may be needed

## CLI Interface

### Commands

```bash
# List available and installed packages
xearthlayer packages list

# Check for updates
xearthlayer packages check

# Install a package
xearthlayer packages install <region> [--type ortho|overlay|all]

# Update packages
xearthlayer packages update [<region>] [--all]

# Remove a package
xearthlayer packages remove <region> [--type ortho|overlay|all]

# Show package details
xearthlayer packages info <region>
```

### Output Examples

```
$ xearthlayer packages list

Available Packages:
  AFRICA        ortho: 1.0.0    overlay: 1.0.0
  ANTARCTICA    ortho: 1.0.0    overlay: 1.0.0
  ASIA          ortho: 1.0.0    overlay: 1.0.0
  AUSTRALIA     ortho: 1.0.0    overlay: 1.0.0
  EUROPE        ortho: 1.2.0    overlay: 1.1.0
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0
  SOUTH AMERICA ortho: 1.0.0    overlay: 1.0.0

Installed:
  EUROPE        ortho: 1.0.0 [update available: 1.2.0]
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0

$ xearthlayer packages install europe --type ortho

Downloading EUROPE ortho package (v1.2.0)...
  Part 1/5: zzXEL_eur-1.2.0.tar.gz.aa [################] 100% (1.0 GB)
  Part 2/5: zzXEL_eur-1.2.0.tar.gz.ab [########        ]  50% (512 MB)
  ...

Verifying checksums... OK
Extracting... OK
Registering for FUSE mount... OK

EUROPE ortho package installed successfully.
Run 'xearthlayer start' to begin serving scenery.
```

## Error Handling

### Network Errors

- Retry transient failures with exponential backoff
- Resume from last successful byte on connection drop
- Clear error message if server unreachable

### Disk Space

- Check available space before download
- Estimate required space (archive + extracted)
- Fail early with clear message if insufficient

### Checksum Failures

- Re-download failed part (up to 3 attempts)
- If persistent, report corruption and suggest re-download
- Never proceed with failed checksum

### Mount Failures

- Log detailed error
- Continue mounting other packages
- Report which packages failed at end

## Library Interface

The Package Manager exposes a library API for use by CLI and future GUI:

```rust
pub trait PackageManager {
    /// Scan local packages and return installed state
    fn scan_installed(&self) -> Result<Vec<InstalledPackage>, PackageError>;

    /// Fetch and parse remote library
    fn fetch_library(&self) -> Result<PackageLibrary, PackageError>;

    /// Check for available updates
    fn check_updates(&self) -> Result<Vec<UpdateAvailable>, PackageError>;

    /// Download and install a package
    fn install(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Update a package to latest version
    fn update(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Remove an installed package
    fn remove(&self, region: &str, package_type: PackageType) -> Result<(), PackageError>;
}
```

## Security Considerations

- **HTTPS only**: All downloads over TLS
- **Checksum verification**: SHA-256 for all parts
- **No arbitrary code execution**: Packages contain only data files
- **Symlink safety**: Validate symlink targets are within expected paths
