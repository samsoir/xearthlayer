//! Byte counts as people read them, for reports and prompts.
//!
//! Deliberately the publisher's own rather than a shared utility. XEarthLayer's
//! configuration formatter is bound by a round-trip requirement (#218): what it
//! prints must parse back to the same value. This one only labels sizes in
//! output, so the two have no reason to agree and coupling the crates through
//! a formatter would be the kind of accidental link the separation removes.
//! The output rules are the same today so that no report changed in the move.

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

/// Format a byte count with the largest unit that fits.
///
/// Exact multiples print as integers, anything else with one decimal, so a
/// 500 MB part size reads as `500 MB` and a 1.5 GB one as `1.5 GB`.
///
/// ```
/// use xearthlayer_publisher::format_size;
///
/// assert_eq!(format_size(1024), "1 KB");
/// assert_eq!(format_size(500 * 1024 * 1024), "500 MB");
/// assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB");
/// assert_eq!(format_size(512), "512 B");
/// ```
pub fn format_size(bytes: u64) -> String {
    let scaled = |unit: u64, label: &str| {
        let value = bytes as f64 / unit as f64;
        if value.fract() == 0.0 {
            format!("{} {label}", value as u64)
        } else {
            format!("{value:.1} {label}")
        }
    };
    if bytes >= GB {
        scaled(GB, "GB")
    } else if bytes >= MB {
        scaled(MB, "MB")
    } else if bytes >= KB {
        scaled(KB, "KB")
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_multiples_print_as_integers() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(KB), "1 KB");
        assert_eq!(format_size(2 * MB), "2 MB");
        assert_eq!(format_size(2 * GB), "2 GB");
    }

    #[test]
    fn fractions_print_with_one_decimal() {
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1536 * MB), "1.5 GB");
        assert_eq!(format_size(2_600_000), "2.5 MB");
    }

    #[test]
    fn the_default_part_size_reads_as_it_is_configured() {
        // 500 MB is the default archive part size; the prompt that shows it
        // must print the value a publisher typed.
        assert_eq!(format_size(500 * MB), "500 MB");
    }
}
