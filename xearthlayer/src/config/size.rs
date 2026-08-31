//! Human-readable size parsing (e.g., "2GB", "500MB").

use std::fmt;
use thiserror::Error;

/// 1 kibibyte in bytes (1024).
pub const KB: usize = 1024;
/// 1 mebibyte in bytes (1024²).
pub const MB: usize = KB * 1024;
/// 1 gibibyte in bytes (1024³).
pub const GB: usize = MB * 1024;

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
/// - Decimal values (e.g. "2.6GB")
/// - B suffix (bytes, e.g. "512B" — what `format_size` emits under 1 KB)
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
/// assert_eq!(parse_size("1.5GB").unwrap(), 1536 * 1024 * 1024);
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
    } else if s_upper.ends_with('B') {
        // Explicit bytes suffix, e.g. "512 B" — this is what `format_size`
        // emits for values under 1 KB, so it must round-trip (see #218).
        let num_part = s[..s.len() - 1].trim();
        (num_part, 1_usize)
    } else {
        // No suffix, treat as bytes
        (s, 1_usize)
    };

    // Parse the numeric part. Decimals are accepted so that any value
    // `format_size` can emit reads back through here — see issue #218.
    let num: f64 = num_str.parse().map_err(|_| SizeParseError::new(s))?;

    // These guards are load-bearing. Rust's float -> int `as` casts saturate
    // rather than trapping, so without them "-1GB" would silently yield 0 and
    // "infGB" would yield usize::MAX — replacing a loud failure with a quiet
    // wrong answer.
    if !num.is_finite() || num < 0.0 {
        return Err(SizeParseError::new(s));
    }

    let bytes = (num * multiplier as f64).round();
    // Deliberately `>`, not `>=`: `usize::MAX as f64` rounds up to 2^64 (a
    // value usize can't hold), and `format_size(usize::MAX)` emits
    // "17179869184 GB", which parses back to exactly 2^64 here. Tightening
    // this to `>=` would reject the formatter's own output and break the
    // round-trip property at the top of the range.
    if bytes > usize::MAX as f64 {
        return Err(SizeParseError::new(s));
    }

    Ok(bytes as usize)
}

/// Format a byte count as a human-readable string.
///
/// Always produces a unit-based output (GB, MB, KB, or bytes).
/// Uses decimal format for non-exact multiples.
///
/// # Examples
///
/// ```
/// use xearthlayer::config::format_size;
///
/// assert_eq!(format_size(1024), "1 KB");
/// assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2 GB");
/// assert_eq!(format_size(500 * 1024 * 1024), "500 MB");
/// assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB");
/// ```
pub fn format_size(bytes: usize) -> String {
    if bytes >= GB {
        let value = bytes as f64 / GB as f64;
        if value.fract() == 0.0 {
            format!("{} GB", value as usize)
        } else {
            format!("{:.1} GB", value)
        }
    } else if bytes >= MB {
        let value = bytes as f64 / MB as f64;
        if value.fract() == 0.0 {
            format!("{} MB", value as usize)
        } else {
            format!("{:.1} MB", value)
        }
    } else if bytes >= KB {
        let value = bytes as f64 / KB as f64;
        if value.fract() == 0.0 {
            format!("{} KB", value as usize)
        } else {
            format!("{:.1} KB", value)
        }
    } else {
        format!("{} B", bytes)
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
        Self(gb * GB)
    }

    pub fn from_mb(mb: usize) -> Self {
        Self(mb * MB)
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
    }

    #[test]
    fn test_parse_decimals() {
        // The value from issue #218 — a 32 GB machine's RAM/12.
        assert_eq!(parse_size("2.6 GB").unwrap(), 2_791_728_742);
        assert_eq!(parse_size("1.5GB").unwrap(), 1536 * 1024 * 1024);
        assert_eq!(parse_size("1.5MB").unwrap(), 1536 * 1024);
        assert_eq!(parse_size("2.5KB").unwrap(), 2560);
    }

    // Rust's float->int `as` casts saturate rather than trapping, so each of
    // these would silently produce a number instead of an error without
    // explicit guards: "-1GB" -> 0, "infGB" -> usize::MAX, overflow -> usize::MAX.
    #[test]
    fn test_parse_rejects_non_finite_and_negative() {
        assert!(parse_size("-1GB").is_err());
        assert!(parse_size("-0.5GB").is_err());
        assert!(parse_size("infGB").is_err());
        assert!(parse_size("NaNGB").is_err());
    }

    #[test]
    fn test_parse_rejects_overflow() {
        // Far beyond usize::MAX once multiplied by the GB unit.
        assert!(parse_size("999999999999GB").is_err());
    }

    // format_size and parse_size are inverses and must round-trip. This is the
    // property whose absence caused issue #218.
    #[test]
    fn test_format_parse_round_trip() {
        for bytes in [
            512_usize,
            2048,
            1536 * 1024,
            500 * 1024 * 1024,
            1024 * 1024 * 1024,
            1536 * 1024 * 1024,
            2_791_728_742,
            50 * 1024 * 1024 * 1024,
        ] {
            let rendered = format_size(bytes);
            // The primary assertion is simply that it parses at all — that is
            // the property whose absence caused #218.
            let parsed = parse_size(&rendered).unwrap_or_else(|e| {
                panic!("{bytes} rendered as {rendered:?} failed to parse: {e}")
            });
            // format_size keeps one decimal place, so the round trip is lossy
            // by design for values that are not exact unit multiples. 10% is
            // deliberately generous: a value just above 1.0 of its unit can
            // lose almost 5%, and a tighter bound would make this flaky at
            // that boundary without testing anything more useful.
            let tolerance = (bytes as f64 * 0.10).max(1.0) as usize;
            assert!(
                parsed.abs_diff(bytes) <= tolerance,
                "{bytes} -> {rendered:?} -> {parsed} exceeds tolerance {tolerance}"
            );
        }
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(1024 * 1024), "1 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1 GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2 GB");
        assert_eq!(format_size(500 * 1024 * 1024), "500 MB");
        assert_eq!(format_size(1000), "1000 B"); // Less than 1KB
        assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB"); // Non-exact multiple
        assert_eq!(format_size(1536 * 1024), "1.5 MB");
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn test_size_roundtrip() {
        // Note: parse_size accepts decimals (see #218), so parsing is no
        // longer the limiting factor here. format_size still rounds to one
        // decimal place, so this test sticks to exact multiples where the
        // string representation itself round-trips unchanged.
        let test_cases = vec![
            ("1KB", "1 KB"),
            ("500MB", "500 MB"),
            ("2GB", "2 GB"),
            ("20GB", "20 GB"),
        ];
        for (input, expected_output) in test_cases {
            let parsed: Size = input.parse().unwrap();
            assert_eq!(parsed.to_string(), expected_output);
        }
    }

    #[test]
    fn test_size_from_helpers() {
        assert_eq!(Size::from_gb(2).bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(Size::from_mb(500).bytes(), 500 * 1024 * 1024);
    }
}
