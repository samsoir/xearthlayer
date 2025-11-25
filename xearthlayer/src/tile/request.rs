//! Tile generation request types.
//!
//! Provides the `TileRequest` type that encapsulates all information
//! needed to generate a tile texture, abstracting away the specific
//! filename parsing details.

/// Request to generate a tile texture.
///
/// Contains the tile coordinates (row/col in Web Mercator projection)
/// and zoom level needed to download and encode a satellite imagery tile.
///
/// # Note
///
/// The row/col values are tile indices in the Web Mercator grid, NOT
/// latitude/longitude in degrees. These typically come from parsed FUSE
/// filenames like `+37-123_BI16.dds`.
///
/// # Example
///
/// ```
/// use xearthlayer::tile::TileRequest;
///
/// let request = TileRequest::new(37, 123, 16);
/// assert_eq!(request.row(), 37);
/// assert_eq!(request.col(), 123);
/// assert_eq!(request.zoom(), 16);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileRequest {
    /// Tile row (Y coordinate in Web Mercator grid)
    row: i32,
    /// Tile column (X coordinate in Web Mercator grid)
    col: i32,
    /// Zoom level
    zoom: u8,
}

impl TileRequest {
    /// Create a new tile request.
    ///
    /// # Arguments
    ///
    /// * `row` - Tile row (Y coordinate in Web Mercator grid)
    /// * `col` - Tile column (X coordinate in Web Mercator grid)
    /// * `zoom` - Zoom level (typically 12-19)
    pub fn new(row: i32, col: i32, zoom: u8) -> Self {
        Self { row, col, zoom }
    }

    /// Get the tile row.
    pub fn row(&self) -> i32 {
        self.row
    }

    /// Get the tile column.
    pub fn col(&self) -> i32 {
        self.col
    }

    /// Get the zoom level.
    pub fn zoom(&self) -> u8 {
        self.zoom
    }
}

impl From<crate::fuse::DdsFilename> for TileRequest {
    fn from(filename: crate::fuse::DdsFilename) -> Self {
        Self {
            row: filename.row,
            col: filename.col,
            zoom: filename.zoom,
        }
    }
}

impl From<&crate::fuse::DdsFilename> for TileRequest {
    fn from(filename: &crate::fuse::DdsFilename) -> Self {
        Self {
            row: filename.row,
            col: filename.col,
            zoom: filename.zoom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::DdsFilename;

    #[test]
    fn test_new() {
        let request = TileRequest::new(37, 123, 16);
        assert_eq!(request.row(), 37);
        assert_eq!(request.col(), 123);
        assert_eq!(request.zoom(), 16);
    }

    #[test]
    fn test_negative_col() {
        // Note: In practice tile columns are positive, but the type allows
        // negative values for compatibility with signed filename parsing
        let request = TileRequest::new(40, -74, 15);
        assert_eq!(request.row(), 40);
        assert_eq!(request.col(), -74);
        assert_eq!(request.zoom(), 15);
    }

    #[test]
    fn test_clone() {
        let request = TileRequest::new(37, 123, 16);
        let cloned = request;
        assert_eq!(request, cloned);
    }

    #[test]
    fn test_copy() {
        let request = TileRequest::new(37, 123, 16);
        let copied = request;
        assert_eq!(request.row(), copied.row());
    }

    #[test]
    fn test_equality() {
        let request1 = TileRequest::new(37, 123, 16);
        let request2 = TileRequest::new(37, 123, 16);
        let request3 = TileRequest::new(38, 123, 16);

        assert_eq!(request1, request2);
        assert_ne!(request1, request3);
    }

    #[test]
    fn test_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(TileRequest::new(37, 123, 16));
        set.insert(TileRequest::new(37, 123, 16));
        set.insert(TileRequest::new(38, 123, 16));

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_debug() {
        let request = TileRequest::new(37, 123, 16);
        let debug_str = format!("{:?}", request);
        assert!(debug_str.contains("37"));
        assert!(debug_str.contains("123"));
        assert!(debug_str.contains("16"));
    }

    #[test]
    fn test_from_dds_filename() {
        let filename = DdsFilename {
            row: 37,
            col: 123,
            zoom: 16,
        };
        let request: TileRequest = filename.into();
        assert_eq!(request.row(), 37);
        assert_eq!(request.col(), 123);
        assert_eq!(request.zoom(), 16);
    }

    #[test]
    fn test_from_dds_filename_ref() {
        let filename = DdsFilename {
            row: 37,
            col: 123,
            zoom: 16,
        };
        let request: TileRequest = (&filename).into();
        assert_eq!(request.row(), 37);
        assert_eq!(request.col(), 123);
        assert_eq!(request.zoom(), 16);
    }
}
