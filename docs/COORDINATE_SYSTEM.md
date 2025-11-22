# Coordinate System Implementation

This document describes the coordinate conversion system implemented in XEarthLayer.

## Overview

XEarthLayer uses the **Web Mercator** (also called Slippy Map) projection system for converting between geographic coordinates (latitude/longitude) and tile coordinates used by satellite imagery providers.

## Coordinate Types

### Geographic Coordinates

Standard latitude/longitude in decimal degrees:
- **Latitude**: -85.05112878° to 85.05112878° (Web Mercator limits)
- **Longitude**: -180° to 180°

### Tile Coordinates

```rust
pub struct TileCoord {
    pub row: u32,    // Y coordinate (north-south), 0 at north
    pub col: u32,    // X coordinate (east-west), 0 at west
    pub zoom: u8,    // Zoom level (0-18)
}
```

At each zoom level:
- **Zoom 0**: 1×1 tile (entire world)
- **Zoom 1**: 2×2 tiles
- **Zoom n**: 2^n × 2^n tiles

Each tile represents a 4096×4096 pixel image composed of 16×16 chunks.

### Chunk Coordinates

```rust
pub struct ChunkCoord {
    pub tile_row: u32,
    pub tile_col: u32,
    pub chunk_row: u8,    // 0-15 within tile
    pub chunk_col: u8,    // 0-15 within tile
    pub zoom: u8,
}
```

Chunks are 256×256 pixel sub-tiles used for parallel downloading. Each tile contains 16×16 = 256 chunks.

### Quadkeys

Quadkeys are Bing Maps' tile naming scheme using base-4 encoding:
- Each digit (0-3) represents which quadrant a tile occupies at each zoom level
- Quadkey length equals the zoom level
- Zoom 0 has an empty string quadkey

**Quadrant Encoding:**
- `0` = top-left (northwest)
- `1` = top-right (northeast)
- `2` = bottom-left (southwest)
- `3` = bottom-right (southeast)

**Example:** Tile (col=3, row=5, zoom=3) → quadkey `"213"`

## Conversion Functions

### Geographic to Tile Coordinates

```rust
pub fn to_tile_coords(lat: f64, lon: f64, zoom: u8) -> Result<TileCoord, CoordError>
```

**Formula (Web Mercator):**
```
n = 2^zoom
col = floor((lon + 180) / 360 * n)
lat_rad = lat * π / 180
row = floor((1 - asinh(tan(lat_rad)) / π) / 2 * n)
```

**Example:**
```rust
let tile = to_tile_coords(40.7128, -74.0060, 16)?;
// Result: TileCoord { row: 24640, col: 19295, zoom: 16 }
```

### Tile to Geographic Coordinates

```rust
pub fn tile_to_lat_lon(tile: &TileCoord) -> (f64, f64)
```

Returns the **northwest corner** of the tile.

**Formula (Inverse Web Mercator):**
```
n = 2^zoom
lon = col / n * 360 - 180
y = row / n
lat_rad = atan(sinh(π * (1 - 2*y)))
lat = lat_rad * 180 / π
```

**Example:**
```rust
let tile = TileCoord { row: 24640, col: 19295, zoom: 16 };
let (lat, lon) = tile_to_lat_lon(&tile);
// Result: approximately (40.713, -74.007)
```

### Geographic to Chunk Coordinates

```rust
pub fn to_chunk_coords(lat: f64, lon: f64, zoom: u8) -> Result<ChunkCoord, CoordError>
```

Directly converts geographic coordinates to a specific chunk within a tile. This is useful for determining which 256×256 pixel chunk to download.

**Algorithm:**
1. First calculates the tile coordinates
2. Calculates position at chunk resolution (zoom + 4, since 2^4 = 16)
3. Extracts chunk position within tile using modulo 16

**Example:**
```rust
let chunk = to_chunk_coords(40.7128, -74.0060, 16)?;
// Result: ChunkCoord {
//   tile_row: 24640, tile_col: 19295,
//   chunk_row: 5, chunk_col: 12,
//   zoom: 16
// }
```

### Chunk to Global Coordinates

```rust
impl ChunkCoord {
    pub fn to_global_coords(&self) -> (u32, u32, u8)
}
```

Converts chunk coordinates to global tile coordinates at chunk resolution. This is used when requesting chunks from satellite imagery providers.

**Formula:**
```
global_row = tile_row * 16 + chunk_row
global_col = tile_col * 16 + chunk_col
```

**Example:**
```rust
let chunk = ChunkCoord {
    tile_row: 100, tile_col: 200,
    chunk_row: 5, chunk_col: 7,
    zoom: 10
};
let (global_row, global_col, zoom) = chunk.to_global_coords();
// Result: (1605, 3207, 10)
```

### Iterating Tile Chunks

```rust
impl TileCoord {
    pub fn chunks(&self) -> TileChunksIterator
}
```

Returns an iterator over all 256 chunks in a tile, yielded in row-major order.

**Example:**
```rust
let tile = TileCoord { row: 100, col: 200, zoom: 12 };
for chunk in tile.chunks() {
    // Process each 256×256 chunk
    let (global_row, global_col, _) = chunk.to_global_coords();
    download_chunk(global_row, global_col, chunk.zoom)?;
}
```

### Tile to Quadkey

```rust
pub fn tile_to_quadkey(tile: &TileCoord) -> String
```

Converts tile coordinates to a Bing Maps quadkey string.

**Algorithm:**
- For each zoom level from zoom down to 1:
  - Check if column bit is set (east/west)
  - Check if row bit is set (north/south)
  - Combine into digit 0-3
  - Append to quadkey string

**Example:**
```rust
let tile = TileCoord { row: 5, col: 3, zoom: 3 };
let quadkey = tile_to_quadkey(&tile);
// Result: "213"
```

### Quadkey to Tile

```rust
pub fn quadkey_to_tile(quadkey: &str) -> Result<TileCoord, CoordError>
```

Parses a Bing Maps quadkey string back to tile coordinates.

**Algorithm:**
- For each character in quadkey (left to right):
  - Shift row and col left by 1 bit
  - If digit & 1, set col bit (east)
  - If digit & 2, set row bit (south)
- Zoom level = quadkey length

**Example:**
```rust
let tile = quadkey_to_tile("213")?;
// Result: TileCoord { row: 5, col: 3, zoom: 3 }
```

## Error Handling

```rust
pub enum CoordError {
    InvalidLatitude(f64),   // Outside ±85.05112878°
    InvalidLongitude(f64),  // Outside ±180°
    InvalidZoom(u8),        // Outside 0-18
    InvalidQuadkey(String), // Invalid characters or too long
}
```

All conversion functions validate inputs and return descriptive errors:
- Geographic coordinates validated against Web Mercator limits
- Zoom levels validated against 0-18 range
- Quadkeys validated for:
  - Only digits 0-3
  - Maximum length of 18 (MAX_ZOOM)

## Precision and Tolerance

At different zoom levels, tiles cover different geographic areas:

| Zoom | Tile Size (degrees) | Tile Size (~km at equator) |
|------|--------------------|-----------------------------|
| 0    | 360°               | ~40,000 km                  |
| 5    | 11.25°             | ~1,250 km                   |
| 10   | 0.35°              | ~39 km                      |
| 15   | 0.011°             | ~1.2 km                     |
| 16   | 0.0055°            | ~600 m                      |
| 18   | 0.0014°            | ~150 m                      |

**Roundtrip Precision**: When converting lat/lon → tile → lat/lon, the result will be within one tile's worth of the original coordinate. This is expected because `tile_to_lat_lon` returns the northwest corner of the tile.

## Testing

### Manual Tests (30 tests)

**Geographic/Tile Conversions:**
- Known coordinates (NYC, London)
- Edge cases (equator, prime meridian)
- Invalid input rejection
- Roundtrip conversions at multiple zoom levels

**Chunk Conversions:**
- Basic chunk coordinate calculation
- Chunk to global coordinate conversion
- Chunk positions at tile boundaries (origin and max)
- Consistency between tile and chunk conversions
- Iterator count, order, and uniqueness (256 chunks per tile)

**Quadkey Conversions:**
- Zoom level 0 and 1 conversions
- Bing Maps official examples
- Invalid character/length rejection
- Roundtrip tile ↔ quadkey conversions

### Property-Based Tests (18 tests)

Using `proptest`, we verify mathematical properties with 100 random cases each:

**Tile Conversions:**
- **Roundtrip property**: lat/lon → tile → lat/lon within tile precision
- **Bounds property**: Generated tiles always in valid range
- **Monotonicity**: Increasing longitude increases column coordinate
- **Reverse bounds**: Tile conversions always return valid coordinates
- **Validation**: Invalid inputs properly rejected

**Chunk Conversions:**
- Chunk coordinates always in valid range (0-15)
- Chunk tile coordinates match direct tile conversion
- Global coordinate calculation correctness
- Iterator always yields exactly 256 chunks
- All iterator chunks have valid coordinates
- Iterator produces no duplicates

**Quadkey Conversions:**
- Roundtrip property: tile → quadkey → tile is identity
- Quadkey length always equals zoom level
- Quadkeys only contain valid digits (0-3)
- Deterministic output for same tile
- Validation of length limits
- Zoom 0 always produces empty string

**Total Test Coverage:** 48 tests (30 manual + 18 property-based)

## Implementation Details

### Why Web Mercator?

Web Mercator is the standard for web mapping because:
- Simple formulas (faster computation)
- Preserves angles (easier navigation)
- Used by all major providers (Google, Bing, OSM)
- Tiles align perfectly at all zoom levels

### Why ±85.05112878° Latitude Limit?

Web Mercator projects the Earth as a square, which requires cutting off near the poles. The limit is chosen so that the projected map is exactly square.

### Performance Optimizations

- Functions marked `#[inline]` for better performance
- Uses `powi()` instead of `powf()` for integer exponents
- Direct float operations (no unnecessary conversions)

## Usage Examples

### Basic Tile Conversion

```rust
use xearthlayer::coord::{to_tile_coords, tile_to_lat_lon};

// Convert user's current location to tile
let location_tile = to_tile_coords(40.7128, -74.0060, 16)?;

// Determine which tile contains a specific coordinate
if location_tile.row == some_tile.row && location_tile.col == some_tile.col {
    println!("Coordinates are in the same tile!");
}

// Get the bounding box of a tile
let (nw_lat, nw_lon) = tile_to_lat_lon(&tile);
let se_tile = TileCoord {
    row: tile.row + 1,
    col: tile.col + 1,
    zoom: tile.zoom
};
let (se_lat, se_lon) = tile_to_lat_lon(&se_tile);
println!("Tile bounds: ({}, {}) to ({}, {})", nw_lat, nw_lon, se_lat, se_lon);
```

### Chunk-Based Parallel Downloads

```rust
use xearthlayer::coord::{to_tile_coords, TileCoord};

// Get tile for a location
let tile = to_tile_coords(51.5074, -0.1278, 15)?;

// Download all chunks in parallel
let chunks: Vec<_> = tile.chunks().collect();
for chunk in chunks {
    let (global_row, global_col, zoom) = chunk.to_global_coords();

    // Download from provider using global coordinates
    let url = format!(
        "https://provider.com/{}/{}/{}.jpg",
        zoom, global_row, global_col
    );
    download_chunk(&url)?;
}
```

### Bing Maps Quadkey Integration

```rust
use xearthlayer::coord::{to_tile_coords, tile_to_quadkey, quadkey_to_tile};

// Convert location to Bing Maps quadkey
let tile = to_tile_coords(47.6062, -122.3321, 18)?; // Seattle
let quadkey = tile_to_quadkey(&tile);

// Use quadkey with Bing Maps API
let url = format!(
    "https://t.ssl.ak.dynamic.tiles.virtualearth.net/comp/ch/{}?mkt=en-US&it=A",
    quadkey
);

// Parse quadkey from API response
let tile = quadkey_to_tile("0231012312")?;
println!("Tile: row={}, col={}, zoom={}", tile.row, tile.col, tile.zoom);
```

### Direct Chunk Lookup

```rust
use xearthlayer::coord::to_chunk_coords;

// Find exact chunk for a coordinate
let chunk = to_chunk_coords(40.7128, -74.0060, 16)?;

println!("Tile: ({}, {})", chunk.tile_row, chunk.tile_col);
println!("Chunk within tile: ({}, {})", chunk.chunk_row, chunk.chunk_col);

// Convert to global coordinates for download
let (global_row, global_col, _) = chunk.to_global_coords();
let url = format!("https://provider.com/{}/{}/{}.jpg",
    chunk.zoom, global_row, global_col);
```

## Future Enhancements

### Bounding Box Operations (Planned)
- Get all tiles within a geographic bounding box
- Get all chunks within a bounding box
- Calculate total tile/chunk coverage area
- Tile/chunk set operations (union, intersection)

### Distance and Proximity (Planned)
- Calculate distance between two geographic points
- Find nearest tile/chunk to a coordinate
- Radius-based tile/chunk queries

### Conversion Optimizations (Planned)
- Batch conversion of multiple coordinates
- SIMD-optimized conversions for large datasets
- Cached tile lookups for repeated queries

## References

- [Slippy Map Tilenames (OSM Wiki)](https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames)
- [Web Mercator Projection (Wikipedia)](https://en.wikipedia.org/wiki/Web_Mercator_projection)
- [Google Maps Tile Coordinates](https://developers.google.com/maps/documentation/javascript/coordinates)
- [Bing Maps Tile System](https://docs.microsoft.com/en-us/bingmaps/articles/bing-maps-tile-system)
