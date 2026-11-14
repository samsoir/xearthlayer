//! Which DSFs X-Plane would load for a 1 degree tile, highest priority
//! first: enabled `scenery_packs.ini` entries in file order, then Global
//! Scenery. Built from directory listings only, once per run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use super::dsf_tile_of_name;
use crate::xworld_pro::{parse_scenery_packs, SCENERY_PACKS_FILE};

/// One DSF that covers a tile. A lower `rank` loads first (wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsfSource {
    pub rank: u32,
    pub path: PathBuf,
}

/// Every DSF of every enabled scenery pack, by 1 degree tile.
#[derive(Debug, Default)]
pub struct PackIndex {
    tiles: HashMap<(i32, i32), Vec<DsfSource>>,
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

impl PackIndex {
    /// Lists the DSFs of the enabled packs of `install`, then Global
    /// Scenery. `skip` holds packs to leave out (XEL's own FUSE mounts).
    /// Only directories are read: no DSF is opened or stat'ed.
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
            .map(|e| pack_root(install, &e.path))
            .filter(|p| !skip.iter().any(|s| s == p))
            .collect();
        let enabled = roots.len();
        roots.push(
            install
                .join("Global Scenery")
                .join("X-Plane 12 Global Scenery"),
        );

        let listed: Vec<Vec<((i32, i32), PathBuf)>> =
            roots.par_iter().map(|r| list_dsfs(r)).collect();
        let mut tiles: HashMap<(i32, i32), Vec<DsfSource>> = HashMap::new();
        let mut dsfs = 0;
        for (rank, files) in listed.into_iter().enumerate() {
            for (tile, path) in files {
                dsfs += 1;
                tiles.entry(tile).or_default().push(DsfSource {
                    rank: rank as u32,
                    path,
                });
            }
        }
        let stats = PackIndexStats {
            packs,
            enabled,
            dsfs,
            tiles: tiles.len(),
            ms: started.elapsed().as_millis() as u64,
        };
        (Self { tiles }, stats)
    }

    /// The DSFs for a tile, highest priority first.
    pub fn sources(&self, lat: i32, lon: i32) -> &[DsfSource] {
        self.tiles.get(&(lat, lon)).map_or(&[], Vec::as_slice)
    }
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

/// `<root>/Earth nav data/*/*.dsf` by name. A pack lists a tile once, so
/// the order within a pack does not matter; unreadable folders are skipped.
fn list_dsfs(root: &Path) -> Vec<((i32, i32), PathBuf)> {
    let mut out = vec![];
    let Ok(blocks) = std::fs::read_dir(root.join("Earth nav data")) else {
        return out;
    };
    for block in blocks.flatten() {
        let Ok(files) = std::fs::read_dir(block.path()) else {
            continue;
        };
        for f in files.flatten() {
            if let Some(tile) = f.file_name().to_str().and_then(dsf_tile_of_name) {
                out.push((tile, f.path()));
            }
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
}
