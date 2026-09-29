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
8. **Delete** a package the library no longer offers

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
│  ┌──────────────────────────────────────────────────────────┐  │
│  │               Zoom Level Dedupe Module                    │  │
│  │                                                           │  │
│  │ Detector: Find overlapping tiles across zoom levels      │  │
│  │ Resolver: Apply priority-based removal (highest/lowest)  │  │
│  │ Gap Analysis: Find incomplete coverage, export to O4XP   │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                    Deletion Module                        │  │
│  │                                                           │  │
│  │ plan_deletion:    Pure. What would go, and how much.     │  │
│  │ execute_deletion: Acts on a plan, index entry first.     │  │
│  │ Never touches assets already uploaded to a host.         │  │
│  └──────────────────────────────────────────────────────────┘  │
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
│   └── na/                              # Region subdirectory
│       └── ortho/                       # Type subdirectory
│           ├── zzXEL_na-1.0.0.tar.gz.aa
│           ├── zzXEL_na-1.0.0.tar.gz.ab
│           └── ...
└── staging/                             # Work in progress
    └── ...
```

### Repository Marker

`.xearthlayer-repo` contains repository metadata and optional configuration:

```
XEARTHLAYER PACKAGE REPOSITORY
1.0.0
2025-12-20T10:00:00Z

[config]
part_size = 500000000
```

The `[config]` section is optional and stores repository-wide settings:

| Key | Description | Default |
|-----|-------------|---------|
| `part_size` | Archive split size in bytes | 500MB |

Part size can be specified with units: `500MB`, `1GB`, `10 MB`, etc.

## Workflow

### 1. Initialize Repository

Create a new package repository:

```bash
xearthlayer-publish init /path/to/repo

# Or in current directory
xearthlayer-publish init .
```

Creates:
- `.xearthlayer-repo` marker
- Empty `xearthlayer_package_library.txt`
- `packages/`, `dist/`, `staging/` directories

### 2. Process Ortho4XP Output

Import Ortho4XP tiles into a regional package:

```bash
xearthlayer-publish add \
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
xearthlayer-publish build --region na --type ortho
```

Steps:

1. Create tar.gz archive of package directory
2. Split into parts (configurable size, default 1GB)
3. Generate SHA-256 checksum for each part
4. Store in `dist/` directory

### 4. Configure URLs

Set download URLs for the package:

```bash
xearthlayer-publish urls \
  --region na \
  --type ortho \
  --base-url https://dl.example.com/packages/na/ortho/
```

Updates metadata file with actual download URLs.

### 5. Publish

Finalize and update library index:

```bash
xearthlayer-publish release
```

Steps:

1. Validate all packages have URLs configured
2. Update `xearthlayer_package_library.txt`:
   - Increment sequence number
   - Update timestamp
   - Add/update package entries
3. Generate checksums for metadata files
4. Mark repository as published

`release` also measures the package and records the result in
`region_metadata.json`, under the region's `size.ortho` or `size.overlay` block:
`download_bytes` from the archive parts in `dist/` and `installed_bytes` from
the package working directory. That file is edited as JSON rather than through
a typed model, so fields the publisher does not model, including the ones the
website reads, survive the write.

Updating the library index is the release. A size that cannot be measured, or a
metadata file that is missing or has no entry for the region, is reported as a
warning on an otherwise successful release rather than an error. An error here
would invite a retry, and a second `release` bumps the library sequence again
for a release that already happened.

### 6. Upload (Manual)

The Publisher creates files but doesn't upload. User uploads:

```bash
# Example using rclone
rclone sync dist/ remote:bucket/packages/

# Or rsync
rsync -avz dist/ user@server:/var/www/packages/

# Or any other method
```

### 7. Delete

Retire a package that the library should no longer offer:

```bash
xearthlayer-publish delete --region na --dry-run   # plan only
xearthlayer-publish delete --region na             # both package types
```

Inspection is split from action. `plan_deletion` is pure: it resolves the
library entry, the package working directory and the dist directory, sums their
apparent size, and changes nothing. `execute_deletion` acts on that plan. A dry
run is the inspection on its own, which is what makes it impossible for the
preview to describe something different from what the real run does.

Three behaviours are deliberate:

- **The index entry is removed first.** If a directory removal then fails, the
  package is already unadvertised, which is the safe half to have completed:
  nobody is offered a package whose archives are being removed underneath them.
- **A region with nothing to delete is an error.** No index entry, no package
  directory and no archives almost always means a mistyped region code. A
  package that was built but never released is not an error, since reclaiming
  its archives is one of the reasons the command exists. Omitting `--type`
  tolerates a region that has only one of the two package types and reports the
  other as skipped.
- **Assets already uploaded to a hosting provider are never touched.** The
  publisher has no model of the hosting, and those files must outlive the index
  entry while users are still installing from them. Taking them down is a
  separate, manual step.

A region code containing a path separator is rejected before any path is built
from it. Both directory paths are formed by joining the region into a path that
is eventually handed to a recursive delete, and a region code is a directory
name under Custom Scenery, so no legitimate code contains a separator.

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
│       ├── 25264_10912_BI16.dds      # REMOVED (orthophoto)
│       ├── 25264_10912_ZL16.png      # KEPT (water mask)
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
    ├── 25264_10912_ZL16.png
    └── ...
```

### File Filtering Rules

| File Type | Action | Reason |
|-----------|--------|--------|
| `*.dsf` | Keep, compressed | Terrain mesh, written as a single-entry 7z container (see DSF Compression) |
| `*.ter` | Keep | Terrain definitions |
| `textures/*.png` | Keep | Water masks. Every PNG under a tile's `textures/` is a mask, named `{x}_{y}_ZL{n}.png` with nothing marking it as a mask, so the processor matches on the extension and keeps all of them |
| `*.dds` | Remove | Generated on-demand |
| `*.pol` | Keep if present | Polygon definitions |
| `*.net` | Keep if present | Network definitions |
| `*.obj` | Keep if present | Object definitions |

### DSF Compression

`DsfCompressor` (`publisher/dsf_compress.rs`) writes each DSF as a 7z archive with one entry named after the file, using LZMA with a 16 MiB dictionary and a plain header. That is the profile of Laminar's own Global Scenery DSF files, and X-Plane has decoded it natively since version 10. The dictionary size is chosen for the consumer rather than the producer: X-Plane allocates the dictionary for every DSF it decodes, so matching Laminar keeps packages inside a memory budget the sim already carries.

Both processors queue their DSF copies and hand the list to `compress_dsf_files`, which runs the batch across rayon's pool because LZMA at this dictionary size is CPU bound and a region holds around two thousand files. A source that already begins with the 7z signature is copied unchanged, so processing a directory twice is a no-op. `ProcessSummary` reports raw and stored bytes, which `publish add` prints as a percentage.

The tar.gz archive around the package is unchanged. Once the DSF files are compressed, only the water masks remain compressible, and moving the container to zstd was measured at 1.1% of download, which did not justify a format change.

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
xearthlayer-publish version --region na --type ortho --bump minor

# Or set explicitly
xearthlayer-publish version --region na --type ortho --set 2.0.0
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
xearthlayer-publish init [<path>] [--part-size <size>]

# Scan Ortho4XP output and report tile information
xearthlayer-publish scan --source <ortho4xp_tiles_path>

# Add Ortho4XP output to a package
xearthlayer-publish add \
  --source <ortho4xp_tiles_path> \
  --region <region_code> \
  [--type <ortho|overlay>] \
  [--version <semver>] \
  [--repo <path>]

# List packages in repository
xearthlayer-publish list [<repo_path>] [--verbose]

# Build archives for a package
xearthlayer-publish build \
  --region <region_code> \
  [--type <ortho|overlay>] \
  [--repo <path>]

# Set download URLs
xearthlayer-publish urls \
  --region <region_code> \
  [--type <ortho|overlay>] \
  --base-url <url> \
  [--verify] \
  [--repo <path>]

# Bump or set version
xearthlayer-publish version \
  --region <region_code> \
  [--type <ortho|overlay>] \
  <--bump major|minor|patch | --set <version>> \
  [--repo <path>]

# Finalize and update library index
xearthlayer-publish release \
  --region <region_code> \
  [--type <ortho|overlay>] \
  --metadata-url <url> \
  [--repo <path>]

# Show package release status
xearthlayer-publish status \
  [--region <region_code>] \
  [--type <ortho|overlay>] \
  [<repo_path>]

# Validate repository integrity
xearthlayer-publish validate [<repo_path>]

# Analyze coverage gaps (incomplete ZL18 coverage over ZL16)
xearthlayer-publish gaps \
  --region <region_code> \
  [--type <ortho|overlay>] \
  [--tile <lat,lon>] \
  [--format <text|json|ortho4xp|summary>] \
  [--output <file>] \
  [<repo_path>]

# Remove overlapping zoom level tiles
xearthlayer-publish dedupe \
  --region <region_code> \
  [--type <ortho|overlay>] \
  [--priority <highest|lowest|zl##>] \
  [--tile <lat,lon>] \
  [--dry-run] \
  [<repo_path>]

# Delete a package: index entry, working directory and archives
xearthlayer-publish delete \
  --region <region_code> \
  [--type <ortho|overlay>] \
  [--dry-run] \
  [--yes] \
  [--repo <path>]
```

### CLI Architecture

The CLI is implemented using the **Command Pattern** with **trait-based dependency injection** for testability and maintainability.

#### Module Structure

```
xearthlayer-publish/src/
├── main.rs       # Binary entry point and command dispatch
├── traits.rs     # Core interfaces (Output, Prompt, PublisherService, CommandHandler)
├── services.rs   # Concrete implementations wrapping xearthlayer-publisher
├── args.rs       # CLI argument types and parsing (clap-derived)
├── handlers.rs   # Command handlers implementing business logic
└── output.rs     # Shared output formatting utilities
```

The binary was split out of `xearthlayer-cli` in #284 so the runtime does not
link the publishing dependencies. See
[Publisher Separation](publisher-separation-design.md).

#### Core Traits

```rust
/// Abstracts console output for testable handlers
pub trait Output: Send + Sync {
    fn println(&self, message: &str);
    fn print(&self, message: &str);
    fn newline(&self);
    fn header(&self, title: &str);
    fn indented(&self, message: &str);
}

/// Abstracts asking the user to confirm an irreversible action.
///
/// Separate from `Output`, which is output only. Keeping confirmation behind
/// its own trait means a handler that deletes data can be tested without a
/// terminal, and a test can assert that declining deletes nothing.
pub trait Prompt: Send + Sync {
    fn confirm(&self, question: &str) -> Result<bool, CliError>;
}

/// Abstracts all publisher operations
pub trait PublisherService: Send + Sync {
    fn init_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError>;
    fn open_repository(&self, path: &Path) -> Result<Box<dyn RepositoryOperations>, CliError>;
    fn scan_scenery(&self, source: &Path) -> Result<SceneryScanResult, CliError>;
    fn process_tiles(...) -> Result<ProcessSummary, CliError>;
    // ... other operations
}

/// Each command handler implements this trait
pub trait CommandHandler {
    type Args;
    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError>;
}
```

#### Design Benefits

| Benefit | Description |
|---------|-------------|
| **Single Responsibility** | Each handler owns one command's logic |
| **Dependency Injection** | Handlers depend only on trait interfaces |
| **Testability** | Handlers can be tested with mock implementations |
| **Extensibility** | Adding commands means adding a handler |
| **Consistent Interface** | CommandHandler trait enforces uniform API |

#### Testing Example

```rust
// Production usage
let output = ConsoleOutput::new();
let publisher = DefaultPublisherService::new();
let prompt = ConsolePrompt::new();
let ctx = CommandContext::new(&output, &publisher, &prompt);
InitHandler::execute(args, &ctx)?;

// Test usage with mocks
let output = MockOutput::new();
let publisher = MockPublisherService::new();
let prompt = MockPrompt::answering(false);
let ctx = CommandContext::new(&output, &publisher, &prompt);
InitHandler::execute(args, &ctx)?;
assert!(output.contains("Initialized"));
```

### Output Examples

```
$ xearthlayer-publish init ~/scenery-repo
Initialized XEarthLayer package repository at /home/user/scenery-repo

$ xearthlayer-publish add --source ~/Ortho4XP/Tiles --region na --type ortho --version 1.0.0
Scanning Ortho4XP output...
Found 45 tiles in /home/user/Ortho4XP/Tiles

Processing tiles:
  +37-118: OK (DSF: 1, TER: 256, masks: 128)
  +37-119: OK (DSF: 1, TER: 312, masks: 156)
  ...

Removed 12,456 DDS files (saving 45.2 GB in package)
Created package: zzXEL_na_ortho v1.0.0

$ xearthlayer-publish build --region na --type ortho
Creating archive...
  Compressing: 2.3 GB → 1.8 GB (22% reduction)
  Splitting into 2 parts (1 GB each)

Generated:
  dist/zzXEL_na-1.0.0.tar.gz.aa (1.0 GB) SHA256: 55e772c1...
  dist/zzXEL_na-1.0.0.tar.gz.ab (0.8 GB) SHA256: b91f75f9...

$ xearthlayer-publish urls --region na --type ortho --base-url https://dl.example.com/na/
Updated URLs in zzXEL_na_ortho metadata

$ xearthlayer-publish release
Updating library index...
  Sequence: 0 → 1
  Packages: 1
  Published: 2025-12-20T15:30:00Z

Repository published successfully.
Upload dist/ contents to your hosting provider.
```

## Package Release States

Each package in the repository has a release status that tracks its position in the publish workflow:

### Release Status

| Status | Description | Next Action |
|--------|-------------|-------------|
| `NotBuilt` | Package exists but no archive built | Run `build` command |
| `AwaitingUrls` | Archive built, parts have checksums but no URLs | Run `urls` command |
| `Ready` | URLs configured and verified, ready for release | Run `release` command |
| `Released` | Package is in the library index | Upload files to hosting |

### Status Detection

The status is determined automatically by examining package metadata:

1. **NotBuilt**: No parts defined in metadata (part count = 0)
2. **AwaitingUrls**: Parts exist but one or more has empty URL
3. **Ready**: All parts have URLs, package not in library index
4. **Released**: Package entry exists in library index

### Context-Aware Validation

Package metadata validation adapts to the release status:

- **Initial context**: Parts optional, URLs not required (post-processing)
- **AwaitingUrls context**: Parts required, URLs may be empty (post-build)
- **Release context**: Parts and URLs required (pre-release validation)

This separation of parsing (lenient) from validation (context-aware) enables the multi-phase workflow while ensuring only complete packages are published.

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
xearthlayer-publish release
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

## Related Documentation

- **Zoom Level Overlap Management**: See `docs/dev/zoom-level-overlap-design.md` for detailed design of the dedupe module, gap analysis algorithm, and gap protection mechanisms.
