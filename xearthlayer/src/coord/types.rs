//! Coordinate type definitions

use std::fmt;

/// Web Mercator valid latitude range
pub const MIN_LAT: f64 = -85.05112878;
pub const MAX_LAT: f64 = 85.05112878;

/// Valid longitude range
pub const MIN_LON: f64 = -180.0;
pub const MAX_LON: f64 = 180.0;

/// Standard zoom levels for X-Plane
pub const MIN_ZOOM: u8 = 0;
pub const MAX_ZOOM: u8 = 18;

/// Tile coordinates in Web Mercator / Slippy Map system.
///
/// Represents a 4096×4096 pixel tile composed of 16×16 chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    /// Y coordinate (north-south), 0 at north
    pub row: u32,
    /// X coordinate (east-west), 0 at west
    pub col: u32,
    /// Zoom level (0-18)
    pub zoom: u8,
}

impl TileCoord {
    /// Returns an iterator over all 256 chunks in this tile.
    ///
    /// Chunks are yielded in row-major order (row 0 columns 0-15, row 1 columns 0-15, etc.).
    #[inline]
    pub fn chunks(&self) -> TileChunksIterator {
        TileChunksIterator {
            tile: *self,
            current: 0,
        }
    }
}

/// Iterator over all chunks in a tile.
///
/// Yields 256 chunks (16×16) in row-major order.
#[derive(Debug, Clone)]
pub struct TileChunksIterator {
    tile: TileCoord,
    current: u16,
}

impl Iterator for TileChunksIterator {
    type Item = ChunkCoord;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= 256 {
            return None;
        }

        // Calculate chunk position in row-major order
        let chunk_row = (self.current / 16) as u8;
        let chunk_col = (self.current % 16) as u8;

        self.current += 1;

        Some(ChunkCoord {
            tile_row: self.tile.row,
            tile_col: self.tile.col,
            chunk_row,
            chunk_col,
            zoom: self.tile.zoom,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (256 - self.current) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TileChunksIterator {
    fn len(&self) -> usize {
        (256 - self.current) as usize
    }
}

/// Chunk coordinates within a tile.
///
/// Represents a 256×256 pixel chunk within a 4096×4096 tile.
/// Each tile contains 16×16 = 256 chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkCoord {
    /// Tile row coordinate
    pub tile_row: u32,
    /// Tile column coordinate
    pub tile_col: u32,
    /// Chunk row within tile (0-15)
    pub chunk_row: u8,
    /// Chunk column within tile (0-15)
    pub chunk_col: u8,
    /// Zoom level
    pub zoom: u8,
}

impl ChunkCoord {
    /// Converts chunk coordinates to global tile coordinates.
    ///
    /// This is used when requesting chunks from satellite imagery providers,
    /// which expect global tile coordinates at the chunk resolution.
    #[inline]
    pub fn to_global_coords(&self) -> (u32, u32, u8) {
        let global_row = self.tile_row * 16 + self.chunk_row as u32;
        let global_col = self.tile_col * 16 + self.chunk_col as u32;
        (global_row, global_col, self.zoom)
    }
}

/// Errors that can occur during coordinate conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum CoordError {
    /// Latitude is outside valid range (-85.05112878 to 85.05112878)
    InvalidLatitude(f64),
    /// Longitude is outside valid range (-180.0 to 180.0)
    InvalidLongitude(f64),
    /// Zoom level is outside valid range (0 to 18)
    InvalidZoom(u8),
    /// Quadkey contains invalid characters or is too long
    InvalidQuadkey(String),
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordError::InvalidLatitude(lat) => {
                write!(
                    f,
                    "Invalid latitude: {} (must be between {} and {})",
                    lat, MIN_LAT, MAX_LAT
                )
            }
            CoordError::InvalidLongitude(lon) => {
                write!(
                    f,
                    "Invalid longitude: {} (must be between {} and {})",
                    lon, MIN_LON, MAX_LON
                )
            }
            CoordError::InvalidZoom(zoom) => {
                write!(
                    f,
                    "Invalid zoom level: {} (must be between {} and {})",
                    zoom, MIN_ZOOM, MAX_ZOOM
                )
            }
            CoordError::InvalidQuadkey(quadkey) => {
                write!(
                    f,
                    "Invalid quadkey: '{}' (must contain only digits 0-3 and length <= {})",
                    quadkey, MAX_ZOOM
                )
            }
        }
    }
}

impl std::error::Error for CoordError {}
