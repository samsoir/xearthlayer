# Running the Streaming Service

The XEarthLayer streaming service generates satellite imagery textures on-demand. It works alongside regional scenery packages to provide complete orthophoto scenery.

## Quick Start

The simplest way to run XEarthLayer is just:

```bash
xearthlayer
```

When no command is specified, XEarthLayer defaults to `run`. This automatically discovers all installed ortho packages and mounts them in your X-Plane Custom Scenery directory.

You can also explicitly use `xearthlayer run` if you prefer.

## How It Works

XEarthLayer creates a virtual filesystem (using FUSE) that overlays regional scenery packages. When X-Plane requests a texture file:

1. XEarthLayer intercepts the DDS request
2. Downloads satellite imagery tiles from the configured provider
3. Encodes them into DDS format
4. Returns the texture to X-Plane

Non-texture files (DSF, TER, etc.) pass through unchanged from the source package.

The result is cached so subsequent requests are instant.

## Prerequisites

- **Linux** with FUSE support (most distributions have this built-in)
- **Configuration completed** (run `xearthlayer setup` for first-time setup)
- **At least one ortho package installed** (use `xearthlayer packages install <region>`)
- **Internet connection** for downloading satellite imagery

**First-time users:** If you haven't configured XEarthLayer yet, running `xearthlayer` will display a welcome message with instructions to run the setup wizard.

## The `run` Command

The `run` command is the primary way to use XEarthLayer. Running `xearthlayer` with no arguments defaults to this command:

```bash
xearthlayer      # Same as 'xearthlayer run'
xearthlayer run  # Explicit form
```

Output:
```
XEarthLayer v0.2.11
========================================

Packages:       /home/user/.local/share/xearthlayer/packages
Custom Scenery: /home/user/X-Plane 12/Custom Scenery
DDS Format:     BC1
Provider:       Bing Maps
FUSE Backend:   fuse3 (async multi-threaded)

Installed ortho packages (2):
  EU v1.0.0
  NA v2.1.0

Cache: 2 GB memory, 20 GB disk

Mounting consolidated ortho scenery...
  ✓ zzXEL_ortho → /home/user/X-Plane 12/Custom Scenery/zzXEL_ortho
    Sources: 2 (0 patches, 2 packages)
    Files: 15234
    Packages:
      • EU
      • NA

  ✓ yzXEL_overlay → /home/user/X-Plane 12/Custom Scenery/yzXEL_overlay (2456 DSF files from 2 packages)

Ready! Consolidated ortho mount active (2 sources)

Start X-Plane to use XEarthLayer scenery.
Press Ctrl+C to stop.
```

### What `run` Does

1. Reads your configuration from `~/.config/xearthlayer/config.ini`
2. Discovers installed ortho packages from `install_location`
3. Creates a single consolidated FUSE mount (`zzXEL_ortho`) combining patches and all ortho packages
4. Creates consolidated overlay symlinks (`yzXEL_overlay`) for all overlay packages
5. Starts the texture streaming service with shared resources
6. Waits for Ctrl+C to cleanly unmount

### Options

| Option | Description |
|--------|-------------|
| `--airport <ICAO>` | Pre-warm cache around an airport before X-Plane loads (e.g., `KJFK`, `EGLL`) |
| `--provider <TYPE>` | Override imagery provider: `bing`, `go2`, `google` |
| `--dds-format <FMT>` | Override texture format: `bc1` or `bc3` |
| `--timeout <SECS>` | Override download timeout |
| `--parallel <NUM>` | Override parallel downloads |
| `--no-cache` | Disable caching (not recommended) |

### No Packages Installed

If no ortho packages are installed, `run` provides helpful guidance:

```
Error: No ortho packages installed

No ortho packages are installed.

To get started:
  1. View available packages:  xearthlayer packages list
  2. Install a region:         xearthlayer packages install <region>

Example:
  xearthlayer packages install na    # Install North America

Packages will be installed to: /home/user/.local/share/xearthlayer/packages
```

## Advanced: Single Package Mode (`start`)

For advanced users, the `start` command allows mounting a single scenery package manually. This is useful for:

- Testing non-package scenery (e.g., Ortho4XP output)
- Custom mount configurations
- Debugging

```bash
xearthlayer start --source /path/to/scenery
```

This creates a mount point at `/path/to/scenery_xel`.

### Options

| Option | Description |
|--------|-------------|
| `--source <PATH>` | Source scenery folder to overlay (required) |
| `--mountpoint <PATH>` | Custom mount point (default: `<source>_xel`) |
| `--provider <TYPE>` | Imagery provider: `bing`, `go2`, `google` |
| `--dds-format <FMT>` | Texture format: `bc1` or `bc3` |
| `--no-cache` | Disable caching (not recommended) |

### Example

```bash
xearthlayer start \
  --source "/home/user/Ortho4XP/Tiles/zOrtho4XP_+37-122" \
  --provider go2
```

When using `start`, you must manually add the `_xel` mount point to X-Plane's `scenery_packs.ini`.

## Cold Start Pre-warming

Use the `--airport` option to pre-load tiles around your departure airport before starting X-Plane. This dramatically reduces initial scenery load times.

```bash
xearthlayer run --airport KJFK
```

### How It Works

1. XEarthLayer parses X-Plane's airport database (`apt.dat`) to find the airport coordinates
2. Scans the SceneryIndex for all tiles within the configured radius (default: 100nm)
3. Downloads and caches tiles at both ZL12 and ZL14 zoom levels
4. Shows progress in the dashboard, then transitions to normal operation

### Why 100nm?

The default 100nm radius is optimized to match X-Plane's DSF loading behavior:

| X-Plane Setting | DSF Tiles Loaded | Approximate Radius |
|-----------------|------------------|-------------------|
| Standard | 3×2 = 6 tiles | ~90nm |
| Extended DSFs | 4×3 = 12 tiles | ~120nm |

X-Plane loads terrain in 1° × 1° DSF tiles centered on the aircraft. With standard settings, only 6 tiles are loaded initially—pre-warming beyond this radius wastes bandwidth on tiles X-Plane won't request until you fly further.

### Examples

```bash
# Pre-warm around JFK before flying
xearthlayer run --airport KJFK

# Pre-warm around Heathrow with Google imagery
xearthlayer run --airport EGLL --provider go2

# Pre-warm around Zurich
xearthlayer run --airport LSZH
```

### Cancelling Pre-warm

Press `c` during pre-warm to cancel and proceed directly to normal operation. Pre-warm is also automatically cancelled if X-Plane makes a FUSE request (flight started).

### Configuration

The pre-warm radius can be adjusted in `~/.config/xearthlayer/config.ini`:

```ini
[prewarm]
radius_nm = 100  ; Default: covers standard DSF loading
; radius_nm = 150  ; Use for Extended DSFs
```

See [Configuration](configuration.md#prewarm) for more details on X-Plane's DSF loading behavior.

## Stopping the Service

Press `Ctrl+C` to stop the service cleanly:

```
^C
Shutting down...
```

**Important:** Always stop the service properly before:
- Shutting down your computer
- Running X-Plane maintenance

## Performance

### First Visit

The first time you fly over an area, textures are downloaded and encoded on-demand. Expect:

- **Good network**: 1-2 seconds per tile
- **Poor network**: May see magenta placeholder tiles temporarily

### Cached Tiles

Subsequent visits load instantly from cache:

- **Memory cache**: ~10ms
- **Disk cache**: ~50-100ms

### Parallel Processing

XEarthLayer downloads and encodes multiple tiles in parallel:

- Default: 8 worker threads
- Each tile: 256 chunk downloads

Configure in `~/.config/xearthlayer/config.ini`:

```ini
[generation]
threads = 8

[download]
parallel = 32
```

## Caching

### Cache Locations

| Cache | Location | Default Size |
|-------|----------|--------------|
| Memory | RAM | 2 GB |
| Disk | `~/.cache/xearthlayer/` | 20 GB |

### Cache Structure

```
~/.cache/xearthlayer/
├── Bing Maps/
│   └── <zoom>/<row>/<col>_bc1.dds
└── Google Maps/
    └── <zoom>/<row>/<col>_bc1.dds
```

### Managing Cache

View cache statistics:

```bash
xearthlayer cache stats
```

Clear the cache:

```bash
xearthlayer cache clear
```

Clear only disk cache (preserve memory):

```bash
xearthlayer cache clear --disk
```

### Scenery Index Cache

The scenery index is a metadata cache that stores information about which tiles exist in your installed packages. This enables efficient prefetching during flight. The index is automatically built on first run and cached to disk for faster subsequent startups.

View scenery index status:

```bash
xearthlayer scenery-index status
```

Rebuild the index after installing new packages:

```bash
xearthlayer scenery-index update
```

Clear the index cache (forces rebuild on next run):

```bash
xearthlayer scenery-index clear
```

## Imagery Providers

Configure the default provider in `~/.config/xearthlayer/config.ini` or override at runtime:

### Bing Maps (Default)

```bash
xearthlayer run --provider bing
```

- Free, no API key required
- Good global coverage
- Recommended for most users

### Google GO2

```bash
xearthlayer run --provider go2
```

- Free, no API key required
- Same imagery as Ortho4XP's GO2 provider
- Best compatibility with Ortho4XP-generated scenery

### Google Maps API

```bash
xearthlayer run --provider google --google-api-key YOUR_API_KEY
```

- Requires paid API key
- Rate limited (15,000 requests/day)
- Not recommended for regular use

## Timeout and Placeholders

If a tile takes too long to generate (network issues, server problems), XEarthLayer returns a magenta placeholder texture to prevent X-Plane from hanging.

Configure timeout in config:

```ini
[generation]
timeout = 10  # seconds
```

Placeholder tiles are not cached and will be retried on next request.

## Running as a Background Service

### Using systemd

Create `/etc/systemd/user/xearthlayer.service`:

```ini
[Unit]
Description=XEarthLayer Streaming Service
After=network.target

[Service]
Type=simple
ExecStart=/path/to/xearthlayer run
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable and start:

```bash
systemctl --user enable xearthlayer
systemctl --user start xearthlayer
```

### Auto-start on Login

Add to your shell profile (`~/.bashrc` or `~/.zshrc`):

```bash
# Start XEarthLayer if not already running
pgrep -x xearthlayer > /dev/null || xearthlayer run &
```

## Upgrading from v0.2.10 or Earlier

Version 0.2.11 introduces **consolidated mounting**, which changes how scenery is presented to X-Plane.

### What Changed

| Before (≤0.2.10) | After (≥0.2.11) |
|------------------|-----------------|
| Multiple mounts: `zzXEL_na_ortho`, `zzXEL_eu_ortho`, etc. | Single mount: `zzXEL_ortho` |
| Separate patches mount: `zzyXEL_patches_ortho` | Patches included in `zzXEL_ortho` |
| Multiple overlay folders: `yzXEL_na_overlay`, `yzXEL_eu_overlay` | Single folder: `yzXEL_overlay` |

### Benefits

- **Reduced resource usage**: Single FUSE session instead of N sessions
- **Simpler scenery_packs.ini**: Only two entries needed
- **Unified caching**: Shared memory and disk cache across all regions
- **Better patches integration**: Patches automatically have highest priority

### Upgrade Steps

1. **Stop XEarthLayer** if running:
   ```bash
   # Press Ctrl+C in the terminal running XEarthLayer
   ```

2. **Clean up old mount points** in X-Plane's Custom Scenery:
   ```bash
   cd "/path/to/X-Plane 12/Custom Scenery"
   rm -rf zzXEL_*_ortho zzyXEL_patches_ortho yzXEL_*_overlay
   ```

3. **Update `scenery_packs.ini`**: Remove old XEL entries and add the new consolidated ones:
   ```
   SCENERY_PACK Custom Scenery/yzXEL_overlay/
   SCENERY_PACK Custom Scenery/zzXEL_ortho/
   ```

4. **Run the new version**:
   ```bash
   xearthlayer run
   ```

### Automatic Cleanup

The first time you run v0.2.11+, XEarthLayer will attempt to clean up old mount points automatically. If cleanup fails (e.g., permission issues), you may need to remove them manually as shown above.

## Logging

View logs in real-time:

```bash
tail -f ~/.local/state/xearthlayer/xearthlayer.log
```

Log location can be configured:

```ini
[logging]
file = ~/.local/state/xearthlayer/xearthlayer.log
```

## Common Issues

### Mount point busy

```
Error: Mount point is busy or already mounted
```

Solution: Stop any existing XEarthLayer instance or unmount manually:

```bash
fusermount -u /path/to/mount_xel
```

### Permission denied

```
Error: Permission denied when creating mount
```

Solution: Ensure you have FUSE permissions:

```bash
# Add yourself to the fuse group
sudo usermod -aG fuse $USER
# Log out and back in
```

### X-Plane doesn't see the scenery

Ensure:
1. The mount point is in your `scenery_packs.ini`
2. X-Plane was restarted after adding the entry
3. The service is running before starting X-Plane

### Magenta tiles everywhere

This indicates network or timeout issues:

1. Check your internet connection
2. Try a different provider
3. Increase the timeout in config
4. Check logs for specific errors
