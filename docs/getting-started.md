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

## Recommended: Enable X-Plane Telemetry

For the **best experience**, enable X-Plane's telemetry output. This allows XEarthLayer to predict where you're flying and pre-download textures before you need them, resulting in smoother scenery loading with fewer pop-ins.

### How to Enable (Strongly Recommended)

1. In X-Plane, go to **Settings** → **Network**
2. Find the **"XAVION, FOREFLIGHT OR OTHER EFB (GDL-90)"** section
3. In the "Enter IP address" field, type `127.0.0.1`
4. Click **"Add Connection to Xavion or other EFB"**

That's it! XEarthLayer will automatically receive your aircraft position and heading on UDP port 49002.

### What This Enables

- **Predictive prefetching**: XEarthLayer downloads tiles ahead of your flight path
- **Heading-aware caching**: Tiles in your direction of travel are prioritized
- **Smoother scenery loading**: Tiles are often ready before X-Plane requests them
- **Reduced stutter**: Fewer texture pop-ins during flight

### Without Telemetry

XEarthLayer still works without telemetry! It will:
- Generate textures on-demand as X-Plane requests them
- Use basic radial prefetching around recently requested tiles
- Cache textures for subsequent visits

However, you may notice more tile pop-ins when flying into new areas, especially at higher speeds.

## Installation

### From Source

```bash
# Clone the repository
git clone https://github.com/youruser/xearthlayer.git
cd xearthlayer

# Build the release binary
make release

# The binary is at target/release/xearthlayer
```

### From Binary Release

Download the latest release from the releases page and extract to a location of your choice.

## Initial Setup

### 1. Create Configuration File

Run the init command to create your configuration file:

```bash
xearthlayer init
```

This creates `~/.xearthlayer/config.ini` with sensible defaults. The command will attempt to auto-detect your X-Plane installation.

### 2. Configure Your Settings

Edit `~/.xearthlayer/config.ini`:

```ini
[xplane]
# Path to your X-Plane Custom Scenery folder
scenery_dir = /path/to/X-Plane 12/Custom Scenery

[packages]
# URL to a package library (get this from your scenery provider)
library_url = https://raw.githubusercontent.com/samsoir/xearthlayer-regional-scenery/main/xearthlayer_package_library.txt
auto_install_overlays = true

[provider]
# Satellite imagery source: bing, go2, or google
type = apple
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
xearthlayer init                      # Create config file

# Packages
xearthlayer packages check            # See available packages
xearthlayer packages install <region> # Install a package
xearthlayer packages list             # List installed packages

# Running
xearthlayer run                       # Mount all packages and start streaming

# Cache
xearthlayer cache stats               # View cache usage
xearthlayer cache clear               # Clear cache
```
