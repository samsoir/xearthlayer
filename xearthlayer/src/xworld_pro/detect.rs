//! Which X-World Pro continents and layers are installed and enabled.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::packs::{parse_scenery_packs, PackEntry, SCENERY_PACKS_FILE};

/// Every X-World Pro folder name starts with this.
pub const LAYER_PREFIX: &str = "simHeaven_X-WORLD-Pro_";
/// The shared library every continent needs.
pub const LIBRARY_NAME: &str = "simHeaven_X-WORLD-Pro_Library";

/// An X-World Pro continent, as sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Continent {
    Africa,
    America,
    Asia,
    AustraliaOceania,
    Europe,
}

impl Continent {
    pub const ALL: [Continent; 5] = [
        Continent::Africa,
        Continent::America,
        Continent::Asia,
        Continent::AustraliaOceania,
        Continent::Europe,
    ];

    /// The name X-World Pro uses for it.
    pub fn label(self) -> &'static str {
        match self {
            Continent::Africa => "Africa",
            Continent::America => "America",
            Continent::Asia => "Asia",
            Continent::AustraliaOceania => "Australia-Oceania",
            Continent::Europe => "Europe",
        }
    }
}

/// Folder tokens, longest first so `Australia-Oceania` wins over a prefix.
/// Antarctica ships as its own folders but belongs to Australia-Oceania.
const FOLDER_TOKENS: [(&str, Continent); 6] = [
    ("Australia-Oceania", Continent::AustraliaOceania),
    ("Antarctica", Continent::AustraliaOceania),
    ("America", Continent::America),
    ("Africa", Continent::Africa),
    ("Europe", Continent::Europe),
    ("Asia", Continent::Asia),
];

/// One continent layer, such as `Europe-01-bridges_O4XP-Mesh`.
#[derive(Debug, Clone)]
pub struct XwpLayer {
    pub entry: PackEntry,
    pub continent: Continent,
    /// The layer part of the name, `<NN>-<layer>[_<variant>]`.
    pub layer: String,
    /// The folder the entry points to.
    pub dir: PathBuf,
    /// Whether that folder exists (symlinks followed).
    pub resolved: bool,
}

/// The shared library entry.
#[derive(Debug, Clone)]
pub struct XwpLibrary {
    pub entry: PackEntry,
    pub dir: PathBuf,
    pub resolved: bool,
}

/// What `scenery_packs.ini` and Custom Scenery say about X-World Pro.
#[derive(Debug, Clone, Default)]
pub struct Detection {
    pub custom_scenery: PathBuf,
    pub layers: Vec<XwpLayer>,
    pub library: Option<XwpLibrary>,
    /// XEarthLayer's own entries (`zzXEL_*`, `yzXEL_*`), for the order check.
    pub xel_entries: Vec<PackEntry>,
    /// Set when `scenery_packs.ini` exists but could not be read.
    pub read_error: Option<String>,
}

impl Detection {
    /// Any X-World Pro entry at all, enabled or not.
    pub fn is_present(&self) -> bool {
        !self.layers.is_empty() || self.library.is_some()
    }

    /// Continents with at least one enabled layer whose folder exists.
    pub fn active_continents(&self) -> BTreeSet<Continent> {
        self.layers
            .iter()
            .filter(|l| l.entry.enabled && l.resolved)
            .map(|l| l.continent)
            .collect()
    }

    /// All five continents active.
    pub fn whole_planet(&self) -> bool {
        self.active_continents().len() == Continent::ALL.len()
    }

    /// Enabled X-World Pro entries whose folder is missing, in file order.
    pub fn broken(&self) -> Vec<&PackEntry> {
        let mut out: Vec<&PackEntry> = self
            .layers
            .iter()
            .filter(|l| l.entry.enabled && !l.resolved)
            .map(|l| &l.entry)
            .chain(
                self.library
                    .iter()
                    .filter(|l| l.entry.enabled && !l.resolved)
                    .map(|l| &l.entry),
            )
            .collect();
        out.sort_by_key(|e| e.line);
        out
    }

    /// The X-Plane installation: the parent of Custom Scenery.
    pub fn xplane_root(&self) -> PathBuf {
        self.custom_scenery
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.custom_scenery.clone())
    }
}

/// Read `scenery_packs.ini` in `custom_scenery` and classify its X-World Pro
/// and XEarthLayer entries. A missing file is nothing detected; an unreadable
/// one is recorded in `read_error`. Never fails.
pub fn detect(custom_scenery: &Path) -> Detection {
    let ini = custom_scenery.join(SCENERY_PACKS_FILE);
    let mut d = Detection {
        custom_scenery: custom_scenery.to_path_buf(),
        ..Default::default()
    };
    let text = match std::fs::read(&ini) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return d,
        Err(e) => {
            d.read_error = Some(format!("{}: {e}", ini.display()));
            return d;
        }
    };
    let root = d.xplane_root();
    for entry in parse_scenery_packs(&text) {
        let name = entry.name().to_string();
        if name.starts_with("zzXEL_") || name.starts_with("yzXEL_") {
            d.xel_entries.push(entry);
            continue;
        }
        if name == LIBRARY_NAME {
            let dir = resolve(&root, &entry.path);
            let resolved = is_dir(&dir);
            d.library = Some(XwpLibrary {
                entry,
                dir,
                resolved,
            });
            continue;
        }
        if let Some((continent, layer)) = split_layer(&name) {
            let dir = resolve(&root, &entry.path);
            let resolved = is_dir(&dir);
            d.layers.push(XwpLayer {
                entry,
                continent,
                layer,
                dir,
                resolved,
            });
        }
    }
    d
}

/// `simHeaven_X-WORLD-Pro_<Continent>-<NN>-<layer>` into its continent and
/// layer; `None` for anything else.
fn split_layer(name: &str) -> Option<(Continent, String)> {
    let rest = name.strip_prefix(LAYER_PREFIX)?;
    FOLDER_TOKENS.iter().find_map(|(token, continent)| {
        let layer = rest.strip_prefix(token)?.strip_prefix('-')?;
        let b = layer.as_bytes();
        (b.len() > 3 && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2] == b'-')
            .then(|| (*continent, layer.to_string()))
    })
}

/// An entry's folder: absolute paths as written, others under the X-Plane root.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let b = trimmed.as_bytes();
    let windows_abs = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if trimmed.starts_with('/') || windows_abs {
        PathBuf::from(trimmed)
    } else {
        root.join(trimmed)
    }
}

/// Whether `p` is a directory, following symlinks, so copied and symlinked
/// folders look the same.
fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// An X-Plane root with Custom Scenery, the given folders copied into it,
    /// and the given (folder, store) pairs symlinked from outside.
    fn root(copied: &[&str], linked: &[&str], ini: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("Custom Scenery");
        std::fs::create_dir_all(&cs).unwrap();
        for name in copied {
            std::fs::create_dir_all(cs.join(name)).unwrap();
        }
        let store = dir.path().join("store");
        for name in linked {
            std::fs::create_dir_all(store.join(name)).unwrap();
            symlink(store.join(name), cs.join(name)).unwrap();
        }
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
    const AM09: &str = "simHeaven_X-WORLD-Pro_America-09-scenery";
    const AO10: &str = "simHeaven_X-WORLD-Pro_Australia-Oceania-10-forests";
    const AN09: &str = "simHeaven_X-WORLD-Pro_Antarctica-09-scenery";

    #[test]
    fn copied_and_symlinked_folders_detect_the_same() {
        let ini = line(true, EU09) + &line(true, AM09);
        let (_d, cs) = root(&[EU09], &[AM09], &ini);
        let d = detect(&cs);
        assert!(d.is_present());
        assert!(d.layers.iter().all(|l| l.resolved));
        assert_eq!(
            d.active_continents().into_iter().collect::<Vec<_>>(),
            vec![Continent::America, Continent::Europe]
        );
    }

    #[test]
    fn layer_names_split_into_continent_and_layer() {
        let ini =
            line(true, AO10) + &line(true, "simHeaven_X-WORLD-Pro_Europe-01-bridges_O4XP-Mesh");
        let (_d, cs) = root(
            &[AO10, "simHeaven_X-WORLD-Pro_Europe-01-bridges_O4XP-Mesh"],
            &[],
            &ini,
        );
        let d = detect(&cs);
        assert_eq!(d.layers[0].continent, Continent::AustraliaOceania);
        assert_eq!(d.layers[0].layer, "10-forests");
        assert_eq!(d.layers[1].layer, "01-bridges_O4XP-Mesh");
    }

    #[test]
    fn xel_entries_are_recorded_including_the_overlay() {
        let ini = line(true, EU09) + &line(true, "zzXEL_ortho") + &line(true, "yzXEL_overlay");
        let (_d, cs) = root(&[EU09], &[], &ini);
        let names: Vec<_> = detect(&cs)
            .xel_entries
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(names, vec!["zzXEL_ortho", "yzXEL_overlay"]);
    }

    #[test]
    fn a_disabled_entry_with_a_missing_folder_is_not_broken() {
        let (_d, cs) = root(&[], &[], &line(false, EU09));
        let d = detect(&cs);
        assert!(d.is_present());
        assert!(d.broken().is_empty());
    }

    #[test]
    fn antarctica_counts_as_australia_oceania() {
        let (_d, cs) = root(&[AN09], &[], &line(true, AN09));
        assert!(detect(&cs)
            .active_continents()
            .contains(&Continent::AustraliaOceania));
    }

    #[test]
    fn a_disabled_continent_is_not_active() {
        let (_d, cs) = root(&[EU09], &[], &line(false, EU09));
        let d = detect(&cs);
        assert!(d.is_present());
        assert!(d.active_continents().is_empty());
    }

    #[test]
    fn missing_folders_and_dangling_links_are_broken_not_active() {
        let (dir, cs) = root(&[], &[AM09], &(line(true, EU09) + &line(true, AM09)));
        std::fs::remove_dir(dir.path().join("store").join(AM09)).unwrap(); // dangling now
        let d = detect(&cs);
        assert!(d.active_continents().is_empty());
        let broken: Vec<&str> = d.broken().iter().map(|e| e.name()).collect();
        assert_eq!(broken, vec![EU09, AM09]);
    }

    #[test]
    fn the_whole_planet_is_all_five_continents() {
        let names = [
            "simHeaven_X-WORLD-Pro_Africa-09-scenery",
            AM09,
            "simHeaven_X-WORLD-Pro_Asia-09-scenery",
            AO10,
            EU09,
        ];
        let ini: String = names.iter().map(|n| line(true, n)).collect();
        let (_d, cs) = root(&names, &[], &ini);
        assert!(detect(&cs).whole_planet());
        let (_d, cs) = root(
            &names[..4],
            &[],
            &names[..4].iter().map(|n| line(true, n)).collect::<String>(),
        );
        assert!(!detect(&cs).whole_planet());
    }

    #[test]
    fn the_library_and_xel_entries_are_recorded() {
        let lib = "simHeaven_X-WORLD-Pro_Library";
        let ini = line(true, EU09) + &line(true, "zzXEL_ortho") + &line(true, lib);
        let (_d, cs) = root(&[EU09, lib], &[], &ini);
        let d = detect(&cs);
        let l = d.library.as_ref().unwrap();
        assert!(l.resolved);
        assert_eq!(l.dir, cs.join(lib));
        assert_eq!(d.xel_entries.len(), 1);
        assert_eq!(d.xel_entries[0].line, 2);
    }

    #[test]
    fn absolute_entries_resolve_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("Custom Scenery");
        let elsewhere = dir.path().join("elsewhere").join(EU09);
        std::fs::create_dir_all(&cs).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            cs.join(SCENERY_PACKS_FILE),
            format!("SCENERY_PACK {}/\n", elsewhere.display()),
        )
        .unwrap();
        assert!(detect(&cs).layers[0].resolved);
    }

    #[test]
    fn no_ini_is_nothing_detected_and_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let d = detect(dir.path());
        assert!(!d.is_present());
        assert!(d.read_error.is_none());
    }

    #[test]
    fn unrelated_simheaven_names_are_not_layers() {
        let odd = "simHeaven_X-WORLD-Pro_Europe-notes";
        let (_d, cs) = root(&[odd], &[], &line(true, odd));
        assert!(!detect(&cs).is_present());
    }
}
