# XEarthLayer Module Status

This document provides a comprehensive overview of all modules in the XEarthLayer project, their implementation status, and remaining work.

## Implementation Status Legend

- ✅ **Implemented**: Fully implemented with comprehensive tests
- 🚧 **In Progress**: Partially implemented or under active development
- ⏳ **Planned**: Designed but not yet implemented
- 📋 **Proposed**: Identified as needed, design pending

## Recent Updates

**2025-01-24**: Implemented DDS texture encoding module with BC1/BC3 compression, mipmap generation, and CLI integration. Added 93 comprehensive tests. Performance: 0.2-0.3s for 4096×4096 encoding.

## Core Library Modules (`xearthlayer/`)

### ✅ Coordinate System (`coord/`)

**Status**: Fully implemented with 48 tests

**Purpose**: Handles all coordinate conversions between geographic coordinates, tile coordinates, chunk coordinates, and Bing Maps quadkeys.

**Files**:
- `types.rs` - Core types (TileCoord, ChunkCoord, CoordError)
- `mod.rs` - Conversion functions and iteration

**Key Functionality**:
- `to_tile_coords()` - Convert lat/lon to tile coordinates (Web Mercator projection)
- `to_chunk_coords()` - Convert lat/lon to chunk coordinates within a tile
- `tile_to_lat_lon()` - Convert tile coordinates back to geographic
- `tile_to_quadkey()` / `quadkey_to_tile()` - Bing Maps quadkey encoding
- `TileCoord::chunks()` - Iterator over all 256 chunks in a tile
- `ChunkCoord::to_global_coords()` - Convert chunk to global tile coordinates at zoom+4

**Test Coverage**:
- 32 unit tests covering edge cases and known values
- 16 property-based tests using proptest for invariant checking
- 2 doctests for public API examples

**Dependencies**:
- None (only std library)

**Design Notes**:
- Uses Web Mercator projection (EPSG:3857)
- Valid latitude range: -85.05112878° to 85.05112878°
- Zoom levels 1-19 supported
- Each tile = 16×16 chunks = 4096×4096 pixels
- Chunks at zoom Z come from provider at zoom Z+4

---

### ✅ Provider Abstraction (`provider/`)

**Status**: Fully implemented with 10 tests

**Purpose**: Abstracts satellite imagery providers behind a trait-based interface, enabling dependency injection and testing without network calls.

**Files**:
- `types.rs` - Provider trait and ProviderError enum
- `http.rs` - HttpClient trait, ReqwestClient, and MockHttpClient
- `bing.rs` - BingMapsProvider implementation
- `mod.rs` - Module exports

**Key Functionality**:
- `Provider` trait - Common interface for all imagery providers
- `HttpClient` trait - Abstraction for HTTP requests (enables mocking)
- `BingMapsProvider` - Bing Maps aerial imagery using quadkey URLs
- `ReqwestClient` - Production HTTP client implementation
- `MockHttpClient` - Test double for unit testing

**Provider Trait Methods**:
- `download_chunk(row, col, zoom)` - Download a single 256×256 tile
- `name()` - Provider name for display
- `min_zoom()` / `max_zoom()` - Supported zoom range
- `supports_zoom(zoom)` - Validate zoom level

**Test Coverage**:
- 10 unit tests covering success, errors, and edge cases
- Mock-based testing without network dependencies

**Dependencies**:
- `reqwest` (0.12) with blocking feature

**Design Notes**:
- Thread-safe (`Send + Sync` bounds)
- Error handling via `ProviderError` enum
- Bing Maps: zoom 1-19, quadkey-based URLs
- Ready for additional providers (NAIP, Sentinel-2, USGS)

---

### ✅ Download Orchestrator (`orchestrator/`)

**Status**: Fully implemented with 3 tests

**Purpose**: Coordinates parallel downloading of 256 chunks per tile and assembles them into complete 4096×4096 pixel images.

**Files**:
- `types.rs` - OrchestratorError, ChunkResult, DownloadStats
- `download.rs` - TileOrchestrator implementation
- `mod.rs` - Module exports

**Key Functionality**:
- `TileOrchestrator` - Main orchestration struct
- `download_tile()` - Download and assemble complete tile
- `download_chunks_parallel()` - Parallel chunk downloading with batching
- `assemble_image()` - Decode JPEGs and composite into final image

**Configuration**:
- `timeout_secs` - Overall timeout for tile download (default: 30s)
- `max_retries_per_chunk` - Retry attempts per failed chunk (default: 3)
- `max_parallel_downloads` - Concurrent thread limit (default: 32)
- Requires 80% chunk success rate minimum

**Error Handling**:
- `OrchestratorError::Timeout` - Exceeded time limit
- `OrchestratorError::TooManyFailures` - < 80% success rate
- `OrchestratorError::ImageError` - JPEG decode/assembly failed
- Per-chunk retry with timeout checking

**Test Coverage**:
- 3 integration tests including real network downloads
- Mock provider support for deterministic testing

**Dependencies**:
- `image` (0.25) for JPEG decoding and image manipulation
- Provider implementation (generic over `Provider` trait)

**Performance**:
- Downloads 256 chunks in ~2.5 seconds (tested)
- Batched threading prevents thread explosion
- `Arc<Provider>` for efficient sharing across threads

**Design Notes**:
- Uses `std::thread` + `mpsc::channel` (no async complexity)
- Batched execution: spawns `max_parallel_downloads` at a time
- Black pixels fill missing chunks (graceful degradation)
- Returns `RgbaImage` (4096×4096) for further processing

---

### ✅ DDS Texture Compression (`dds/`)

**Status**: Fully implemented with 93 tests

**Purpose**: Encode satellite imagery into DirectX DDS format with BC1/BC3 compression for X-Plane compatibility.

**Files**:
- `types.rs` - Core types, errors, DDS header structures
- `conversion.rs` - RGB565 conversion and color distance calculations
- `header.rs` - DDS header construction and serialization
- `bc1.rs` - BC1/DXT1 compression implementation
- `bc3.rs` - BC3/DXT5 compression implementation
- `mipmap.rs` - Mipmap chain generation with box filtering
- `encoder.rs` - Main DDS encoder API
- `mod.rs` - Public API exports

**Key Functionality**:
- `DdsEncoder::new()` - Create encoder with BC1 or BC3 format
- `encode()` - Encode RgbaImage to complete DDS file
- `with_mipmap_count()` - Configure mipmap levels
- `MipmapGenerator::generate_chain()` - Generate full mipmap chain
- `Bc1Encoder::compress_block()` - Compress 4×4 pixel blocks
- `Bc3Encoder::compress_block()` - Compress with alpha channel

**Compression Formats**:
- **BC1/DXT1**: 8 bytes per 4×4 block (0.5 bytes/pixel)
  - Two RGB565 color endpoints
  - 2-bit indices for 4-color palette
  - Best for opaque satellite imagery
- **BC3/DXT5**: 16 bytes per 4×4 block (1 byte/pixel)
  - 8 bytes for alpha channel (two endpoints + 3-bit indices)
  - 8 bytes for RGB (same as BC1)
  - Best for textures with smooth alpha

**Mipmap Generation**:
- Box filter downsampling (2×2 average)
- Configurable level count
- Default: 5 levels (4096→2048→1024→512→256)
- Automatic chain to 1×1 if requested

**Test Coverage**:
- 4 tests for core types and header structure
- 15 tests for RGB565 conversion and color distance
- 24 tests for DDS header construction
- 15 tests for BC1 compression
- 17 tests for BC3 compression
- 14 tests for mipmap generation
- 16 integration tests for full encoding pipeline

**Performance**:
- BC1 encoding: ~0.21s for 4096×4096 with 5 mipmaps
- BC3 encoding: ~0.32s for 4096×4096 with 5 mipmaps
- Well under 1-second target

**File Sizes** (4096×4096 with 5 mipmaps):
- BC1: ~11 MB
- BC3: ~21 MB (2× BC1, as expected)
- JPEG: ~8.8 MB (for comparison)

**Dependencies**:
- `image` (0.25) for RgbaImage type

**Design Notes**:
- Bounding box method for color endpoint selection (fast, good quality)
- Perceptual color distance with green weighting (RGB weights 3:6:1)
- Sequential block processing (parallelization possible)
- Compatible with X-Plane, DirectX 9+, OpenGL texture compression

**CLI Integration**:
- `--format dds|jpeg` flag for output format
- `--dds-format bc1|bc3` for compression selection
- `--mipmap-count N` for custom mipmap levels
- Auto-detection from `.dds` file extension

**Verification**:
- Recognized by `file` command as DDS/DXT1/DXT5
- Readable by ImageMagick, GIMP, Paint.NET
- Compatible with X-Plane texture system

---

## Command-Line Interface (`xearthlayer-cli/`)

### ✅ CLI Binary (`src/main.rs`)

**Status**: Fully implemented

**Purpose**: Standalone binary for testing tile downloads and saving satellite imagery to disk.

**Key Functionality**:
- Command-line argument parsing with `clap`
- Coordinate validation and tile conversion
- Provider zoom level validation
- JPEG encoding with quality control
- Progress reporting

**Arguments**:
- `--lat <LAT>` - Latitude (required)
- `--lon <LON>` - Longitude (required)
- `--zoom <ZOOM>` - Zoom level 1-15 for Bing, 1-18 for Google (default: 15)
- `--output <PATH>` - Output file path (required)
- `--provider <PROVIDER>` - Imagery provider: bing, google (default: bing)
- `--google-api-key <KEY>` - Google Maps API key (required for Google)
- `--format <FORMAT>` - Output format: jpeg, dds (auto-detected from extension)
- `--dds-format <FORMAT>` - DDS compression: bc1, bc3 (default: bc1)
- `--mipmap-count <COUNT>` - Number of mipmap levels for DDS (default: 5)

**Validation**:
- Zoom range: 1-19 (enforced by coord module)
- Provider-specific: zoom+4 ≤ max_zoom (enforced by CLI)
- Geographic bounds: lat ∈ [-85.05, 85.05], lon ∈ [-180, 180]

**Output Formats**:
- **JPEG**: 4096×4096 at 90% quality (~8-9 MB)
- **DDS BC1**: 4096×4096 with mipmaps (~11 MB)
- **DDS BC3**: 4096×4096 with mipmaps (~21 MB)

**Dependencies**:
- `clap` (4.5) with derive feature for argument parsing
- `image` (0.25) for JPEG encoding
- `xearthlayer` library

**Example Usage**:
```bash
./target/release/xearthlayer \
  --lat 40.7128 \
  --lon -74.0060 \
  --zoom 15 \
  --output nyc.jpg
```

---

## Planned Modules

### ⏳ FUSE Virtual Filesystem (`fuse/`)

**Purpose**: Virtual filesystem that intercepts X-Plane texture file requests and generates imagery on-demand.

**Planned Files**:
- `types.rs` - FUSE operation types
- `filesystem.rs` - Main FUSE implementation
- `parser.rs` - DDS filename pattern matching

**Required Functionality**:
- `getattr()` - File metadata for virtual files
- `read()` - Generate DDS on-the-fly when X-Plane reads
- `readdir()` - List scenery directory contents
- Regex pattern: `(\d+)[-_](\d+)[-_]((?!ZL)\S*)(\d{2}).dds`
- Pass-through for non-DDS files

**Crates to Consider**:
- `fuser` - Pure Rust FUSE library
- `polyfuse` - Alternative async FUSE implementation

**Design Decisions Needed**:
- Blocking vs async I/O
- Error handling for missing tiles
- Cache integration strategy

---

### ⏳ Tile Cache Manager (`cache/`)

**Purpose**: Multi-tier caching to minimize redundant downloads and provide fast tile access.

**Planned Files**:
- `types.rs` - Cache types and errors
- `memory.rs` - In-memory LRU cache
- `disk.rs` - Persistent disk cache
- `manager.rs` - Unified cache coordinator

**Required Functionality**:
- **Memory Cache**:
  - LRU eviction policy
  - Size limit: 1-2GB (50-100 tiles)
  - Thread-safe access
  - Holds complete 4096×4096 images

- **Disk Cache**:
  - Store individual 256×256 JPEG chunks
  - Size limit: 30GB default
  - Directory structure by zoom/row/col
  - Corruption detection (magic bytes)

- **Cache Manager**:
  - Check memory → check disk → download
  - Background cleanup thread
  - Cache statistics and monitoring

**Crates to Consider**:
- `lru` - LRU cache implementation
- `parking_lot` - High-performance RwLock
- `dashmap` - Concurrent HashMap

---

### ⏳ Configuration Management (`config/`)

**Purpose**: Load and manage user configuration from INI files.

**Planned Files**:
- `types.rs` - Configuration structures
- `loader.rs` - INI parsing and validation
- `defaults.rs` - Default values

**Required Functionality**:
- Read `~/.autoortho/autoortho.ini`
- Sections: general, paths, autoortho, pydds, scenery, fuse, cache
- X-Plane installation auto-detection
- Provider selection and prioritization
- Cache size limits
- Thread pool sizing

**Crates to Consider**:
- `ini` - INI file parser
- `serde` - Serialization framework
- `config` - Multi-format configuration

---

### 📋 Additional Imagery Providers (`provider/`)

**Purpose**: Add support for additional satellite imagery sources beyond Bing Maps.

**Planned Providers**:
- **NAIP** - USDA National Agriculture Imagery Program
- **Sentinel-2** - ESA Copernicus satellite (via EOX)
- **USGS** - US Geological Survey imagery

**Required Functionality**:
- Implement `Provider` trait for each source
- Provider-specific URL construction
- Authentication handling (if required)
- Zoom level mapping
- Fallback logic when primary fails

---

### 📋 Scenery Package Management (`scenery/`)

**Purpose**: Download and install pre-built X-Plane scenery packages that contain DSF/terrain files but no imagery.

**Planned Files**:
- `types.rs` - Package metadata
- `downloader.rs` - GitHub releases API
- `installer.rs` - Package extraction and mounting

**Required Functionality**:
- Fetch from `https://api.github.com/repos/kubilus1/autoortho-scenery/releases`
- Download `.zip` packages
- Extract to scenery directory
- Verify checksums
- Track installed packages

**Crates to Consider**:
- `reqwest` - HTTP client (already in use)
- `zip` - ZIP archive handling
- `sha2` - Checksum verification

---

### 📋 Flight Data Integration (`flight/`)

**Purpose**: Listen to X-Plane's UDP position broadcasts for predictive tile preloading.

**Planned Files**:
- `types.rs` - Flight data structures
- `listener.rs` - UDP socket handler
- `predictor.rs` - Tile prediction based on heading/speed

**Required Functionality**:
- UDP listener on port 49000
- Parse X-Plane data format
- Calculate required tiles based on position
- Predict upcoming tiles from heading/speed
- Queue preload requests

**Crates to Consider**:
- `tokio` - Async runtime for UDP
- Standard library `UdpSocket` for simplicity

---

### 📋 Web UI Dashboard (`web/`)

**Purpose**: Provide a web-based dashboard for monitoring and configuration.

**Planned Files**:
- `server.rs` - HTTP server
- `handlers.rs` - Request handlers
- `websocket.rs` - Real-time updates

**Required Functionality**:
- HTTP server on port 5000
- Real-time download statistics
- Cache hit/miss metrics
- Active downloads display
- Configuration management
- Manual cache clearing

**Crates to Consider**:
- `axum` - Modern web framework
- `tokio-tungstenite` - WebSocket support
- `tower-http` - HTTP middleware

**Alternative**: Could be a separate service

---

## Module Dependencies Graph

```
xearthlayer-cli
    ├─→ coord (geographic ↔ tile conversion)
    ├─→ provider (imagery sources)
    └─→ orchestrator (parallel downloads)
            ├─→ coord (chunk iteration)
            └─→ provider (chunk downloads)

[Future FUSE filesystem]
    ├─→ coord (filename parsing)
    ├─→ cache (tile retrieval)
    ├─→ dds (texture encoding)
    └─→ config (settings)
            └─→ cache
                    ├─→ orchestrator (on cache miss)
                    └─→ disk (persistent storage)
```

## Next Implementation Priorities

Based on the AutoOrtho architecture and current progress:

1. ~~**DDS Texture Compression**~~ - ✅ **COMPLETED** (93 tests, 0.2-0.3s encoding)
2. **Tile Cache Manager** - Essential for performance (memory + disk LRU)
3. **FUSE Virtual Filesystem** - Core integration point with X-Plane
4. **Configuration Management** - User-facing settings
5. **Additional Providers** - Improved imagery quality/coverage
6. **Flight Data Integration** - Predictive preloading
7. **Scenery Package Management** - Simplified installation
8. **Web UI Dashboard** - Optional monitoring/management

## Testing Strategy

**Current Test Coverage**: 161 tests passing
- Unit tests: 155
- Integration tests: 3 (with real network)
- Doctests: 2

**Target Coverage**: 80% minimum (90% goal)

**Testing Approach**:
- TDD: Write tests before implementation
- Property-based testing with `proptest` for invariants
- Dependency injection for mockable components
- Integration tests with real providers (limited to avoid rate limits)
- Benchmark tests for performance-critical paths

## Documentation Status

- ✅ README.md - Project overview and CLI usage
- ✅ CLAUDE.md - Development guidance
- ✅ DESIGN_PRINCIPLES.md - SOLID and TDD guidelines
- ✅ COORDINATE_SYSTEM.md - Detailed coord module docs
- ✅ MODULE_STATUS.md - This document
- ⏳ API documentation (rustdoc) - Partial coverage
- ⏳ Architecture diagrams - Planned
- ⏳ Performance benchmarks - Planned

## Performance Targets

Based on AutoOrtho benchmarks:

- **Tile Download**: < 3 seconds for 256 chunks (✅ Currently ~2.5s)
- **DDS Encoding**: < 1 second for 4096×4096 image
- **Cache Lookup**: < 10ms memory, < 50ms disk
- **FUSE Operations**: < 100ms total latency
- **Memory Usage**: < 2GB for in-memory cache
- **Disk Cache**: 30GB default, configurable

## Platform Support

- **Linux**: Primary target (native FUSE support) - ✅ Working
- **Windows**: Requires Dokan/WinFSP - ⏳ Planned
- **macOS**: Requires macFUSE - ⏳ Planned

## License

MIT License - See LICENSE file for details
