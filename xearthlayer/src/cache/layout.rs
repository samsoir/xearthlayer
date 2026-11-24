//! Where each disk tier lives under `cache.directory`.
//!
//! The one statement of the on-disk layout, shared by the code that creates
//! the tiers (`service::cache_layer`) and the code that finds them again
//! (`cache clear` and `cache stats`, the legacy chunk cleanup; #398, #399):
//!
//! ```text
//! {cache_dir}/
//!   {provider}/dds/{region}/      DDS disk tier, per provider
//!   {provider}/chunks/{region}/   chunk disk tier, per provider
//!   night/{region}/               lit disk tier (night lighting)
//! ```
//!
//! `{provider}` is the provider's display name (`Bing Maps`, `Google GO2`,
//! ...). `{region}` is a 1°×1° DSF tile name, `+37-123`, as
//! [`crate::coord::format_dsf_name`] writes it. Anything else in
//! `cache.directory` (the scenery and ortho union index caches, the package
//! staging directory `tmp/`) belongs to something other than the tiers.

use std::path::{Path, PathBuf};

/// Directory, under a provider directory, of the DDS disk tier.
pub const DDS_DIR: &str = "dds";

/// Directory, under a provider directory, of the chunk disk tier.
pub const CHUNKS_DIR: &str = "chunks";

/// Directory, directly under `cache.directory`, of the lit disk tier.
pub const LIT_DIR: &str = "night";

/// The DDS disk tier's directory for `provider`.
pub fn dds_dir(cache_dir: &Path, provider: &str) -> PathBuf {
    cache_dir.join(provider).join(DDS_DIR)
}

/// The chunk disk tier's directory for `provider`.
pub fn chunk_dir(cache_dir: &Path, provider: &str) -> PathBuf {
    cache_dir.join(provider).join(CHUNKS_DIR)
}

/// The lit disk tier's directory.
pub fn lit_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join(LIT_DIR)
}

/// Whether `name` is a region directory name: `+DD+DDD`, the shape
/// [`crate::coord::format_dsf_name`] writes (sign, two latitude digits, sign,
/// three longitude digits). It checks the shape, not the range.
pub fn is_region_dir_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && matches!(b[0], b'+' | b'-')
        && b[1..3].iter().all(u8::is_ascii_digit)
        && matches!(b[3], b'+' | b'-')
        && b[4..7].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::format_dsf_name;

    #[test]
    fn every_region_name_the_cache_writes_is_recognised() {
        for lat in -90..=89 {
            for lon in -180..=179 {
                let name = format_dsf_name(lat, lon);
                assert!(is_region_dir_name(&name), "{name}");
            }
        }
    }

    #[test]
    fn other_names_in_the_cache_directory_are_not_regions() {
        for name in [
            "Bing Maps",
            "Google GO2",
            "night",
            "dds",
            "chunks",
            "tmp",
            "scenery_index.cache",
            "ortho_union_index.cache",
            "+37-123.cache",
            "+37-12",
            "+37-1234",
            "37-123",
            "+3a-123",
            "",
        ] {
            assert!(!is_region_dir_name(name), "{name:?}");
        }
    }

    #[test]
    fn tier_directories() {
        let root = Path::new("/cache");
        assert_eq!(
            dds_dir(root, "Bing Maps"),
            Path::new("/cache/Bing Maps/dds")
        );
        assert_eq!(
            chunk_dir(root, "Bing Maps"),
            Path::new("/cache/Bing Maps/chunks")
        );
        assert_eq!(lit_dir(root), Path::new("/cache/night"));
    }
}
