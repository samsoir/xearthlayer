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

## The Package Manager

The package manager is a terminal screen for installing, updating and removing packages. It does what the commands below do, with the library, your installed packages and the running jobs on one screen.

Open it with:

```bash
xearthlayer packages
```

The setup wizard offers to open it when it finishes, so a first install can go from setup straight to choosing scenery. The package manager needs a terminal. When input or output is not a terminal (in a script, or piped), `xearthlayer packages` prints the command help instead, and the commands below work as they always have.

### The Three Tabs

Switch tabs with `1`, `2` and `3`, or with Tab and Shift+Tab.

| Tab | Shows |
|-----|-------|
| 1 Local | The packages installed on this machine, with version, size and whether the library has a newer version. |
| 2 Library | Every package the library offers, with whether it is not installed, up to date, or has an update. `t` cycles the list between all packages, ortho only and overlay only. |
| 3 Progress | The jobs of this session: queued, downloading, extracting, installing, removing, done, failed or cancelled. The tab title counts the unfinished ones. |

Local and Library have a detail pane beside the list for the selected package, which names its continent from the region code (`AF-NORTH` is Africa). Marked rows are drawn in cyan. While the installed list is first read, the Local tab shows a spinner with "Reading the installed packages…", and while either list is read again its title shows the spinner and "reading" beside the count; after the queue changes an overlay, the status line says "Rebuilding the overlay folder…" until the rebuild is done. Queuing a job never switches tabs: the status line says how many jobs are queued, and you look at Progress when you want to.

### Keys

Press `?` in the package manager for this list.

| Keys | Where | Action |
|------|-------|--------|
| `1` `2` `3`, Tab, Shift+Tab | everywhere | Switch tabs |
| Up, Down, `j`, `k`, PgUp, PgDn, Home, End | everywhere | Move the selection |
| `r` | everywhere | Refresh the lists |
| `?` | everywhere | Help |
| `q`, Esc, Ctrl+C | everywhere | Quit |
| Space | Local, Library | Mark or unmark the selected row |
| `a` | Local, Library | Mark all rows shown, or clear the marks when all are marked |
| `u` | Local | Update |
| `d` | Local | Delete |
| `t` | Library | Cycle the type filter |
| `i` | Library | Install |
| `c` | Progress | Cancel the selected job |
| `C` | Progress | Cancel every unfinished job |
| `x` | Progress | Clear finished jobs from the list |

`u`, `d` and `i` act on the marked rows. With nothing marked, they act on the selected row. A confirmation asks before an install, a delete, a cancel of all jobs and a quit: `y` or Enter confirms, `n` or Esc backs out. An update starts at once, without a dialog, and leaves out packages that are already up to date.

### Example: Install Two Regions

1. Run `xearthlayer packages` and press `2` for the Library tab.
2. Move to the first region you want and press Space to mark it. Move to the second and press Space again.
3. Press `i`. A dialog lists the packages that will install and asks you to confirm. With `auto_install_overlays` on, each ortho package is followed by the overlay for the same region. Packages that are already up to date are left out, and the dialog says so.
4. Press `y`. The status line reads "N jobs queued: see Progress".
5. Press `3` to watch Progress. Downloads run ahead of installs, as they do on the command line, and a job waits with "waiting for disk space" when the temp folder is short of room.
6. When the last job finishes, the lists refresh and the overlay folder is rebuilt if an overlay changed. Press `q` to leave. The package manager prints one line saying what the session did.

Cancel a job with `c` on the Progress tab. As on the command line, a package being replaced keeps its old version, and temporary files are removed.

### Quitting and Signals

`q` asks to confirm. With jobs running, confirming cancels them and leaves when they have stopped. Press `q` again while it cancels to leave at once and let the next package command clean up.

A SIGINT sent from outside (for example `kill -INT`) quits the same way as a confirmed `q`. A second one, or SIGTERM, leaves at once. SIGHUP, a closed terminal, leaves at once and stops drawing. The exit codes are 130, 143 and 129 by the first signal. In every case an installed package is complete, either the old version or the new one. See [Stopping an Install](#stopping-an-install).

### Using It While XEarthLayer Runs

The package manager works while `xearthlayer run` is active, with the same rules the commands follow (see [While XEarthLayer is running](#while-xearthlayer-is-running)). A banner at the top says so, with the process ID.

- Installing a package that is not installed is allowed. The status line reminds you to restart `xearthlayer run` to use it.
- Updating, deleting, and installing over an installed package are refused, with the message "XEarthLayer is running (pid N): stop it before updating or removing packages". In a batch that mixes new and installed packages, the installed ones are left out and the new ones install.

Only one package operation runs at a time. If a package command is already running in another terminal, the package manager says "another package operation is running" when you queue a job, and nothing is lost: your marks come back.

## Command Line

### Selecting Packages by Region Code

Region codes are hierarchical, from continent to country to part of a
country, so `install`, `update`, `remove` and `info` all accept the start of a
code and act on every package it selects. Matching ignores case.

| You type | Selects |
|----------|---------|
| `NA` | every North America package |
| `NA-CANADA` | `NA-CANADA-WEST` and `NA-CANADA-EAST` |
| `AS-RUSSIA` | `AS-RUSSIA-WEST` and `AS-RUSSIA-EAST` |
| `AF-SOUTH` | exactly `AF-SOUTH` |

A code that selects one package behaves exactly as a full code always has.
When it selects several, the matches are listed first and you are asked to
confirm before anything is downloaded:

```
$ xearthlayer packages install NA
Installing NA (ortho)...

Fetching library index...
NA matches 4 packages (ortho):
  NA-CANADA-EAST v0.1.0
  NA-CANADA-WEST v0.1.0
  NA-GREENLAND v0.1.0
  NA-USA-MX-CENTRAL v0.1.1

Proceed with install? [y/N]:
```

If one package in the batch fails to install, the rest still install and the
command reports how many failed. A code that selects nothing lists the codes
you could have used: the library's for `install`, and your installed packages
for `update`, `remove` and `info`.

A full code is still the start of every longer code, so `EU` selects `EU`,
`EU-WEST` and `EU-EAST` together. There is no way to name only the shorter
one while the longer ones are installed.

`install` and `remove` take several codes at once and act on everything any of
them selects, each package once, so `xearthlayer packages install NA-CANADA
AF` installs both halves of Canada and every Africa package as one batch. If
any of the codes selects nothing, the command stops before downloading or
removing anything. `--all` selects every package of the chosen type instead:
the whole library for `install`, everything installed for `remove`. It works
with every other option, and like any selection of several packages it is
listed and confirmed first (`remove --force` skips the confirmation).
`install --all` treats each package exactly as if you had named it, so a
package already installed at the library's version reports "already
installed" and the command ends with an error, while an older installed
version is replaced.

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

NA-SOCAL (ortho) v1.2.0
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_na-socal_ortho
  Size: 8.7 GB
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

While `xearthlayer run` is active, `update` is refused (see [While XEarthLayer is running](#while-xearthlayer-is-running)).

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

When a code selects several packages, every one of them is listed, and the
removal is checked as a whole before anything is deleted. Then a single prompt covers the set:

```
Are you sure you want to remove these 2 packages? [y/N]:
```

To skip the confirmation prompt:

```bash
xearthlayer packages remove eu-paris --force
```

`--force` skips only the prompt. While `xearthlayer run` is running, nothing is removed (see below).

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

`<REGION>` is a region code or the start of one; see
[Selecting Packages by Region Code](#selecting-packages-by-region-code).
`install` and `remove` take one or more, or `--all` instead.

| Option | Description |
|--------|-------------|
| `--library-url <URL>` | Override the library URL from config |
| `--install-dir <PATH>` | Override the installation directory |
| `--type <TYPE>` | Package type: `ortho` or `overlay` |

### install

```bash
xearthlayer packages install <REGION>... [OPTIONS]
xearthlayer packages install --all [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--all` | Install every package of the type in the library |
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
xearthlayer packages remove <REGION>... [OPTIONS]
xearthlayer packages remove --all [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--all` | Remove every installed package of the type |
| `-f, --force` | Skip confirmation prompt |
| `--type <TYPE>` | Package type (default: ortho) |

## Disk Space

Large packages are split into multiple parts for easier downloading. The parts download into the temp folder and are deleted as they are extracted, so the temp folder needs at least room for the largest download in a batch; more space lets downloads run further ahead of installs. Each package is extracted straight into a hidden folder beside the place it installs, so the disk holding your packages needs room for the extracted package, which XEarthLayer estimates at three times its download. An update needs room for the new version beside the old one until the swap. Temporary files are removed as soon as each package finishes, whether it succeeds or fails.

When a command installs or updates several packages, they download one after another without waiting for installs, while a second stage installs them in order, one at a time. A batch therefore takes about as long as its downloads. Each package is checked against both disks when its turn to download comes, counting the packages installed earlier in the same batch, which stay on the packages disk. When the temp folder is short of space for the moment, the download waits, shown as "waiting for disk space", and starts as soon as an install finishes. A package that the packages disk cannot hold, or whose download alone is larger than the temp folder's free space, fails before it downloads, and the rest of the batch continues.

When the temp folder and the packages folder are on the same disk, both needs add up on it.

The free space is measured when a batch starts. Other programs writing to the same disks during a long batch, including XEarthLayer's own tile cache when it shares a disk, are not seen until the next batch. A package that runs out of space anyway fails cleanly: its temporary files are removed and any version of it already installed is untouched.

### While XEarthLayer is running

`xearthlayer run` records its process ID in the instance lock, and the package commands read it. While that process is alive:

- `update` and `remove` refuse the whole batch before doing anything, with "XEarthLayer is running (pid N): stop it before updating or removing packages". `run` serves every installed package through one filesystem, so a package cannot change under it.
- `install` refuses each package that is already installed, at any version, with the same message, because installing over it is an update. Packages that are not installed install normally and the rest of the batch continues. A new package is picked up the next time `xearthlayer run` starts, and the command says so.

Only one package command runs at a time. A second one, or one started while another is running, stops with "another package operation is running".

No external tools are needed to unpack a package: archives are extracted inside XEarthLayer itself.

## Stopping an Install

Press Ctrl+C once to cancel: downloads stop, a package being replaced is left at its old version, and temporary files are removed before the command exits. Press Ctrl+C again to quit at once and leave the cleanup for the next package command. A system shutdown, `kill`, or closing the terminal also quits at once. In every case an installed package is always complete, either the old version or the new one, never neither, and the next package command removes whatever an interrupted one left behind. When a command removes several packages, the first Ctrl+C stops the rest of them too.

The one step that is never interrupted is the final swap of the new version for the old one, which takes moments. A forced quit (a second Ctrl+C, a shutdown, or a closed terminal) can leave a hidden, half-written folder beside the package, which the next package command removes.

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

If you also use SimHeaven X-World Pro, see [X-World Pro](x-world-pro.md) for how XEarthLayer leaves its overlays out where X-World Pro is active.

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
