//! Human-readable size parsing (e.g., "2GB", "500MB").

use std::fmt;
use thiserror::Error;

/// Error parsing a size string.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("Invalid size '{input}' - expected format like '2GB', '500MB', or '1024KB'")]
pub struct SizeParseError {
    input: String,
}

impl SizeParseError {
    fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
        }
    }
}

/// Parse a human-readable size string into bytes.
///
/// Supports:
/// - Bare numbers (treated as bytes)
/// - KB/K suffix (1024 bytes)
/// - MB/M suffix (1024² bytes)
/// - GB/G suffix (1024³ bytes)
/// - Case-insensitive
/// - Whitespace tolerant
///
/// # Examples
///
/// ```
/// use xearthlayer::config::parse_size;
///
/// assert_eq!(parse_size("1024").unwrap(), 1024);
/// assert_eq!(parse_size("1KB").unwrap(), 1024);
/// assert_eq!(parse_size("1 KB").unwrap(), 1024);
/// assert_eq!(parse_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
/// assert_eq!(parse_size("500mb").unwrap(), 500 * 1024 * 1024);
/// ```
pub fn parse_size(s: &str) -> Result<usize, SizeParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(SizeParseError::new(s));
    }

    // Find where the numeric part ends
    let s_upper = s.to_uppercase();
    let s_upper = s_upper.trim();

    // Try to find suffix
    let (num_str, multiplier) = if s_upper.ends_with("GB") || s_upper.ends_with("G") {
        let suffix_len = if s_upper.ends_with("GB") { 2 } else { 1 };
        let num_part = s[..s.len() - suffix_len].trim();
        (num_part, 1024_usize * 1024 * 1024)
    } else if s_upper.ends_with("MB") || s_upper.ends_with("M") {
        let suffix_len = if s_upper.ends_with("MB") { 2 } else { 1 };
        let num_part = s[..s.len() - suffix_len].trim();
        (num_part, 1024_usize * 1024)
    } else if s_upper.ends_with("KB") || s_upper.ends_with("K") {
        let suffix_len = if s_upper.ends_with("KB") { 2 } else { 1 };
        let num_part = s[..s.len() - suffix_len].trim();
        (num_part, 1024_usize)
    } else {
        // No suffix, treat as bytes
        (s, 1_usize)
    };

    // Parse the numeric part
    let num: usize = num_str.parse().map_err(|_| SizeParseError::new(s))?;

    num.checked_mul(multiplier)
        .ok_or_else(|| SizeParseError::new(s))
}

/// Format a byte count as a human-readable string.
///
/// # Examples
///
/// ```
/// use xearthlayer::config::format_size;
///
/// assert_eq!(format_size(1024), "1KB");
/// assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2GB");
/// assert_eq!(format_size(500 * 1024 * 1024), "500MB");
/// ```
pub fn format_size(bytes: usize) -> String {
    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    const KB: usize = 1024;

    if bytes >= GB && bytes.is_multiple_of(GB) {
        format!("{}GB", bytes / GB)
    } else if bytes >= MB && bytes.is_multiple_of(MB) {
        format!("{}MB", bytes / MB)
    } else if bytes >= KB && bytes.is_multiple_of(KB) {
        format!("{}KB", bytes / KB)
    } else {
        format!("{}", bytes)
    }
}

/// A size value that can be parsed from and formatted to human-readable strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size(pub usize);

impl Size {
    pub fn bytes(self) -> usize {
        self.0
    }

    pub fn from_gb(gb: usize) -> Self {
        Self(gb * 1024 * 1024 * 1024)
    }

    pub fn from_mb(mb: usize) -> Self {
        Self(mb * 1024 * 1024)
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", format_size(self.0))
    }
}

impl std::str::FromStr for Size {
    type Err = SizeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_size(s).map(Size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bare_number() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("999999").unwrap(), 999999);
    }

    #[test]
    fn test_parse_kb() {
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1kb").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("100KB").unwrap(), 100 * 1024);
    }

    #[test]
    fn test_parse_mb() {
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("500MB").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_gb() {
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("20GB").unwrap(), 20 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_whitespace() {
        assert_eq!(parse_size("  2GB  ").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2 GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("500 MB").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("2TB").is_err()); // Not supported
        assert!(parse_size("-1GB").is_err());
        assert!(parse_size("1.5GB").is_err()); // Decimals not supported
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1024), "1KB");
        assert_eq!(format_size(1024 * 1024), "1MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2GB");
        assert_eq!(format_size(500 * 1024 * 1024), "500MB");
        assert_eq!(format_size(1000), "1000"); // Not evenly divisible
    }

    #[test]
    fn test_size_roundtrip() {
        let sizes = vec!["1KB", "500MB", "2GB", "20GB"];
        for s in sizes {
            let parsed: Size = s.parse().unwrap();
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn test_size_from_helpers() {
        assert_eq!(Size::from_gb(2).bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(Size::from_mb(500).bytes(), 500 * 1024 * 1024);
    }
}
