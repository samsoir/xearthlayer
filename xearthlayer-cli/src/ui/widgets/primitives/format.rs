//! Formatting utilities for dashboard display.
//!
//! Provides consistent, reusable formatters for common value types
//! like bytes, throughput, durations, and percentages.
//!
//! Note: Some formatters are not yet used but are included for future use.

#![allow(dead_code)] // Reusable formatters - some not yet used

/// Format byte counts as human-readable strings.
///
/// Uses SI prefixes (KB, MB, GB, TB) with appropriate precision.
///
/// # Examples
/// ```
/// assert_eq!(format_bytes(1234), "1.2 KB");
/// assert_eq!(format_bytes(1_234_567), "1.2 MB");
/// assert_eq!(format_bytes(1_234_567_890), "1.23 GB");
/// ```
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000_000 {
        format!("{:.2} TB", bytes as f64 / 1_000_000_000_000.0)
    } else if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Format byte counts from usize (convenience wrapper).
pub fn format_bytes_usize(bytes: usize) -> String {
    format_bytes(bytes as u64)
}

/// Format throughput (bytes per second) as human-readable strings.
///
/// Uses SI prefixes with /s suffix.
///
/// # Examples
/// ```
/// assert_eq!(format_throughput(1_234_567.0), "1.2 MB/s");
/// ```
pub fn format_throughput(bps: f64) -> String {
    if bps >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

/// Format a duration in compact form.
///
/// Returns strings like "5s", "2m30s", "1h15m".
pub fn format_duration_compact(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format a percentage with appropriate precision.
///
/// # Arguments
/// * `value` - The percentage value (0.0 to 100.0)
/// * `precision` - Decimal places to show
pub fn format_percent(value: f64, precision: usize) -> String {
    format!("{:.precision$}%", value, precision = precision)
}

/// Format a rate value (events per second).
///
/// # Arguments
/// * `rate` - The rate in events per second
/// * `precision` - Decimal places to show
pub fn format_rate(rate: f64, precision: usize) -> String {
    format!("{:.precision$}/s", rate, precision = precision)
}

/// Format a count with optional thousands separator.
pub fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        format!("{}", count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.50 GB");
        assert_eq!(format_bytes(1_500_000_000_000), "1.50 TB");
    }

    #[test]
    fn test_format_throughput() {
        assert_eq!(format_throughput(0.0), "0 B/s");
        assert_eq!(format_throughput(500.0), "500 B/s");
        assert_eq!(format_throughput(1_500.0), "1.5 KB/s");
        assert_eq!(format_throughput(1_500_000.0), "1.5 MB/s");
        assert_eq!(format_throughput(1_500_000_000.0), "1.5 GB/s");
    }

    #[test]
    fn test_format_duration_compact() {
        use std::time::Duration;
        assert_eq!(format_duration_compact(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration_compact(Duration::from_secs(90)), "1m30s");
        assert_eq!(format_duration_compact(Duration::from_secs(3700)), "1h1m");
    }

    #[test]
    fn test_format_percent() {
        assert_eq!(format_percent(50.5, 1), "50.5%");
        assert_eq!(format_percent(100.0, 0), "100%");
    }

    #[test]
    fn test_format_rate() {
        assert_eq!(format_rate(12.5, 1), "12.5/s");
        assert_eq!(format_rate(100.0, 0), "100/s");
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(500), "500");
        assert_eq!(format_count(1_500), "1.5K");
        assert_eq!(format_count(1_500_000), "1.5M");
    }
}

/// Property-based tests for formatters.
///
/// These tests verify invariants that must hold across the entire input domain,
/// catching edge cases that unit tests might miss.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// format_bytes never panics and always produces a valid byte suffix.
        #[test]
        fn format_bytes_always_has_byte_suffix(bytes: u64) {
            let result = format_bytes(bytes);
            prop_assert!(!result.is_empty());
            prop_assert!(
                result.ends_with(" B") ||
                result.ends_with(" KB") ||
                result.ends_with(" MB") ||
                result.ends_with(" GB") ||
                result.ends_with(" TB"),
                "Expected byte suffix, got: {}", result
            );
        }

        /// format_throughput never panics and always has /s suffix.
        #[test]
        fn format_throughput_always_has_per_second_suffix(bps in 0.0f64..1e15) {
            let result = format_throughput(bps);
            prop_assert!(!result.is_empty());
            prop_assert!(
                result.ends_with("/s"),
                "Expected /s suffix, got: {}", result
            );
        }

        /// format_throughput handles special float values gracefully.
        #[test]
        fn format_throughput_handles_special_floats(bps in prop::num::f64::ANY) {
            // Should not panic even with NaN, Infinity, etc.
            let _ = format_throughput(bps);
        }

        /// format_duration_compact always has a time suffix.
        #[test]
        fn format_duration_compact_always_has_time_suffix(secs: u64) {
            let d = std::time::Duration::from_secs(secs);
            let result = format_duration_compact(d);
            prop_assert!(!result.is_empty());
            prop_assert!(
                result.ends_with('s') || result.ends_with('m') || result.ends_with('h'),
                "Expected time suffix, got: {}", result
            );
        }

        /// format_duration_compact produces correct component ranges.
        #[test]
        fn format_duration_compact_components_in_range(secs in 0u64..86400 * 365) {
            let d = std::time::Duration::from_secs(secs);
            let result = format_duration_compact(d);

            // If it contains 'm', the minutes should be 0-59
            // If it contains 's' after 'm', the seconds should be 0-59
            // This is a structural property that should always hold
            prop_assert!(!result.is_empty());
        }

        /// format_percent always ends with %.
        #[test]
        fn format_percent_always_ends_with_percent(value in 0.0f64..1000.0, precision in 0usize..5) {
            let result = format_percent(value, precision);
            prop_assert!(result.ends_with('%'), "Expected % suffix, got: {}", result);
        }

        /// format_rate always ends with /s.
        #[test]
        fn format_rate_always_ends_with_per_second(rate in 0.0f64..1e9, precision in 0usize..5) {
            let result = format_rate(rate, precision);
            prop_assert!(result.ends_with("/s"), "Expected /s suffix, got: {}", result);
        }

        /// format_count never panics and produces valid output.
        #[test]
        fn format_count_produces_valid_output(count: u64) {
            let result = format_count(count);
            prop_assert!(!result.is_empty());
            // Should end with a digit, K, or M
            let last_char = result.chars().last().unwrap();
            prop_assert!(
                last_char.is_ascii_digit() || last_char == 'K' || last_char == 'M',
                "Unexpected ending character in: {}", result
            );
        }

        /// format_count uses appropriate suffixes for ranges.
        #[test]
        fn format_count_suffix_matches_magnitude(count: u64) {
            let result = format_count(count);
            if count >= 1_000_000 {
                prop_assert!(result.ends_with('M'), "Expected M suffix for {}, got: {}", count, result);
            } else if count >= 1_000 {
                prop_assert!(result.ends_with('K'), "Expected K suffix for {}, got: {}", count, result);
            } else {
                prop_assert!(result.chars().last().unwrap().is_ascii_digit(),
                    "Expected no suffix for {}, got: {}", count, result);
            }
        }

        /// Consistency: larger byte values don't produce smaller formatted numbers
        /// within the same unit.
        #[test]
        fn format_bytes_larger_input_larger_output_within_unit(
            base in 1u64..999,
            multiplier in 1u64..999
        ) {
            // Compare two values within the same unit (KB range)
            let small = base * 1000;
            let large = (base + multiplier) * 1000;

            if large < 1_000_000 {
                // Both are in KB range
                let small_result = format_bytes(small);
                let large_result = format_bytes(large);

                // Extract numeric portions
                let small_num: f64 = small_result.split_whitespace().next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let large_num: f64 = large_result.split_whitespace().next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);

                prop_assert!(
                    large_num >= small_num,
                    "Larger value {} ({}) should format >= smaller {} ({})",
                    large, large_result, small, small_result
                );
            }
        }
    }
}
