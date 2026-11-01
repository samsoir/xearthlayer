//! What an X-World Pro install should change for ortho scenery: the load
//! order, the ortho library, mesh variants, and the install script's links.
//! Advice only: XEarthLayer never changes these itself.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::detect::{Continent, Detection};
use super::packs::PackEntry;

const O4XP_SUFFIX: &str = "_O4XP-Mesh";
const XP12_SUFFIX: &str = "_XP12-Mesh";
const OVERLAY_ENTRY: &str = "yzXEL_overlay";
const SEASONS: [&str; 4] = ["spr", "sum", "fal", "win"];

/// An XEarthLayer entry loading above an X-World Pro layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderViolation {
    /// The topmost enabled XEarthLayer entry.
    pub xel: PackEntry,
    /// The lowest enabled X-World Pro layer below it.
    pub xwp: PackEntry,
}

impl OrderViolation {
    pub fn describe(&self) -> String {
        format!(
            "{} (scenery_packs.ini line {}) loads above {} (line {})",
            self.xel.name(),
            self.xel.line,
            self.xwp.name(),
            self.xwp.line
        )
    }
}

/// Every enabled XEarthLayer entry that will load must come after every enabled
/// X-World Pro layer that will load. The library is not checked: it has no terrain.
pub fn order_violation(d: &Detection) -> Option<OrderViolation> {
    let xel = d
        .xel_entries
        .iter()
        .filter(|e| e.enabled)
        // XEarthLayer removes yzXEL_overlay itself (whole planet, disabled
        // overlays); an absent folder is not loaded, so it cannot be misplaced.
        .filter(|e| e.name() != OVERLAY_ENTRY || d.custom_scenery.join(OVERLAY_ENTRY).is_dir())
        .min_by_key(|e| e.line)?;
    let xwp = d
        .layers
        .iter()
        .filter(|l| l.entry.enabled && l.resolved)
        .map(|l| &l.entry)
        .max_by_key(|e| e.line)?;
    (xel.line < xwp.line).then(|| OrderViolation {
        xel: xel.clone(),
        xwp: xwp.clone(),
    })
}

/// One thing to fix in an X-World Pro install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// `library.txt` differs from `library - ortho.txt`.
    LibraryNotOrtho { library: PathBuf },
    /// A layer with mesh variants does not have only `_O4XP-Mesh` enabled.
    MeshVariant { continent: Continent, base: String },
    /// An install-script link into X-Plane's `Resources` is missing.
    MissingInstallLink { expected: PathBuf },
    /// An enabled entry whose folder does not exist.
    BrokenEntry { name: String, line: usize },
}

impl Finding {
    pub fn message(&self) -> String {
        match self {
            Finding::LibraryNotOrtho { library } => format!(
                "X-World Pro library.txt is not the ortho version: replace it with \
                 'library - ortho.txt' as the X-World Pro manual describes ({})",
                library.display()
            ),
            Finding::MeshVariant { continent, base } => format!(
                "X-World Pro {} {}: enable the {} version and disable {} \
                 (XEarthLayer scenery uses the Ortho4XP mesh)",
                continent.label(),
                base,
                O4XP_SUFFIX.trim_start_matches('_'),
                XP12_SUFFIX.trim_start_matches('_')
            ),
            Finding::MissingInstallLink { expected } => format!(
                "X-World Pro install link missing: {}. Run the library's install \
                 script (install_XWP_Linux_Mac.sh) again in the X-World Pro library folder \
                 inside Custom Scenery (X-Plane updates can remove these links)",
                expected.display()
            ),
            Finding::BrokenEntry { name, line } => format!(
                "X-World Pro entry {name} (scenery_packs.ini line {line}) points to a \
                 folder that does not exist"
            ),
        }
    }
}

/// Everything worth fixing, in a stable order: broken entries, the library,
/// mesh variants, install links.
pub fn setup_findings(d: &Detection) -> Vec<Finding> {
    let mut out: Vec<Finding> = d
        .broken()
        .into_iter()
        .map(|e| Finding::BrokenEntry {
            name: e.name().to_string(),
            line: e.line,
        })
        .collect();

    if let Some(lib) = d.library.as_ref().filter(|l| l.resolved) {
        let active = std::fs::read(lib.dir.join("library.txt"));
        let ortho = std::fs::read(lib.dir.join("library - ortho.txt"));
        if let (Ok(active), Ok(ortho)) = (active, ortho) {
            if active != ortho {
                out.push(Finding::LibraryNotOrtho {
                    library: lib.dir.clone(),
                });
            }
        }
    }

    let active = d.active_continents();
    // (continent, base) -> (o4xp enabled, xp12 enabled), None when absent.
    let mut pairs: BTreeMap<(Continent, String), (Option<bool>, Option<bool>)> = BTreeMap::new();
    for l in d.layers.iter().filter(|l| active.contains(&l.continent)) {
        if let Some(base) = l.layer.strip_suffix(O4XP_SUFFIX) {
            pairs.entry((l.continent, base.to_string())).or_default().0 = Some(l.entry.enabled);
        } else if let Some(base) = l.layer.strip_suffix(XP12_SUFFIX) {
            pairs.entry((l.continent, base.to_string())).or_default().1 = Some(l.entry.enabled);
        }
    }
    for ((continent, base), (o4xp, xp12)) in pairs {
        if let (Some(o4xp), Some(xp12)) = (o4xp, xp12) {
            if !o4xp || xp12 {
                out.push(Finding::MeshVariant { continent, base });
            }
        }
    }

    if let Some(lib) = d.library.as_ref().filter(|l| l.resolved) {
        let scenery = d.xplane_root().join("Resources").join("default scenery");
        for season in SEASONS {
            let Ok(files) = std::fs::read_dir(lib.dir.join("mod").join(season)) else {
                continue;
            };
            let mut names: Vec<_> = files
                .flatten()
                .map(|f| f.file_name())
                .filter(|n| n.to_string_lossy().ends_with(".for"))
                .collect();
            names.sort();
            for name in names {
                let expected = scenery.join("1200 forests").join(season).join(&name);
                if !expected.exists() {
                    out.push(Finding::MissingInstallLink { expected });
                }
            }
        }
        if lib.dir.join("mod").join("Boats_1.dds").exists() {
            let expected = scenery
                .join("sim objects")
                .join("ships")
                .join("Boats_1.dds");
            if !expected.exists() {
                out.push(Finding::MissingInstallLink { expected });
            }
        }
    }
    out
}

/// The X-World Pro manual in the library folder (the last by name, so a
/// versioned copy wins over an unversioned one), if any.
pub fn manual_path(d: &Detection) -> Option<PathBuf> {
    let lib = d.library.as_ref().filter(|l| l.resolved)?;
    let mut manuals: Vec<PathBuf> = std::fs::read_dir(&lib.dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().map(|n| n.to_string_lossy()).is_some_and(|n| {
                n.starts_with("simHeaven_X-WORLD-Pro_Manual") && n.ends_with(".pdf")
            })
        })
        .collect();
    manuals.sort();
    manuals.pop()
}

#[cfg(all(test, unix))]
mod tests {
    use crate::xworld_pro::{detect, SCENERY_PACKS_FILE};

    fn root(copied: &[&str], ini: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("Custom Scenery");
        for name in copied {
            std::fs::create_dir_all(cs.join(name)).unwrap();
        }
        std::fs::create_dir_all(&cs).unwrap();
        std::fs::write(cs.join(SCENERY_PACKS_FILE), ini).unwrap();
        (dir, cs)
    }

    fn line(enabled: bool, name: &str) -> String {
        let kind = if enabled {
            "SCENERY_PACK"
        } else {
            "SCENERY_PACK_DISABLED"
        };
        format!("{kind} Custom Scenery/{name}/\n")
    }

    const EU09: &str = "simHeaven_X-WORLD-Pro_Europe-09-scenery";
    const LIB: &str = "simHeaven_X-WORLD-Pro_Library";
    const O4XP: &str = "simHeaven_X-WORLD-Pro_Europe-01-bridges_O4XP-Mesh";
    const XP12: &str = "simHeaven_X-WORLD-Pro_Europe-01-bridges_XP12-Mesh";

    #[test]
    fn xel_below_every_layer_is_in_order_and_the_library_is_ignored() {
        let ini = line(true, EU09) + &line(true, "zzXEL_ortho") + &line(true, LIB);
        let (_d, cs) = root(&[EU09, LIB], &ini);
        assert!(super::order_violation(&detect(&cs)).is_none());
    }

    #[test]
    fn xel_above_a_layer_names_the_pair() {
        let ini = line(true, "zzXEL_ortho") + &line(true, EU09);
        let (_d, cs) = root(&[EU09], &ini);
        let v = super::order_violation(&detect(&cs)).unwrap();
        assert_eq!((v.xel.line, v.xwp.line), (1, 2));
        assert_eq!(
            v.describe(),
            "zzXEL_ortho (scenery_packs.ini line 1) loads above \
             simHeaven_X-WORLD-Pro_Europe-09-scenery (line 2)"
        );
    }

    #[test]
    fn disabled_entries_do_not_count_for_order() {
        let ini = line(false, "zzXEL_ortho") + &line(true, EU09) + &line(true, "zzXEL_ortho");
        let (_d, cs) = root(&[EU09], &ini);
        assert!(super::order_violation(&detect(&cs)).is_none());
        let ini = line(true, "zzXEL_ortho") + &line(false, EU09);
        let (_d, cs) = root(&[EU09], &ini);
        assert!(super::order_violation(&detect(&cs)).is_none());
    }

    #[test]
    fn an_unresolved_layer_below_xel_is_not_a_violation() {
        let ini = line(true, "zzXEL_ortho") + &line(true, EU09);
        let (_d, cs) = root(&[], &ini);
        assert!(super::order_violation(&detect(&cs)).is_none());
    }

    #[test]
    fn a_missing_overlay_folder_is_not_a_violation() {
        let ini = line(true, "yzXEL_overlay") + &line(true, EU09);
        let (_d, cs) = root(&[EU09], &ini);
        assert!(super::order_violation(&detect(&cs)).is_none());
        std::fs::create_dir_all(cs.join("yzXEL_overlay")).unwrap();
        let v = super::order_violation(&detect(&cs)).unwrap();
        assert_eq!(v.xel.name(), "yzXEL_overlay");
    }

    #[test]
    fn several_xel_entries_name_the_topmost_and_the_lowest_layer() {
        let am = "simHeaven_X-WORLD-Pro_America-09-scenery";
        let ini = line(true, "zzXEL_ortho")
            + &line(true, EU09)
            + &line(true, "yzXEL_overlay")
            + &line(true, am);
        let (_d, cs) = root(&[EU09, am, "yzXEL_overlay"], &ini);
        let v = super::order_violation(&detect(&cs)).unwrap();
        assert_eq!((v.xel.name(), v.xel.line), ("zzXEL_ortho", 1));
        assert_eq!((v.xwp.name(), v.xwp.line), (am, 4));
    }

    #[test]
    fn the_non_ortho_library_is_flagged() {
        let (_d, cs) = root(&[EU09, LIB], &(line(true, EU09) + &line(true, LIB)));
        std::fs::write(cs.join(LIB).join("library.txt"), "standard").unwrap();
        std::fs::write(cs.join(LIB).join("library - ortho.txt"), "ortho").unwrap();
        assert!(super::setup_findings(&detect(&cs))
            .iter()
            .any(|f| matches!(f, super::Finding::LibraryNotOrtho { .. })));
        std::fs::write(cs.join(LIB).join("library.txt"), "ortho").unwrap();
        assert!(!super::setup_findings(&detect(&cs))
            .iter()
            .any(|f| matches!(f, super::Finding::LibraryNotOrtho { .. })));
        std::fs::remove_file(cs.join(LIB).join("library - ortho.txt")).unwrap();
        assert!(!super::setup_findings(&detect(&cs))
            .iter()
            .any(|f| matches!(f, super::Finding::LibraryNotOrtho { .. })));
    }

    fn mesh_findings(ini: &str, folders: &[&str]) -> Vec<super::Finding> {
        let (_d, cs) = root(folders, ini);
        super::setup_findings(&detect(&cs))
            .into_iter()
            .filter(|f| matches!(f, super::Finding::MeshVariant { .. }))
            .collect()
    }

    #[test]
    fn mesh_variants_want_o4xp_on_and_xp12_off() {
        let ok = line(true, O4XP) + &line(false, XP12);
        assert!(mesh_findings(&ok, &[O4XP, XP12]).is_empty());
        let wrong = line(false, O4XP) + &line(true, XP12);
        let f = mesh_findings(&wrong, &[O4XP, XP12]);
        assert_eq!(f.len(), 1);
        assert!(matches!(
            &f[0],
            super::Finding::MeshVariant { continent: crate::xworld_pro::Continent::Europe, base } if base == "01-bridges"
        ));
        let both = line(true, O4XP) + &line(true, XP12);
        assert_eq!(mesh_findings(&both, &[O4XP, XP12]).len(), 1);
    }

    #[test]
    fn mesh_variants_are_found_on_any_continent_and_layer() {
        let a = "simHeaven_X-WORLD-Pro_America-13-net3-roads_O4XP-Mesh";
        let b = "simHeaven_X-WORLD-Pro_America-13-net3-roads_XP12-Mesh";
        let f = mesh_findings(&(line(false, a) + &line(true, b)), &[a, b]);
        assert!(matches!(
            &f[0],
            super::Finding::MeshVariant { continent: crate::xworld_pro::Continent::America, base } if base == "13-net3-roads"
        ));
    }

    #[test]
    fn missing_install_links_are_found() {
        let (dir, cs) = root(&[EU09, LIB], &(line(true, EU09) + &line(true, LIB)));
        let lib = cs.join(LIB);
        std::fs::create_dir_all(lib.join("mod/sum")).unwrap();
        std::fs::write(lib.join("mod/sum/trees.for"), "x").unwrap();
        std::fs::write(lib.join("mod/Boats_1.dds"), "x").unwrap();
        let ds = dir.path().join("Resources/default scenery");
        std::fs::create_dir_all(ds.join("1200 forests/sum")).unwrap();
        std::fs::write(ds.join("1200 forests/sum/trees.for"), "x").unwrap();
        let missing: Vec<_> = super::setup_findings(&detect(&cs))
            .into_iter()
            .filter_map(|f| match f {
                super::Finding::MissingInstallLink { expected } => Some(expected),
                _ => None,
            })
            .collect();
        assert_eq!(missing, vec![ds.join("sim objects/ships/Boats_1.dds")]);
    }

    #[test]
    fn broken_entries_are_findings() {
        let (_d, cs) = root(&[], &line(true, EU09));
        assert!(super::setup_findings(&detect(&cs))
            .iter()
            .any(|f| matches!(f, super::Finding::BrokenEntry { line: 1, .. })));
    }

    #[test]
    fn the_newest_manual_in_the_library_is_found() {
        let (_d, cs) = root(&[LIB], &line(true, LIB));
        assert!(super::manual_path(&detect(&cs)).is_none());
        std::fs::write(cs.join(LIB).join("simHeaven_X-WORLD-Pro_Manual.pdf"), "x").unwrap();
        std::fs::write(
            cs.join(LIB).join("simHeaven_X-WORLD-Pro_Manual_v1.1.pdf"),
            "x",
        )
        .unwrap();
        assert_eq!(
            super::manual_path(&detect(&cs)),
            Some(cs.join(LIB).join("simHeaven_X-WORLD-Pro_Manual_v1.1.pdf"))
        );
    }

    #[test]
    fn a_layer_with_one_mesh_variant_is_not_flagged() {
        let lone_xp12 = "simHeaven_X-WORLD-Pro_Europe-02-roads_XP12-Mesh";
        let eu09 = EU09;
        let ini = line(true, eu09) + &line(true, lone_xp12);
        let f = mesh_findings(&ini, &[eu09, lone_xp12]);
        assert!(f.is_empty());

        let lone_o4xp = "simHeaven_X-WORLD-Pro_Europe-03-cities_O4XP-Mesh";
        let ini = line(true, eu09) + &line(false, lone_o4xp);
        let f = mesh_findings(&ini, &[eu09, lone_o4xp]);
        assert!(f.is_empty());
    }
}
