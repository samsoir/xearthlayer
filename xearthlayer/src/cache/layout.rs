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

/// The temp file one write of a disk tier entry goes through before it is
/// renamed over the entry: `{entry file name}.{pid}.{n}.tmp`, `n` counting
/// the process's writes, so concurrent writes never share one (see
/// `providers::disk`). It ends in `.tmp`, which [`holds_only_cache_entries`]
/// accepts and the index scan, reading `*.cache` only, skips.
pub fn temp_file_name(entry: &std::ffi::OsStr, pid: u32, n: u64) -> std::ffi::OsString {
    let mut name = entry.to_os_string();
    name.push(format!(".{pid}.{n}.tmp"));
    name
}

/// The process that wrote the temp file `name` ([`temp_file_name`] of a
/// `*.cache` entry), or `None` for any other name: an entry, a temp file
/// of the older `{stem}.tmp` form, anything else.
pub fn temp_file_pid(name: &str) -> Option<u32> {
    let rest = name.strip_suffix(".tmp")?;
    let (rest, n) = rest.rsplit_once('.')?;
    let (entry, pid) = rest.rsplit_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(n) && digits(pid) && entry.ends_with(".cache")) {
        return None;
    }
    n.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// Whether the region directory `region` holds only what a disk tier
/// writes: regular files named `*.cache`, or `*.tmp` left by an interrupted
/// write. No subdirectory, no symbolic link, nothing else. An empty region
/// qualifies; one that cannot be read does not.
///
/// The guard for code that deletes tiers (`cache clear`, the legacy chunk
/// cleanup): a region name alone is not proof, because `cache.directory`
/// can point at a folder that also holds the user's own files, scenery
/// folders named like regions among them. Reads the directory.
pub fn holds_only_cache_entries(region: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(region) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            let name = entry.file_name();
            let name = name.to_string_lossy();
            is_file && (name.ends_with(".cache") || name.ends_with(".tmp"))
        })
    })
}

/// Whether `dir` looks like a disk tier: a directory, not a link to one,
/// whose every entry is a region directory (by [`is_region_dir_name`])
/// holding only cache entries ([`holds_only_cache_entries`]). An empty
/// directory qualifies. Reads the directory.
pub fn looks_like_tier_dir(dir: &Path) -> bool {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            entry.file_type().is_ok_and(|t| t.is_dir())
                && entry.file_name().to_str().is_some_and(is_region_dir_name)
                && holds_only_cache_entries(&entry.path())
        })
    })
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

    fn region_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn a_temp_file_names_the_process_that_wrote_it() {
        let name = temp_file_name("tile_12_1_2.cache".as_ref(), 4242, 7);
        assert_eq!(name, "tile_12_1_2.cache.4242.7.tmp");
        assert_eq!(temp_file_pid(name.to_str().unwrap()), Some(4242));
        for other in [
            "tile_12_1_2.cache",
            "tile_12_1_2.tmp",
            "tile_12_1_2.cache.tmp",
            "tile_12_1_2.cache.42.tmp",
            "tile_12_1_2.cache.x.7.tmp",
            "tile_12_1_2.cache.42.y.tmp",
            "notes.4242.7.tmp",
            "tile_12_1_2.cache.99999999999.7.tmp",
        ] {
            assert_eq!(temp_file_pid(other), None, "{other}");
        }
    }

    #[test]
    fn a_region_of_cache_files_holds_only_cache_entries() {
        let dir = region_with(&[
            "tile_12_1_2.cache",
            "chunk_12_1_2_0_0.cache",
            "tile_12_1_3.tmp",
        ]);
        assert!(holds_only_cache_entries(dir.path()));
        assert!(holds_only_cache_entries(region_with(&[]).path()));
    }

    #[test]
    fn a_region_holding_anything_else_does_not() {
        // Real scenery: a DSF in a folder named like a region (#398 review).
        assert!(!holds_only_cache_entries(
            region_with(&["+30-120.dsf"]).path()
        ));
        assert!(!holds_only_cache_entries(
            region_with(&["tile.cache", "notes.txt"]).path()
        ));

        let with_subdir = region_with(&["tile.cache"]);
        std::fs::create_dir(with_subdir.path().join("nested")).unwrap();
        assert!(!holds_only_cache_entries(with_subdir.path()));

        let missing = tempfile::tempdir().unwrap();
        assert!(!holds_only_cache_entries(&missing.path().join("absent")));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_cache_file_is_not_a_cache_entry() {
        let target = region_with(&["tile.cache"]);
        let region = region_with(&[]);
        std::os::unix::fs::symlink(
            target.path().join("tile.cache"),
            region.path().join("tile.cache"),
        )
        .unwrap();
        assert!(!holds_only_cache_entries(region.path()));
    }

    #[test]
    fn a_tier_directory_holds_only_regions_of_cache_entries() {
        let tier = tempfile::tempdir().unwrap();
        assert!(looks_like_tier_dir(tier.path()), "an empty tier");
        std::fs::create_dir(tier.path().join("+37-123")).unwrap();
        std::fs::write(tier.path().join("+37-123/tile_12_1_2.cache"), b"x").unwrap();
        assert!(looks_like_tier_dir(tier.path()));
    }

    #[test]
    fn a_directory_that_merely_has_a_tier_name_is_not_a_tier() {
        // `~/work/dds` when `cache.directory` is the home directory.
        let not_tier = tempfile::tempdir().unwrap();
        std::fs::write(not_tier.path().join("report.pdf"), b"x").unwrap();
        assert!(!looks_like_tier_dir(not_tier.path()));

        let other_subdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(other_subdir.path().join("src")).unwrap();
        assert!(!looks_like_tier_dir(other_subdir.path()));

        let region_with_dsf = tempfile::tempdir().unwrap();
        std::fs::create_dir(region_with_dsf.path().join("+30-120")).unwrap();
        std::fs::write(region_with_dsf.path().join("+30-120/+30-120.dsf"), b"x").unwrap();
        assert!(!looks_like_tier_dir(region_with_dsf.path()));
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
