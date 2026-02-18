# Scenery Patches (Bring Your Own Scenery)

**Status**: Implemented
**Created**: 2026-02-17
**Issue**: #51

## Overview

Scenery patches allow users to install pre-built ortho scenery (from Ortho4XP or similar tools) alongside XEarthLayer's regional packages. Patches provide complete scenery — DSF mesh, terrain definitions, and pre-built DDS textures — for specific airport areas. XEarthLayer serves patch files via pure passthrough and uses the [GeoIndex](geo-index-design.md) to ensure package files in the same geographic region are hidden.

This is a "bring your own scenery" model: patches must include their own textures. XEarthLayer does not generate textures for patched regions. For the planned future capability where XEL generates textures dynamically for patches, see [Airport Scenery Integration](airport-scenery-integration.md).

### How It Works

1. User places complete Ortho4XP tile folders in the patches directory (`~/.xearthlayer/patches/`)
2. At startup, XEL scans patch folders for DSF files to determine which 1x1 regions are covered
3. These regions are registered in GeoIndex as `PatchCoverage` entries
4. FUSE serves patch files via passthrough and hides package files in the same regions
5. Prewarm and prefetch skip patched regions entirely (no DDS generation needed)

### Why Patches Need Region-Level Ownership

Patches and packages use **different filenames** for the same geographic area. A patch might contain `terrain/1234_5678_GO218.ter` while a package has `terrain/1234_5678_BI16.ter` — both covering DSF region `+45+011`. Filename-based merging cannot detect this overlap.

Without region ownership, X-Plane loads the package's `.ter` file, which references a `.dds` texture that FUSE won't generate (the region is patched). This causes X-Plane to crash with missing texture errors.

The solution: GeoIndex tracks which regions are owned by patches. FUSE uses this to hide **all** package files in patched regions, regardless of filename.

---

## Architecture

### Data Flow

```
Startup:
  MountManager
    └── build OrthoUnionIndex (patches + packages)
    └── scan patch sources for DSF regions
    └── geo_index.populate::<PatchCoverage>(regions)
    └── pass Arc<GeoIndex> to FUSE, prewarm, prefetch

FUSE read (e.g., terrain/1234_5678_GO218.ter):
  Fuse3OrthoUnionFS
    ├── is_geo_filtered("1234_5678_GO218.ter")?
    │   └── parse filename → DsfRegion(+45, +011)
    │   └── geo_index.contains::<PatchCoverage>(region) → true
    │
    ├── Yes (patched region):
    │   └── resolve_lazy_filtered(path, |source| source.is_patch())
    │   └── serves file from patch source only
    │
    └── No (normal region):
        └── resolve_lazy(path)
        └── serves file from any source (normal priority)

FUSE read (.dds in patched region):
  ├── is_geo_filtered() → true
  ├── resolve_lazy_filtered() → finds real .dds in patch folder → passthrough
  └── if .dds NOT found in patch → ENOENT (no generation, no fallback to package)
```

### Component Responsibilities

```
GeoIndex                          OrthoUnionIndex
──────────────                    ──────────────────
"Is +45+011 patched?"             "Where is GO218.ter on disk?"
contains::<PatchCoverage>()       resolve_lazy_filtered(predicate)
                    \               /
                     \             /
                      ↓           ↓
               FUSE (composition)
               ──────────────────
               is_geo_filtered() → GeoIndex
               resolve_lazy_geo() → OrthoUnionIndex
```

### Prewarm and Prefetch

Both systems query GeoIndex independently to skip patched regions:

```rust
// Before submitting tiles for generation:
if geo_index.contains::<PatchCoverage>(&DsfRegion::new(lat, lon)) {
    // Skip — patch provides its own textures
}
```

This prevents wasted work generating DDS textures that will never be served (the patch's own textures take precedence).

---

## Patch Directory Structure

Patches live in `~/.xearthlayer/patches/` (configurable via `patches.directory`). Each subfolder is a complete Ortho4XP tile output:

```
~/.xearthlayer/patches/
├── A_KDEN_Mesh/                        # 'A' prefix → highest priority
│   ├── Earth nav data/
│   │   └── +30-110/
│   │       └── +39-105.dsf             # Custom mesh for KDEN area
│   ├── terrain/
│   │   ├── 24800_13648_USA_216.ter
│   │   └── ...
│   └── textures/
│       ├── 24800_13648_USA_216.dds     # Pre-built textures (REQUIRED)
│       └── ...
│
└── B_KLAX_Mesh/                        # 'B' prefix → second priority
    ├── Earth nav data/
    │   └── +30-120/
    │       └── +33-119.dsf
    ├── terrain/
    │   └── ...
    └── textures/
        └── ...                          # Pre-built textures (REQUIRED)
```

### Key Requirements

- **DSF files are mandatory** — they define which 1x1 regions the patch covers. Without DSF files, GeoIndex has no region data and package files won't be hidden.
- **Textures are mandatory** — XEL does not generate DDS textures for patched regions. Missing textures will result in ENOENT errors.
- **Folder naming controls priority** — alphabetical ordering determines which patch wins when multiple patches cover the same file. Prefix with `A_`, `B_`, etc.

---

## GeoIndex Integration

### Population

During startup, `MountManager` scans patch sources for DSF files using `extract_dsf_regions()`:

```rust
for source in index.sources() {
    if source.is_patch() {
        for (lat, lon) in extract_dsf_regions(&source.source_path) {
            entries.push((
                DsfRegion::new(lat, lon),
                PatchCoverage { patch_name: source.display_name.clone() },
            ));
        }
    }
}
geo_index.populate(entries);  // Atomic bulk load
```

### Query Pattern

All consumers use the same pattern:

```rust
geo_index.contains::<PatchCoverage>(&DsfRegion::new(lat, lon))
```

This returns `true` if any patch provides a DSF file covering the given 1x1 region.

---

## Configuration

```ini
[patches]
; Enable/disable patches functionality (default: true)
enabled = true

; Directory containing patch tiles (default: ~/.xearthlayer/patches)
directory = ~/.xearthlayer/patches
```

---

## Limitations

### No Dynamic Texture Generation

In the current BYOS model, XEL does **not** generate DDS textures for patched regions. If a `.dds` file referenced by a patch's `.ter` file doesn't exist in the patch folder, X-Plane will see a missing texture. Users must ensure patches include all required textures.

This limitation will be addressed by the planned [Airport Scenery Integration](airport-scenery-integration.md) feature, which will re-enable dynamic texture generation for patches.

### Region Granularity

Ownership is tracked at 1x1 DSF region granularity. If a patch provides a DSF for region `+45+011`, **all** package files in that entire 1x1 region are hidden — even if the patch only covers a small airport area within the region. This is consistent with how X-Plane's scenery system works (DSF files are 1x1).

### No Hot Reload

Patches are scanned at startup only. Adding or removing patches requires restarting XEL.

---

## Related Documents

| Document | Description |
|----------|-------------|
| [GeoIndex Design](geo-index-design.md) | Thread-safe geospatial reference database |
| [Airport Scenery Integration](airport-scenery-integration.md) | Planned: dynamic texture generation for patches (0.3.x) |
| [Consolidated Mounting](consolidated-mounting-design.md) | Single FUSE mount architecture |

---

## Revision History

| Date | Change |
|------|--------|
| 2026-02-17 | Initial document — describes BYOS patch model with GeoIndex region ownership (#51) |
