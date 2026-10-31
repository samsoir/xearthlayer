# Content Publishing

This guide explains how to create and distribute XEarthLayer scenery packages from Ortho4XP-generated tiles.

## Overview

The publishing workflow:

1. **Generate tiles** with Ortho4XP
2. **Initialize** a package repository
3. **Scan and add** tiles to create packages
4. **Build** distributable archives
5. **Release** to a package library
6. **Host** files for users to download

## Prerequisites

- Ortho4XP-generated scenery tiles
- `xearthlayer-publish` installed. It ships as its own package, separate from the `xearthlayer` runtime, and needs neither X-Plane nor FUSE on the machine that builds packages. On an Apple Silicon Mac: `brew install --cask xearthlayer/tap/xearthlayer-publish` (or `xearthlayer-publish@prerelease` for dev, alpha, beta and rc builds). From source: `make install-xearthlayer-publish`
- Web hosting for distributing packages (any HTTP server, CDN, or cloud storage)

## Step 1: Generate Tiles with Ortho4XP

Use Ortho4XP to generate your scenery tiles as usual. XEarthLayer supports:

- **Orthophoto tiles** - Satellite/aerial imagery
- **Overlay tiles** - Roads, buildings, and other features

Your Ortho4XP output should have a structure like:

```
Ortho4XP_Tiles/
├── zOrtho4XP_+48+002/
│   ├── Earth nav data/
│   │   └── +40+000/
│   │       └── +48+002.dsf
│   ├── terrain/
│   │   └── *.ter
│   └── textures/
│       └── *.dds
├── zOrtho4XP_+48+003/
│   └── ...
```

## Step 2: Initialize Repository

Create a package repository to manage your packages:

```bash
cd /path/to/my-scenery-packages
xearthlayer-publish init
```

Output:
```
Initialized XEarthLayer package repository at:
  /path/to/my-scenery-packages

Repository structure:
  v1/           - Library generation v1 (current): its library index and metadata
  v1/packages/  - Package working directories
  v1/dist/      - Built archives for distribution
  staging/      - Work area shared by every generation

Archive part size: 500 MB
```

A repository holds one or more **library generations**, each a complete scenery world in its own folder: `v1/`, `v2/`, and so on. A new repository starts with `v1`. Every command works in the current generation; see [Library Generations](#library-generations).

## Step 3: Scan Tiles

Preview what tiles will be included before adding them:

```bash
# Scan ortho tiles (default)
xearthlayer-publish scan --source /path/to/Ortho4XP_Tiles

# Scan overlay tiles
xearthlayer-publish scan --source /path/to/Overlay_Tiles --type overlay
```

Output:
```
Scanning Ortho4XP tiles at: /path/to/Ortho4XP_Tiles

Scan Results
============

Tiles:  12

Tiles found:
  +48+002 - 1 DSF, 1 TER, 0 masks
  +48+003 - 1 DSF, 1 TER, 0 masks
  +48+004 - 1 DSF, 1 TER, 0 masks
  ...

Region Suggestion
-----------------
Suggested region: EUR (Europe)
```

## Step 4: Add Tiles to Package

Create a package from the scanned tiles:

```bash
xearthlayer-publish add --source /path/to/Ortho4XP_Tiles --region eu-paris
```

For overlay packages:

```bash
xearthlayer-publish add --source /path/to/Overlay_Tiles --region eu-paris --type overlay
```

Output:
```
Processing Ortho4XP tiles output...
  Source: /path/to/Ortho4XP_Tiles
  Region: EU-PARIS
  Type:   ortho
  Version: 1.0.0

Scanning...
Found 12 tiles

Processing into package...
Processing Summary
------------------
Tiles processed: 12
DSF files:       12
DSF compressed:  245.3 MB -> 108.1 MB (44%)
TER files:       12
Mask files:      0
DDS skipped:     1536

Generating metadata...

Package created successfully!
  Location: ./v1/packages/zzXEL_eu-paris_ortho

Next steps:
  1. Run 'xearthlayer-publish build --region eu-paris --type ortho' to create archives
```

**Note:** DDS texture files are intentionally skipped - XEarthLayer streams these on-demand.

DSF files are written as 7z containers, which X-Plane reads directly. This is where the size saving comes from, and it is CPU bound: expect `publish add` to take a few seconds per DSF per core. If a source DSF is already a 7z container it is copied as it is and reported on a `DSF unchanged:` line.

## Step 4a: Analyze Zoom Level Overlaps (Optional)

If your Ortho4XP tiles include multiple zoom levels (e.g., ZL16 base with ZL18 airports), you may have overlapping tiles that cause visual artifacts (striping) in X-Plane.

### Scan for Overlaps

The scan command automatically reports zoom level overlaps:

```bash
xearthlayer-publish scan --source /path/to/Ortho4XP_Tiles
```

Output includes:
```
Zoom Level Analysis
-------------------
  ZL16 tiles: 12,456
  ZL18 tiles: 847

Overlaps Detected:
  ZL18 overlaps ZL16: 312 tiles
  Total redundant tiles: 312

Recommendation:
  Use 'publish dedupe' to remove redundant tiles before building.
```

### Analyze Coverage Gaps

Before deduplicating, check if your ZL18 coverage is complete:

```bash
xearthlayer-publish gaps --region eu-paris --type ortho
```

Output:
```
Coverage Gap Analysis
=====================
Tiles analyzed: 13,303
Zoom levels:    [16, 18]

Gaps Found
  42 parent tiles have incomplete ZL18 coverage
  398 ZL18 tiles needed to complete coverage

Estimated download: ~4.8 GB (to generate missing tiles)
```

**Important:** Only deduplicate if coverage is complete (0 gaps). Partial coverage means some areas would have no texture if the ZL16 parent is removed.

### Export Missing Tiles for Ortho4XP

Generate coordinates for Ortho4XP to regenerate missing tiles:

```bash
xearthlayer-publish gaps --region eu-paris --type ortho \
  --format ortho4xp --output missing_tiles.txt
```

This creates a file with coordinates you can use in Ortho4XP to generate the missing ZL18 tiles.

### Deduplicate Overlapping Tiles

Remove redundant lower-resolution tiles where higher-resolution exists:

```bash
# Preview what would be removed (dry run)
xearthlayer-publish dedupe --region eu-paris --type ortho --dry-run

# Actually remove redundant tiles
xearthlayer-publish dedupe --region eu-paris --type ortho
```

Options:
- `--priority highest` (default): Keep highest zoom level, remove lower
- `--priority lowest`: Keep lowest zoom level, remove higher
- `--priority zl16`: Keep specific zoom level
- `--tile 48,2`: Target a specific 1°×1° tile

**Note:** Deduplication only removes tiles when ALL higher-resolution children exist. This prevents creating visible gaps in scenery.

## Step 5: Build Archives

Create distributable archive files:

```bash
xearthlayer-publish build --region eu-paris --type ortho
```

Output:
```
Building archive for EU-PARIS ortho...
  Part size: 500 MB

Archive created successfully!

  Archive: zzXEL_eu-paris_ortho-1.0.0.tar.gz
  Parts:   3
  Size:    1.2 GB

Archive parts:
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.aa (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ab (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ac (200 MB)

Next steps:
  1. Upload archive parts to your hosting provider
  2. Run 'xearthlayer-publish urls --region eu-paris --type ortho --base-url <url>' to configure URLs
```

Archives are stored in `<generation>/dist/<region>/<type>/`, for example `v1/dist/eu-paris/ortho/`.

## Step 6: Upload Archives

Upload the archive parts to your web hosting:

```bash
# Example using AWS S3
aws s3 cp v1/dist/eu-paris/ortho/ s3://my-bucket/packages/eu-paris/ortho/ --recursive

# Example using rsync
rsync -av dist/eu-paris/ortho/ user@server:/var/www/packages/eu-paris/ortho/
```

## Step 7: Configure URLs

Tell XEarthLayer where the archives are hosted:

```bash
xearthlayer-publish urls --region eu-paris --type ortho \
  --base-url https://my-cdn.example.com/packages/eu-paris/ortho
```

Output:
```
Configuring URLs for EU-PARIS ortho...
  Base URL: https://my-cdn.example.com/packages/eu-paris/ortho

Generated URLs:
  https://my-cdn.example.com/packages/eu-paris/ortho/zzXEL_eu-paris_ortho-1.0.0.tar.gz.aa
  https://my-cdn.example.com/packages/eu-paris/ortho/zzXEL_eu-paris_ortho-1.0.0.tar.gz.ab
  https://my-cdn.example.com/packages/eu-paris/ortho/zzXEL_eu-paris_ortho-1.0.0.tar.gz.ac

URLs configured successfully!
```

## Step 8: Release to Library

Add the package to your library index:

```bash
xearthlayer-publish release --region eu-paris --type ortho \
  --metadata-url https://my-cdn.example.com/packages/eu-paris/metadata.txt
```

Output:
```
Releasing EU-PARIS ortho to library index...

Package released successfully!

  Region:    EU-PARIS
  Type:      ortho
  Version:   1.0.0
  Sequence:  1
  Download:  1.2 GB
  Installed: 3.4 GB

Library index updated:
  ./xearthlayer_package_library.txt
```

The two sizes are measured as the release runs, from the archive parts in
`dist/` and from the package working directory, and are recorded in
`region_metadata.json` under the region's `size.ortho` or `size.overlay` block.
That file is optional: if it is missing, or has no entry for this region, the
release still succeeds and prints a warning instead. Updating the library index
is the release; the sizes are a description published alongside it.

## Step 9: Upload Library and Metadata

Upload the updated files:

```bash
# Upload library index
aws s3 cp xearthlayer_package_library.txt s3://my-bucket/

# Upload package metadata
aws s3 cp packages/zzXEL_eu-paris_ortho/xearthlayer_scenery_package.txt \
  s3://my-bucket/packages/eu-paris/metadata.txt
```

## Step 10: Share with Users

Users can now install your packages by configuring their library URL:

```ini
[packages]
library_url = https://my-cdn.example.com/xearthlayer_package_library.txt
```

Then:

```bash
xearthlayer packages check
xearthlayer packages install eu-paris
```

## Managing Versions

### Bump Version

Before releasing an update:

```bash
# Patch version (1.0.0 -> 1.0.1)
xearthlayer-publish version --region eu-paris --type ortho --bump patch

# Minor version (1.0.1 -> 1.1.0)
xearthlayer-publish version --region eu-paris --type ortho --bump minor

# Major version (1.1.0 -> 2.0.0)
xearthlayer-publish version --region eu-paris --type ortho --bump major
```

Then rebuild and release:

```bash
xearthlayer-publish build --region eu-paris --type ortho
# Upload new archives...
xearthlayer-publish urls --region eu-paris --type ortho --base-url <url>
xearthlayer-publish release --region eu-paris --type ortho --metadata-url <url>
# Upload updated library and metadata...
```

## Repository Management

### List Packages

```bash
xearthlayer-publish list
```

```
Packages in repository:

  EU-PARIS ortho v1.0.0 - Released
  EU-ALPS ortho v2.1.0 - Built (not released)
  NA-SOCAL ortho v1.0.0 - Not Built
```

### Check Status

```bash
xearthlayer-publish status
```

```
Package Status
==============

EU-PARIS ortho
  Status: Released
  Version: 1.0.0
  Parts: 3
  Validation: OK

EU-ALPS ortho
  Status: Built
  Version: 2.1.0
  Parts: 5
  Validation: Missing metadata URL
```

### Validate Repository

Check for issues:

```bash
xearthlayer-publish validate
```

### Delete a Package

Remove a package from the repository: its entry in the library index, its
working directory under `packages/`, and its built archives under `dist/`.

```bash
# See what would go, and how much disk it frees, without touching anything
xearthlayer-publish delete --region eu-paris --dry-run

# Delete both the ortho and the overlay package for the region
xearthlayer-publish delete --region eu-paris

# Delete only one of the two
xearthlayer-publish delete --region eu-paris --type ortho
```

| Flag | Meaning |
|------|---------|
| `--region <code>` | Region to delete. Required. |
| `--type <ortho\|overlay>` | One package type. Omit to delete both, which is what retiring a region means. |
| `--dry-run` | Print the plan and stop. Never prompts, never deletes. |
| `--yes` | Skip the confirmation prompt, for use in scripts. |
| `--repo <path>` | Repository root. Defaults to the current directory. |

The deletion is planned before it is confirmed, so the prompt can name every
directory that would go and the total it frees. `--dry-run` prints that same
plan and stops, which is why it is the safe way to check a region code before
committing to it. Answering anything other than yes at the prompt leaves
everything in place.

The sizes reported are apparent size, summed from file lengths, so they read a
little under `du` for a package holding hundreds of thousands of small `.ter`
files. The figure is there to help you judge the disk cost, not to reconcile
with the filesystem.

A region that legitimately has only one of the two package types is not an
error: the missing type is reported as skipped and the other is deleted. A
region with neither is an error, because the likeliest explanation is a
mistyped region code. A region code containing a path separator is rejected
outright, since the code becomes a directory name and no real region code
contains a slash.

**Archives already uploaded to a hosting provider are not touched.** The
publisher has no knowledge of your hosting, and those files need to outlive the
index entry regardless: a user who started an install before you deleted the
entry is still fetching parts from those URLs. Taking the hosted copies down is
a separate, manual step, made once you are satisfied that nobody is still
installing the old version.

## Library Generations

Each scenery world is a generation: a library index, region metadata, coverage map, package working folders and archives under `v<N>/`. A library URL clients read never changes meaning, so a rebuilt world goes into a new generation instead of replacing the packages of the old one, and clients of the old world keep working.

```bash
xearthlayer-publish library list               # the generations, the current one marked *
xearthlayer-publish library new v2             # start an empty generation
xearthlayer-publish library use v2             # make it current
xearthlayer-publish --library v1 status        # one command in another generation
```

Publish each generation's `xearthlayer_package_library.txt` at its own URL, for example `https://example.com/packages/v2/xearthlayer_package_library.txt`, and point clients at the generation they should read.

### Upgrading a Repository From Before Generations

A repository created before generations keeps its library at the root, and every command refuses it until it is upgraded:

```bash
xearthlayer-publish upgrade --root-as v1 --dry-run   # see what would change
xearthlayer-publish upgrade --root-as v1
```

The upgrade copies the root library index, region metadata and coverage map into `v1/`, makes `v1` current, and moves into `v1/` the package and archive folders the root index lists. Folders it does not list are left where they are and reported. The root files stay, byte for byte: clients from before generations keep reading them, and no command writes them again.

A library kept elsewhere, for example on another branch of the repository, becomes a generation with `library import`, which copies its files and moves the root working folders its index lists:

```bash
xearthlayer-publish library import v2 --from /path/to/library-files --dry-run
xearthlayer-publish library import v2 --from /path/to/library-files
xearthlayer-publish library use v2
```

## File Formats

### Library Index

`xearthlayer_package_library.txt`:

```
XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
1
2024-01-15T10:30:00Z
2


<sha256>  Z  EU-PARIS  1.0.0  https://example.com/eu-paris/metadata.txt
<sha256>  Z  NA-SOCAL  1.2.0  https://example.com/na-socal/metadata.txt
```

### Package Metadata

`xearthlayer_scenery_package.txt`:

```
REGIONAL SCENERY PACKAGE
1.0.0
EU-PARIS  1.0.0
2024-01-15T10:30:00Z
Z
zzXEL_eu-paris_ortho
zzXEL_eu-paris_ortho-1.0.0.tar.gz
3


<sha256>  zzXEL_eu-paris_ortho-1.0.0.tar.gz.aa  https://example.com/.../file.aa
<sha256>  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ab  https://example.com/.../file.ab
<sha256>  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ac  https://example.com/.../file.ac
```

## Best Practices

### Region Naming

Use clear, consistent region codes:

- `eu-paris` - Paris, France region
- `eu-alps` - European Alps
- `na-socal` - Southern California
- `as-japan` - Japan

### Archive Size

The default 500 MB part size works well for most hosting. Adjust with:

```bash
xearthlayer-publish init --part-size 250MB
```

Smaller parts are easier to resume on poor connections but require more HTTP requests.

### Versioning

Follow semantic versioning:

- **Patch** (1.0.x): Bug fixes, minor adjustments
- **Minor** (1.x.0): New tiles added, improvements
- **Major** (x.0.0): Breaking changes, major regeneration

### Hosting

Recommended hosting options:

- **CDN** (CloudFlare, AWS CloudFront): Best performance
- **Object Storage** (S3, GCS, B2): Cost-effective for large files
- **Traditional Web Host**: Works for smaller packages

Ensure your hosting supports:
- Large file downloads
- Resume/range requests (for reliability)
- HTTPS (recommended)
