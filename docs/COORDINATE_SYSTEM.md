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

Chunks are 256×256 pixel sub-tiles used for parallel downloading.

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

## Error Handling

```rust
pub enum CoordError {
    InvalidLatitude(f64),   // Outside ±85.05112878°
    InvalidLongitude(f64),  // Outside ±180°
    InvalidZoom(u8),        // Outside 0-18
}
```

All conversion functions validate inputs and return descriptive errors.

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

### Manual Tests
- Known coordinates (NYC, London)
- Edge cases (equator, prime meridian)
- Invalid input rejection
- Roundtrip conversions at multiple zoom levels

### Property-Based Tests
Using `proptest`, we verify with random inputs:
- **Roundtrip property**: Any valid coordinate roundtrips within tile precision
- **Bounds property**: Generated tiles always in valid range
- **Monotonicity**: Increasing longitude increases column coordinate
- **Reverse bounds**: Tile conversions always return valid coordinates
- **Validation**: Invalid inputs properly rejected

Each property test runs 100 random cases by default (~600 total test cases).

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

## Usage Example

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

## Future Enhancements

### Chunk Coordinate Support (Next)
- Convert lat/lon directly to chunk coordinates
- Iterate all chunks in a tile
- Convert chunks to global tile coordinates for providers

### Bing Quadkey Conversion (Planned)
- Convert tiles to Bing Maps quadkey strings
- Support for Bing Maps API integration

### Bounding Box Operations (Planned)
- Get all tiles within a bounding box
- Calculate tile coverage area
- Distance calculations

## References

- [Slippy Map Tilenames (OSM Wiki)](https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames)
- [Web Mercator Projection (Wikipedia)](https://en.wikipedia.org/wiki/Web_Mercator_projection)
- [Google Maps Tile Coordinates](https://developers.google.com/maps/documentation/javascript/coordinates)
