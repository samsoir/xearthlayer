//! Which DSFs X-Plane would load for a 1 degree tile, highest priority
//! first: enabled `scenery_packs.ini` entries in file order, then Global
//! Scenery. Built from directory listings only, once per run.
//!
//! The index lives for the session, so it holds no path per DSF: one root
//! per pack and a sorted array of six-byte entries (tile, rank, spelling).
//! A DSF spelled as X-Plane names it (`Earth nav data/+40+000/+46+006.dsf`)
//! has its path rebuilt from its pack's root; any other spelling the
//! listing accepts keeps its relative path in a small side table.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use super::dsf_tile_of_name;
use crate::coord::format_dsf_name;
use crate::xworld_pro::{parse_scenery_packs, SCENERY_PACKS_FILE};

/// One DSF that covers a tile. A lower `rank` loads first (wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsfSource {
    pub rank: u32,
    pub path: PathBuf,
}

/// One DSF of one pack. Sorted by tile, then rank, so a tile's sources are
/// one run, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    lat: i8,
    lon: i16,
    rank: u16,
    /// Not the standard spelling: its relative path is in the side table.
    unusual: bool,
}

/// A side table key: tile and rank, ordered as [`Entry`] orders them.
type SideKey = (i8, i16, u16);

impl Entry {
    fn key(&self) -> SideKey {
        (self.lat, self.lon, self.rank)
    }
}

/// Every DSF of every enabled scenery pack, by 1 degree tile.
#[derive(Debug, Default)]
pub struct PackIndex {
    /// Pack roots; a pack's rank is its index.
    roots: Vec<PathBuf>,
    entries: Vec<Entry>,
    /// The relative path of every unusual entry, sorted by key; entries of
    /// one key keep their listing order.
    side: Vec<(SideKey, PathBuf)>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PackIndexStats {
    /// Pack lines in `scenery_packs.ini`, enabled or not.
    pub packs: usize,
    /// Enabled packs that were listed (XEL's own mounts left out).
    pub enabled: usize,
    pub dsfs: usize,
    pub tiles: usize,
    pub ms: u64,
}

const GLOBAL_AIRPORTS: &str = "*GLOBAL_AIRPORTS*";

const EARTH_NAV_DATA: &str = "Earth nav data";

/// The most packs a rank can name: enabled packs past `MAX_PACKS - 1` are
/// left out, so Global Scenery always has a rank. No installation comes
/// near it.
const MAX_PACKS: usize = u16::MAX as usize + 1;

impl PackIndex {
    /// Lists the DSFs of the enabled packs of `install`, then Global
    /// Scenery. `skip` holds packs to leave out (XEL's own FUSE mounts).
    /// Only directories are read: no DSF is opened or stat'ed.
    ///
    /// A pack is left out when its resolved root equals a `skip` path
    /// (trailing separators ignored), or when its ini entry is a relative
    /// path inside the install and its folder name equals a `skip` path's
    /// folder name. The name rule covers a mount point spelled with another
    /// prefix (symlink, case); an absolute entry elsewhere that happens to
    /// share the name is kept, because XEL's mounts live in `Custom Scenery`.
    pub fn build(install: &Path, skip: &[PathBuf]) -> (Self, PackIndexStats) {
        let started = Instant::now();
        let ini = install.join("Custom Scenery").join(SCENERY_PACKS_FILE);
        let entries = std::fs::read_to_string(&ini)
            .map(|t| parse_scenery_packs(&t))
            .unwrap_or_default();
        let packs = entries.len();
        let mut roots: Vec<PathBuf> = entries
            .iter()
            .filter(|e| e.enabled)
            .filter_map(|e| {
                let root = pack_root(install, &e.path);
                (!is_skipped(&e.path, &root, skip)).then_some(root)
            })
            .take(MAX_PACKS - 1)
            .collect();
        let enabled = roots.len();
        roots.push(
            install
                .join("Global Scenery")
                .join("X-Plane 12 Global Scenery"),
        );

        let listed: Vec<Listing> = roots
            .par_iter()
            .enumerate()
            .map(|(rank, r)| list_dsfs(r, rank as u16))
            .collect();
        let mut entries = Vec::with_capacity(listed.iter().map(|l| l.entries.len()).sum());
        let mut side = Vec::with_capacity(listed.iter().map(|l| l.side.len()).sum());
        for l in listed {
            entries.extend(l.entries);
            side.extend(l.side);
        }
        entries.sort_unstable();
        // Stable: entries of one key keep their listing order.
        side.sort_by_key(|&(k, _)| k);
        let tiles = entries
            .chunk_by(|a, b| (a.lat, a.lon) == (b.lat, b.lon))
            .count();
        let stats = PackIndexStats {
            packs,
            enabled,
            dsfs: entries.len(),
            tiles,
            ms: started.elapsed().as_millis() as u64,
        };
        roots.shrink_to_fit();
        (
            Self {
                roots,
                entries,
                side,
            },
            stats,
        )
    }

    /// The DSFs for a tile, highest priority first.
    pub fn sources(&self, lat: i32, lon: i32) -> Vec<DsfSource> {
        let (Ok(lat8), Ok(lon16)) = (i8::try_from(lat), i16::try_from(lon)) else {
            return Vec::new();
        };
        let tile = (lat8, lon16);
        let run = |v: &[Entry]| {
            let lo = v.partition_point(|e| (e.lat, e.lon) < tile);
            let hi = lo + v[lo..].partition_point(|e| (e.lat, e.lon) == tile);
            lo..hi
        };
        let entries = &self.entries[run(&self.entries)];
        let lo = self.side.partition_point(|&((a, b, _), _)| (a, b) < tile);
        // Unusual entries and their side paths come in the same order.
        let mut side = self.side[lo..].iter().map(|(_, p)| p);
        entries
            .iter()
            .filter_map(|e| {
                let root = &self.roots[usize::from(e.rank)];
                let path = if e.unusual {
                    root.join(side.next()?)
                } else {
                    root.join(standard_relative(lat, lon))
                };
                Some(DsfSource {
                    rank: u32::from(e.rank),
                    path,
                })
            })
            .collect()
    }

    /// Heap and struct bytes held: roots, entries and the side table.
    pub fn bytes(&self) -> usize {
        let roots: usize = self.roots.iter().map(PathBuf::capacity).sum();
        let side: usize = self.side.iter().map(|(_, p)| p.capacity()).sum();
        std::mem::size_of::<Self>()
            + self.roots.capacity() * std::mem::size_of::<PathBuf>()
            + roots
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.side.capacity() * std::mem::size_of::<(SideKey, PathBuf)>()
            + side
    }
}

/// The 10 degree block folder of a tile, as X-Plane names it: `+40+000`.
fn block_name(lat: i32, lon: i32) -> String {
    format!(
        "{:+03}{:+04}",
        lat.div_euclid(10) * 10,
        lon.div_euclid(10) * 10
    )
}

/// The DSF file name of a tile, as X-Plane names it: `+46+006.dsf`.
fn file_name(lat: i32, lon: i32) -> String {
    format!("{}.dsf", format_dsf_name(lat, lon))
}

/// A tile's DSF path under its pack root, as X-Plane names it.
fn standard_relative(lat: i32, lon: i32) -> PathBuf {
    Path::new(EARTH_NAV_DATA)
        .join(block_name(lat, lon))
        .join(file_name(lat, lon))
}

/// Whether the pack of ini `entry` (resolving to `root`) is one of `skip`.
fn is_skipped(entry: &str, root: &Path, skip: &[PathBuf]) -> bool {
    let entry = entry.trim();
    let inside_install = entry != GLOBAL_AIRPORTS && !Path::new(entry).is_absolute();
    skip.iter().any(|s| {
        // `Path` equality compares components, so separators at the end
        // never matter.
        root == s.as_path()
            || (inside_install && s.file_name().is_some() && root.file_name() == s.file_name())
    })
}

/// The folder a `scenery_packs.ini` path names.
fn pack_root(install: &Path, entry: &str) -> PathBuf {
    let entry = entry.trim();
    if entry == GLOBAL_AIRPORTS {
        return install.join("Global Scenery").join("Global Airports");
    }
    let p = Path::new(entry.trim_end_matches(['/', '\\']));
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        install.join(p)
    }
}

/// One pack's DSFs.
#[derive(Default)]
struct Listing {
    entries: Vec<Entry>,
    side: Vec<(SideKey, PathBuf)>,
}

/// `<root>/Earth nav data/*/*.dsf` by name, straight into entries: only an
/// unusual spelling allocates a path. Unreadable folders are skipped.
fn list_dsfs(root: &Path, rank: u16) -> Listing {
    let mut out = Listing::default();
    let Ok(blocks) = std::fs::read_dir(root.join(EARTH_NAV_DATA)) else {
        return out;
    };
    for block in blocks.flatten() {
        let Ok(files) = std::fs::read_dir(block.path()) else {
            continue;
        };
        let block_folder = block.file_name();
        for f in files.flatten() {
            let name = f.file_name();
            let Some((lat, lon)) = name.to_str().and_then(dsf_tile_of_name) else {
                continue;
            };
            let unusual = name.to_str() != Some(&file_name(lat, lon))
                || block_folder.to_str() != Some(&block_name(lat, lon));
            let e = Entry {
                lat: lat as i8,
                lon: lon as i16,
                rank,
                unusual,
            };
            if unusual {
                let rel = Path::new(EARTH_NAV_DATA).join(&block_folder).join(&name);
                out.side.push((e.key(), rel));
            }
            out.entries.push(e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &std::path::Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn sources_follow_scenery_packs_order_with_global_scenery_last() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        std::fs::create_dir_all(i.join("Custom Scenery")).unwrap();
        std::fs::write(
            i.join("Custom Scenery/scenery_packs.ini"),
            "I\n1000 Version\nSCENERY\n\n\
             SCENERY_PACK Custom Scenery/Airport/\n\
             SCENERY_PACK_DISABLED Custom Scenery/Off/\n\
             SCENERY_PACK *GLOBAL_AIRPORTS*\n\
             SCENERY_PACK Custom Scenery/XWP-roads/\n\
             SCENERY_PACK Custom Scenery/zzXEL_ortho/\n",
        )
        .unwrap();
        for p in ["Airport", "Off", "XWP-roads", "zzXEL_ortho"] {
            touch(&i.join(format!(
                "Custom Scenery/{p}/Earth nav data/+40+000/+46+006.dsf"
            )));
        }
        touch(&i.join("Global Scenery/Global Airports/Earth nav data/+40+000/+46+006.dsf"));
        touch(
            &i.join("Global Scenery/X-Plane 12 Global Scenery/Earth nav data/+40+000/+46+006.dsf"),
        );
        touch(
            &i.join("Global Scenery/X-Plane 12 Global Scenery/Earth nav data/+40+000/+47+006.dsf"),
        );
        let (idx, stats) = PackIndex::build(i, &[i.join("Custom Scenery/zzXEL_ortho")]);
        let names: Vec<String> = idx
            .sources(46, 6)
            .iter()
            .map(|s| {
                s.path
                    .strip_prefix(i)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "Custom Scenery/Airport/Earth nav data/+40+000/+46+006.dsf",
                "Global Scenery/Global Airports/Earth nav data/+40+000/+46+006.dsf",
                "Custom Scenery/XWP-roads/Earth nav data/+40+000/+46+006.dsf",
                "Global Scenery/X-Plane 12 Global Scenery/Earth nav data/+40+000/+46+006.dsf",
            ]
        );
        let ranks: Vec<u32> = idx.sources(46, 6).iter().map(|s| s.rank).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
        assert_eq!(idx.sources(47, 6).len(), 1);
        assert!(idx.sources(10, 10).is_empty());
        assert_eq!(stats.packs, 5);
        assert_eq!(stats.enabled, 3);
        assert_eq!(stats.dsfs, 5);
        assert_eq!(stats.tiles, 2);
    }

    #[test]
    fn without_scenery_packs_only_global_scenery_is_indexed() {
        let t = tempfile::tempdir().unwrap();
        touch(
            &t.path().join(
                "Global Scenery/X-Plane 12 Global Scenery/Earth nav data/+40+000/+46+006.dsf",
            ),
        );
        let (idx, _) = PackIndex::build(t.path(), &[]);
        assert_eq!(idx.sources(46, 6).len(), 1);
    }

    #[test]
    fn absolute_pack_paths_are_kept_and_other_files_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let i = t.path();
        std::fs::create_dir_all(i.join("Custom Scenery")).unwrap();
        std::fs::write(
            i.join("Custom Scenery/scenery_packs.ini"),
            format!(
                "I\n1000 Version\nSCENERY\n\nSCENERY_PACK {}/\n",
                elsewhere.path().display()
            ),
        )
        .unwrap();
        touch(&elsewhere.path().join("Earth nav data/+40+000/+46+006.DSF"));
        touch(
            &elsewhere
                .path()
                .join("Earth nav data/+40+000/+46+006.dsf.bak"),
        );
        touch(&elsewhere.path().join("Earth nav data/readme.txt"));
        let (idx, stats) = PackIndex::build(i, &[]);
        assert_eq!(idx.sources(46, 6).len(), 1);
        assert_eq!(stats.dsfs, 1);
    }

    /// An ini listing of `entries` (all enabled) for an install in `i`.
    fn write_ini(i: &std::path::Path, entries: &[String]) {
        std::fs::create_dir_all(i.join("Custom Scenery")).unwrap();
        let mut text = String::from("I\n1000 Version\nSCENERY\n\n");
        for e in entries {
            text.push_str(&format!("SCENERY_PACK {e}\n"));
        }
        std::fs::write(i.join("Custom Scenery/scenery_packs.ini"), text).unwrap();
    }

    const DSF: &str = "Earth nav data/+40+000/+46+006.dsf";

    #[test]
    fn a_skip_path_spelled_with_trailing_separators_still_skips() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        write_ini(i, &["Custom Scenery/zzXEL_ortho/".into()]);
        touch(&i.join(format!("Custom Scenery/zzXEL_ortho/{DSF}")));
        let skip = PathBuf::from(format!("{}/Custom Scenery/zzXEL_ortho//", i.display()));
        let (idx, stats) = PackIndex::build(i, &[skip]);
        assert!(idx.sources(46, 6).is_empty());
        assert_eq!(stats.enabled, 0);
    }

    #[test]
    fn a_relative_pack_is_skipped_by_folder_name_when_the_prefix_differs() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        write_ini(i, &["Custom Scenery/zzXEL_ortho/".into()]);
        touch(&i.join(format!("Custom Scenery/zzXEL_ortho/{DSF}")));
        // The mount manager's spelling of the same folder: another prefix.
        let skip = PathBuf::from("/somewhere/else/entirely/zzXEL_ortho");
        let (idx, stats) = PackIndex::build(i, &[skip]);
        assert!(idx.sources(46, 6).is_empty());
        assert_eq!(stats.enabled, 0);
    }

    #[test]
    fn an_absolute_pack_with_the_same_folder_name_is_not_skipped_by_name() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let i = t.path();
        let pack = elsewhere.path().join("zzXEL_ortho");
        write_ini(i, &[format!("{}/", pack.display())]);
        touch(&pack.join(DSF));
        let (idx, stats) = PackIndex::build(i, &[i.join("Custom Scenery/zzXEL_ortho")]);
        assert_eq!(idx.sources(46, 6).len(), 1);
        assert_eq!(stats.enabled, 1);
    }

    /// A pack `Custom Scenery/<pack>` holding `files` (paths under the
    /// pack root), listed alone in the ini.
    fn one_pack(i: &std::path::Path, pack: &str, files: &[&str]) {
        write_ini(i, &[format!("Custom Scenery/{pack}/")]);
        for f in files {
            touch(&i.join(format!("Custom Scenery/{pack}/{f}")));
        }
    }

    #[test]
    fn a_standard_spelling_rebuilds_the_path_that_exists_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        let files = [
            ((46, 6), "Earth nav data/+40+000/+46+006.dsf"),
            ((-34, -71), "Earth nav data/-40-080/-34-071.dsf"),
            ((0, 0), "Earth nav data/+00+000/+00+000.dsf"),
            ((-1, -1), "Earth nav data/-10-010/-01-001.dsf"),
            ((89, 179), "Earth nav data/+80+170/+89+179.dsf"),
            ((-90, -180), "Earth nav data/-90-180/-90-180.dsf"),
            ((9, -100), "Earth nav data/+00-100/+09-100.dsf"),
        ];
        one_pack(i, "P", &files.map(|(_, f)| f));
        let (idx, stats) = PackIndex::build(i, &[]);
        assert_eq!(stats.dsfs, files.len());
        assert!(idx.side.is_empty(), "every spelling is standard");
        for ((lat, lon), f) in files {
            let s = idx.sources(lat, lon);
            assert_eq!(s.len(), 1, "{f}");
            assert_eq!(s[0].path, i.join("Custom Scenery/P").join(f));
            assert!(s[0].path.is_file(), "{f}");
        }
    }

    #[test]
    fn an_unusual_spelling_keeps_its_real_path() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        let files = [
            ((46, 6), "Earth nav data/+40+000/+46+006.DSF"),
            // Filed under another block's folder.
            ((46, 7), "Earth nav data/+30+000/+46+007.dsf"),
            // A block folder spelled without the zero padding.
            ((46, 8), "Earth nav data/+40+0/+46+008.dsf"),
            ((46, 9), "Earth nav data/+40+000/+46+009.dsf"),
        ];
        one_pack(i, "P", &files.map(|(_, f)| f));
        let (idx, stats) = PackIndex::build(i, &[]);
        assert_eq!(stats.dsfs, files.len());
        assert_eq!(idx.side.len(), 3, "the standard one needs no name");
        for ((lat, lon), f) in files {
            let s = idx.sources(lat, lon);
            assert_eq!(s.len(), 1, "{f}");
            assert_eq!(s[0].path, i.join("Custom Scenery/P").join(f));
            assert!(s[0].path.is_file(), "{f}");
        }
    }

    #[test]
    fn both_spellings_of_a_tile_in_one_pack_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        let files = [
            "Earth nav data/+40+000/+46+006.dsf",
            "Earth nav data/+40+000/+46+006.DSF",
            "Earth nav data/+30+000/+46+006.dsf",
        ];
        one_pack(i, "P", &files);
        let (idx, stats) = PackIndex::build(i, &[]);
        let mut got: Vec<PathBuf> = idx.sources(46, 6).into_iter().map(|s| s.path).collect();
        got.sort();
        let mut want: Vec<PathBuf> = files
            .iter()
            .map(|f| i.join("Custom Scenery/P").join(f))
            .collect();
        want.sort();
        assert_eq!(got, want);
        assert_eq!((stats.dsfs, stats.tiles), (3, 1));
    }

    #[test]
    fn sources_mix_spellings_in_rank_order() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        let packs = ["A", "B", "C", "D"];
        write_ini(i, &packs.map(|p| format!("Custom Scenery/{p}/")));
        // Standard and unusual spellings alternate down the ranks.
        let file = |k: usize| {
            if k % 2 == 0 {
                "Earth nav data/+40+000/+46+006.dsf"
            } else {
                "Earth nav data/+40+000/+46+006.DSF"
            }
        };
        for (k, p) in packs.iter().enumerate() {
            touch(&i.join(format!("Custom Scenery/{p}/{}", file(k))));
        }
        let (idx, _) = PackIndex::build(i, &[]);
        let s = idx.sources(46, 6);
        assert_eq!(s.iter().map(|s| s.rank).collect::<Vec<_>>(), [0, 1, 2, 3]);
        for (k, (src, p)) in s.iter().zip(packs).enumerate() {
            assert_eq!(src.path, i.join(format!("Custom Scenery/{p}/{}", file(k))));
        }
    }

    #[test]
    fn the_index_holds_six_bytes_per_dsf_and_counts_every_byte() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        let packs = ["A", "B", "C"];
        write_ini(i, &packs.map(|p| format!("Custom Scenery/{p}/")));
        let mut dsfs = 0;
        for p in packs {
            for lat in 40..50 {
                for lon in -60..60 {
                    let name = crate::coord::format_dsf_name(lat, lon);
                    let block = block_name(lat, lon);
                    touch(&i.join(format!(
                        "Custom Scenery/{p}/Earth nav data/{block}/{name}.dsf"
                    )));
                    dsfs += 1;
                }
            }
        }
        touch(&i.join("Custom Scenery/A/Earth nav data/+40+000/+41+001.DSF"));
        let (idx, stats) = PackIndex::build(i, &[]);
        assert_eq!(stats.dsfs, dsfs + 1);
        assert_eq!(stats.tiles, 10 * 120);
        assert_eq!(std::mem::size_of::<Entry>(), 6, "an entry must not grow");
        assert_eq!(std::mem::size_of_val(&*idx.entries), 6 * (dsfs + 1));
        let roots: usize = idx
            .roots
            .iter()
            .map(|r| std::mem::size_of::<PathBuf>() + r.capacity())
            .sum();
        let side: usize = idx
            .side
            .iter()
            .map(|(_, p)| std::mem::size_of::<(SideKey, PathBuf)>() + p.capacity())
            .sum();
        assert_eq!(idx.side.len(), 1);
        assert_eq!(
            idx.bytes(),
            std::mem::size_of::<PackIndex>() + roots + 6 * (dsfs + 1) + side
        );
        // Under 7 bytes per DSF all told, at this size.
        assert!(idx.bytes() < 7 * (dsfs + 1), "{}", idx.bytes());
    }

    #[test]
    fn packs_past_the_last_rank_are_left_out_and_global_scenery_stays_last() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        // More enabled packs than ranks; only the first and the last exist.
        let mut entries: Vec<String> = (0..=usize::from(u16::MAX))
            .map(|k| format!("Custom Scenery/P{k}/"))
            .collect();
        entries[0] = "Custom Scenery/First/".into();
        write_ini(i, &entries);
        touch(&i.join(format!("Custom Scenery/First/{DSF}")));
        touch(&i.join(format!("Custom Scenery/P{}/{DSF}", u16::MAX)));
        touch(&i.join(format!("Global Scenery/X-Plane 12 Global Scenery/{DSF}")));
        let (idx, stats) = PackIndex::build(i, &[]);
        let s = idx.sources(46, 6);
        assert_eq!(stats.enabled, usize::from(u16::MAX));
        assert_eq!(s.len(), 2, "the pack past the last rank is left out");
        assert_eq!(s[0].rank, 0);
        assert_eq!(s[1].rank, u32::from(u16::MAX));
        assert!(s[1].path.starts_with(i.join("Global Scenery")));
    }

    #[test]
    fn a_skip_path_does_not_skip_a_pack_with_another_name() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        write_ini(i, &["Custom Scenery/Other/".into()]);
        touch(&i.join(format!("Custom Scenery/Other/{DSF}")));
        let (idx, _) = PackIndex::build(i, &[i.join("Custom Scenery/zzXEL_ortho")]);
        assert_eq!(idx.sources(46, 6).len(), 1);
    }
}
