# Consolidated FUSE Mounting Design

**Status**: Speclet
**Created**: 2026-01-08
**Last Updated**: 2026-01-08

## Overview

This document describes the design for consolidating XEarthLayer's FUSE mounts into a single unified mount. Currently, each ortho package (and patches) has its own FUSE mount. This design merges all ortho sources into one mount at `zzXEL_ortho`.

### Goals

1. **Single mount point** - One FUSE mount (`zzXEL_ortho`) for all ortho sources (patches + regional packages)
2. **Resource efficiency** - Shared `XEarthLayerService`, memory cache, disk cache, concurrency limiters
3. **Simpler X-Plane setup** - Only two scenery entries: `yzXEL_overlay` and `zzXEL_ortho`
4. **Clear precedence** - Alphabetical ordering determines tile priority (patches first)
5. **Backward compatibility** - Legacy per-package mounting available via config flag

### Non-Goals

- Per-tile priority configuration (too complex, alphabetical is deterministic)
- Runtime hot-reload of packages (restart required)
- Overlay FUSE mounting (overlays remain symlink-based)

---

## Architecture Overview

```
┌────────────────────────────────────────────────────────────────────────────────┐
│                              X-PLANE APPLICATION                               │
│  Reads: zzXEL_ortho/textures/12345_67890_ZL16.dds                              │
└───────────────────────────────────────┬────────────────────────────────────────┘
                                        │
                                        ▼
┌────────────────────────────────────────────────────────────────────────────────┐
│                         Fuse3OrthoUnionFS                                      │
│                                                                                │
│  Single FUSE mount at: Custom Scenery/zzXEL_ortho/                             │
│                                                                                │
│  Operations:                                                                   │
│    lookup() → OrthoUnionIndex.resolve_file()                                   │
│    read()   → Passthrough for .ter/.dsf, DdsHandler for .dds                   │
│    readdir() → OrthoUnionIndex.list_directory()                                │
└───────────────────────────────────────┬────────────────────────────────────────┘
                                        │
                                        ▼
┌────────────────────────────────────────────────────────────────────────────────┐
│                           OrthoUnionIndex                                      │
│                                                                                │
│  Sources (sorted alphabetically for priority):                                 │
│    1. _patches/A_KDEN_Mesh     ← Patches prefixed with '_' for highest priority│
│    2. _patches/B_KLAX_Mesh                                                     │
│    3. eu                        ← Regional packages by code                    │
│    4. na                                                                       │
│    5. sa                                                                       │
│                                                                                │
│  Data Structures:                                                              │
│    files: HashMap<VirtualPath, (source_name, RealPath)>                        │
│    directories: HashMap<VirtualPath, Vec<DirEntry>>                            │
│    sources: Vec<OrthoSource>                                                   │
└───────────────────────────────────────┬────────────────────────────────────────┘
                                        │
                                        ▼
┌────────────────────────────────────────────────────────────────────────────────┐
│                      Shared Job Executor Daemon                                │
│                                                                                │
│  Single executor instance serving all sources:                                 │
│    - DdsClient for job submission                                              │
│    - Shared memory cache (LRU, ~2GB)                                           │
│    - Shared disk cache (LRU, ~20GB)                                            │
│    - Resource pools (Network, CPU, DiskIO concurrency limiting)                │
└────────────────────────────────────────────────────────────────────────────────┘
```

---

## Data Structures

### Package (Base Type)

Core package identity shared across publisher, package manager, and runtime contexts.

```rust
/// Core package identity.
///
/// This is the base type representing a scenery package. It contains
/// the essential identifying information used across all contexts:
/// - Publisher: creating and versioning packages
/// - Package Manager: downloading and installing
/// - Runtime: mounting and serving to X-Plane
/// - Library Index: listing available packages
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Region code (e.g., "na", "eu", "sa")
    pub region: String,

    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,

    /// Package version
    pub version: semver::Version,
}

impl Package {
    /// Create a new package.
    pub fn new(
        region: impl Into<String>,
        package_type: PackageType,
        version: semver::Version,
    ) -> Self;

    /// Check if this is an ortho package.
    pub fn is_ortho(&self) -> bool {
        self.package_type == PackageType::Ortho
    }

    /// Check if this is an overlay package.
    pub fn is_overlay(&self) -> bool {
        self.package_type == PackageType::Overlay
    }

    /// Get the canonical folder name (e.g., "zzXEL_na_ortho")
    pub fn folder_name(&self) -> String;
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} v{}", self.region, self.package_type, self.version)
    }
}
```

**Location**: `xearthlayer/src/package/core.rs`

### InstalledPackage (Composition Extension)

Extends `Package` with installation-specific context using composition.

```rust
/// An installed scenery package.
///
/// Uses composition to extend `Package` with installation context:
/// - Filesystem path where the package is installed
/// - Enabled state for UI toggle feature
///
/// # Example
///
/// ```rust
/// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
/// let installed = InstalledPackage::new(package, "/path/to/na_ortho");
///
/// // Access base package fields via Deref
/// println!("Region: {}", installed.region);
/// println!("Enabled: {}", installed.enabled);
/// ```
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    /// Core package identity (composition)
    pub package: Package,

    /// Filesystem path to the installed package directory
    pub path: PathBuf,

    /// Whether this package is enabled for mounting.
    /// Controlled via UI or config. Disabled packages are not mounted.
    pub enabled: bool,
}

impl InstalledPackage {
    /// Create a new enabled installed package.
    pub fn new(package: Package, path: impl Into<PathBuf>) -> Self {
        Self {
            package,
            path: path.into(),
            enabled: true,
        }
    }

    /// Create a disabled installed package.
    pub fn new_disabled(package: Package, path: impl Into<PathBuf>) -> Self {
        Self {
            package,
            path: path.into(),
            enabled: false,
        }
    }

    /// Set enabled state (builder pattern).
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// Deref to Package for convenient access to base fields.
impl std::ops::Deref for InstalledPackage {
    type Target = Package;

    fn deref(&self) -> &Self::Target {
        &self.package
    }
}

/// Convert InstalledPackage back to Package (drops installation context).
impl From<InstalledPackage> for Package {
    fn from(installed: InstalledPackage) -> Self {
        installed.package
    }
}
```

**Location**: `xearthlayer/src/package/installed.rs`

### Relationship to Existing Types

| Existing Type | Relationship to Package |
|---------------|------------------------|
| `PackageMetadata` | Contains same fields + archive parts, checksums. Consider refactoring to use `Package`. |
| `LibraryEntry` | Contains same fields + download URL. Consider refactoring to use `Package`. |
| `PackageType` | Reused as `Package.package_type` |

### OrthoSource

Represents a single source of ortho tiles (either a patch or regional package). Used internally by `OrthoUnionIndex`.

```rust
/// Type of ortho source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// User-provided mesh from ~/.xearthlayer/patches/
    Patch,
    /// Installed regional package (na, eu, sa, etc.)
    RegionalPackage,
}

/// A single ortho source (patch or package).
#[derive(Debug, Clone)]
pub struct OrthoSource {
    /// Sort key for priority (e.g., "_patches/KLAX_Mesh", "na")
    /// Underscore prefix ensures patches sort first alphabetically.
    pub sort_key: String,

    /// Display name (e.g., "KLAX_Mesh", "North America")
    pub display_name: String,

    /// Real filesystem path to the source directory
    pub source_path: PathBuf,

    /// Type of source
    pub source_type: SourceType,

    /// Whether this source is enabled
    /// For patches: always true
    /// For packages: from InstalledPackage.enabled
    pub enabled: bool,
}
```

### OrthoUnionIndex

Merged index of all tiles across patches and regional packages.

```rust
/// Information about a file's source.
#[derive(Debug, Clone)]
pub struct FileSource {
    /// Index into OrthoUnionIndex.sources
    pub source_idx: usize,

    /// Real filesystem path
    pub real_path: PathBuf,
}

/// A directory entry in the union filesystem.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: OsString,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: SystemTime,
}

/// Merged index of all ortho sources.
pub struct OrthoUnionIndex {
    /// All sources, sorted by sort_key (patches first, then regions alphabetically)
    sources: Vec<OrthoSource>,

    /// Map from virtual path → file source
    /// Keys are relative paths (e.g., "terrain/12345_67890_ZL16.ter")
    files: HashMap<PathBuf, FileSource>,

    /// Virtual directory structure for readdir
    /// Keys are directory paths, values are entries in that directory
    directories: HashMap<PathBuf, Vec<DirEntry>>,

    /// Statistics
    total_files: usize,
}
```

---

## Precedence Rules

### Sorting Strategy

Sources are sorted alphabetically by `sort_key`. To ensure patches always have highest priority:

1. **Patches** get sort key: `_patches/{folder_name}` (underscore sorts before letters)
2. **Regional packages** get sort key: `{region_code}` (e.g., "eu", "na", "sa")

**Example sort order:**
```
_patches/A_KDEN_Mesh   ← Highest priority
_patches/B_KLAX_Mesh
eu                      ← Regions alphabetical (eu < na < sa)
na
sa                      ← Lowest priority
```

### Collision Resolution

When multiple sources contain the same file path (e.g., same 1° tile in different packages):

1. Sources are processed in sort order
2. First source wins - subsequent sources are skipped for that path
3. This matches existing `PatchUnionIndex` behavior

**Example:** If both `na` and `sa` packages contain `Earth nav data/+10-070/+10-070.dsf`:
- `na` sorts before `sa` alphabetically
- `na`'s DSF is used, `sa`'s is ignored

---

## Module Structure

### New Files

```
xearthlayer/src/
├── package/
│   ├── mod.rs              # Add: pub mod core; pub mod installed;
│   ├── core.rs             # NEW: Package struct
│   └── installed.rs        # NEW: InstalledPackage struct
└── ortho_union/
    ├── mod.rs              # Module exports
    ├── source.rs           # OrthoSource, SourceType
    ├── index.rs            # OrthoUnionIndex (read-only after construction)
    └── builder.rs          # OrthoUnionIndexBuilder (strict builder pattern)
```

### OrthoUnionIndexBuilder (Strict Builder Pattern)

The builder separates configuration from construction, enabling a fluent API:

```rust
// builder.rs

/// Builder for constructing an OrthoUnionIndex.
///
/// Follows the strict builder pattern:
/// - Configuration methods return `Self` for chaining
/// - `build()` performs validation and construction
/// - Result is immutable after construction
pub struct OrthoUnionIndexBuilder {
    patches_dir: Option<PathBuf>,
    packages: Vec<InstalledPackage>,
}

impl OrthoUnionIndexBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            patches_dir: None,
            packages: Vec::new(),
        }
    }

    /// Set the patches directory to scan.
    /// Only valid patches (with Earth nav data + DSF files) are included.
    pub fn with_patches_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.patches_dir = Some(dir.into());
        self
    }

    /// Add an installed package.
    /// Only enabled ortho packages are included in the final index.
    pub fn add_package(mut self, package: InstalledPackage) -> Self {
        self.packages.push(package);
        self
    }

    /// Add multiple installed packages.
    pub fn add_packages(mut self, packages: impl IntoIterator<Item = InstalledPackage>) -> Self {
        self.packages.extend(packages);
        self
    }

    /// Build the OrthoUnionIndex.
    ///
    /// This method:
    /// 1. Discovers valid patches from patches_dir (if set)
    /// 2. Filters packages to enabled ortho packages only
    /// 3. Creates OrthoSource for each source
    ///    - Patches: sort_key = "_patches/{folder_name}"
    ///    - Packages: sort_key = "{region}"
    /// 4. Sorts sources alphabetically by sort_key
    /// 5. Scans all sources to build the file index (first source wins on collision)
    ///
    /// # Errors
    ///
    /// Returns an error if directory scanning fails.
    pub fn build(self) -> std::io::Result<OrthoUnionIndex>;
}

impl Default for OrthoUnionIndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}
```

**Usage Example:**

```rust
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::package::{Package, InstalledPackage, PackageType};
use semver::Version;

// Create installed packages
let na = InstalledPackage::new(
    Package::new("na", PackageType::Ortho, Version::new(1, 0, 0)),
    "/home/user/.xearthlayer/packages/na_ortho",
);
let eu = InstalledPackage::new(
    Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
    "/home/user/.xearthlayer/packages/eu_ortho",
);

// Build the index
let index = OrthoUnionIndexBuilder::new()
    .with_patches_dir("/home/user/.xearthlayer/patches")
    .add_package(na)
    .add_package(eu)
    .build()?;

// Use the index
println!("Sources: {:?}", index.sources().iter().map(|s| &s.sort_key).collect::<Vec<_>>());
// Output: ["_patches/A_KDEN_Mesh", "_patches/B_KLAX_Mesh", "eu", "na"]
```

### OrthoUnionIndex (Read-Only After Construction)

```rust
// index.rs

/// Merged index of all ortho sources (patches + packages).
///
/// This struct is immutable after construction via `OrthoUnionIndexBuilder`.
/// It provides read-only access to the merged file structure.
impl OrthoUnionIndex {
    // No public constructor - use OrthoUnionIndexBuilder

    /// Resolve a virtual path to its file source.
    pub fn resolve(&self, virtual_path: &Path) -> Option<&FileSource>;

    /// Get the real filesystem path for a virtual path.
    pub fn resolve_path(&self, virtual_path: &Path) -> Option<&PathBuf>;

    /// Check if path exists in the index.
    pub fn contains(&self, virtual_path: &Path) -> bool;

    /// Check if path is a directory.
    pub fn is_directory(&self, virtual_path: &Path) -> bool;

    /// List entries in a directory.
    pub fn list_directory(&self, virtual_path: &Path) -> Vec<&DirEntry>;

    /// Get all sources in priority order.
    pub fn sources(&self) -> &[OrthoSource];

    /// Get the source at a given index.
    pub fn get_source(&self, idx: usize) -> Option<&OrthoSource>;

    /// Get total file count.
    pub fn file_count(&self) -> usize;

    /// Get total directory count.
    pub fn directory_count(&self) -> usize;

    /// Check if index is empty (no sources).
    pub fn is_empty(&self) -> bool;

    /// Iterate over all files in the index.
    pub fn files(&self) -> impl Iterator<Item = (&PathBuf, &FileSource)>;
}
```

---

## FUSE Integration

### Fuse3OrthoUnionFS

New FUSE filesystem that wraps `OrthoUnionIndex` and `DdsHandler`.

```rust
pub struct Fuse3OrthoUnionFS {
    /// Union index for file resolution
    index: Arc<OrthoUnionIndex>,

    /// DDS handler for on-demand texture generation
    dds_handler: Arc<DdsHandler>,

    /// Inode management (similar to existing Fuse3PassthroughFS)
    inodes: InodeManager,
}
```

### FUSE Operations

| Operation | Behavior |
|-----------|----------|
| `lookup(parent, name)` | Resolve via `index.resolve()`, return file attributes |
| `getattr(ino)` | Get attributes for inode |
| `read(ino, offset, size)` | If `.dds`: generate via `DdsHandler`. Else: passthrough read |
| `readdir(ino)` | Return `index.list_directory()` entries |
| `open(ino)` | Track open file handles |
| `release(ino)` | Close file handles |

### DDS Handling

For `.dds` files, the filename encodes tile coordinates:

```
textures/12345_67890_ZL16.dds
         ^     ^     ^
         row   col   zoom
```

The `DdsHandler` generates the texture using:
1. Parse (row, col, zoom) from filename
2. Download 256 imagery chunks from provider
3. Assemble into 4096×4096 image
4. Encode to BC1/BC3 DDS with mipmaps
5. Cache and return

---

## Overlay Consolidation

Overlays remain symlink-based but are consolidated into a single folder.

### Current Structure

```
Custom Scenery/
├── yzXEL_na_overlay -> ~/.xearthlayer/packages/na_overlay/
├── yzXEL_eu_overlay -> ~/.xearthlayer/packages/eu_overlay/
└── yzXEL_sa_overlay -> ~/.xearthlayer/packages/sa_overlay/
```

### Consolidated Structure

```
Custom Scenery/
└── yzXEL_overlay/
    └── Earth nav data/
        ├── +30-090/  -> ~/.xearthlayer/packages/na_overlay/Earth nav data/+30-090/
        ├── +40-010/  -> ~/.xearthlayer/packages/eu_overlay/Earth nav data/+40-010/
        └── -10-070/  -> ~/.xearthlayer/packages/sa_overlay/Earth nav data/-10-070/
```

Each 10° grid directory is symlinked individually. On collision (same grid in multiple packages), first package alphabetically wins.

---

## Configuration

### New Config Section

```ini
[mounting]
; Use consolidated FUSE mount (default: true)
; Set to false for legacy per-package mounting
consolidated = true
```

### ConfigKey Addition

```rust
// config/keys.rs
pub enum ConfigKey {
    // ... existing keys

    /// Whether to use consolidated mounting (default: true)
    MountingConsolidated,
}
```

---

## Migration Path

### New Installations

- `mounting.consolidated = true` by default
- Single `zzXEL_ortho` mount created
- Single `yzXEL_overlay` folder created

### Existing Installations

1. `xearthlayer config upgrade` adds `[mounting]` section
2. Old mount points cleaned up on startup if consolidated enabled
3. Users can set `consolidated = false` for legacy behavior

### Cleanup Logic

On startup with `consolidated = true`:
1. Unmount any existing `zzXEL_*_ortho` mounts
2. Remove any existing `zzyXEL_patches_ortho` mount
3. Remove any existing `yzXEL_*_overlay` symlinks
4. Create consolidated mount/folder

---

## Integration Points

### MountManager Changes

```rust
impl MountManager {
    /// Mount all ortho sources as a single consolidated FUSE mount.
    pub async fn mount_consolidated_ortho(
        &mut self,
        patches_dir: Option<&Path>,
        packages: &[InstalledOrthoPackage],
        custom_scenery: &Path,
    ) -> Result<()>;

    /// Create consolidated overlay folder with symlinks.
    pub fn create_consolidated_overlay(
        &self,
        packages: &[InstalledOverlayPackage],
        custom_scenery: &Path,
    ) -> Result<()>;
}
```

### Prefetch System Changes

The `RadialPrefetcher` needs access to `OrthoUnionIndex` to:
1. Check if a tile coordinate belongs to any installed package
2. Skip prefetch for tiles not owned by any source

```rust
impl RadialPrefetcher {
    pub fn new(
        dds_handler: Arc<DdsHandler>,
        ortho_index: Arc<OrthoUnionIndex>,  // New dependency
    ) -> Self;
}
```

---

## Testing Strategy

### Unit Tests

1. **OrthoSource**: Sort key generation, equality
2. **OrthoUnionIndex**: Build from mock packages, collision resolution, directory listing
3. **Precedence**: Verify patches win over packages, alphabetical ordering

### Integration Tests

1. **FUSE Operations**: Mount, read files, verify content
2. **DDS Generation**: Request DDS, verify generation pipeline invoked
3. **Overlay Consolidation**: Create symlinks, verify structure

### Manual Testing

1. Install multiple overlapping packages
2. Verify correct tile served (first alphabetically)
3. Test with patches and packages
4. Verify X-Plane loads scenery correctly

---

## Deprecation Plan

### Potentially Deprecated Code

| Component | Status |
|-----------|--------|
| `patches/` module | Integrate into `ortho_union/` |
| `Fuse3UnionFS` | Superseded by `Fuse3OrthoUnionFS` |
| Per-package mounting in `MountManager` | Keep as fallback |
| `mount_packages()` method | Keep for `consolidated = false` |

### Timeline

1. **v0.2.11**: Implement consolidated mounting (default enabled)
2. **v0.3.0**: Evaluate removing legacy code based on user feedback
3. **v1.0.0**: Consider removing legacy per-package mounting

---

## Open Questions

### Resolved

1. **Should patches be merged into ortho union?**
   - **Yes** - Single mount is simpler, patches get priority via `_` prefix

2. **How to resolve tile collisions between regions?**
   - **Alphabetical by region code** - Simple, deterministic (eu < na < sa)

### Open

1. **Should we validate tile coverage doesn't have gaps after consolidation?**
   - Consider adding a `--validate` flag to startup

2. **How to handle packages being added/removed while running?**
   - Currently requires restart. Hot-reload is future work.
