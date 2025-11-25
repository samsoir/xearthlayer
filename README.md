# XEarthLayer

A high-performance Rust implementation for streaming satellite imagery to X-Plane flight simulator.

## Overview

XEarthLayer provides on-demand satellite imagery streaming for X-Plane using a FUSE virtual filesystem. Instead of downloading massive scenery packages, it dynamically fetches only the imagery you need as you fly, from multiple satellite providers (Bing Maps, NAIP, EOX Sentinel-2, USGS).

## Current Implementation Status

### Implemented Modules

#### 1. Coordinate System (`xearthlayer/src/coord/`)
- Geographic to tile coordinate conversion (Web Mercator projection)
- Tile to chunk coordinate mapping (16×16 chunks per tile)
- Bing Maps quadkey encoding/decoding
- **48 comprehensive tests** including property-based testing

#### 2. Provider Abstraction (`xearthlayer/src/provider/`)
- `Provider` trait for satellite imagery sources
- `HttpClient` trait with GET and POST support for dependency injection
- **Bing Maps** provider (free, no API key required)
  - Quadkey-based tile URLs
  - Zoom levels 1-19
- **Google Maps** provider (paid, requires API key)
  - Session token authentication via Map Tiles API
  - XYZ coordinate system
  - Zoom levels 0-22
- Mock support for testing without network calls
- **16 tests** covering both providers and HTTP operations

#### 3. Download Orchestrator (`xearthlayer/src/orchestrator/`)
- Parallel downloading of 256 chunks per tile (16×16 grid)
- Batched threading with configurable parallelism (default: 32 concurrent)
- Per-chunk retry logic with overall timeout
- Error tolerance (requires 80% success rate minimum)
- Image assembly into 4096×4096 RGBA images
- **3 integration tests** including real network downloads

#### 4. Command-Line Interface (`xearthlayer-cli/`)
- Download satellite imagery tiles to disk for testing
- Provider selection (Bing Maps or Google Maps)
- JPEG encoding with 90% quality
- Automatic zoom level validation
- Google Maps session token management
- Successfully downloads real imagery in ~2-3 seconds

**Total Test Coverage**: 68 passing tests

### Modules Not Yet Implemented

The following modules from the AutoOrtho architecture remain to be implemented:

1. **FUSE Virtual Filesystem**
   - File operation handlers (getattr, read, readdir)
   - DDS/KTX2 filename pattern matching
   - Integration with X-Plane scenery system

2. **Tile Cache Manager**
   - In-memory LRU cache (1-2GB limit)
   - Disk cache for JPEG chunks (30GB limit)
   - Cache eviction and cleanup strategies

3. **DDS Texture Compression**
   - DirectX texture format encoding (BC1/BC3)
   - 5-level mipmap chain generation
   - Progressive loading optimization

4. **Configuration Management**
   - INI-based configuration from `~/.autoortho`
   - X-Plane scenery path auto-detection
   - Provider and cache settings

5. **Additional Imagery Providers**
   - NAIP (National Agriculture Imagery Program)
   - EOX Sentinel-2 satellite imagery
   - USGS imagery sources
   - Provider fallback logic

6. **Scenery Package Management**
   - Download pre-built scenery packages from GitHub
   - Package installation and mounting

7. **Flight Data Integration**
   - UDP listener for X-Plane position data (port 49000)
   - Predictive tile preloading based on flight path

8. **Web UI Dashboard**
   - Flask/SocketIO server (port 5000)
   - Real-time download statistics
   - Cache management interface

## Getting Started

### Development Setup

```bash
# Initialize development environment
make init

# Run all tests
make test

# Format and verify code
make verify

# Build release binary
cargo build --release
```

See `make help` for all available commands.

### Testing the CLI

Download a satellite imagery tile for any location:

```bash
# Build the CLI
cargo build --release

# Download NYC area at zoom 15 (maximum for Bing Maps)
./target/release/xearthlayer \
  --lat 40.7128 \
  --lon -74.0060 \
  --zoom 15 \
  --output nyc_tile.jpg

# Download San Francisco
./target/release/xearthlayer \
  --lat 37.7749 \
  --lon -122.4194 \
  --zoom 15 \
  --output sf_tile.jpg
```

**Note**: The maximum usable zoom level is 15 for Bing Maps because tiles at zoom Z require chunks from zoom Z+4, and Bing's maximum zoom is 19 (15+4=19).

### Provider Comparison

XEarthLayer supports multiple satellite imagery providers with different characteristics:

| Provider | Cost | API Key Required | Max Zoom | Max Usable Zoom* | Setup Difficulty |
|----------|------|------------------|----------|------------------|------------------|
| **Bing Maps** | Free | No | 19 | 15 | Easy (works out-of-box) |
| **Google Maps** | Paid | Yes | 22 | 18 | Medium (requires GCP account) |

*Max usable zoom: Tiles at zoom Z require chunks from zoom Z+4, so max usable = max zoom - 4

### Using Google Maps Provider

Google Maps provides higher resolution imagery (up to zoom 18 vs Bing's zoom 15) but requires a Google Cloud Platform account with billing enabled.

#### Prerequisites

1. **Google Cloud Platform Account**
   - Sign up at https://console.cloud.google.com
   - Billing must be enabled for the project

2. **Enable Map Tiles API**
   - Navigate to "APIs & Services" → "Library"
   - Search for "Map Tiles API"
   - Click "Enable"

3. **Create API Key**
   - Go to "APIs & Services" → "Credentials"
   - Click "Create Credentials" → "API Key"
   - (Recommended) Restrict the key to "Map Tiles API" only

#### Using Google Maps in CLI

```bash
# Download with Google Maps provider
./target/release/xearthlayer \
  --lat 37.7749 \
  --lon=-122.4194 \
  --zoom 18 \
  --provider google \
  --google-api-key "YOUR_API_KEY_HERE" \
  --output sf_tile_high_res.jpg

# Comparison: Bing Maps (zoom 15) vs Google Maps (zoom 18)
# Bing - lower resolution, free
./target/release/xearthlayer \
  --lat 40.7128 \
  --lon=-74.0060 \
  --zoom 15 \
  --provider bing \
  --output nyc_bing.jpg

# Google - higher resolution, requires API key
./target/release/xearthlayer \
  --lat 40.7128 \
  --lon=-74.0060 \
  --zoom 18 \
  --provider google \
  --google-api-key "YOUR_API_KEY_HERE" \
  --output nyc_google.jpg
```

**Important**: Use `--lon=-VALUE` (with equals sign) for negative longitudes to prevent argument parsing errors.

#### Session Token Management

The Google Maps provider automatically creates a session token when initialized. You'll see:
```
Creating Google Maps session...
Using provider: Google Maps (session created successfully)
```

If session creation fails, check that:
1. Map Tiles API is enabled in Google Cloud Console
2. Billing is enabled for your project
3. Your API key is valid and has appropriate permissions

### CLI Options

```
--lat <LAT>              Latitude in decimal degrees (required)
--lon <LON>              Longitude in decimal degrees (required)
--zoom <ZOOM>            Zoom level (default: 15)
                         Bing: 1-15, Google: 1-18
--provider <PROVIDER>    Imagery provider (default: bing)
                         Options: bing, google
--google-api-key <KEY>   Google Maps API key (required when --provider google)
--output <PATH>          Output JPEG file path (required)
```

### Output Format

The CLI generates 4096×4096 pixel JPEG images at 90% quality by:
1. Converting lat/lon to tile coordinates at the requested zoom level
2. Downloading 256 chunks (16×16 grid) at zoom+4 from selected provider
3. Assembling chunks into a single high-resolution image in parallel
4. Encoding as JPEG with optimized quality settings

Typical output size: 4-6 MB per tile
Download time: ~2-3 seconds with 32 parallel workers

### Troubleshooting

#### Google Maps Issues

**Error: "POST request failed" or "session is a required URL parameter"**
- Ensure Map Tiles API is enabled in Google Cloud Console
- Verify billing is enabled for your project
- Check that your API key is valid

**Error: "HTTP 403" or "API key not valid"**
- Verify the API key is correct
- Check that the API key has permission to use Map Tiles API
- If using restrictions, ensure Map Tiles API is in the allowed list

**Error: "Failed to parse session token"**
- This usually indicates the API is not enabled or billing is not set up
- Check Google Cloud Console for any project configuration issues

#### General Issues

**Error: "unexpected argument '-X' found"**
- For negative coordinates, use equals sign: `--lon=-122.4194` not `--lon -122.4194`

**Slow downloads or timeouts**
- Default timeout is 30 seconds for entire tile
- Check your internet connection
- Try reducing parallelism or zoom level

## Credits

This project is architecturally influenced by [AutoOrtho](https://github.com/kubilus1/autoortho) by [kubilus1](https://github.com/kubilus1). XEarthLayer is an independent Rust implementation focused on performance, memory safety, and cross-platform compatibility.

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for details.
