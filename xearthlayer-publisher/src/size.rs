//! Byte counts as people read them, for reports and prompts, and measuring
//! them from disk.
//!
//! Deliberately the publisher's own rather than a shared utility. XEarthLayer's
//! configuration formatter is bound by a round-trip requirement (#218): what it
//! prints must parse back to the same value. This one only labels and measures
//! sizes for output, so the two have no reason to agree and coupling the
//! crates through a formatter would be the kind of accidental link the
//! separation removes. The output rules are the same today so that no report
//! changed in the move.

use std::fs;
use std::path::Path;

use super::{PublishError, PublishResult};

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

/// Total apparent size of every regular file beneath `path`, in bytes.
///
/// A path that does not exist is zero rather than an error, so a caller can ask
/// about a package that was never built without branching first.
///
/// This is apparent size, not allocated size. A tree of many small files
/// therefore reads low against `du`: a package holding half a million `.ter`
/// files loses roughly half a 4 KiB block each to slack, a few percent of a
/// large ortho package. The number is advisory, shown to help a user judge disk
/// cost, and apparent size keeps the walk portable.
///
/// Symlinks are skipped rather than followed, so a link into the tree cannot
/// double count and a link out of it cannot escape the measurement.
pub fn directory_size(path: &Path) -> PublishResult<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let mut total: u64 = 0;

    let entries = fs::read_dir(path).map_err(|source| PublishError::ReadFailed {
        path: path.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| PublishError::ReadFailed {
            path: path.to_path_buf(),
            source,
        })?;

        let file_type = entry
            .file_type()
            .map_err(|source| PublishError::ReadFailed {
                path: entry.path(),
                source,
            })?;

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            let metadata = entry
                .metadata()
                .map_err(|source| PublishError::ReadFailed {
                    path: entry.path(),
                    source,
                })?;
            total = total.saturating_add(metadata.len());
        }
    }

    Ok(total)
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

    #[test]
    fn directory_size_sums_files_at_every_depth() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.bin"), vec![0u8; 100]).unwrap();
        let sub = temp.path().join("sub").join("deeper");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("b.bin"), vec![0u8; 250]).unwrap();

        assert_eq!(directory_size(temp.path()).unwrap(), 350);
    }

    #[test]
    fn directory_size_of_a_missing_path_is_zero() {
        let temp = tempfile::TempDir::new().unwrap();

        assert_eq!(directory_size(&temp.path().join("absent")).unwrap(), 0);
    }

    #[test]
    fn directory_size_of_an_empty_directory_is_zero() {
        let temp = tempfile::TempDir::new().unwrap();

        assert_eq!(directory_size(temp.path()).unwrap(), 0);
    }
}
