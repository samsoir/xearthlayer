# Getting Started with XEarthLayer

This guide will help you get XEarthLayer up and running with X-Plane. By the end, you'll have satellite imagery scenery streaming in your simulator.

## How It Works

XEarthLayer provides satellite imagery through two components that work together:

1. **Regional Scenery Packages** - Small packages containing terrain definitions (what textures go where)
2. **Streaming Service** - Generates the actual textures on-demand from satellite imagery

You need **both**: the package tells X-Plane what to display, and the streaming service provides the imagery.

See [How It Works](how-it-works.md) for a detailed architectural explanation.

## Prerequisites

- **X-Plane 12** (X-Plane 11 may work but is untested)
- **Linux** (Windows and macOS support planned)
- **Internet connection** for streaming imagery
- Basic familiarity with the command line

## Automatic Telemetry

XEarthLayer connects to X-Plane automatically via the **Web API** -- no configuration required. When X-Plane is running, XEarthLayer reads aircraft position, heading, and speed directly from the simulator.

This enables:

- **Adaptive prefetching**: Tiles ahead of your flight path are pre-downloaded before you need them
- **Track-based band loading**: Tiles are prioritized based on your ground track
- **Flight phase detection**: Different strategies for ground ops vs cruise flight
- **Smoother scenery loading**: Tiles are often ready before X-Plane requests them

If X-Plane is not yet running when XEarthLayer starts, it will fall back to inferring position from FUSE file access patterns until the Web API connection is established.

## Installation

### From a Binary Release

The simplest option. Download the package for your distribution from the
[latest release](https://github.com/samsoir/xearthlayer/releases/latest):

| Distribution | Asset |
|---|---|
| Debian, Ubuntu | `xearthlayer_<version>_amd64.deb` |
| Fedora, RHEL | `xearthlayer-<version>.x86_64.rpm` |
| Anything else | `xearthlayer-v<version>-x86_64-linux.tar.gz` |
| Arch, CachyOS | Build from the AUR |

Prebuilt binaries require **glibc 2.34 or newer**. That covers Ubuntu 22.04 LTS,
Debian 12 and RHEL 9 onwards. Check yours with `ldd --version`. On anything
older, build from source.

Verify with:

```bash
xearthlayer --version
```

### From Source

Building needs **Rust 1.97.1 or newer** and the FUSE 3 development headers.

Install Rust through [rustup](https://rustup.rs) rather than your
distribution's package manager. Several dependencies require a recent
compiler, and most distributions ship one that is too old to build this
project at all. rustup also reads the repository's `rust-toolchain.toml` and
selects the correct version for you.

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# FUSE 3 headers
sudo apt install libfuse3-dev      # Debian, Ubuntu
sudo dnf install fuse3-devel       # Fedora, RHEL

# Clone, build and install
git clone https://github.com/samsoir/xearthlayer.git
cd xearthlayer
make install  # Builds, then installs to ~/.local/bin (no sudo required)

# Verify installation
xearthlayer --version
```

**Custom install location:**
```bash
make install PREFIX=/usr/local  # Requires sudo for /usr/local/bin
```

If the build stops with `rustc <version> is not supported by the following
packages`, your Rust is too old. Install rustup as above and start a new
shell so it takes precedence.

### GPU Encoding

GPU-accelerated DDS encoding offloads texture compression from the CPU,
freeing it for X-Plane. It needs no special build and is included in every
binary. Select it at runtime:

```ini
[texture]
compressor = gpu
```

Then confirm your hardware is detected:

```bash
xearthlayer diagnostics
```

The output lists the detected GPU adapters. Any wgpu-compatible GPU works,
which is most modern hardware from AMD, NVIDIA and Intel.

GPU encoding helps most when you have an idle integrated GPU, such as AMD
Radeon graphics on a Ryzen or Intel UHD, while your discrete card handles
X-Plane. See the [texture configuration](configuration.md#texture) for the
full list of compressor backends.

## Initial Setup

### Run the Setup Wizard (Recommended)

The easiest way to configure XEarthLayer is with the interactive setup wizard:

```bash
xearthlayer setup
```

The wizard will guide you through:

1. **X-Plane Custom Scenery** - Auto-detects your X-Plane 12 installation or lets you specify the path
2. **Package Location** - Where to store scenery packages (default: `~/.xearthlayer/packages`)
3. **Cache Location** - Where to store cached tiles with storage type detection (NVMe/SSD/HDD)
4. **System Configuration** - Recommends optimal memory and disk cache sizes based on your hardware

The wizard detects your system's CPU, memory, and storage type to recommend the best settings.

### Alternative: Manual Configuration

If you prefer manual setup:

```bash
xearthlayer init  # Creates ~/.xearthlayer/config.ini with defaults
```

Then edit `~/.xearthlayer/config.ini`:

```ini
[xplane]
scenery_dir = /path/to/X-Plane 12/Custom Scenery

[packages]
# library_url defaults to https://xearthlayer.app/packages/xearthlayer_package_library.txt
auto_install_overlays = true

[provider]
type = bing  # Options: bing, go2, google, apple, arcgis, mapbox, usgs
```

See the [Configuration Guide](configuration.md) for all available options.

## Step 1: Install a Regional Package

First, install a scenery package for the region you want to fly:

```bash
# Check available packages
xearthlayer packages check
```

```
Checking for package updates...

  EU (ortho) v0.1.0 - Not installed
  NA (ortho) v0.1.1 - Not installed

2 package(s) available for installation.
```

```bash
# Install a package
xearthlayer packages install eu
```

```
Installing eu-paris (ortho)...

Fetching library index...
Fetching package metadata...
Package: EU v0.1.0

Success: Installed EU (ortho) v0.1.0 to Custom Scenery/zzXEL_eu-paris_ortho
```

**Note:** The package is smaller than a regular Ortho4XP tile because it only contains terrain definitions, not textures.

## Step 2: Start XEarthLayer

Now start XEarthLayer to mount your packages and provide the textures:

```bash
xearthlayer  # Defaults to 'run' when no command specified
```

Or explicitly:

```bash
xearthlayer run
```

```
XEarthLayer v0.1.0
========================================

Packages:       /home/user/.xearthlayer/packages
Custom Scenery: /home/user/X-Plane 12/Custom Scenery
DDS Format:     BC1
Provider:       Bing Maps

Installed ortho packages (1):
  EU v0.1.0

Cache: 2 GB memory, 20 GB disk

Mounting packages to Custom Scenery...
  ✓ EU → /home/user/X-Plane 12/Custom Scenery/zzXEL_eu_ortho

Ready! 1 package(s) mounted

Start X-Plane to use XEarthLayer scenery.
Press Ctrl+C to stop.
```

The `run` command automatically:
- Discovers all installed ortho packages
- Mounts each package as a FUSE filesystem in Custom Scenery
- Generates DDS textures on-demand when X-Plane requests them

## Step 3: Fly!

Start X-Plane and load a flight in the Paris region.

**First visit:** There may be a brief delay (1-2 seconds per tile) as textures are downloaded and encoded. You might see magenta placeholder tiles momentarily.

**Subsequent visits:** Textures load instantly from cache.

## Stopping the Service

Complete your flight in X-Plane and close down the simulator. Once the sim has exited fully:

```bash
Press the 'q' key and follow the prompt to confirm
```

Always stop XEarthLayer cleanly before shutting down. Terminating the XEarthLayer process while X-Plane 12 is running will cause X-Plane to crash to desktop.

## Managing Multiple Regions

Install additional packages for other regions:

```bash
xearthlayer packages install na
xearthlayer packages install eu
```

All installed packages are mounted automatically when you run `xearthlayer run`.

## Updating Packages

Check for and install package updates:

```bash
# Check what's available
xearthlayer packages check

# Update a specific package
xearthlayer packages update eu

# Update all packages
xearthlayer packages update --all
```

## Next Steps

- [How It Works](how-it-works.md) - Understand the architecture
- [Configuration Guide](configuration.md) - Customize cache sizes, providers, and more
- [Package Management](package-management.md) - Detailed package operations
- [Running the Service](running-service.md) - Advanced streaming options
- [Content Publishing](content-publishing.md) - Create and share your own packages

## Quick Reference

```bash
# Setup
xearthlayer setup                     # Interactive setup wizard (recommended)
xearthlayer init                      # Create config file with defaults

# Packages
xearthlayer packages check            # See available packages
xearthlayer packages install <region> # Install a package
xearthlayer packages list             # List installed packages

# Running
xearthlayer                           # Start streaming (defaults to 'run')
xearthlayer run                       # Mount all packages with dashboard
xearthlayer run --airport KJFK        # Pre-warm cache around airport

# Cache
xearthlayer cache stats               # View cache usage
xearthlayer cache clear               # Clear cache
```
