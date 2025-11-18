# AutoOrtho Architecture Analysis
## Comprehensive Documentation for Rust Migration

## Github Reference
- [Autoortho Project Repository](https://github.com/kubilus1/autoortho)

---

## Table of Contents
1. [Overview](#overview)
2. [The Big Idea](#the-big-idea)
3. [Five-Layer Architecture](#five-layer-architecture)
4. [Scenery Package System](#scenery-package-system)
5. [Complete Directory Structure](#complete-directory-structure)
6. [Data Flow](#data-flow)
7. [Key Components](#key-components)
8. [Coordinate System](#coordinate-system)
9. [Threading Model](#threading-model)
10. [Caching Strategy](#caching-strategy)
11. [Key Technical Details](#key-technical-details)
12. [Code Statistics](#code-statistics)

---

## Overview

**AutoOrtho** is a Python-based X-Plane integration tool that provides on-demand satellite imagery streaming. Rather than requiring gigabytes of pre-downloaded satellite photos, it dynamically fetches only needed imagery as the user flies, using multiple satellite imagery sources.

**Key Innovation**: Uses FUSE (Filesystem in Userspace) to create a virtual filesystem that intercepts X-Plane's texture file requests and generates them on-demand.

**Key Stats:**
- ~6,232 lines of Python code across 22 files
- Multi-platform support (Linux, Windows, macOS)
- Python 3.12+ requirement
- Hybrid architecture: Python frontend + C/C++ image processing

---

## The Big Idea

### The Problem AutoOrtho Solves

X-Plane flight simulator normally requires users to download massive scenery packages (20-30GB per region) containing pre-rendered satellite imagery. This is expensive in terms of storage and you need to download everything upfront, even for areas you might never fly over.

### AutoOrtho's Solution

Instead of storing all that imagery locally, AutoOrtho creates a "virtual" scenery pack. When X-Plane asks for a texture file, AutoOrtho downloads just that specific imagery on-demand from satellite providers (Bing Maps, NAIP, EOX Sentinel-2, USGS, etc.) and serves it to X-Plane as if it were a real file on disk.

### How It Works - The Magic Trick

AutoOrtho uses **FUSE** to create what looks like a normal directory to X-Plane, but it's actually a virtual filesystem controlled by Python code:

```
X-Plane thinks:                    Reality:
├── Custom Scenery/               ├── Custom Scenery/
│   ├── z_autoortho/              │   ├── z_autoortho/  ← FUSE mount point
│   │   ├── textures/             │   │   ├── textures/  ← Virtual directory
│   │   │   ├── tile_001.dds      │   │   │   ├── [Python generates on-the-fly]
│   │   │   ├── tile_002.dds      │   │   │   ├── [Downloads from Bing/NAIP]
│   │   │   └── tile_003.dds      │   │   │   └── [Compresses to DDS format]
```

---

## Five-Layer Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Layer 1: X-Plane Flight Simulator                     │
│  - Reads texture files for terrain rendering           │
│  - Thinks it's reading from a normal disk drive        │
└────────────────────┬────────────────────────────────────┘
                     │ OS file read()
                     ▼
┌─────────────────────────────────────────────────────────┐
│  Layer 2: FUSE Virtual Filesystem (autoortho_fuse.py)  │
│  - Intercepts X-Plane's file operations                │
│  - Parses filename to extract coordinates & zoom level  │
│  - Returns fake file metadata (size, permissions)      │
│  - Handles actual read() calls by fetching data        │
└────────────────────┬────────────────────────────────────┘
                     │ Request tile at (row, col, zoom)
                     ▼
┌─────────────────────────────────────────────────────────┐
│  Layer 3: Tile Manager (getortho.py - TileCacher)      │
│  - Manages in-memory cache of tiles                    │
│  - Evicts old tiles when memory limit reached          │
│  - Coordinates between file reads and downloads        │
└────────────────────┬────────────────────────────────────┘
                     │ Need tile data
                     ▼
┌─────────────────────────────────────────────────────────┐
│  Layer 4: Download Orchestrator (getortho.py - Chunk)  │
│  - Each tile = 16×16 chunks (256 individual images)    │
│  - 32 worker threads download chunks in parallel       │
│  - Checks disk cache before downloading                │
│  - Assembles chunks into complete 4096×4096 tile       │
└────────────────────┬────────────────────────────────────┘
                     │ HTTP requests
                     ▼
┌─────────────────────────────────────────────────────────┐
│  Layer 5: Satellite Imagery Providers                  │
│  - Bing Maps, NAIP, EOX Sentinel-2, USGS, etc.        │
│  - Return JPEG images (256×256 pixels each)            │
└─────────────────────────────────────────────────────────┘
```

---

## Scenery Package System

### The Critical Missing Piece

X-Plane needs **DSF files** (Digital Scenery Format) that tell it:
- What terrain geometry to use
- Which texture files to load for each area

**AutoOrtho is a two-part system:**
1. **Scenery packages** (DSF + terrain files) - Tell X-Plane what to load
2. **FUSE virtual filesystem** - Dynamically generates the textures those DSF files reference

### How Scenery Packages Work

**Pre-built packages from GitHub:**
- Repository: `kubilus1/autoortho-scenery`
- Managed by: `downloader.py` (OrthoManager class)
- Size: ~50-500MB per region (vs 20-30GB with imagery!)

**What's INCLUDED in downloads:**
- ✅ DSF files (terrain mesh definitions)
- ✅ Terrain files (.ter)
- ✅ Metadata (region info, version)
- ✅ Some overlay PNGs (coastlines, etc.)

**What's NOT included:**
- ❌ Satellite imagery DDS files (that's what AutoOrtho generates on-demand!)

### Scenery Package Creation

Created using **Ortho4XP** tool with these settings:
```python
skip_downloads = True      # Don't download satellite imagery!
high_zl_airports = True    # High detail around airports
```

This generates DSF files that reference textures like `+40-112_BI16.dds` but doesn't download the actual imagery.

---

## Complete Directory Structure

### After Installation

```
X-Plane Installation/
├── Custom Scenery/
│   ├── scenery_packs.ini          ← X-Plane reads this to know what scenery exists
│   └── z_autoortho/
│       ├── scenery/               ← Downloaded scenery packages stored here
│       │   ├── NorthAmerica_1/    ← Example: Pre-built scenery package
│       │   │   ├── Earth nav data/   ← DSF FILES (real files on disk)
│       │   │   │   ├── +40-120/
│       │   │   │   │   ├── +40-112.dsf   ← "Use texture +40-112_BI16.dds"
│       │   │   │   │   ├── +40-113.dsf
│       │   │   │   │   └── ...
│       │   │   │   ├── +40-128/
│       │   │   │   └── ...
│       │   │   └── terrain/          ← TERRAIN FILES (real files on disk)
│       │   │       ├── terrain_01.ter
│       │   │       └── ...
│       │   │
│       │   └── Europe_1/          ← Another scenery package
│       │       ├── Earth nav data/
│       │       └── terrain/
│       │
│       └── [region]_info.json     ← Metadata about installed scenery
│
├── Custom Scenery/
│   ├── NorthAmerica_1/            ← FUSE MOUNT POINT
│   │   ├── Earth nav data/        ← Symlink/passthrough to z_autoortho/scenery/NorthAmerica_1/
│   │   ├── terrain/               ← Symlink/passthrough to z_autoortho/scenery/NorthAmerica_1/
│   │   └── textures/              ← VIRTUAL! Created by FUSE, doesn't exist on disk
│   │       └── +40-112_BI16.dds   ← Virtual file, generated on-demand
│   │
│   └── Europe_1/                  ← Another FUSE mount point
│       └── textures/              ← Also virtual
```

### FUSE Mount Strategy

For each scenery package, AutoOrtho creates a FUSE mount:
- **Root** = `/Custom Scenery/z_autoortho/scenery/NorthAmerica_1/` (real files)
- **Mountpoint** = `/Custom Scenery/NorthAmerica_1/` (FUSE virtual filesystem)

The FUSE filesystem acts as a **pass-through** for most files:
- DSF files in `Earth nav data/` → passed through from real disk
- Terrain files in `terrain/` → passed through from real disk
- **But** `.dds` files in `textures/` → **intercepted and generated on-demand**

---

## Data Flow

### Complete Request Flow

```
Step 1: User downloads scenery package
   ↓
[downloader.py]
   ├─ Fetches from: github.com/kubilus1/autoortho-scenery/releases
   ├─ Downloads ZIP files containing DSF + terrain files (NO imagery)
   ├─ Extracts to: Custom Scenery/z_autoortho/scenery/[RegionName]/
   └─ Size: ~50-500MB per region (vs 20-30GB with imagery!)

Step 2: AutoOrtho starts up
   ↓
[autoortho.py - AOConfig]
   ├─ Scans: Custom Scenery/z_autoortho/scenery/
   ├─ Finds: ["NorthAmerica_1", "Europe_1", ...]
   └─ Creates scenery_mounts list:
       [
         {
           "root": "Custom Scenery/z_autoortho/scenery/NorthAmerica_1",
           "mount": "Custom Scenery/NorthAmerica_1"
         },
         ...
       ]

Step 3: FUSE mount for each scenery
   ↓
[autoortho_fuse.py - AutoOrtho class]
   ├─ Creates FUSE filesystem at mountpoint
   ├─ Root directory = real scenery files on disk
   └─ Intercepts file operations:
       ├─ readdir("Earth nav data") → Pass through to real directory
       ├─ read("+40-112.dsf") → Pass through to real file
       ├─ readdir("textures") → Return virtual directory listing
       └─ read("+40-112_BI16.dds") → ⚡ INTERCEPT! Generate on-demand

Step 4: X-Plane loads scenery
   ↓
X-Plane reads: Custom Scenery/NorthAmerica_1/Earth nav data/+40-120/+40-112.dsf
   ↓
DSF file contains terrain instructions:
   "At coordinates +40-112, use texture: ../textures/+40-112_BI16.dds"
   ↓
X-Plane opens: Custom Scenery/NorthAmerica_1/textures/+40-112_BI16.dds
   ↓
FUSE intercepts this file open:
   ├─ Parses filename: row=40, col=112, maptype="BI", zoom=16
   ├─ Creates Tile object
   ├─ Downloads 256 chunks from Bing Maps
   ├─ Assembles into 4096×4096 DDS file
   └─ Returns bytes to X-Plane
```

### Detailed Tile Request Flow

```
X-Plane Texture Read Request
        │
        ▼
FUSE getattr(path) → Check DDS regex pattern
        │
        ├─ Non-DDS: Pass through to OS
        │
        └─ DDS Match: Extract (row, col, maptype, zoom)
            │
            ▼
        TileCacher._get_tile(row, col, maptype, zoom)
            │
            ├─ Cache hit? Return cached Tile
            │
            └─ Cache miss? Create new Tile()
                │
                ▼
            Tile.get_bytes(offset, length)
                │
                ▼
            Determine required mipmap level from offset
                │
                ├─ Already have mipmap? Return data
                │
                └─ Need to fetch:
                    │
                    ▼
                Tile._create_chunks(quick_zoom)
                    │
                    ▼
                ChunkGetter worker thread pool
                    │
                    ├─ Check local cache
                    ├─ HTTP GET from map source
                    └─ Decompress JPEG → AoImage
                        │
                        ▼
                    Compose 16x16 chunks into Tile
                        │
                        ▼
                    Compress to DDS with mipmaps
                        │
                        ▼
                    Return requested byte range to X-Plane
```

---

## Key Components

### 1. FUSE Layer (autoortho_fuse.py - 511 lines)

**Class: AutoOrtho(Operations)**
- Inherits from `refuse.high.Operations`
- Implements filesystem operations: `getattr()`, `read()`, `readdir()`, etc.
- Pattern matches DDS/KTX2/DSF/TER files with regex:
  - DDS: `r".*/(\d+)[-_](\d+)[-_]((?!ZL)\S*)(\d{2}).dds"`
  - Extracts: col, row, maptype, zoomlevel
- Thread-safe with RLock
- LRU cache for frequent metadata queries

**File Pattern Recognition:**
- Detects satellite texture requests via regex
- Extracts coordinates and zoom from filename
- Example: `+40-120_BI16.dds` → row=40, col=120, maptype="BI", zoom=16

### 2. Tile/Chunk Management (getortho.py - 1080 lines)

**Class: Chunk(object)**
- Represents a single 256×256 tile from remote servers
- Maps to geographic coordinates + zoom level + map type
- Handles individual HTTP requests
- Caches raw JPEG data locally
- Priority-based queue system

**Class: Tile(object)**
- Aggregates 16×16 Chunks (4096×4096 pixels)
- Manages DDS file format with mipmaps (5 levels)
- Handles lazy loading by mipmap level
- Thread-safe ref-counting for lifecycle management
- Tracks read offsets for efficient partial loading

**Class: TileCacher(object)**
- In-memory tile cache with eviction
- Memory limit enforcement (1GB default, tuned per platform)
- Tile count limits (25-50 depending on OS)
- Daemon cleanup thread every 15 seconds
- Uses `psutil` to monitor process memory

**Class: ChunkGetter(Getter)**
- 32 worker threads (configurable)
- Priority queue for chunk requests
- Respects maxwait timeout (0.5s default)
- Fallback to lower zoom levels if timeout occurs
- Request session pooling with HTTP keep-alive

### 3. Configuration (aoconfig.py - 136 lines)

**Class: AOConfig(object)**
- INI-based configuration system
- Reads from `~/.autoortho` (home directory)
- Sections: general, paths, autoortho, pydds, scenery, fuse, flightdata, cache, windows
- Auto-detects scenery installations
- Provides defaults and persists user settings

### 4. Application Control (autoortho.py - 365 lines)

**Class: AOMount(object)**
- Manages FUSE mount lifecycle
- Platform-specific setup (Windows Dokan/WinFSP vs Linux FUSE)
- Multi-threaded mount initialization
- Diagnostic checks post-mount

**Mount Strategy:**
```python
scenery_mounts = [{
    "root": os.path.join(ao_scenery_path, scenery_name),
    "mount": os.path.join(xplane_custom_scenery_path, scenery_name)
} for scenery_name in installed_sceneries]
```

### 5. Scenery Package Management (downloader.py - 799 lines)

**Class: OrthoManager(object)**
- Fetches scenery metadata from GitHub API
- URL: `https://api.github.com/repos/kubilus1/autoortho-scenery/releases`
- Caches release info locally

**Class: Region(object)**
- Represents a geographic region/scenery set
- Manages multiple releases/versions

**Class: Release(object)**
- Specific version of a region
- Handles download, installation, uninstallation

**Class: Package(object)**
- Individual downloadable file
- Handles multi-part ZIP downloads
- Hash verification

### 6. DDS Compression (pydds.py - 544 lines)

**Class: DDS(Structure)**
- DirectX texture format handler
- Converts RGBA to BC1/BC3 (DXT1/DXT5) compression
- Generates 5-level mipmap chain
- Uses ISPC or STB DXT compressors via ctypes
- Calculates mipmap offsets for partial reads

### 7. Image Processing (aoimage/ - C library)

**AoImage (C + Python wrapper)**
- JPEG reading/writing with libjpeg-turbo
- Image scaling, reduction (by 2x), cropping, pasting
- Direct memory access for performance
- Avoids Python byte copy overhead

---

## Coordinate System

### Web Mercator Projection

The system uses the **Web Mercator** (Slippy Map) projection - same as Google Maps:

```
World at zoom level 0:  1×1 tile  (entire Earth)
World at zoom level 1:  2×2 tiles
World at zoom level 2:  4×4 tiles
...
World at zoom level 16: 65536×65536 tiles

Each tile covers:
- Zoom 12: ~19km × 19km
- Zoom 16: ~1.2km × 1.2km (typical for X-Plane)
- Zoom 17: ~600m × 600m (high detail)
```

### Tile Grid Structure

```
Tile Grid (Slippy Map / Web Mercator):
- col (x): 0 to 2^zoom
- row (y): 0 to 2^zoom
- zoom: 0-18+ (higher = more detail)

Each Tile = 16×16 Chunks
Each Chunk = 256×256 pixels (standard tile size)
Each Tile = 4096×4096 pixels
```

### Coordinate Conversion

**Geographic to Tile Coordinates:**
```python
def deg2num(lat_deg, lon_deg, zoom):
    """Convert latitude/longitude to tile coordinates"""
    lat_rad = math.radians(lat_deg)
    n = 2.0 ** zoom
    col = int((lon_deg + 180.0) / 360.0 * n)
    row = int((1.0 - math.asinh(math.tan(lat_rad)) / math.pi) / 2.0 * n)
    return (row, col)
```

**Bing Maps Quadkey Conversion:**
```python
def _gtile_to_quadkey(til_x, til_y, zoomlevel):
    """Google tile coords → Bing quadkey string"""
    # Used for Bing Maps API compatibility
```

---

## Threading Model

### Thread Types

1. **Main FUSE thread**
   - Handles X-Plane file operations
   - Blocks on FUSE event loop indefinitely
   - LRU cache prevents thrashing

2. **Download Workers** (32 default)
   - Pull Chunks from PriorityQueue
   - HTTP requests with requests library
   - Session pooling for connection reuse
   - Handle retries with exponential backoff

3. **Tile Cleanup Thread**
   - Daemon thread in TileCacher
   - Runs every 15 seconds
   - Evicts LRU tiles when memory limit exceeded
   - Respects ref-count (only evicts when refs=0)

4. **Flight Tracker Thread**
   - UDP listener for X-Plane data (port 49000)
   - Handles socket timeouts
   - Tracks aircraft position, altitude, heading, speed
   - Powers web dashboard

5. **Web UI Thread** (Optional)
   - Flask + SocketIO
   - Serves dashboard at localhost:5000
   - Real-time map updates

### Synchronization Primitives

- **TileCacher**: `threading.RLock()` for tile dictionary access
- **Tile**: `threading.RLock()` for chunk dictionary
- **Chunk**: `threading.Event()` for ready signaling
- **FlightTracker**: Simple atomic assignment for position data
- **FUSE**: Inherent thread safety from refuse library

---

## Caching Strategy

### Three-Tier Cache System

```
┌─────────────────────────────────────────────────────────┐
│  Memory Cache (Layer 1) - Hot tiles                     │
│  - 1-2GB in RAM                                         │
│  - 25-50 tiles (depending on OS)                        │
│  - LRU eviction when full                               │
│  - Instant access                                       │
└─────────────────────┬───────────────────────────────────┘
                      │ Cache miss
                      ▼
┌─────────────────────────────────────────────────────────┐
│  Disk Cache (Layer 2) - Chunks as JPEG                 │
│  - 30GB on disk (configurable)                          │
│  - Individual chunk files: 120_40_16_BI.jpg             │
│  - ~100-200KB per chunk                                 │
│  - Validated with JPEG magic bytes (FFD8FF)             │
└─────────────────────┬───────────────────────────────────┘
                      │ Cache miss
                      ▼
┌─────────────────────────────────────────────────────────┐
│  Remote Servers (Layer 3) - Satellite imagery          │
│  - Bing Maps, NAIP, EOX, USGS, etc.                    │
│  - HTTP requests with retry logic                       │
│  - Connection pooling                                   │
└─────────────────────────────────────────────────────────┘
```

### DDS Mipmap Structure

```
DDS Mipmaps (5 levels for 4096×4096):
- Level 0: 4096×4096 (256KB blocks for BC1)
- Level 1: 2048×2048 (64KB blocks)
- Level 2: 1024×1024 (16KB blocks)
- Level 3: 512×512 (4KB blocks)
- Level 4: 256×256 (1KB blocks)
```

### Progressive Loading Optimization

**The Smart Part:**
Instead of downloading full 4096×4096 textures, AutoOrtho:
1. Starts with low-res mipmap (256×256) - only 1 chunk
2. If X-Plane reads higher offsets → fetches higher mipmap
3. Timeout mechanism: waits `maxwait` seconds for full res, falls back to lower

**Benefits:**
- Minimal startup stutter
- Progressive quality improvement
- User sees something immediately

### Memory Management

```
Memory Limit: 1GB (Windows: 2GB+)
Tile Limit: 25-50 concurrent tiles
Cleanup: Automatic via daemon thread every 15s
LRU Eviction: When memory > limit AND ref_count == 0
```

---

## Key Technical Details

### Satellite Imagery Sources

Configured in `getortho.py` Chunk class:

- **EOX (Sentinel-2)**: `https://a.s2maps-tiles.eu/wmts?layer=s2cloudless-2023_3857&...`
- **BI (Bing Maps)**: `https://ecn.t0.tiles.virtualearth.net/tiles/a{quadkey}.jpeg`
- **NAIP**: `http://naip.maptiles.arcgis.com/arcgis/rest/services/NAIP/MapServer/tile/...`
- **USGS**: `https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/...`
- **ArcGIS**: `http://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/...`
- **Firefly**: `https://fly.maptiles.arcgis.com/arcgis/rest/services/World_Imagery_Firefly/...`

### Configuration System

**INI File Format** (`~/.autoortho`):

```ini
[general]
gui = True
showconfig = True
debug = False

[paths]
xplane_path = <path>
scenery_path = <path>
cache_dir = ~/.autoortho-data/cache
download_dir = ~/.autoortho-data/downloads

[autoortho]
min_zoom = 12
maxwait = 0.5          # Timeout for high-res downloads (seconds)
maptypes = ['Null', 'BI', 'NAIP', 'EOX', 'USGS', 'Firefly']
fetch_threads = 32

[pydds]
compressor = ISPC      # or STB
format = BC1           # or BC3 (DXT1 or DXT5)

[fuse]
threading = True

[flightdata]
webui_port = 5000
xplane_udp_port = 49000

[cache]
file_cache_size = 30   # GB (minimum 10GB)

[windows]
prefer_winfsp = False
```

### X-Plane UDP Integration

**Protocol**: Custom UDP listener
- **Port**: 49000 (configurable)
- **Command**: `RREF` (request dataref values)
- **Response**: 8-byte packets (index, float)
- **Datarefs**: latitude, longitude, altitude, heading, speed

### Web Dashboard

**Stack**: Flask + SocketIO + eventlet
- **URL**: `http://localhost:5000/`
- **Endpoints**:
  - `/` - Live flight map
  - `/map` - Map view
  - `/stats` - Statistics charts
  - `/metrics` - JSON metrics endpoint

**Real-time updates**: SocketIO pushes aircraft position to browser

### Error Handling

**Network Errors:**
- HTTP 4xx/5xx: Log, track error rate, retry
- Timeout: Fallback to lower zoom
- DNS: Handled by requests library

**Corruption Detection:**
- JPEG magic byte validation (FFD8FF)
- ZIP file integrity with hash verification
- DSF/TER files: pass-through (no validation)

**Memory Pressure:**
- Automatic LRU eviction
- Configurable limits per platform
- psutil monitoring

### Platform-Specific Notes

**Linux:**
- Native FUSE support
- Requires `user_allow_other` in `/etc/fuse.conf`
- Standard mount/unmount operations

**Windows:**
- Requires Dokan OR WinFSP (third-party FUSE)
- Dynamic library loading via ctypes
- Larger cache limits (50 tiles vs 25)

**macOS:**
- Theoretically supported via OSXFUSE
- Currently untested in production

---

## Code Statistics

### Project Structure

```
autoortho/
├── __main__.py                 # Entry point, logging setup
├── autoortho.py                # Main orchestrator (365 lines)
├── autoortho_fuse.py           # FUSE filesystem (511 lines)
├── aoconfig.py                 # Configuration (136 lines)
├── getortho.py                 # Tile/chunk download (1080 lines)
├── downloader.py               # Scenery packages (799 lines)
├── pydds.py                    # DDS compression (544 lines)
├── config_ui.py                # GUI (532 lines)
├── flighttrack.py              # Flight tracking (210 lines)
├── aostats.py                  # Statistics tracking
├── xp_udp.py                   # X-Plane UDP protocol
├── version.py                  # Version info
└── aoimage/                    # C image processing library
    ├── AoImage.py              # Python wrapper
    ├── aoimage.c               # C implementation
    └── lib/                    # ISPC/STB compression libs
```

### Summary Statistics

| Aspect | Details |
|--------|---------|
| **Total Code** | ~6,232 Python lines + C extensions |
| **Main Modules** | 8 core modules |
| **Key Classes** | 28 total |
| **Download Threads** | 32 concurrent HTTP workers |
| **Memory Model** | 1-2GB in-memory, 30GB disk cache |
| **Threading** | 5+ daemon threads |
| **FUSE** | Python wrapper (refuse library) |
| **Web UI** | Flask + SocketIO on port 5000 |
| **Platforms** | Linux (native), Windows (Dokan/WinFSP), macOS (OSXFUSE) |
| **Languages** | Python 3.12, C, HTML/JS |
| **Build** | Nuitka (Python→binary) |

### External Dependencies

**Runtime:**
- Flask 3.0.3 + flask-socketio 5.3.7
- eventlet 0.37.0
- requests 2.32.3
- psutil 6.0.0
- geocoder 1.38.1
- PySimpleGUI 4.70.1
- refuse 0.0.4 (FUSE library)

---

## Why This Architecture is Brilliant

### The Core Innovation

AutoOrtho's brilliance is that it **makes streaming satellite imagery look like local files** to X-Plane:

✅ **No massive downloads** - Download only what you fly over
✅ **Smart caching** - Visited areas load instantly next time
✅ **Progressive quality** - See something immediately, get HD when ready
✅ **Multiple sources** - Falls back if one provider is down
✅ **Cross-platform** - Works on Linux/Windows/Mac via FUSE

**The Magic**: X-Plane has no idea this is happening. It thinks it's reading normal files from a normal directory!

### Key Challenges for Rust Migration

1. **FUSE complexity** - Need to implement filesystem operations correctly
2. **Performance** - Must be fast enough that X-Plane doesn't stutter
3. **Memory management** - Balance quality vs. memory usage
4. **Error handling** - Network failures, corrupted downloads, etc.
5. **Cross-platform** - Different FUSE implementations per OS
6. **Configuration compatibility** - Maintain exact same INI format
7. **Image processing** - DDS compression, JPEG handling, mipmaps

### Rust Migration Opportunities

**Performance Advantages:**
- **Zero-copy I/O**: Direct buffer passing (vs Python byte copies)
- **Compiled FUSE**: Avoid Python interpreter overhead in hot path
- **Better threading**: Tokio/async vs GIL constraints
- **Memory safety**: Bounds checking at compile time
- **Smaller binary**: No Python runtime bundling

---

## End of Architecture Analysis

This documentation provides a comprehensive overview of the AutoOrtho architecture for reference during the Rust migration.

**Date**: 2025-11-15
**Source Code Version**: Python implementation
**Purpose**: Architecture analysis for Rust rewrite
