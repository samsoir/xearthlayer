# XEarthLayer Scenery System Overview

This document provides a high-level overview of the XEarthLayer Scenery Package system. For detailed specifications, see the linked documents below.

## Related Documentation

- [Scenery Package Specification](SCENERY_PACKAGES.md) - Package formats and conventions
- [Package Manager Design](PACKAGE_MANAGER.md) - Consuming and installing packages
- [Package Publisher Design](PACKAGE_PUBLISHER.md) - Creating and publishing packages
- [Implementation Plan](SCENERY_PACKAGE_PLAN.md) - Development roadmap and progress

## System Overview

XEarthLayer Scenery Packages are pre-built X-Plane 12 scenery containers that work with the XEarthLayer FUSE filesystem to provide streaming orthophoto imagery. The system consists of three main components:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Package Ecosystem                                 │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│   ┌──────────────┐         ┌──────────────┐         ┌──────────────┐   │
│   │   Publisher  │         │   Library    │         │   Manager    │   │
│   │              │ ──────► │   (Remote)   │ ──────► │              │   │
│   │ Creates and  │ publish │              │  fetch  │ Downloads &  │   │
│   │ uploads pkgs │         │ Index of all │         │ installs     │   │
│   └──────────────┘         │ available    │         └──────┬───────┘   │
│         ▲                  │ packages     │                │           │
│         │                  └──────────────┘                │           │
│         │                                                  ▼           │
│   ┌─────┴────────┐                                 ┌──────────────┐   │
│   │  Ortho4XP    │                                 │ Local        │   │
│   │  Output      │                                 │ Packages     │   │
│   └──────────────┘                                 └──────┬───────┘   │
│                                                           │           │
└───────────────────────────────────────────────────────────┼───────────┘
                                                            │
                                                            ▼
                                              ┌─────────────────────────┐
                                              │   XEarthLayer FUSE      │
                                              │                         │
                                              │ Mounts ortho packages   │
                                              │ Symlinks overlay pkgs   │
                                              └────────────┬────────────┘
                                                           │
                                                           ▼
                                              ┌─────────────────────────┐
                                              │   X-Plane 12            │
                                              │   Custom Scenery/       │
                                              └─────────────────────────┘
```

## Terminology

- **XEarthLayer Application**: The XEarthLayer binary providing streaming orthophoto scenery for X-Plane 12. Requires one or more regional scenery packages to operate.

- **Regional Scenery Package**: A container encapsulating X-Plane 12 scenery files for a specific region. Regions align with the 7 continents: Africa, Antarctica, Asia, Australia, Europe, North America, and South America.

- **Package Manager**: Component responsible for discovering, downloading, installing, updating, and removing scenery packages on the user's system.

- **Package Publisher**: Component responsible for creating publishable packages from Ortho4XP output, managing versioning, and maintaining the package library index.

- **Package Library**: A remote index file listing all available packages, their versions, and download locations.

- **Ortho Package**: Contains orthophoto scenery (satellite imagery textures). Mounted via FUSE for on-demand texture generation.

- **Overlay Package**: Contains roads, railways, powerlines, and other objects that render above orthophoto scenery. Installed via symlink (no FUSE needed).

## Package Types

### Orthophoto Packages (Type: Z)

Orthophoto packages contain:
- DSF files defining terrain mesh
- `.ter` terrain files referencing textures
- Water mask PNG files

The actual DDS textures are NOT included - XEarthLayer generates them on-demand via the FUSE filesystem when X-Plane requests them.

**Naming**: `zzXEL_<region>_ortho` (e.g., `zzXEL_eur_ortho`)

**Installation**: FUSE mount from package library to Custom Scenery

### Overlay Packages (Type: Y)

Overlay packages contain:
- DSF files with roads, railways, powerlines, objects
- All required static resources

No dynamic generation needed - these are purely static files.

**Naming**: `yzXEL_<region>_overlay` (e.g., `yzXEL_eur_overlay`)

**Installation**: Symlink from package library to Custom Scenery

## Region Codes

| Region | Code | Ortho Folder | Overlay Folder |
|--------|------|--------------|----------------|
| Africa | afr | zzXEL_afr_ortho | yzXEL_afr_overlay |
| Antarctica | ant | zzXEL_ant_ortho | yzXEL_ant_overlay |
| Asia | asi | zzXEL_asi_ortho | yzXEL_asi_overlay |
| Australia | aus | zzXEL_aus_ortho | yzXEL_aus_overlay |
| Europe | eur | zzXEL_eur_ortho | yzXEL_eur_overlay |
| North America | na | zzXEL_na_ortho | yzXEL_na_overlay |
| South America | sa | zzXEL_sa_ortho | yzXEL_sa_overlay |

## Data Flow

### Package Creation (Publisher)

1. User runs Ortho4XP to generate scenery tiles
2. Publisher processes Ortho4XP output:
   - Organizes tiles into regional packages
   - Removes DDS textures (kept: DSF, .ter, water masks)
   - Generates package metadata
   - Splits into downloadable parts with checksums
3. Publisher updates library index
4. Packages uploaded to hosting (manual or automated)

### Package Installation (Manager)

1. Manager fetches library index from configured `library_root`
2. User selects region to install
3. Manager downloads package parts (resumable, parallel)
4. Each part verified via SHA256 checksum
5. Parts reassembled and extracted to package library location
6. For ortho: registered for FUSE mounting
7. For overlay: symlink created in Custom Scenery

### Runtime (FUSE)

1. XEarthLayer mounts each installed ortho package
2. X-Plane starts and indexes Custom Scenery
3. X-Plane requests texture file (e.g., `101424_42144_BI18.dds`)
4. FUSE intercepts request, XEarthLayer generates texture on-demand
5. Texture served to X-Plane and cached for future requests

## X-Plane Integration

XEarthLayer does not modify `scenery_packs.ini`. The naming convention ensures proper load order:

1. User's custom scenery (airports, landmarks, etc.)
2. `yz*` - XEarthLayer overlays (below user overlays)
3. `zz*` - XEarthLayer ortho (below everything)

X-Plane automatically discovers and indexes mounted folders on startup.

## Versioning

- **Spec Version**: Version of the file format specification (e.g., `1.0.0`)
- **Package Version**: Semantic version of a specific regional package (e.g., `1.2.0`)
- **Library Sequence**: Integer that increments with each library update, enabling quick change detection

## Configuration

Key settings in `~/.xearthlayer/config.ini`:

```ini
[packages]
# Remote library index location
library_root = https://packages.xearthlayer.org

# Local package storage location
install_location = ~/.xearthlayer/packages
```

## Next Steps

- See [Scenery Package Specification](SCENERY_PACKAGES.md) for detailed file formats
- See [Package Manager Design](PACKAGE_MANAGER.md) for installation workflow
- See [Package Publisher Design](PACKAGE_PUBLISHER.md) for creating packages
- See [Implementation Plan](SCENERY_PACKAGE_PLAN.md) for development status
