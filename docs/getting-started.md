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
library_url = https://example.com/xearthlayer_package_library.txt

[provider]
# Satellite imagery source: bing, go2, or google
type = bing
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

  EU-PARIS (ortho) v1.0.0 - Not installed
  NA-SOCAL (ortho) v2.1.0 - Not installed

2 package(s) available for installation.
```

```bash
# Install a package
xearthlayer packages install eu-paris
```

```
Installing eu-paris (ortho)...

Fetching library index...
Fetching package metadata...
Package: EU-PARIS v1.0.0

Success: Installed EU-PARIS (ortho) v1.0.0 to Custom Scenery/zzXEL_eu-paris_ortho
```

**Note:** The package is small (megabytes) because it only contains terrain definitions, not textures.

## Step 2: Start the Streaming Service

Now start XEarthLayer to provide the textures:

```bash
xearthlayer start --source "Custom Scenery/zzXEL_eu-paris_ortho"
```

```
XEarthLayer Streaming Service
=============================

Source:    /path/to/Custom Scenery/zzXEL_eu-paris_ortho
Mount:     /path/to/Custom Scenery/zzXEL_eu-paris_ortho_xel
Provider:  Bing Maps
Cache:     ~/.cache/xearthlayer

Service started. Press Ctrl+C to stop.
```

This creates a virtual mount at `zzXEL_eu-paris_ortho_xel` that:
- Passes through all files from the source package
- Generates DDS textures on-demand when X-Plane requests them

## Step 3: Configure X-Plane

Add the **mount point** (not the source) to X-Plane:

1. Edit `X-Plane 12/Custom Scenery/scenery_packs.ini`
2. Add the `_xel` mount point:
   ```
   SCENERY_PACK Custom Scenery/zzXEL_eu-paris_ortho_xel/
   ```
3. Restart X-Plane if it's running

**Important:** Use the `_xel` mount point, not the original package folder.

## Step 4: Fly!

Start X-Plane and load a flight in the Paris region.

**First visit:** There may be a brief delay (1-2 seconds per tile) as textures are downloaded and encoded. You might see magenta placeholder tiles momentarily.

**Subsequent visits:** Textures load instantly from cache.

## Stopping the Service

When you're done flying:

```bash
# Press Ctrl+C in the terminal running XEarthLayer
^C
Unmounting...
Service stopped.
```

Always stop XEarthLayer cleanly before shutting down.

## Managing Multiple Regions

Install additional packages for other regions:

```bash
xearthlayer packages install na-socal
xearthlayer packages install eu-alps
```

You can run XEarthLayer with multiple source folders, or start multiple instances (one per region).

## Updating Packages

Check for and install package updates:

```bash
# Check what's available
xearthlayer packages check

# Update a specific package
xearthlayer packages update eu-paris

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

# Streaming
xearthlayer start --source <path>     # Start streaming service

# Cache
xearthlayer cache stats               # View cache usage
xearthlayer cache clear               # Clear cache
```
