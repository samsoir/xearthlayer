//! DDS filename parsing for X-Plane texture coordinates.
//!
//! Parses filenames like `+37-123_BI16.dds` to extract:
//! - Row (latitude tile coordinate): +37
//! - Column (longitude tile coordinate): -123
//! - Zoom level: 16
//!
//! The map type identifier (e.g., "BI") is ignored as we use the configured provider.

use regex::Regex;
use std::sync::OnceLock;

/// Parsed DDS filename containing tile coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdsFilename {
    /// Tile row (latitude-based coordinate)
    pub row: i32,
    /// Tile column (longitude-based coordinate)
    pub col: i32,
    /// Zoom level
    pub zoom: u8,
}

/// Error parsing DDS filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Filename doesn't match expected pattern
    InvalidPattern,
    /// Row coordinate is invalid
    InvalidRow(String),
    /// Column coordinate is invalid
    InvalidColumn(String),
    /// Zoom level is invalid
    InvalidZoom(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidPattern => write!(f, "Filename doesn't match DDS pattern"),
            ParseError::InvalidRow(s) => write!(f, "Invalid row coordinate: {}", s),
            ParseError::InvalidColumn(s) => write!(f, "Invalid column coordinate: {}", s),
            ParseError::InvalidZoom(s) => write!(f, "Invalid zoom level: {}", s),
        }
    }
}

impl std::error::Error for ParseError {}

/// Get the DDS filename regex pattern.
///
/// Pattern: `(+/-)<row>(-/+)<col>_<maptype><zoom>.dds`
/// Example: `+37-123_BI16.dds`
///
/// We capture:
/// - Group 1: row with sign (e.g., "+37" or "-12")
/// - Group 2: col with sign (e.g., "-123" or "+45")
/// - Group 3: zoom level (e.g., "16")
///
/// Map type identifier is not captured (we ignore it).
fn dds_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // Pattern breakdown:
        // ([+-]\d+) - row with sign
        // ([+-]\d+) - column with sign
        // _         - separator
        // [^\d]*    - map type (any non-digits, ignored)
        // (\d{2})   - zoom level (exactly 2 digits)
        // \.dds     - extension
        //
        // Using [^\d]* instead of \S* ensures we don't accidentally
        // capture the last 2 digits of a longer number
        Regex::new(r"([+-]\d+)([+-]\d+)_[^\d]*(\d{2})\.dds$").unwrap()
    })
}

/// Parse a DDS filename to extract tile coordinates.
///
/// # Arguments
///
/// * `filename` - Filename to parse (e.g., "+37-123_BI16.dds")
///
/// # Returns
///
/// Parsed coordinates or error if filename doesn't match pattern
///
/// # Examples
///
/// ```
/// use xearthlayer::fuse::parse_dds_filename;
///
/// let coords = parse_dds_filename("+37-123_BI16.dds").unwrap();
/// assert_eq!(coords.row, 37);
/// assert_eq!(coords.col, -123);
/// assert_eq!(coords.zoom, 16);
/// ```
pub fn parse_dds_filename(filename: &str) -> Result<DdsFilename, ParseError> {
    let pattern = dds_pattern();

    let captures = pattern
        .captures(filename)
        .ok_or(ParseError::InvalidPattern)?;

    // Parse row (includes sign)
    let row_str = captures.get(1).unwrap().as_str();
    let row = row_str
        .parse::<i32>()
        .map_err(|_| ParseError::InvalidRow(row_str.to_string()))?;

    // Parse column (includes sign)
    let col_str = captures.get(2).unwrap().as_str();
    let col = col_str
        .parse::<i32>()
        .map_err(|_| ParseError::InvalidColumn(col_str.to_string()))?;

    // Parse zoom
    let zoom_str = captures.get(3).unwrap().as_str();
    let zoom = zoom_str
        .parse::<u8>()
        .map_err(|_| ParseError::InvalidZoom(zoom_str.to_string()))?;

    Ok(DdsFilename { row, col, zoom })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_positive_coords() {
        let result = parse_dds_filename("+37-123_BI16.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 37);
        assert_eq!(coords.col, -123);
        assert_eq!(coords.zoom, 16);
    }

    #[test]
    fn test_parse_negative_row() {
        let result = parse_dds_filename("-12+045_BI15.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, -12);
        assert_eq!(coords.col, 45);
        assert_eq!(coords.zoom, 15);
    }

    #[test]
    fn test_parse_both_negative() {
        let result = parse_dds_filename("-40-074_BI17.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, -40);
        assert_eq!(coords.col, -74);
        assert_eq!(coords.zoom, 17);
    }

    #[test]
    fn test_parse_both_positive() {
        let result = parse_dds_filename("+51+000_BI14.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 51);
        assert_eq!(coords.col, 0);
        assert_eq!(coords.zoom, 14);
    }

    #[test]
    fn test_parse_different_map_type() {
        // Map type should be ignored
        let result = parse_dds_filename("+37-123_GM16.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 37);
        assert_eq!(coords.col, -123);
        assert_eq!(coords.zoom, 16);
    }

    #[test]
    fn test_parse_empty_map_type() {
        // Even with no map type identifier, should work
        let result = parse_dds_filename("+37-123_16.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 37);
        assert_eq!(coords.col, -123);
        assert_eq!(coords.zoom, 16);
    }

    #[test]
    fn test_parse_long_map_type() {
        let result = parse_dds_filename("+37-123_BINGMAPS16.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 37);
        assert_eq!(coords.col, -123);
        assert_eq!(coords.zoom, 16);
    }

    #[test]
    fn test_parse_with_path() {
        // Should work even if filename has path prefix
        let result = parse_dds_filename("/path/to/textures/+37-123_BI16.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 37);
        assert_eq!(coords.col, -123);
        assert_eq!(coords.zoom, 16);
    }

    #[test]
    fn test_parse_large_coordinates() {
        let result = parse_dds_filename("+180-180_BI19.dds");
        assert!(result.is_ok());
        let coords = result.unwrap();
        assert_eq!(coords.row, 180);
        assert_eq!(coords.col, -180);
        assert_eq!(coords.zoom, 19);
    }

    #[test]
    fn test_parse_zoom_levels() {
        // Test various zoom levels
        for zoom in 10..=19 {
            let filename = format!("+37-123_BI{}.dds", zoom);
            let result = parse_dds_filename(&filename);
            assert!(result.is_ok());
            assert_eq!(result.unwrap().zoom, zoom);
        }
    }

    #[test]
    fn test_parse_invalid_pattern_no_sign() {
        let result = parse_dds_filename("37-123_BI16.dds");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_pattern_no_underscore() {
        let result = parse_dds_filename("+37-123BI16.dds");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_pattern_wrong_extension() {
        let result = parse_dds_filename("+37-123_BI16.jpg");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_pattern_no_zoom() {
        let result = parse_dds_filename("+37-123_BI.dds");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_pattern_single_digit_zoom() {
        let result = parse_dds_filename("+37-123_BI9.dds");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_pattern_three_digit_zoom() {
        let result = parse_dds_filename("+37-123_BI100.dds");
        assert!(matches!(result, Err(ParseError::InvalidPattern)));
    }

    #[test]
    fn test_parse_invalid_row_overflow() {
        // Test row that exceeds i32 range
        let result = parse_dds_filename("+9999999999-123_BI16.dds");
        assert!(matches!(result, Err(ParseError::InvalidRow(_))));
    }

    #[test]
    fn test_parse_error_display() {
        let err = ParseError::InvalidPattern;
        assert_eq!(err.to_string(), "Filename doesn't match DDS pattern");

        let err = ParseError::InvalidRow("999999999999".to_string());
        assert_eq!(err.to_string(), "Invalid row coordinate: 999999999999");

        let err = ParseError::InvalidColumn("-999999999999".to_string());
        assert_eq!(err.to_string(), "Invalid column coordinate: -999999999999");

        let err = ParseError::InvalidZoom("99".to_string());
        assert_eq!(err.to_string(), "Invalid zoom level: 99");
    }

    #[test]
    fn test_dds_filename_equality() {
        let coords1 = DdsFilename {
            row: 37,
            col: -123,
            zoom: 16,
        };
        let coords2 = DdsFilename {
            row: 37,
            col: -123,
            zoom: 16,
        };
        let coords3 = DdsFilename {
            row: 38,
            col: -123,
            zoom: 16,
        };

        assert_eq!(coords1, coords2);
        assert_ne!(coords1, coords3);
    }

    #[test]
    fn test_dds_filename_clone() {
        let coords = DdsFilename {
            row: 37,
            col: -123,
            zoom: 16,
        };
        let cloned = coords.clone();
        assert_eq!(coords, cloned);
    }

    #[test]
    fn test_dds_filename_debug() {
        let coords = DdsFilename {
            row: 37,
            col: -123,
            zoom: 16,
        };
        let debug_str = format!("{:?}", coords);
        assert!(debug_str.contains("37"));
        assert!(debug_str.contains("-123"));
        assert!(debug_str.contains("16"));
    }
}
