//! `cache clear` and `cache stats`: finding the disk tiers in
//! `cache.directory` and measuring or deleting them.
//!
//! `cache.directory` is shared. Besides the tiers it holds the scenery and
//! ortho union index caches and, by default, the package manager's staging
//! directory `tmp/` with its queue lock (#399). Only what
//! [`layout`](super::layout) names as a tier is touched here:
//!
//! - `{provider}/dds/` and `{provider}/chunks/` of every directory directly
//!   in the root that is neither a region nor `night/`;
//! - `night/`, the lit tier;
//! - region directories directly in the root: the chunk tier before it moved
//!   under its provider (#398), shared by every provider.
//!
//! Everything else in the root, and anything else in a provider directory,
//! is neither counted nor deleted.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::layout;

/// One disk tier, as found in `cache.directory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTier {
    /// Encoded DDS tiles of one provider.
    Dds { provider: String },
    /// Downloaded imagery chunks of one provider.
    Chunks { provider: String },
    /// Chunks in region directories directly in the root: the shared chunk
    /// tier of releases before #398, whose provider cannot be known.
    LegacyChunks,
    /// Lit textures and road extracts (night lighting).
    NightLighting,
}

impl fmt::Display for CacheTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dds { provider } => write!(f, "DDS tiles ({provider})"),
            Self::Chunks { provider } => write!(f, "Chunks ({provider})"),
            Self::LegacyChunks => write!(f, "Chunks (old shared layout)"),
            Self::NightLighting => write!(f, "Night lighting"),
        }
    }
}

/// The directories that hold one tier. Every tier is one directory except
/// [`CacheTier::LegacyChunks`], one per region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDirectories {
    pub tier: CacheTier,
    pub dirs: Vec<PathBuf>,
}

/// Files and bytes in one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierUsage {
    pub tier: CacheTier,
    pub files: usize,
    pub bytes: u64,
}

/// Files and bytes per tier: what `cache stats` counts, or what `cache
/// clear` removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskCacheUsage {
    /// In [`find_tiers`] order. A tier with no directory is absent.
    pub tiers: Vec<TierUsage>,
}

impl DiskCacheUsage {
    /// Files in every tier.
    pub fn files(&self) -> usize {
        self.tiers.iter().map(|t| t.files).sum()
    }

    /// Bytes in every tier.
    pub fn bytes(&self) -> u64 {
        self.tiers.iter().map(|t| t.bytes).sum()
    }
}

/// The tier directories in `cache_dir`, as [`layout`](super::layout)
/// defines them: per provider (by directory name) its DDS then its chunk
/// tier, providers in name order; then the legacy shared chunk regions; then
/// night lighting. Symbolic links are never followed. A missing `cache_dir`
/// has no tiers.
pub fn find_tiers(cache_dir: &Path) -> io::Result<Vec<TierDirectories>> {
    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut providers = Vec::new();
    let mut legacy = Vec::new();
    let mut lit = false;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // Every name the layout writes is UTF-8; another name is not ours.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == layout::LIT_DIR {
            lit = layout::looks_like_tier_dir(&entry.path());
        } else if layout::is_region_dir_name(&name) {
            // The pre-#398 chunk tier, but only a region of cache files: a
            // folder of the user's named like a region is not ours.
            if layout::holds_only_cache_entries(&entry.path()) {
                legacy.push(entry.path());
            }
        } else {
            providers.push(name);
        }
    }
    providers.sort();
    legacy.sort();

    let mut tiers = Vec::new();
    for provider in providers {
        // A directory is a tier only if it holds regions of cache files and
        // nothing else: `cache.directory` may be a folder that also holds
        // the user's own `dds` or `chunks` (#399 review).
        let dds = layout::dds_dir(cache_dir, &provider);
        if layout::looks_like_tier_dir(&dds) {
            tiers.push(TierDirectories {
                tier: CacheTier::Dds {
                    provider: provider.clone(),
                },
                dirs: vec![dds],
            });
        }
        let chunks = layout::chunk_dir(cache_dir, &provider);
        if layout::looks_like_tier_dir(&chunks) {
            tiers.push(TierDirectories {
                tier: CacheTier::Chunks { provider },
                dirs: vec![chunks],
            });
        }
    }
    if !legacy.is_empty() {
        tiers.push(TierDirectories {
            tier: CacheTier::LegacyChunks,
            dirs: legacy,
        });
    }
    if lit {
        tiers.push(TierDirectories {
            tier: CacheTier::NightLighting,
            dirs: vec![layout::lit_dir(cache_dir)],
        });
    }
    Ok(tiers)
}

/// Files and bytes under `dir`, links counted as files and never followed.
///
/// A file or directory that disappears while it is counted is skipped:
/// `stats` does not refuse while `run` is active, and the GC and the cache's
/// write-then-rename remove and rename files at any moment.
fn count_dir_contents(dir: &Path) -> io::Result<(usize, u64)> {
    let gone = |e: &io::Error| e.kind() == io::ErrorKind::NotFound;
    let mut files = 0;
    let mut bytes = 0;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if gone(&e) => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        };
        if file_type.is_dir() {
            let (sub_files, sub_bytes) = count_dir_contents(&entry.path())?;
            files += sub_files;
            bytes += sub_bytes;
        } else {
            match entry.metadata() {
                Ok(m) => {
                    files += 1;
                    bytes += m.len();
                }
                Err(e) if gone(&e) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok((files, bytes))
}

fn measure(tier: &TierDirectories) -> io::Result<TierUsage> {
    let mut usage = TierUsage {
        tier: tier.tier.clone(),
        files: 0,
        bytes: 0,
    };
    for dir in &tier.dirs {
        let (files, bytes) = count_dir_contents(dir)?;
        usage.files += files;
        usage.bytes += bytes;
    }
    Ok(usage)
}

/// Count the files and bytes of every tier in `cache_dir`. Nothing else in
/// it is counted.
pub fn disk_cache_stats(cache_dir: &Path) -> io::Result<DiskCacheUsage> {
    let tiers = find_tiers(cache_dir)?
        .iter()
        .map(measure)
        .collect::<io::Result<_>>()?;
    Ok(DiskCacheUsage { tiers })
}

/// Delete every tier directory in `cache_dir` and report what each held.
/// Nothing else in it is touched: the index caches, the package staging
/// directory and unknown entries stay, and so does each provider directory
/// itself.
///
/// It does not check whether the cache is in use; the caller refuses while
/// `xearthlayer run` or a package operation is running.
pub fn clear_disk_cache(cache_dir: &Path) -> io::Result<DiskCacheUsage> {
    let mut removed = DiskCacheUsage::default();
    for tier in find_tiers(cache_dir)? {
        let usage = measure(&tier)?;
        for dir in &tier.dirs {
            fs::remove_dir_all(dir)?;
        }
        removed.tiers.push(usage);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    const BING: &str = "Bing Maps";
    const GO2: &str = "Google GO2";

    fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0xA5; len]).unwrap();
    }

    /// A cache directory laid out like a real one, with tiers of distinct
    /// sizes so each is told apart, and everything that is not a tier.
    fn real_cache() -> TempDir {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Tiers.
        let bing_dds = layout::dds_dir(root, BING);
        write(&bing_dds.join("+37-123/tile_15_1_2.cache"), 100);
        write(&bing_dds.join("+38-122/tile_15_3_4.cache"), 200);
        write(
            &layout::chunk_dir(root, BING).join("+37-123/chunk_a.cache"),
            10,
        );
        write(
            &layout::dds_dir(root, GO2).join("+42+006/tile_15_5_6.cache"),
            300,
        );
        let go2_chunks = layout::chunk_dir(root, GO2);
        write(&go2_chunks.join("+42+006/chunk_b.cache"), 20);
        write(&go2_chunks.join("+42+006/chunk_c.cache"), 30);
        write(&go2_chunks.join("+42+006/chunk_d.cache.tmp"), 5);
        let night = layout::lit_dir(root);
        write(&night.join("+37-123/lit_1.cache"), 1000);
        write(&night.join("+37-123/roads_1.cache"), 2000);
        write(&root.join("+42+006/chunk_old_1.cache"), 7);
        write(&root.join("-33+151/chunk_old_2.cache"), 8);

        // Not tiers.
        write(&root.join("tmp/.queue.lock"), 0);
        write(&root.join("tmp/install_eu_ortho_1.0.0/part.001"), 4096);
        write(&root.join("scenery_index.cache"), 512);
        write(&root.join("ortho_union_index.cache"), 256);
        write(&root.join("notes.txt"), 3);
        write(&root.join("something-else/keep.bin"), 64);
        write(&root.join(BING).join("other/keep.bin"), 16);
        write(&root.join(BING).join("readme"), 4);
        temp
    }

    /// Every file under `root` that is not in a tier, with its contents.
    fn not_tiers(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        for rel in [
            "tmp/.queue.lock",
            "tmp/install_eu_ortho_1.0.0/part.001",
            "scenery_index.cache",
            "ortho_union_index.cache",
            "notes.txt",
            "something-else/keep.bin",
            "Bing Maps/other/keep.bin",
            "Bing Maps/readme",
        ] {
            out.insert(PathBuf::from(rel), fs::read(root.join(rel)).unwrap());
        }
        out
    }

    /// Every file under `root`, relative, with its contents.
    fn every_file(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn usage(tier: CacheTier, files: usize, bytes: u64) -> TierUsage {
        TierUsage { tier, files, bytes }
    }

    fn dds(provider: &str) -> CacheTier {
        CacheTier::Dds {
            provider: provider.to_string(),
        }
    }

    fn chunks(provider: &str) -> CacheTier {
        CacheTier::Chunks {
            provider: provider.to_string(),
        }
    }

    fn expected_usage() -> DiskCacheUsage {
        DiskCacheUsage {
            tiers: vec![
                usage(dds(BING), 2, 300),
                usage(chunks(BING), 1, 10),
                usage(dds(GO2), 1, 300),
                usage(chunks(GO2), 3, 55),
                usage(CacheTier::LegacyChunks, 2, 15),
                usage(CacheTier::NightLighting, 2, 3000),
            ],
        }
    }

    #[test]
    fn the_tiers_are_found_where_the_layout_puts_them() {
        let cache = real_cache();
        let root = cache.path();

        let found = find_tiers(root).unwrap();

        let dirs = |tier: CacheTier, dirs: Vec<PathBuf>| TierDirectories { tier, dirs };
        assert_eq!(
            found,
            vec![
                dirs(dds(BING), vec![layout::dds_dir(root, BING)]),
                dirs(chunks(BING), vec![layout::chunk_dir(root, BING)]),
                dirs(dds(GO2), vec![layout::dds_dir(root, GO2)]),
                dirs(chunks(GO2), vec![layout::chunk_dir(root, GO2)]),
                dirs(
                    CacheTier::LegacyChunks,
                    vec![root.join("+42+006"), root.join("-33+151")]
                ),
                dirs(CacheTier::NightLighting, vec![layout::lit_dir(root)]),
            ]
        );
    }

    #[test]
    fn stats_count_exactly_the_tier_files_per_tier() {
        let cache = real_cache();

        let stats = disk_cache_stats(cache.path()).unwrap();

        assert_eq!(stats, expected_usage());
        assert_eq!(stats.files(), 11);
        assert_eq!(stats.bytes(), 3680);
    }

    #[test]
    fn stats_read_and_change_nothing() {
        let cache = real_cache();
        let before = every_file(cache.path());

        disk_cache_stats(cache.path()).unwrap();

        assert_eq!(every_file(cache.path()), before);
    }

    #[test]
    fn clear_removes_exactly_the_tier_directories_and_reports_them() {
        let cache = real_cache();
        let root = cache.path();
        let kept = not_tiers(root);

        let removed = clear_disk_cache(root).unwrap();

        assert_eq!(removed, expected_usage());
        assert_eq!(every_file(root), kept);
        for gone in [
            layout::dds_dir(root, BING),
            layout::chunk_dir(root, BING),
            layout::dds_dir(root, GO2),
            layout::chunk_dir(root, GO2),
            layout::lit_dir(root),
            root.join("+42+006"),
            root.join("-33+151"),
        ] {
            assert!(!gone.exists(), "{}", gone.display());
        }
        // The provider directories themselves stay.
        assert!(root.join(BING).is_dir());
        assert!(root.join(GO2).is_dir());
        // A second clear finds nothing.
        assert_eq!(clear_disk_cache(root).unwrap(), DiskCacheUsage::default());
    }

    #[test]
    fn a_missing_cache_directory_has_no_tiers() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("no-cache");

        assert!(find_tiers(&missing).unwrap().is_empty());
        assert_eq!(
            disk_cache_stats(&missing).unwrap(),
            DiskCacheUsage::default()
        );
        assert_eq!(
            clear_disk_cache(&missing).unwrap(),
            DiskCacheUsage::default()
        );
    }

    /// A tier name that is a file, not a directory, is not a tier.
    #[test]
    fn folders_that_are_not_tiers_are_never_touched() {
        // `cache.directory` pointed at a folder that also holds other things,
        // the home directory for example (#399 review).
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(&root.join("Documents/dds/report.pdf"), 10);
        write(&root.join("work/chunks/src/main.rs"), 10);
        write(&root.join("night/notes.txt"), 10);
        write(&root.join("+30-120/+30-120.dsf"), 10);
        let before = every_file(root);

        assert!(find_tiers(root).unwrap().is_empty());
        clear_disk_cache(root).unwrap();
        assert_eq!(every_file(root), before);
    }

    #[test]
    fn files_named_like_tiers_are_not_tiers() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(&root.join("night"), 1);
        write(&root.join("+37-123"), 1);
        write(&root.join(BING).join("dds"), 1);
        let before = every_file(root);

        assert!(find_tiers(root).unwrap().is_empty());
        clear_disk_cache(root).unwrap();
        assert_eq!(every_file(root), before);
    }

    /// A link is never followed: a tier directory that is a symbolic link to
    /// somewhere else must not empty that somewhere else.
    #[test]
    fn symbolic_links_are_not_followed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let elsewhere = temp.path().join("elsewhere");
        write(&elsewhere.join("precious.bin"), 9);
        fs::create_dir_all(root.join(BING)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(BING).join("dds")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("night")).unwrap();
        write(&layout::chunk_dir(&root, GO2).join("+37-123/c.cache"), 4);
        std::os::unix::fs::symlink(
            &elsewhere,
            layout::chunk_dir(&root, GO2).join("+37-123/link"),
        )
        .unwrap();

        clear_disk_cache(&root).unwrap();

        assert!(elsewhere.join("precious.bin").is_file());
        // A region holding a link is not a region of cache files, so the
        // whole tier is left alone rather than trusted (see
        // `layout::looks_like_tier_dir`).
        assert!(layout::chunk_dir(&root, GO2)
            .join("+37-123/c.cache")
            .is_file());
    }

    #[test]
    fn tiers_name_themselves_for_people() {
        assert_eq!(dds(BING).to_string(), "DDS tiles (Bing Maps)");
        assert_eq!(chunks(GO2).to_string(), "Chunks (Google GO2)");
        assert_eq!(
            CacheTier::LegacyChunks.to_string(),
            "Chunks (old shared layout)"
        );
        assert_eq!(CacheTier::NightLighting.to_string(), "Night lighting");
    }
}
