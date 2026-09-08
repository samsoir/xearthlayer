# Package Management

XEarthLayer's package manager lets you install, update, and remove pre-built scenery packages. This is the easiest way to get high-quality satellite imagery for specific regions.

## Overview

Scenery packages are distributed through **package libraries** - index files that list available packages and their download locations. To use packages, you need:

1. A library URL from a scenery provider
2. XEarthLayer configured with that library URL

## Configuration

Set your library URL in `~/.config/xearthlayer/config.ini`:

```ini
[xplane]
scenery_dir = /path/to/X-Plane 12/Custom Scenery

[packages]
# library_url defaults to https://xearthlayer.app/packages/xearthlayer_package_library.txt
install_location = ~/.local/share/xearthlayer/packages
auto_install_overlays = true
```

The default `library_url` points to the official XEarthLayer package library, so most users don't need to configure this.

### Auto-Install Overlays

When `auto_install_overlays = true`, installing an ortho package will automatically install the matching overlay package for the same region (if available). This saves time when setting up new regions.

## Commands

### Check Available Packages

See what packages are available and their update status:

```bash
xearthlayer packages check
```

Output:
```
Checking for package updates...

  EU-PARIS (ortho) v1.0.0 - Not installed
  EU-ALPS (ortho) v2.0.0 - Installed (up to date)
  NA-SOCAL (ortho) v1.2.0 - Installed (update available: v1.3.0)

1 package(s) available for installation.
1 package(s) have updates available.
```

### List Installed Packages

View packages currently installed on your system:

```bash
xearthlayer packages list
```

Output:
```
Installed Packages (2)
======================

  EU-ALPS (ortho) v2.0.0 - 4.2 GB
  NA-SOCAL (ortho) v1.2.0 - 8.7 GB
```

For more details:

```bash
xearthlayer packages list --verbose
```

Output:
```
Installed Packages (2)
======================

EU-ALPS (ortho) v2.0.0
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-alps_ortho
  Size: 4.2 GB
  Mount: Not mounted

NA-SOCAL (ortho) v1.2.0
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_na-socal_ortho
  Size: 8.7 GB
  Mount: Not mounted
```

### Install a Package

Install a package by region code:

```bash
xearthlayer packages install eu-paris
```

For overlay packages (roads, buildings):

```bash
xearthlayer packages install eu-paris --type overlay
```

Output:
```
Installing eu-paris (ortho)...

Fetching library index...
Fetching package metadata...
Package: EU-PARIS v1.0.0
Parts: 3 (zzXEL_eu-paris_ortho-1.0.0.tar.gz)

Downloading part 1/3... [=========>          ] 45%
Downloading part 2/3... [====================] 100%
Downloading part 3/3... [====================] 100%

Verifying checksums...
Extracting...

Success: Installed EU-PARIS (ortho) v1.0.0 to /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
Downloaded 1.2 GB, extracted 4521 files
```

### Package Info

Get detailed information about an installed package:

```bash
xearthlayer packages info eu-paris
```

Output:
```
EU-PARIS (ortho) v1.0.0
=======================

Package Details:
  Title: EU-PARIS
  Type: Orthophoto (satellite imagery)
  Version: 1.0.0
  Mountpoint: zzXEL_eu-paris_ortho

Installation:
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
  Size: 1.2 GB

Mount Status:
  Not mounted

Archive Parts:
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.aa (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ab (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ac (200 MB)
```

### Update Packages

Update a specific package:

```bash
xearthlayer packages update eu-paris
```

Update all packages with available updates:

```bash
xearthlayer packages update --all
```

The update process:
1. Downloads the new version
2. Removes the old version
3. Extracts the new version

### Remove a Package

Remove an installed package:

```bash
xearthlayer packages remove eu-paris
```

You'll be prompted for confirmation:

```
Package: EU-PARIS (ortho) v1.0.0
Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
Size: 1.2 GB

Remove this package? [y/N]: y

Removing package...
Success: Removed eu-paris (ortho)
```

To skip the confirmation prompt:

```bash
xearthlayer packages remove eu-paris --force
```

## Package Types

XEarthLayer supports two package types:

| Type | Description | Default |
|------|-------------|---------|
| `ortho` | Satellite/aerial imagery (orthophotos) | Yes |
| `overlay` | Roads, buildings, landmarks | No |

Specify the type with `--type`:

```bash
xearthlayer packages install eu-paris --type overlay
```

## CLI Reference

All package commands support these common options:

| Option | Description |
|--------|-------------|
| `--library-url <URL>` | Override the library URL from config |
| `--install-dir <PATH>` | Override the installation directory |
| `--type <TYPE>` | Package type: `ortho` or `overlay` |

### install

```bash
xearthlayer packages install <REGION> [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--type <TYPE>` | Package type (default: ortho) |
| `--temp-dir <PATH>` | Temporary download directory |

### update

```bash
xearthlayer packages update [REGION] [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--all` | Update all packages |
| `--type <TYPE>` | Package type filter |

### remove

```bash
xearthlayer packages remove <REGION> [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-f, --force` | Skip confirmation prompt |
| `--type <TYPE>` | Package type (default: ortho) |

## Disk Space

Large packages are split into multiple parts (typically 500 MB each) for easier downloading. During installation, you'll need:

- Enough space for the download (in temp directory)
- Enough space for the extracted package (in scenery directory)

After extraction, temporary files are automatically cleaned up.

## Network Issues

If a download fails:

1. The partial download is preserved
2. Re-running the install command will resume where it left off
3. Checksums are verified to ensure integrity

## X-Plane Integration

Installed packages appear in your Custom Scenery folder with names like:

- `zzXEL_eu-paris_ortho` (orthophoto)
- `yzXEL_eu-paris_overlay` (overlay)

### Package Installation Methods

**Ortho packages** are installed to the package directory (e.g., `~/.local/share/xearthlayer/packages/`) and mounted via FUSE to Custom Scenery when you run `xearthlayer run`.

**Overlay packages** are installed to the package directory, then a symlink is automatically created in Custom Scenery pointing to the installed package. This allows X-Plane to access the overlay files directly without FUSE.

### Running XEarthLayer

After installing packages, start XEarthLayer to mount them:

```bash
xearthlayer run
```

This automatically discovers all installed ortho packages and mounts them in Custom Scenery. See [Running the Service](running-service.md) for details.

### Scenery Load Order

The `zz` and `yz` prefixes ensure correct scenery loading order in X-Plane:

- Overlays (`yz`) load before orthophotos (`zz`)
- Both load after most other custom scenery

You may need to adjust your `scenery_packs.ini` order if you have conflicts with other scenery.
