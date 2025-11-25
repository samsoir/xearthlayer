//! Cache path construction and filename handling.

use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use std::path::{Path, PathBuf};

/// Construct the full path for a cached DDS file.
///
/// Creates a hierarchical path structure:
/// ```text
/// <cache_dir>/<provider>/<format>/<zoom>/<row>/<format>_<zoom>_<row>_<col>.dds
/// ```
///
/// # Arguments
///
/// * `cache_dir` - Root cache directory
/// * `provider` - Provider name (e.g., "bing", "google")
/// * `tile` - Tile coordinates
/// * `format` - DDS compression format (BC1 or BC3)
///
/// # Example
///
/// ```
/// use std::path::PathBuf;
/// use xearthlayer::cache::cache_path;
/// use xearthlayer::coord::TileCoord;
/// use xearthlayer::dds::DdsFormat;
///
/// let cache_dir = PathBuf::from("/cache");
/// let tile = TileCoord { row: 12754, col: 5279, zoom: 15 };
/// let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);
///
/// assert_eq!(
///     path,
///     PathBuf::from("/cache/bing/BC1/15/12754/BC1_15_12754_5279.dds")
/// );
/// ```
pub fn cache_path(
    cache_dir: &Path,
    provider: &str,
    tile: &TileCoord,
    format: DdsFormat,
) -> PathBuf {
    let format_str = format_to_string(format);

    cache_dir
        .join(provider)
        .join(&format_str)
        .join(tile.zoom.to_string())
        .join(tile.row.to_string())
        .join(format!(
            "{}_{}_{}_{}.dds",
            format_str, tile.zoom, tile.row, tile.col
        ))
}

/// Get the directory path for a specific row.
///
/// Returns the directory where all tiles for a given row are stored.
///
/// # Example
///
/// ```
/// use std::path::PathBuf;
/// use xearthlayer::cache::row_directory;
/// use xearthlayer::coord::TileCoord;
/// use xearthlayer::dds::DdsFormat;
///
/// let cache_dir = PathBuf::from("/cache");
/// let tile = TileCoord { row: 12754, col: 5279, zoom: 15 };
/// let dir = row_directory(&cache_dir, "bing", &tile, DdsFormat::BC1);
///
/// assert_eq!(dir, PathBuf::from("/cache/bing/BC1/15/12754"));
/// ```
pub fn row_directory(
    cache_dir: &Path,
    provider: &str,
    tile: &TileCoord,
    format: DdsFormat,
) -> PathBuf {
    let format_str = format_to_string(format);

    cache_dir
        .join(provider)
        .join(&format_str)
        .join(tile.zoom.to_string())
        .join(tile.row.to_string())
}

/// Get the provider directory path.
///
/// # Example
///
/// ```
/// use std::path::PathBuf;
/// use xearthlayer::cache::provider_directory;
///
/// let cache_dir = PathBuf::from("/cache");
/// let dir = provider_directory(&cache_dir, "bing");
///
/// assert_eq!(dir, PathBuf::from("/cache/bing"));
/// ```
pub fn provider_directory(cache_dir: &Path, provider: &str) -> PathBuf {
    cache_dir.join(provider)
}

/// Convert DdsFormat to string representation for paths.
fn format_to_string(format: DdsFormat) -> String {
    match format {
        DdsFormat::BC1 => "BC1".to_string(),
        DdsFormat::BC3 => "BC3".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_path_bing_bc1() {
        let cache_dir = PathBuf::from("/home/user/.cache/xearthlayer");
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };

        let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);

        assert_eq!(
            path,
            PathBuf::from("/home/user/.cache/xearthlayer/bing/BC1/15/12754/BC1_15_12754_5279.dds")
        );
    }

    #[test]
    fn test_cache_path_google_bc3() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 25508,
            col: 10376,
            zoom: 16,
        };

        let path = cache_path(&cache_dir, "google", &tile, DdsFormat::BC3);

        assert_eq!(
            path,
            PathBuf::from("/cache/google/BC3/16/25508/BC3_16_25508_10376.dds")
        );
    }

    #[test]
    fn test_cache_path_different_providers_same_tile() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        let bing_path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);
        let google_path = cache_path(&cache_dir, "google", &tile, DdsFormat::BC1);

        assert_ne!(bing_path, google_path);
        assert!(bing_path.to_string_lossy().contains("bing"));
        assert!(google_path.to_string_lossy().contains("google"));
    }

    #[test]
    fn test_cache_path_different_formats_same_tile() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        };

        let bc1_path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);
        let bc3_path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC3);

        assert_ne!(bc1_path, bc3_path);
        assert!(bc1_path.to_string_lossy().contains("BC1"));
        assert!(bc3_path.to_string_lossy().contains("BC3"));
    }

    #[test]
    fn test_cache_path_components_present() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };

        let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);
        let path_str = path.to_string_lossy();

        // Check all components are present
        assert!(path_str.contains("bing"));
        assert!(path_str.contains("BC1"));
        assert!(path_str.contains("15"));
        assert!(path_str.contains("12754"));
        assert!(path_str.contains("5279"));
        assert!(path_str.ends_with(".dds"));
    }

    #[test]
    fn test_cache_path_filename_format() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };

        let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);
        let filename = path.file_name().unwrap().to_string_lossy();

        // Filename should be: BC1_15_12754_5279.dds
        assert_eq!(filename, "BC1_15_12754_5279.dds");
    }

    #[test]
    fn test_row_directory() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };

        let dir = row_directory(&cache_dir, "bing", &tile, DdsFormat::BC1);

        assert_eq!(dir, PathBuf::from("/cache/bing/BC1/15/12754"));
    }

    #[test]
    fn test_row_directory_contains_all_components() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 15,
        };

        let dir = row_directory(&cache_dir, "bing", &tile, DdsFormat::BC1);
        let dir_str = dir.to_string_lossy();

        assert!(dir_str.contains("bing"));
        assert!(dir_str.contains("BC1"));
        assert!(dir_str.contains("15"));
        assert!(dir_str.contains("12754"));
        assert!(!dir_str.contains("5279")); // Column not in directory
    }

    #[test]
    fn test_provider_directory() {
        let cache_dir = PathBuf::from("/cache");
        let dir = provider_directory(&cache_dir, "bing");

        assert_eq!(dir, PathBuf::from("/cache/bing"));
    }

    #[test]
    fn test_provider_directory_different_providers() {
        let cache_dir = PathBuf::from("/cache");

        let bing_dir = provider_directory(&cache_dir, "bing");
        let google_dir = provider_directory(&cache_dir, "google");

        assert_ne!(bing_dir, google_dir);
        assert!(bing_dir.to_string_lossy().ends_with("bing"));
        assert!(google_dir.to_string_lossy().ends_with("google"));
    }

    #[test]
    fn test_format_to_string() {
        assert_eq!(format_to_string(DdsFormat::BC1), "BC1");
        assert_eq!(format_to_string(DdsFormat::BC3), "BC3");
    }

    #[test]
    fn test_cache_path_with_zero_coordinates() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 0,
            col: 0,
            zoom: 1,
        };

        let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC1);

        assert_eq!(path, PathBuf::from("/cache/bing/BC1/1/0/BC1_1_0_0.dds"));
    }

    #[test]
    fn test_cache_path_with_large_coordinates() {
        let cache_dir = PathBuf::from("/cache");
        let tile = TileCoord {
            row: 99999,
            col: 88888,
            zoom: 19,
        };

        let path = cache_path(&cache_dir, "bing", &tile, DdsFormat::BC3);

        assert_eq!(
            path,
            PathBuf::from("/cache/bing/BC3/19/99999/BC3_19_99999_88888.dds")
        );
    }
}
