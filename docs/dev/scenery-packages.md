# XEarthLayer Scenery Package Specification

This document defines the file formats and conventions for XEarthLayer Scenery Packages.

## Overview

XEarthLayer Scenery Packages are X-Plane 12 compatible scenery containers distributed as compressed archives. Each package contains pre-built DSF tiles, terrain definitions, and water masks for a specific geographic region.

## Package Types

### Orthophoto Packages (Type: Z)

Contain terrain mesh and texture references for satellite imagery. DDS textures are generated on-demand by XEarthLayer FUSE.

### Overlay Packages (Type: Y)

Contain roads, railways, powerlines, and other objects rendered above orthophoto scenery. All resources are static (no FUSE generation needed).

## Naming Conventions

### Package Folder Name

Format: `<sort_prefix><xel_prefix>_<region>_<type>`

Components:
- **sort_prefix**: `zz` for ortho, `yz` for overlay (ensures correct X-Plane load order)
- **xel_prefix**: `XEL` (XEarthLayer identifier)
- **region**: Region code (see table below)
- **type**: `ortho` or `overlay`

### Region Codes

| Region | Code | Example Ortho | Example Overlay |
|--------|------|---------------|-----------------|
| Africa | afr | zzXEL_afr_ortho | yzXEL_afr_overlay |
| Antarctica | ant | zzXEL_ant_ortho | yzXEL_ant_overlay |
| Asia | asia | zzXEL_asia_ortho | yzXEL_asia_overlay |
| Australia | aus | zzXEL_aus_ortho | yzXEL_aus_overlay |
| Europe | eur | zzXEL_eur_ortho | yzXEL_eur_overlay |
| North America | na | zzXEL_na_ortho | yzXEL_na_overlay |
| South America | sa | zzXEL_sa_ortho | yzXEL_sa_overlay |

## Package Internal Layout

Each package conforms to X-Plane 12 DSF specification structure:

```
zzXEL_na_ortho/
├── xearthlayer_scenery_package.txt    # XEarthLayer metadata
├── Earth nav data/                     # DSF files
│   └── +30-120/                        # 10° × 10° grid folder
│       ├── +32-115.dsf
│       ├── +37-118.dsf
│       └── +39-119.dsf
├── terrain/                            # Terrain type files
│   ├── 25264_10912_BI16.ter
│   ├── 25264_10912_BI16_sea.ter
│   └── ...
└── textures/                           # Water masks only
    ├── 25264_10912_ZL16.png
    └── ...
```

### Earth nav data/

Contains DSF (Distribution Scenery Format) files organized by geographic location.

**Folder naming**: `+<lat><lon>` where both latitude and longitude are rounded down to the nearest 10 degrees.
- Example: `+37-127.dsf` is in folder `+30-130/`
- Example: `+41-118.dsf` is in folder `+40-120/`
- Grid latitude: 2-digit zero-padded with sign (e.g., `+30`, `-10`, `+00`)
- Grid longitude: 3-digit zero-padded with sign (e.g., `-120`, `+010`, `+000`)

Each folder contains up to 100 DSF files covering a 10° latitude × 10° longitude area.

#### DSF files are 7z containers

Every `.dsf` in a package is a single-entry 7z archive holding the DSF of the same name. This is the format X-Plane has read natively since version 10 and the one Laminar uses for its own Global Scenery. The file keeps the `.dsf` extension. Publishers write it with the profile Laminar uses: LZMA, a 16 MiB dictionary (`LZMA:24`), one entry, plain header. XEarthLayer never opens a DSF; FUSE serves the bytes as they are on disk and X-Plane decompresses them.

This is where most of a package's size lives. On the European ortho package the uncompressed DSF files were 58% of 52 GB installed. Compressed in place they take roughly 44% of that space, which is a 32% reduction of the installed package and a 12% reduction of the download, with no change to the archive format around them.

### terrain/

Contains X-Plane Terrain Type (`.ter`) files defining how mesh patches are rendered.

These files reference:
- DDS texture files for land (generated on-demand by XEarthLayer)
- PNG mask files for water bodies (included in package)

### textures/

Contains only water mask PNG files (e.g., `25264_10912_ZL16.png`). These define water boundaries for coastal and lake areas.

DDS orthophoto textures are NOT included - XEarthLayer generates them dynamically via FUSE when X-Plane requests them.

## Package Metadata File

File: `xearthlayer_scenery_package.txt`

This file provides package identification and download verification information.

### Format

```
REGIONAL SCENERY PACKAGE
<specversion>
<title>  <pkgversion>
<datetime>
<packagetype>
<mountpoint>
<filename>
<partcount>

<sha256sum>  <localfilename>  <remoteurl>
<sha256sum>  <localfilename>  <remoteurl>
...
```

### Fields

| Line | Field | Description |
|------|-------|-------------|
| 1 | identifier | Always `REGIONAL SCENERY PACKAGE` |
| 2 | specversion | Specification version (e.g., `1.0.0`) |
| 3 | title + pkgversion | Region name and package version, separated by two spaces |
| 4 | datetime | Publish timestamp in UTC/Zulu (e.g., `2025-12-20T20:49:23Z`) |
| 5 | packagetype | `Z` for ortho, `Y` for overlay |
| 6 | mountpoint | Folder name in Custom Scenery (e.g., `zzXEL_eur_ortho`) |
| 7 | filename | Complete archive filename (e.g., `zzXEL_eur-1.0.0.tar.gz`) |
| 8 | partcount | Number of archive parts (may be 0 for draft metadata) |

After line 8, two blank lines, then one line per part:
- **sha256sum**: SHA-256 checksum of the part file
- **localfilename**: Filename for the part (e.g., `zzXEL_eur-1.0.0.tar.gz.aa`)
- **remoteurl**: Download URL for the part (may be empty during draft phase)

Fields within each line are separated by **two spaces**.

### Validation Contexts

Package metadata is validated differently depending on its lifecycle stage:

| Context | Part Count | URLs | Use Case |
|---------|-----------|------|----------|
| Initial | 0 allowed | Not required | After processing, before archive build |
| AwaitingUrls | >= 1 required | May be empty | After build, before URL configuration |
| Release | >= 1 required | All required | Ready for publication or already published |

The parser is lenient (accepts 0 parts, empty URLs), while validation is context-aware. This design supports the multi-phase publish workflow where metadata evolves through stages.

### Example

```
REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur-1.0.0.tar.gz
5


55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725  zzXEL_eur-1.0.0.tar.gz.aa  https://dl.example.com/zzXEL_eur-1.0.0.tar.gz.aa
b91f75f976768c62f6215d4e16e1e0e8a5948dfb9fccf73e67699a6818933335  zzXEL_eur-1.0.0.tar.gz.ab  https://dl.example.com/zzXEL_eur-1.0.0.tar.gz.ab
f5e5a7881b2156f1b53baf1982b4a11db3662449ce63abe0867841ed139aedf0  zzXEL_eur-1.0.0.tar.gz.ac  https://dl.example.com/zzXEL_eur-1.0.0.tar.gz.ac
031bf3288532038142cf5bc4a7453eb87340b9441da5b381c567781c23ef33fe  zzXEL_eur-1.0.0.tar.gz.ad  https://dl.example.com/zzXEL_eur-1.0.0.tar.gz.ad
15d85a6c5d9b8a6db1d6745cac1c2819972a689088e7f12bf5a2f076d7bc03d9  zzXEL_eur-1.0.0.tar.gz.ae  https://dl.example.com/zzXEL_eur-1.0.0.tar.gz.ae
```

## Package Library Index

File: `xearthlayer_package_library.txt`

Central index of all available packages, hosted at a known URL.

### Format

```
XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
<specversion>
<scope>
<sequence>
<datetime>
<packagecount>

<sha256sum>  <packagetype>  <title>  <pkgversion>  <metadataurl>
<sha256sum>  <packagetype>  <title>  <pkgversion>  <metadataurl>
...
```

### Fields

| Line | Field | Description |
|------|-------|-------------|
| 1 | identifier | Always `XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY` |
| 2 | specversion | Specification version (e.g., `1.0.0`) |
| 3 | scope | Library scope, currently `EARTH` |
| 4 | sequence | Integer incrementing with each publish (for change detection) |
| 5 | datetime | Publish timestamp in UTC/Zulu |
| 6 | packagecount | Number of packages listed (u8) |

After line 6, two blank lines, then one line per package:
- **sha256sum**: SHA-256 checksum of the package metadata file
- **packagetype**: `Z` for ortho, `Y` for overlay
- **title**: Region name (e.g., `EUROPE`, `NORTH AMERICA`)
- **pkgversion**: Package version
- **metadataurl**: URL to the package's `xearthlayer_scenery_package.txt`

Fields within each line are separated by **two spaces**.

### Example

```
XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
1
2025-12-20T20:49:23Z
14


55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725  Z  AFRICA  1.0.0  https://dl.example.com/afr/ortho/xearthlayer_scenery_package.txt
a1b2c3d4e5f6789012345678901234567890123456789012345678901234abcd  Y  AFRICA  1.0.0  https://dl.example.com/afr/overlay/xearthlayer_scenery_package.txt
b91f75f976768c62f6215d4e16e1e0e8a5948dfb9fccf73e67699a6818933335  Z  ANTARCTICA  1.0.0  https://dl.example.com/ant/ortho/xearthlayer_scenery_package.txt
c2d3e4f5a6b7890123456789012345678901234567890123456789012345bcde  Y  ANTARCTICA  1.0.0  https://dl.example.com/ant/overlay/xearthlayer_scenery_package.txt
...
```

## Versioning

### Semantic Versioning

All versions follow [Semantic Versioning](https://semver.org/):
- **MAJOR**: Incompatible changes (new spec version)
- **MINOR**: New tiles or regions added
- **PATCH**: Bug fixes, metadata corrections

### Specification Compatibility

A client reads a library index or package metadata file only if its specification MAJOR is at or below the MAJOR the client was built for (`SUPPORTED_SPEC_MAJOR` in `package/spec.rs`, currently 1). A higher MAJOR is refused with a message asking the user to upgrade XEarthLayer. MINOR and PATCH differences are accepted in both directions. Any future incompatible change to either format therefore ships as a new MAJOR, and must be published at a separate URL so that clients built before it never fetch it.

### Library Sequence Number

The library maintains a sequence number (integer) that increments with each published change. Clients can quickly check if updates exist by comparing sequence numbers.

## Archive Format

Packages are distributed as gzip-compressed tar archives, optionally split into multiple parts for large packages.

### Splitting

Large archives are split using the `split` command convention:
- `zzXEL_eur-1.0.0.tar.gz.aa`
- `zzXEL_eur-1.0.0.tar.gz.ab`
- `zzXEL_eur-1.0.0.tar.gz.ac`
- etc.

Parts are reassembled with `cat *.tar.gz.* > archive.tar.gz` or equivalent.

### Recommended Part Size

Suggested part size: 500MB - 1GB (balances download resumability with management overhead).

## X-Plane Compatibility

Packages conform to X-Plane 12 DSF specification:
- [DSF File Format Specification](https://developer.x-plane.com/article/dsf-file-format-specification/)
- [Terrain Type File Specification](https://developer.x-plane.com/article/terrain-type-ter-file-format-specification/)

The `zz` and `yz` prefixes ensure XEarthLayer scenery loads at lowest priority, allowing user scenery to override.
