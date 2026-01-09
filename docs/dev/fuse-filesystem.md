# FUSE Virtual Filesystem Architecture

This document describes XEarthLayer's FUSE virtual filesystem implementation for serving X-Plane scenery with on-demand orthophoto generation.

## Current Implementation Status

The following features are **implemented and working**:

| Feature | Status | Description |
|---------|--------|-------------|
| PassthroughFS | ✅ Complete | FUSE filesystem that overlays existing scenery packs |
| UnionFS | ✅ Complete | Union filesystem for consolidated multi-source mounting |
| X-Plane Path Detection | ✅ Complete | Auto-detects X-Plane 12 install from reference file |
| On-demand DDS Generation | ✅ Complete | Downloads satellite imagery and encodes to BC1/BC3 DDS |
| Multi-provider Support | ✅ Complete | Bing Maps, Google GO2, Google Maps API |
| Two-tier Caching | ✅ Complete | Memory + disk cache for generated textures |
| Async Pipeline | ✅ Complete | Request coalescing, parallel downloads, async encoding |
| Graceful Shutdown | ✅ Complete | Auto-unmount on SIGTERM/SIGINT |
| Package Management | ✅ Complete | Install, update, remove regional packages |
| Consolidated Mounting | ✅ Complete | Single mount for all sources (v0.2.11+) |
| Tile Patches | ✅ Complete | Custom mesh/elevation from airport addons |
| Overlay Support | ✅ Complete | Consolidated overlay symlinks |
| Predictive Prefetch | ✅ Complete | Radial prefetcher with X-Plane telemetry |
| Scenery Index | ✅ Complete | Fast tile coordinate lookup across packages |

## Overview

XEarthLayer creates virtual filesystem mounts that appear as standard X-Plane scenery folders. The FUSE layer intercepts file requests and either:
1. **Passes through** real files from the scenery pack
2. **Generates on-demand** orthophoto DDS textures
3. **Returns not found** for unrecognized resources

This allows distributing compact scenery packs (DSF + terrain files) without the massive storage requirements of pre-rendered orthophotos.

## Consolidated Mounting (v0.2.11+)

Starting with v0.2.11, XEarthLayer uses **consolidated mounting** - a single FUSE mount that combines all ortho sources (patches + regional packages) into one unified view.

### Benefits

| Before (≤0.2.10) | After (≥0.2.11) |
|------------------|-----------------|
| N mounts for N packages | Single `zzXEL_ortho` mount |
| Separate patches mount | Patches integrated into main mount |
| Per-region overlays | Single `yzXEL_overlay` folder |
| N FUSE sessions | Single shared FUSE session |
| N XEarthLayerService instances | Single shared service |

### Mount Points

| Mount | Type | Description | Priority |
|-------|------|-------------|----------|
| `yzXEL_overlay` | Symlinks | Consolidated overlay DSFs | Highest (loaded first) |
| `zzXEL_ortho` | FUSE | All ortho sources (patches + packages) | Lowest (loaded last) |

### Source Priority

Within the consolidated mount, file resolution follows **alphabetical folder name** ordering:

```
Priority (highest to lowest):
1. _patches/A_KDEN_Mesh/    ('_' prefix sorts first)
2. _patches/B_KLAX_Mesh/
3. eu/                      (alphabetically: eu < na < sa)
4. na/
5. sa/
```

Patches automatically have highest priority because their internal `_patches/` prefix sorts before region codes.

## Regional Scenery Packages

XEarthLayer distributes scenery in regional packages. Each region includes both ortho and overlay components.

### Available Regions

| Region | Code | Description |
|--------|------|-------------|
| North America | `na` | Continental US, Canada, Mexico |
| Europe | `eu` | European continent |
| South America | `sa` | South American continent |

### Package Structure

Each regional package follows standard X-Plane scenery structure:

```
na/
├── ortho/
│   ├── Earth nav data/
│   │   └── +30-120/
│   │       ├── +33-119.dsf
│   │       └── ...
│   └── terrain/
│       ├── 94800_47888_BI18.ter
│       └── ...
└── overlay/
    └── Earth nav data/
        └── +30-120/
            ├── +33-119.dsf
            └── ...
```

**Included in ortho packages:**
- DSF files (mesh, airports, objects)
- Terrain files (.ter)
- Water masks and transitions (.png)

**NOT included in ortho packages:**
- Orthophoto textures (.dds) - generated on-demand

**Included in overlay packages:**
- DSF files with road networks, railways, powerlines, forest placement

## Overlays

Overlays restore roads, railways, powerlines, and forests that are excluded from orthophoto tiles.

### Why Overlays Are Needed

Orthophoto imagery shows the ground as photographed from satellites, but X-Plane needs vector data for:
- **Roads** - For AI traffic and visual accuracy
- **Railways** - Track routing and visual rendering
- **Powerlines** - Visual scenery elements
- **Forests** - 3D tree placement and autogen

Without overlays, orthophoto scenery would show roads in the imagery but lack the 3D elements X-Plane expects.

### Consolidated Overlays

All overlay packages are combined into a single `yzXEL_overlay` folder:

```
X-Plane 12/Custom Scenery/yzXEL_overlay/
└── Earth nav data/
    ├── +30-120/           <- From na/overlay
    │   ├── +33-119.dsf
    │   └── ...
    ├── +40+000/           <- From eu/overlay
    │   ├── +48+002.dsf
    │   └── ...
    └── ...
```

The `yz` prefix ensures overlays load **before** the `zz` orthophoto mount (alphabetically), which is required for proper rendering - overlay elements draw on top of the orthophoto terrain.

## Tile Patches

Tile patches allow using custom mesh/elevation data from third-party airport addons. See [patches.md](../patches.md) for full documentation.

### How Patches Integrate

Patches are stored in `~/.xearthlayer/patches/` and automatically included in the consolidated mount with highest priority:

```
zzXEL_ortho (consolidated mount)
├── _patches/A_KLAX_Mesh/   <- Highest priority
├── _patches/B_KDEN_Mesh/
├── eu/                      <- Regional packages
├── na/
└── sa/
```

## Request Routing

```
┌─────────────────────────────────────────────────────────────────┐
│                     X-Plane File Request                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  FUSE Intercept │
                    └─────────────────┘
                              │
                              ▼
                 ┌────────────────────────┐
                 │  OrthoUnionIndex       │
                 │  Resolve source        │
                 └────────────────────────┘
                              │
                              ▼
                 ┌────────────────────────┐
                 │ File exists in source? │
                 └────────────────────────┘
                    │              │
                   Yes             No
                    │              │
                    ▼              ▼
            ┌──────────────┐  ┌─────────────────────┐
            │  Passthrough │  │ DDS orthophoto      │
            │  (serve file)│  │ pattern match?      │
            └──────────────┘  └─────────────────────┘
                                  │            │
                                 Yes           No
                                  │            │
                                  ▼            ▼
                          ┌─────────────┐  ┌─────────┐
                          │  Generate   │  │ ENOENT  │
                          │  on-demand  │  │ (not    │
                          │  via async  │  │ found)  │
                          │  pipeline   │  └─────────┘
                          └─────────────┘
```

### File Type Handling

| File Type | Pattern | Handling |
|-----------|---------|----------|
| DSF | `*.dsf` | Passthrough from source |
| Terrain | `*.ter` | Passthrough from source |
| Water masks | `water_*.png` | Passthrough from source |
| Other PNG | `*.png` | Passthrough if exists, else ENOENT |
| Orthophoto DDS | `{row}_{col}_{maptype}{zoom}.dds` | Generate on-demand |
| Other DDS | `*.dds` | ENOENT (not an orthophoto) |
| Unknown | `*` | Passthrough if exists, else ENOENT |

### DDS Pattern Recognition

Orthophoto DDS files follow the Ortho4XP naming convention:

```
{row}_{col}_{maptype}{zoom}.dds
```

**Examples:**
- `100000_125184_BI18.dds` - Valid orthophoto, generate on-demand
- `airplane_texture.dds` - Not an orthophoto pattern, return ENOENT
- `normals_100000_125184.dds` - Not an orthophoto pattern, return ENOENT

The regex pattern for recognition:
```regex
(\d+)_(\d+)_([A-Za-z]+)(\d{2})\.dds$
```

## FUSE Operations

### Directory Operations

| Operation | Behavior |
|-----------|----------|
| `readdir` | Union of all sources, with priority ordering |
| `getattr` | Return attributes from winning source |
| `lookup` | Resolve via OrthoUnionIndex, pattern match for DDS |

### File Operations

| Operation | Behavior |
|-----------|----------|
| `open` | For DDS: trigger async pipeline; For others: open from source |
| `read` | For DDS: serve from cache/generated; For others: read from source |
| `release` | Clean up handles |

### Synthesized DDS Attributes

For on-demand DDS files, we synthesize file attributes:

```rust
FileAttr {
    size: expected_dds_size,  // Based on encoder settings
    kind: RegularFile,
    perm: 0o444,              // Read-only
    mtime: pack_mtime,        // Use pack modification time
    ...
}
```

## Package Management

### CLI Commands

| Command | Description |
|---------|-------------|
| `xearthlayer packages list` | List available and installed packages |
| `xearthlayer packages install <region>` | Download and install a region |
| `xearthlayer packages update [region]` | Update installed packages |
| `xearthlayer packages remove <region>` | Remove a package |
| `xearthlayer packages check` | Check for available updates |

Installing a region (e.g., `na`) downloads both ortho and overlay components.

### Package Distribution

Packages are distributed as split archives:
- Format: `.tar.gz` split into parts
- Hosted on: GitHub Releases
- Versioning: Semantic versioning per region
- Discovery: Library index at configured URL

## Storage Locations

### Package Location (Configurable)

```
Default: ~/.xearthlayer/packages/
         ├── na/
         │   ├── ortho/
         │   └── overlay/
         ├── eu/
         │   ├── ortho/
         │   └── overlay/
         └── ...
```

### Patches Location

```
~/.xearthlayer/patches/
├── A_KLAX_Mesh/
│   ├── Earth nav data/
│   └── terrain/
└── B_KDEN_Mesh/
    └── ...
```

### Mount Points (X-Plane Custom Scenery)

```
X-Plane 12/Custom Scenery/
├── Global Airports/              <- Real folder (X-Plane default)
├── yzXEL_overlay/                <- Symlink folder (consolidated overlays)
└── zzXEL_ortho/                  <- FUSE mount (consolidated ortho)
```

## Mount Lifecycle

### Startup Sequence

```
1. Read configuration
2. Discover installed packages from packages directory
3. Discover patches from patches directory
4. Build OrthoUnionIndex (patches + packages, sorted)
5. Create shared XEarthLayerService (cache, pipeline, limiters)
6. Mount Fuse3OrthoUnionFS at zzXEL_ortho
7. Create consolidated overlay symlinks at yzXEL_overlay
8. Start prefetch listener if telemetry enabled
9. Display dashboard and wait for Ctrl+C
```

### Shutdown Sequence

```
1. Signal shutdown (Ctrl+C received)
2. Stop prefetch listener
3. Unmount FUSE filesystem
4. Flush pending cache writes
5. Clean up resources
6. Remove overlay symlinks (optional)
```

### Runtime Management

```bash
# Primary command - mount all installed packages
xearthlayer run

# With pre-warming around an airport
xearthlayer run --airport KJFK

# Override provider for this session
xearthlayer run --provider go2

# Advanced: mount single scenery pack (debugging)
xearthlayer start --source /path/to/scenery
```

## Implementation Modules

```
xearthlayer/src/
├── fuse/
│   ├── mod.rs              # Module exports
│   ├── fuse3/              # Async multi-threaded FUSE (primary)
│   │   ├── mod.rs          # Fuse3 module exports
│   │   ├── passthrough_fs.rs   # Single-source passthrough
│   │   ├── union_fs.rs         # Multi-source union (patches)
│   │   └── ortho_union_fs.rs   # Consolidated ortho mount
│   ├── filename.rs         # DDS filename parsing
│   └── placeholder.rs      # Magenta placeholder generation
├── ortho_union/
│   ├── mod.rs              # Module exports
│   ├── index.rs            # OrthoUnionIndex (tile → source mapping)
│   └── source.rs           # OrthoSource, SourceType
├── patches/
│   ├── mod.rs              # Module exports
│   ├── discovery.rs        # Patch directory scanning
│   └── index.rs            # PatchUnionIndex
├── service/
│   ├── mod.rs              # Module exports
│   ├── facade.rs           # High-level service API
│   ├── dds_handler.rs      # DDS request handling
│   └── fuse_mount.rs       # FUSE mount management
├── manager/
│   ├── mod.rs              # Module exports
│   ├── mounts.rs           # MountManager for consolidated mounting
│   └── symlinks.rs         # Overlay symlink management
├── pipeline/
│   ├── mod.rs              # Module exports
│   ├── runner.rs           # DDS pipeline with coalescing
│   ├── coalesce.rs         # Request coalescing
│   └── stages/             # Pipeline stages (download, assemble, encode)
├── prefetch/
│   ├── mod.rs              # Module exports
│   ├── strategy.rs         # Prefetcher trait
│   ├── radial.rs           # RadialPrefetcher (recommended)
│   └── telemetry.rs        # X-Plane UDP telemetry
├── config/                 # Configuration system
├── cache/                  # Two-tier caching
├── provider/               # Imagery providers
└── texture/                # DDS encoding
```

### Key Types

| Type | Module | Purpose |
|------|--------|---------|
| `Fuse3OrthoUnionFS` | `fuse/fuse3/ortho_union_fs.rs` | Consolidated FUSE filesystem |
| `OrthoUnionIndex` | `ortho_union/index.rs` | Maps files to sources with priority |
| `MountManager` | `manager/mounts.rs` | Orchestrates consolidated mounting |
| `DdsHandler` | `service/dds_handler.rs` | Async DDS generation pipeline |
| `RadialPrefetcher` | `prefetch/radial.rs` | Predictive tile caching |

## Error Handling

### Runtime Errors

| Error | Handling |
|-------|----------|
| DDS generation fails | Return magenta placeholder texture |
| Network timeout | Return cached version if available, else placeholder |
| Source file missing | Check next source in priority order |
| Disk full | Log warning, evict cache entries |

### Graceful Degradation

XEarthLayer prioritizes availability:
1. Always return something for valid orthophoto requests (placeholder if needed)
2. Never crash the FUSE mount due to transient errors
3. Log errors for debugging but don't block X-Plane

## Performance Considerations

### Caching Strategy

See [cache-design.md](cache-design.md) for detailed caching architecture.

Summary:
1. Memory cache for hot tiles (2GB default)
2. Disk cache for generated DDS files (20GB default)
3. Request coalescing prevents duplicate work

### Async Pipeline

The DDS generation pipeline is fully async:
- HTTP downloads via tokio + reqwest
- Request coalescing via broadcast channels
- CPU-bound work (encode) via spawn_blocking
- Semaphore-based concurrency limiting

### Concurrency Limiting

| Resource | Limiter | Scaling |
|----------|---------|---------|
| HTTP | `ConcurrencyLimiter` | `min(cpus * 16, 256)` |
| CPU (assemble + encode) | `CPUConcurrencyLimiter` | `max(cpus * 1.25, cpus + 2)` |
| Disk I/O | `StorageConcurrencyLimiter` | Auto-detected by storage type |

## Security Considerations

### Mount Permissions

- FUSE mounts are user-space, no root required (with fuse group membership)
- Mount points are read-only to X-Plane
- Source files are read-only

### Network Access

- Orthophoto downloads only from configured providers
- HTTPS required for all downloads
- No execution of downloaded content

## Compatibility

### X-Plane Versions

- X-Plane 11: Supported
- X-Plane 12: Supported (primary target)

### Operating Systems

- Linux: Native FUSE support (✅ fully working)
- macOS: Requires macFUSE (⏳ planned)
- Windows: Requires WinFSP (⏳ planned)

### Scenery Compatibility

XEarthLayer manages its own scenery packages created with Ortho4XP. Third-party patches (custom airport mesh) are supported via the patches system.
