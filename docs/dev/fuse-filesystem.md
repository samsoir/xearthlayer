# FUSE Virtual Filesystem Architecture

This document describes XEarthLayer's FUSE virtual filesystem implementation for serving X-Plane scenery with on-demand orthophoto generation.

## Current Implementation Status

The following features are **implemented and working**:

| Feature | Status | Description |
|---------|--------|-------------|
| PassthroughFS | ✅ Complete | FUSE filesystem that overlays existing scenery packs |
| X-Plane Path Detection | ✅ Complete | Auto-detects X-Plane 12 install from reference file |
| On-demand DDS Generation | ✅ Complete | Downloads Bing imagery and encodes to BC1/BC3 DDS |
| CLI `mount` Command | ✅ Complete | Mount scenery packs with optional auto-detection |
| Two-tier Caching | ✅ Complete | Memory + disk cache for generated textures |
| Parallel Tile Generation | ✅ Complete | Thread pool with request coalescing |
| Graceful Shutdown | ✅ Complete | Auto-unmount on SIGTERM/SIGINT |
| INI Configuration | ✅ Complete | `~/.xearthlayer/config.ini` |

**Not yet implemented** (planned for future releases):
- Pack management CLI (`pack install`, `pack update`)
- Overlay pack support (`y_xel_*` packs)
- Multi-region simultaneous mounting

## Overview

XEarthLayer creates virtual filesystem mounts that appear as standard X-Plane scenery folders. The FUSE layer intercepts file requests and either:
1. **Passes through** real files from the scenery pack
2. **Generates on-demand** orthophoto DDS textures
3. **Returns not found** for unrecognized resources

This allows distributing compact scenery packs (DSF + terrain files) without the massive storage requirements of pre-rendered orthophotos.

## Regional Scenery Packs

XEarthLayer distributes scenery in continent-based regional packs:

| Region | Pack Name | Mount Point |
|--------|-----------|-------------|
| Europe | `z_xel_europe` | `/Custom Scenery/z_xel_europe/` |
| Africa | `z_xel_africa` | `/Custom Scenery/z_xel_africa/` |
| North America | `z_xel_north_america` | `/Custom Scenery/z_xel_north_america/` |
| South America | `z_xel_south_america` | `/Custom Scenery/z_xel_south_america/` |
| Asia | `z_xel_asia` | `/Custom Scenery/z_xel_asia/` |
| Australia | `z_xel_australia` | `/Custom Scenery/z_xel_australia/` |
| Antarctica | `z_xel_antarctica` | `/Custom Scenery/z_xel_antarctica/` |

The `z_` prefix ensures these packs load after default scenery in X-Plane's alphabetical loading order.

## Overlay Packs

Overlays restore roads, railways, powerlines, and forests that are excluded from orthophoto tiles. Each regional scenery pack has a corresponding overlay pack.

### Why Overlays Are Needed

Orthophoto imagery shows the ground as photographed from satellites, but X-Plane needs vector data for:
- **Roads** - For AI traffic and visual accuracy
- **Railways** - Track routing and visual rendering
- **Powerlines** - Visual scenery elements
- **Forests** - 3D tree placement and autogen

Without overlays, orthophoto scenery would show roads in the imagery but lack the 3D elements X-Plane expects.

### Overlay Pack Structure

| Region | Overlay Pack Name | Mount Point |
|--------|-------------------|-------------|
| Europe | `y_xel_europe_overlays` | `/Custom Scenery/y_xel_europe_overlays/` |
| Africa | `y_xel_africa_overlays` | `/Custom Scenery/y_xel_africa_overlays/` |
| North America | `y_xel_north_america_overlays` | `/Custom Scenery/y_xel_north_america_overlays/` |
| South America | `y_xel_south_america_overlays` | `/Custom Scenery/y_xel_south_america_overlays/` |
| Asia | `y_xel_asia_overlays` | `/Custom Scenery/y_xel_asia_overlays/` |
| Australia | `y_xel_australia_overlays` | `/Custom Scenery/y_xel_australia_overlays/` |
| Antarctica | `y_xel_antarctica_overlays` | `/Custom Scenery/y_xel_antarctica_overlays/` |

The `y_` prefix ensures overlays load **before** the `z_` orthophoto packs (alphabetically), which is required for proper rendering - overlay elements draw on top of the orthophoto terrain.

### Overlay Contents

Overlay packs contain only DSF files with vector overlay data:

```
y_xel_europe_overlays/
└── Earth nav data/
    ├── +40-010/
    │   ├── +40-001.dsf
    │   ├── +40-002.dsf
    │   └── ...
    ├── +50+000/
    │   ├── +50+000.dsf
    │   └── ...
    └── ...
```

**Included in overlay packs:**
- DSF files with road networks, railways, powerlines, forest placement

**NOT included:**
- Terrain files (.ter) - these are in the main orthophoto pack
- Textures - overlays use X-Plane's built-in textures

### Regional Scope

Unlike AutoOrtho which provides a single global overlay pack, XEarthLayer provides **region-specific overlays**. This is an improvement because:

1. **No conflicts** - Only overlay data for regions you're using is loaded
2. **Smaller downloads** - Download only the overlays you need
3. **Faster loading** - X-Plane doesn't scan unnecessary DSF files
4. **Cleaner organization** - Each region is self-contained

### Overlay FUSE Mounts

Overlay packs use FUSE mounts (not symlinks) for clean lifecycle management:

- **When XEarthLayer starts**: Overlay mounts appear in Custom Scenery
- **When XEarthLayer stops**: Overlay mounts disappear cleanly
- **No artifacts**: X-Plane's scenery folder stays clean when XEarthLayer isn't running

Since overlay packs contain only static DSF files, the FUSE mount is pure passthrough - no on-demand generation is needed.

### Load Order

X-Plane loads scenery alphabetically. XEarthLayer's naming ensures correct order:

```
Custom Scenery/
├── Global Airports/              # X-Plane default
├── y_xel_europe_overlays/        # 1. Overlays load first (y_)
├── y_xel_asia_overlays/          #    Roads, railways, forests
├── z_xel_europe/                 # 2. Ortho packs load second (z_)
├── z_xel_asia/                   #    Terrain with orthophotos
└── ...
```

This order ensures overlay elements (roads, trees) render on top of the orthophoto terrain.

### Pack Structure

Each regional pack follows standard X-Plane scenery structure:

```
z_xel_europe/
├── Earth nav data/
│   └── +40-010/
│       ├── +39-009.dsf
│       ├── +39-008.dsf
│       └── ...
├── terrain/
│   ├── 100000_125184_BI18.ter
│   ├── 100016_125184_BI18.ter
│   └── ...
└── textures/
    ├── water_transition.png
    ├── water_mask_100000_125184.png
    └── ...
```

**Included in packs:**
- DSF files (mesh, airports, objects)
- Terrain files (.ter)
- Water masks and transitions (.png)

**NOT included in packs:**
- Orthophoto textures (.dds) - generated on-demand

## Storage Locations

### Source Pack Location (Configurable)

Users can configure where scenery packs are stored:

```
Default: ~/.xearthlayer/scenery_packs/
         ├── y_xel_europe_overlays/
         ├── y_xel_africa_overlays/
         ├── z_xel_europe/
         ├── z_xel_africa/
         └── ...

Custom:  /mnt/large_drive/xearthlayer/scenery_packs/
         ├── y_xel_europe_overlays/
         ├── z_xel_europe/
         └── ...
```

This flexibility allows users with limited SSD space to store packs on larger drives.

### Mount Points (X-Plane Custom Scenery)

FUSE mounts appear in X-Plane's Custom Scenery folder:

```
X-Plane 12/Custom Scenery/
├── Global Airports/              <- Real folder
├── y_xel_europe_overlays/        <- FUSE mount (overlays)
├── y_xel_asia_overlays/          <- FUSE mount (overlays)
├── z_xel_europe/                 <- FUSE mount (ortho)
├── z_xel_asia/                   <- FUSE mount (ortho)
└── ...
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
                 │ File exists in pack?   │
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
                          │  via tile   │  │ found)  │
                          │  pipeline   │  └─────────┘
                          └─────────────┘
```

### File Type Handling

| File Type | Pattern | Handling |
|-----------|---------|----------|
| DSF | `*.dsf` | Passthrough from pack |
| Terrain | `*.ter` | Passthrough from pack |
| Water masks | `water_*.png` | Passthrough from pack |
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
| `readdir` | List real files from pack + synthesize DDS entries from .ter references |
| `getattr` | Return real attributes for pack files, synthesized attributes for DDS |
| `lookup` | Check pack first, then pattern match for DDS |

### File Operations

| Operation | Behavior |
|-----------|----------|
| `open` | For DDS: trigger generation pipeline; For others: open real file |
| `read` | For DDS: serve from cache/generated; For others: read real file |
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

## Pack Management

### User Configuration

XEarthLayer uses INI configuration at `~/.xearthlayer/config.ini`. See [configuration.md](../configuration.md) for full reference.

Example configuration excerpt:
```ini
[xplane]
; Auto-detected from ~/.x-plane/x-plane_install_12.txt if not specified
; scenery_dir = /path/to/X-Plane 12/Custom Scenery

[cache]
memory_size = 2GB
disk_size = 20GB
```

### Pack Operations

| Operation | Description |
|-----------|-------------|
| `xearthlayer pack list` | List available and installed packs |
| `xearthlayer pack install <region>` | Download and install ortho + overlay packs for a region |
| `xearthlayer pack update <region>` | Update installed packs for a region |
| `xearthlayer pack remove <region>` | Remove ortho + overlay packs for a region |
| `xearthlayer pack status` | Show status of all packs |

Installing a region (e.g., `europe`) downloads and installs both:
- `z_xel_europe` (orthophoto pack)
- `y_xel_europe_overlays` (overlay pack)

### Pack Distribution

Packs are distributed as compressed archives:
- Format: `.tar.zst` (tar with Zstandard compression)
- Hosted on: GitHub Releases or dedicated CDN
- Versioning: Semantic versioning per region
- Each region has two archives: `z_xel_<region>.tar.zst` and `y_xel_<region>_overlays.tar.zst`

## Mount Lifecycle

### Startup Sequence

```
1. Read configuration
2. For each enabled region:
   a. Verify overlay pack exists at configured location
   b. Create FUSE mount for overlay at X-Plane Custom Scenery (y_xel_*)
   c. Verify ortho pack exists at configured location
   d. Create FUSE mount for ortho at X-Plane Custom Scenery (z_xel_*)
   e. Initialize tile generator for on-demand DDS
3. Start background services (cache cleanup, etc.)
```

### Shutdown Sequence

```
1. Signal shutdown to all mounts
2. Flush pending writes to cache
3. Unmount all FUSE filesystems (ortho and overlay mounts)
4. Clean up resources
5. X-Plane Custom Scenery folder is left clean (no XEarthLayer artifacts)
```

### Runtime Management

```bash
# Start with a scenery pack using auto-detected X-Plane path
xearthlayer start --source /path/to/scenery/z_xel_europe

# Start with explicit mountpoint
xearthlayer start \
  --source /path/to/scenery/z_xel_europe \
  --mountpoint "/path/to/X-Plane 12/Custom Scenery/z_xel_europe"

# Start without caching (for testing)
xearthlayer start --source /path/to/scenery/z_xel_test --no-cache

# Unmount (use fusermount on Linux/macOS)
fusermount -u "/path/to/X-Plane 12/Custom Scenery/z_xel_europe"
```

**X-Plane Path Detection:**

When `--mountpoint` is omitted, XEarthLayer automatically detects the X-Plane 12 installation:
- **Linux/macOS**: Reads `~/.x-plane/x-plane_install_12.txt`
- **Windows**: Reads `%LOCALAPPDATA%\x-plane\x-plane_install_12.txt`

The mountpoint is derived as: `{X-Plane 12}/Custom Scenery/{source_pack_name}`

## Error Handling

### Pack Errors

| Error | Handling |
|-------|----------|
| Pack not found | Log error, skip mount, continue with others |
| Pack corrupted | Log error, skip mount, suggest re-download |
| Mount point busy | Log error, skip mount |

### Runtime Errors

| Error | Handling |
|-------|----------|
| DDS generation fails | Return magenta placeholder texture |
| Network timeout | Return cached version if available, else placeholder |
| Disk full | Log warning, evict cache entries |

### Graceful Degradation

XEarthLayer prioritizes availability:
1. Always return something for valid orthophoto requests (placeholder if needed)
2. Never crash the FUSE mount due to transient errors
3. Log errors for debugging but don't block X-Plane

## Implementation Modules

```
xearthlayer/src/
├── fuse/
│   ├── mod.rs           # Module exports
│   ├── filesystem.rs    # Pure virtual filesystem (textures/ directory only)
│   ├── filename.rs      # DDS filename parsing (row_col_maptypeZZ.dds)
│   ├── passthrough.rs   # Passthrough + on-demand DDS (IMPLEMENTED)
│   └── placeholder.rs   # Magenta placeholder generation
├── config/
│   ├── mod.rs           # Module exports
│   ├── download.rs      # Download configuration
│   ├── texture.rs       # Texture encoding configuration
│   └── xplane.rs        # X-Plane path detection (IMPLEMENTED)
├── service/
│   ├── mod.rs           # Module exports
│   ├── config.rs        # Service configuration
│   ├── error.rs         # Service errors
│   └── facade.rs        # High-level service API (serve, serve_passthrough)
├── tile/                # Tile generation pipeline
├── orchestrator/        # Chunk download orchestration
├── texture/             # DDS texture encoding
├── cache/               # Two-tier caching system
└── provider/            # Satellite imagery providers (Bing, Google)
```

### FUSE Filesystem Types

XEarthLayer currently implements two FUSE filesystem types:

| Type | Module | Purpose | On-demand Generation |
|------|--------|---------|---------------------|
| Virtual | `filesystem.rs` | Pure virtual textures/ directory | Yes (all DDS files) |
| Passthrough | `passthrough.rs` | Overlay existing scenery pack | Yes (missing DDS files only) |

**PassthroughFS** is the primary filesystem for X-Plane integration:
- **Real files**: DSF, .ter, .png files pass through directly from source
- **Virtual DDS**: Non-existent .dds files are generated on-demand
- **Transparent**: X-Plane sees a complete scenery pack

**XEarthLayerFS** is a simpler virtual filesystem for testing:
- Only exposes a `textures/` directory
- All DDS files are generated on-demand
- No passthrough capability

## Security Considerations

### Mount Permissions

- FUSE mounts are user-space, no root required (with fuse group membership)
- Mount points are read-only to X-Plane
- Pack files are read-only

### Network Access

- Orthophoto downloads only from configured providers
- HTTPS required for all downloads
- No execution of downloaded content

## Performance Considerations

### Caching Strategy

See [cache-design.md](cache-design.md) for detailed caching architecture.

Summary:
1. Memory cache for hot tiles
2. Disk cache for generated DDS files
3. Chunk cache for downloaded imagery

### Passthrough Optimization

For real files in packs:
- Direct file descriptor passing (no copy)
- mmap for large files when supported
- Minimal overhead vs. real filesystem

### Directory Listing

- Cache directory listings
- Lazy DDS entry synthesis
- Incremental readdir support

## Compatibility Notes

### X-Plane Versions

- X-Plane 11: Supported
- X-Plane 12: Supported (primary target)

### Operating Systems

- Linux: Native FUSE support
- macOS: Requires macFUSE
- Windows: Requires WinFSP (future)

### Scenery Compatibility

XEarthLayer manages its own scenery packs created with Ortho4XP. It is **not** designed to be compatible with:
- AutoOrtho scenery packs
- Other third-party orthophoto systems

This allows XEarthLayer to optimize its format and features without legacy constraints.

## Future Enhancements

### Planned Features

- [ ] Pack integrity verification (checksums)
- [ ] Delta updates (download only changed files)
- [ ] Pack subscription service
- [ ] Multi-region pack bundles
- [ ] Custom regional boundaries

### Under Consideration

- Peer-to-peer pack distribution
- User-contributed regional packs
- Integration with Ortho4XP for custom pack creation
