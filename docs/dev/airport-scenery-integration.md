# Airport Scenery Integration (Dynamic Patch Textures)

**Status**: Planned (0.3.x) — temporarily removed, will return with GeoIndex integration
**Created**: 2026-01-07
**Last Updated**: 2026-02-17

> **Note**: This feature was previously implemented and functional (v0.2.x). It has been
> temporarily removed in favor of the simpler [Scenery Patches](scenery-patches.md)
> (bring-your-own-scenery) model introduced with [GeoIndex](geo-index-design.md) in
> issue #51. When this feature returns in a later 0.3.x release, it will use GeoIndex
> as its region ownership layer, replacing the previous `patched_regions` HashSet
> approach. See [Scenery Patches](scenery-patches.md) for the current patch behavior.

## Overview

The Airport Scenery Integration feature enables XEarthLayer to serve pre-built Ortho4XP tiles containing custom mesh/elevation data from third-party airport addons, while generating textures dynamically using XEL's configured imagery provider.

### Goals

1. **Addon Compatibility** - Support airport addons that require custom elevation/mesh
2. **Visual Consistency** - Use XEL's provider for all textures (no seams at tile boundaries)
3. **Provider Flexibility** - Enable providers not available in Ortho4XP (Apple Maps, Mapbox)
4. **Non-Destructive** - Never modify user's patch source files
5. **Simple User Workflow** - Just copy built Ortho4XP tile to patches folder

### Non-Goals

- Automatic patch detection from existing scenery packages
- Integration with Ortho4XP build process
- Provider-specific texture matching

---

## Problem Statement

Third-party X-Plane scenery developers (airport addons) provide custom elevation/mesh data that requires ortho tiles built with matching elevation. The challenge is that if users build these tiles with a different imagery provider than XEL uses, there will be visible seams at tile boundaries.

### Root Cause

Airport addons like SFD KLAX or X-Codr KDEN include custom DSF files with corrected elevation for the airport area. When users build ortho tiles:

1. **With standard elevation**: Airport mesh doesn't match, causing visual artifacts
2. **With addon's elevation but different provider**: Imagery doesn't match adjacent XEL tiles

### Real-World Examples

**KLAX (SFD):**
- Location: `/run/media/sdefreyssinet/FlightSim/X-Plane Scenery/SFD_KLAX_Los_Angeles_HD_1_Airport`
- Provides: `!Ortho4XP_Patch/Elevations/+33-119.tif` (custom elevation)
- Provides: `!Ortho4XP_Patch/Patches/+30-120/+33-119/KLAX.patch.osm` (OSM modifications)

**KDEN (X-Codr):**
- Location: `/run/media/sdefreyssinet/FlightSim/X-Plane Scenery/KDEN Ortho/Z KDEN Mesh`
- Structure: Complete Ortho4XP output with `Earth nav data/`, `terrain/`, `textures/`

---

## Design Decisions

### D1: Patches = Complete Ortho4XP Tiles

**Decision**: Patches are complete Ortho4XP tile folders, NOT individual DDS files.

**Patch structure:**
```
~/.xearthlayer/patches/
└── KLAX_Mesh/                        # User-named folder
    ├── Earth nav data/
    │   └── +30-120/
    │       └── +33-119.dsf           # DSF with custom mesh
    ├── terrain/
    │   ├── 94800_47888_BI18.ter      # Terrain definition files
    │   └── ...
    └── textures/                      # May be empty or sparse
        └── ...                        # XEL generates these on-demand
```

**Rationale:**
- Matches standard Ortho4XP output structure
- User simply moves built tile to patches folder
- DSF contains the custom mesh/elevation data
- Terrain files define texture references

### D2: XEL Generates Textures Dynamically

**Decision**: When X-Plane requests DDS textures from a patch, XEL generates them using its configured imagery provider.

**Rationale:**
- Ensures consistent imagery across all tiles (no seams)
- Supports providers not available in Ortho4XP
- User doesn't need to pre-generate textures
- Patch provides mesh, XEL provides imagery

### D3: Patches Have Highest Priority

**Decision**: Patches ALWAYS have higher priority than any regional package.

**Mount order:**
1. Patches folder (highest priority - `zzy` prefix)
2. Regional ortho packages (`zz` prefix)
3. Overlays (`yz` prefix - loaded first by X-Plane)

**Rationale:**
- When a tile exists in patches, use patch's mesh
- Ensures addon elevation data is used
- Simple, predictable behavior

### D4: Union Filesystem for Patches

**Decision**: Present all patches as a **single unified scenery package** via FUSE, merging contents from all patch folders non-destructively.

**Rationale:**
- Non-destructive: Real filesystem unchanged
- Single mount point: Simple for X-Plane scenery.ini
- Predictable priority: Users control via folder naming
- Reusable: This pattern extends to consolidated regional mounts later

---

## Architecture

### Union Filesystem Pattern

The patches FUSE mount implements an **overlay/union** pattern similar to Linux OverlayFS:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         REAL FILESYSTEM (Source)                                 │
├─────────────────────────────────────────────────────────────────────────────────┤
│  ~/.xearthlayer/patches/                                                         │
│  ├── A_KDEN_Mesh/                      # 'A' prefix - highest priority          │
│  │   ├── Earth nav data/+30-110/+39-105.dsf                                     │
│  │   └── terrain/24800_13648_USA_216.ter                                        │
│  ├── B_KLAX_Mesh/                      # 'B' prefix - second priority           │
│  │   ├── Earth nav data/+30-120/+33-119.dsf                                     │
│  │   └── terrain/94800_47888_BI18.ter                                           │
│  └── Z_SomeOther/                      # 'Z' prefix - lowest priority           │
│      └── ...                                                                     │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        │ FUSE Union Mount
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         VIRTUAL FILESYSTEM (FUSE)                                │
├─────────────────────────────────────────────────────────────────────────────────┤
│  Custom Scenery/zzyXEL_patches_ortho/                                            │
│  ├── Earth nav data/                                                             │
│  │   ├── +30-110/+39-105.dsf            # From A_KDEN_Mesh (first alphabetically)│
│  │   └── +30-120/+33-119.dsf            # From B_KLAX_Mesh                       │
│  ├── terrain/                                                                    │
│  │   ├── 24800_13648_USA_216.ter        # From A_KDEN_Mesh                       │
│  │   └── 94800_47888_BI18.ter           # From B_KLAX_Mesh                       │
│  └── textures/                                                                   │
│      └── (XEL generates on-demand via DdsHandler)                               │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Collision resolution**: When multiple patches contain the same file path, the patch with the alphabetically-first folder name wins (A < B < Z).

### Integration with Existing Pipeline

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              MountManager                                        │
├─────────────────────────────────────────────────────────────────────────────────┤
│  1. Mount patches folder (if patches.enabled && patches exist)                  │
│     └── zzyXEL_patches_ortho/  (higher priority mount)                          │
│  2. Mount regional packages (existing)                                           │
│     └── zzXEL_na_ortho, zzXEL_eu_ortho, etc.                                    │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         Fuse3UnionFS                                             │
├─────────────────────────────────────────────────────────────────────────────────┤
│  PatchUnionIndex: Merged index of all files across patches                      │
│                                                                                  │
│  lookup() / read():                                                              │
│  ├── DSF files  → passthrough from source patch folder                          │
│  ├── .ter files → passthrough from source patch folder                          │
│  └── .dds files → check cache, then generate via DdsHandler                     │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Data Structures

```rust
/// Merged index of all files across all patches.
pub struct PatchUnionIndex {
    /// Map from virtual path → (source_patch_name, real_path)
    /// Sorted by patch name so first match wins
    files: HashMap<PathBuf, (String, PathBuf)>,

    /// Virtual directory structure for readdir
    directories: HashMap<PathBuf, Vec<DirEntry>>,
}

impl PatchUnionIndex {
    /// Build index from all patches (sorted alphabetically by name)
    pub fn build(patches_dir: &Path) -> Result<Self> {
        let mut patches: Vec<_> = fs::read_dir(patches_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("Earth nav data").exists())
            .collect();

        // Sort by name (alphabetically) for priority
        patches.sort_by_key(|e| e.file_name());

        let mut index = Self::new();
        for patch in patches {
            index.add_patch(&patch.path())?;
        }
        Ok(index)
    }

    /// Add files from a patch, respecting existing entries (first wins)
    fn add_patch(&mut self, patch_path: &Path) -> Result<()> {
        for entry in WalkDir::new(patch_path) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(patch_path)?;

            // Only insert if not already present (first patch wins)
            if !self.files.contains_key(relative) {
                let patch_name = patch_path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                self.files.insert(
                    relative.to_path_buf(),
                    (patch_name, entry.path().to_path_buf())
                );
            }
        }
        Ok(())
    }

    /// Resolve virtual path to real file path
    pub fn resolve(&self, virtual_path: &Path) -> Option<&PathBuf> {
        self.files.get(virtual_path).map(|(_, real)| real)
    }
}
```

### FUSE Operations

```rust
impl Fuse3UnionFS {
    /// readdir: Return merged directory listing
    async fn readdir(&self, path: &Path) -> Vec<DirEntry> {
        self.index.directories.get(path).cloned().unwrap_or_default()
    }

    /// lookup: Check if file exists in union
    async fn lookup(&self, parent: &Path, name: &str) -> Option<FileAttr> {
        let virtual_path = parent.join(name);

        if let Some(real_path) = self.index.resolve(&virtual_path) {
            // Return attributes of real file
            return Some(self.get_attr(real_path).await?);
        }

        // For .dds files, may be virtual (generated on-demand)
        if name.ends_with(".dds") {
            return Some(self.create_virtual_dds_attr(name));
        }

        None
    }

    /// read: Passthrough or generate DDS
    async fn read(&self, path: &Path, offset: u64, size: u32) -> Vec<u8> {
        if let Some(real_path) = self.index.resolve(path) {
            // Passthrough read from real file
            return self.read_file(real_path, offset, size).await;
        }

        if path.extension() == Some("dds") {
            // Generate DDS on-demand using shared DdsHandler
            return self.dds_handler.request_dds(path).await;
        }

        vec![]
    }
}
```

---

## Configuration

### New Config Section: `[patches]`

```ini
[patches]
; Enable/disable patches functionality (default: true)
enabled = true

; Directory containing patch tiles (default: ~/.xearthlayer/patches)
directory = ~/.xearthlayer/patches
```

### New ConfigKeys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `patches.enabled` | bool | `true` | Enable patches folder mounting |
| `patches.directory` | path | `~/.xearthlayer/patches` | Location of patch tiles |

---

## Module Structure

### New Files

```
xearthlayer/src/
├── patches/
│   ├── mod.rs               # Module exports
│   ├── discovery.rs         # Patch folder discovery and validation
│   └── union_index.rs       # Merged file index with priority handling
├── fuse/fuse3/
│   └── union_fs.rs          # Union FUSE filesystem implementation (new)
└── ...

xearthlayer-cli/src/commands/
└── patches.rs               # CLI commands (list, validate, path) (new)

docs/
└── patches.md               # User documentation and workflow guide (new)
```

### Modified Files

| File | Changes |
|------|---------|
| `xearthlayer/src/lib.rs` | Add `pub mod patches;` |
| `xearthlayer/src/config/keys.rs` | Add `PatchesEnabled`, `PatchesDirectory` |
| `xearthlayer/src/config/file.rs` | Add `PatchesConfig` struct |
| `xearthlayer/src/fuse/fuse3/mod.rs` | Export `union_fs` module |
| `xearthlayer/src/manager/mounts.rs` | Mount patches before regional packages |
| `xearthlayer-cli/src/commands/run.rs` | Wire patches into mount flow |
| `xearthlayer-cli/src/commands/mod.rs` | Add patches subcommand |
| `docs/configuration.md` | Document patches config |

---

## Implementation Plan

### Phase 1: Configuration

1. Add `PatchesEnabled` and `PatchesDirectory` to ConfigKey enum
2. Add `PatchesConfig` struct to config/file.rs
3. Add `[patches]` section parsing
4. Update `config upgrade` to add new keys

**Files:**
- `xearthlayer/src/config/keys.rs`
- `xearthlayer/src/config/file.rs`

### Phase 2: Patch Discovery & Union Index

1. Create `xearthlayer/src/patches/` module
2. Implement `PatchDiscovery` to find and validate patch folders
3. Implement `PatchUnionIndex` to build merged file index
4. Handle priority via alphabetical folder name sorting

**Files:**
- `xearthlayer/src/patches/mod.rs`
- `xearthlayer/src/patches/discovery.rs`
- `xearthlayer/src/patches/union_index.rs`

### Phase 3: Union FUSE Filesystem

1. Create `Fuse3UnionFS` that wraps `PatchUnionIndex`
2. Implement `readdir` to return merged directory listings
3. Implement `lookup` to resolve files via union index
4. Implement `read` with passthrough for real files, DDS generation for textures
5. Reuse existing `DdsHandler` for texture generation

**Files:**
- `xearthlayer/src/fuse/fuse3/union_fs.rs` (new)
- `xearthlayer/src/fuse/fuse3/mod.rs`

### Phase 4: Mount Integration

1. Update `MountManager` to mount patches union FS before regional packages
2. Create mount point: `Custom Scenery/zzyXEL_patches_ortho/`
3. Wire DdsHandler and shared resources to union FS
4. Ensure patches only mount if patches directory exists and has content

**Files:**
- `xearthlayer/src/manager/mounts.rs`
- `xearthlayer-cli/src/commands/run.rs`

### Phase 5: CLI Commands

1. Add `xearthlayer patches list` - show installed patches with priority order
2. Add `xearthlayer patches validate` - check patch structure (Earth nav data/, terrain/)
3. Add `xearthlayer patches path` - show patches directory location

**Files:**
- `xearthlayer-cli/src/commands/patches.rs`
- `xearthlayer-cli/src/commands/mod.rs`

### Phase 6: Documentation

1. Document patch creation workflow (Ortho4XP + elevation data)
2. Document XEL-compatible Ortho4XP configuration
3. Document folder naming for priority control
4. Add patches section to configuration.md

**Files:**
- `docs/patches.md` (new)
- `docs/configuration.md`

---

## User Workflow

### Creating a Patch Tile

1. **Get addon's Ortho4XP patch data**
   - Elevation TIF: `!Ortho4XP_Patch/Elevations/+33-119.tif`
   - OSM patches: `!Ortho4XP_Patch/Patches/+30-120/+33-119/*.osm`

2. **Configure Ortho4XP for XEL compatibility**
   - Use addon's elevation data source
   - Build tile with standard Ortho4XP workflow

3. **Build tile in Ortho4XP**
   - Use addon's elevation data
   - Output: Complete tile with DSF, terrain, (optional textures)

4. **Install patch**
   ```bash
   mv ~/Ortho4XP/Tiles/+33-119/ ~/.xearthlayer/patches/KLAX_Mesh/
   ```

5. **Restart XEL**
   - Patch automatically mounted with higher priority
   - XEL generates textures using configured provider

---

## CLI Commands

```bash
# List installed patches with priority order
xearthlayer patches list

# Example output:
# Installed Patches (priority order):
#   1. A_KDEN_Mesh       - 1 DSF, 193 terrain files
#   2. B_KLAX_Mesh       - 1 DSF, 156 terrain files
#   3. Z_Custom_Airport  - 2 DSF, 342 terrain files

# Validate patch structure
xearthlayer patches validate [--name <patch_name>]

# Example output:
# Validating: B_KLAX_Mesh
#   ✓ Earth nav data/ found
#   ✓ DSF file found: +30-120/+33-119.dsf
#   ✓ terrain/ found (156 .ter files)
#   ✓ Valid Ortho4XP tile structure

# Show patches directory location
xearthlayer patches path

# Example output:
# ~/.xearthlayer/patches
```

---

## X-Plane Scenery Loading Order

XEL mount points are prefixed to control X-Plane's loading order:

| Prefix | Type | Example | Load Order |
|--------|------|---------|------------|
| `yz*` | Overlays | `yzXEL_na_overlay` | First (highest priority) |
| `zzy*` | **Patches** | `zzyXEL_patches_ortho` | Second |
| `zz*` | Orthos | `zzXEL_na_ortho` | Last (lowest priority) |

This ensures patches override regional ortho tiles, while overlays remain on top of everything.

---

## Testing Strategy

### Unit Tests

- `PatchUnionIndex::build()` correctly scans and indexes patches
- Alphabetical priority ordering works correctly
- Collision resolution (first wins) works as expected
- Empty patches directory handled gracefully
- Invalid patch structures detected and skipped

### Integration Tests

- Union FS correctly resolves files to source patches
- DSF and terrain files pass through correctly
- DDS requests are routed to DdsHandler
- Multiple patches merge correctly
- Mount/unmount lifecycle works

### Manual Testing

- End-to-end with X-Plane using KDEN or KLAX mesh patches
- Verify correct elevation at airport
- Verify consistent textures with surrounding scenery
- Verify no visual seams at tile boundaries

---

## Re-implementation with GeoIndex

When this feature returns, it will build on the infrastructure introduced in #51:

- **GeoIndex** (`PatchCoverage` layer) provides region ownership — replaces the removed `patched_regions` HashSet
- **`resolve_lazy_filtered()`** on `OrthoUnionIndex` enables source-aware file resolution
- **FUSE composition** (`is_geo_filtered()` + `resolve_lazy_geo()`) already handles the geospatial filtering

The main new work will be:

1. **DDS generation for patched regions** — Currently FUSE operates in pure passthrough mode for patched regions (no DDS generation). This feature re-enables generation using XEL's configured imagery provider when a `.dds` file is requested but doesn't exist on disk.
2. **A new GeoIndex layer** (e.g., `DynamicTextureCoverage`) to distinguish regions where XEL should generate textures from regions where it should only pass through.
3. **Texture seam handling** — Ensuring generated textures match adjacent XEL tiles.

## Future Considerations

### Hot Reload

Currently patches require restart to detect. Future enhancement could monitor patches directory for changes and rebuild the GeoIndex dynamically.

### Patch Validation Tool

A more comprehensive validation tool could:
- Verify DSF mesh integrity
- Check terrain file references
- Detect missing texture definitions
- Suggest fixes for common issues

---

## Revision History

| Date | Change |
|------|--------|
| 2026-01-07 | Initial design document |
| 2026-02-08 | Add region-level ownership: patches with DSF files own the 1x1 region, FUSE/prewarm/prefetch skip generation (#51) |
| 2026-02-17 | Renamed from `tile-patches.md`. Marked as planned — feature temporarily removed in favor of BYOS model with GeoIndex. Added re-implementation notes. |
