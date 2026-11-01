//! Palette names to the user's Laminar lit terrain.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::library::LibraryIndex;
use super::pattern::{load_pattern, LitPattern};
use super::ter::{parse_lit_terrain, LitTerrain};
use crate::night::FileIdentity;

/// The lit art of one X-Plane installation (`1000 world terrain`).
pub struct LaminarArt {
    terrain_root: PathBuf,
    library: OnceLock<Option<LibraryIndex>>,
    resolved: DashMap<String, Option<Arc<LitTerrain>>>,
    /// Decoded patterns by `texture|ground scale`; Laminar has nine textures.
    patterns: DashMap<String, Option<Arc<LitPattern>>>,
    fingerprint: OnceLock<[u8; 32]>,
    /// Set when a file could not be read or decoded, so the session's
    /// answers depend on more than the fingerprinted tree.
    failed: AtomicBool,
}

impl LaminarArt {
    /// `terrain_root` is `<install>/Resources/default scenery/1000 world terrain`.
    pub fn new(terrain_root: PathBuf) -> Self {
        Self {
            terrain_root,
            library: OnceLock::new(),
            resolved: DashMap::new(),
            patterns: DashMap::new(),
            fingerprint: OnceLock::new(),
            failed: AtomicBool::new(false),
        }
    }

    /// Digest of the art a lit texture can draw on: the library, every
    /// `.ter` and every `_LIT.dds` under the terrain root, by relative path,
    /// size and modification time. A missing root has a fingerprint too.
    ///
    /// Blocking directory walk (~30 ms on a real install), done once per
    /// session.
    pub fn fingerprint(&self) -> [u8; 32] {
        *self.fingerprint.get_or_init(|| {
            let mut files = Vec::new();
            let mut dirs = vec![self.terrain_root.clone()];
            while let Some(dir) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(meta) = entry.metadata() else { continue };
                    if meta.is_dir() {
                        dirs.push(path);
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == "library.txt" || name.ends_with(".ter") || name.ends_with("_LIT.dds")
                    {
                        let relative = path
                            .strip_prefix(&self.terrain_root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        let id = FileIdentity::from_metadata(&meta);
                        files.push((relative, id.len, id.mtime_nanos));
                    }
                }
            }
            files.sort();
            let mut hasher = Sha256::new();
            for (relative, len, mtime) in files {
                hasher.update(relative.as_bytes());
                hasher.update([0u8]);
                hasher.update(len.to_le_bytes());
                hasher.update(mtime.to_le_bytes());
            }
            hasher.finalize().into()
        })
    }

    /// Whether reading or decoding an art file has failed this session (the
    /// library, a `.ter` or a lit texture). A class that is simply not lit
    /// is not a failure. Answers given after a failure may differ from a
    /// later session's, so they must not be persisted.
    pub fn had_failure(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn record_failure(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }

    /// The decoded lit pattern a palette name stands for, or `None` (dark).
    ///
    /// Blocking file I/O and decoding on first use of a texture.
    pub fn pattern(&self, palette_name: &str) -> Option<Arc<LitPattern>> {
        let lit = self.lit_terrain(palette_name)?;
        let key = format!("{}|{}", lit.texture.display(), lit.ground_scale_m);
        if let Some(hit) = self.patterns.get(&key) {
            return hit.clone();
        }
        let pattern = load_pattern(&lit.texture, lit.ground_scale_m).map(Arc::new);
        if pattern.is_none() {
            self.record_failure();
            warn!(texture = %lit.texture.display(),
                  "Night lighting: lit texture is not a readable DXT1 DDS; this class stays dark");
        }
        self.patterns.insert(key, pattern.clone());
        pattern
    }

    /// The lit terrain a recipe palette name stands for, or `None` (dark).
    ///
    /// Blocking file I/O on first use of a name: call from a blocking context.
    pub fn lit_terrain(&self, palette_name: &str) -> Option<Arc<LitTerrain>> {
        if let Some(hit) = self.resolved.get(palette_name) {
            return hit.clone();
        }
        let result = self.resolve(palette_name).map(Arc::new);
        self.resolved
            .insert(palette_name.to_string(), result.clone());
        result
    }

    fn library(&self) -> Option<&LibraryIndex> {
        self.library
            .get_or_init(|| {
                let path = self.terrain_root.join("library.txt");
                match std::fs::read_to_string(&path) {
                    Ok(text) => Some(LibraryIndex::parse(&text)),
                    Err(e) => {
                        self.record_failure();
                        warn!(path = %path.display(), error = %e,
                              "Night lighting: Laminar library unavailable; every lit texture stays dark");
                        None
                    }
                }
            })
            .as_ref()
    }

    fn resolve(&self, palette_name: &str) -> Option<LitTerrain> {
        let Some(file) = self.library()?.resolve(palette_name) else {
            warn!(
                name = palette_name,
                "Night lighting: palette name not in the Laminar library; this class stays dark"
            );
            return None;
        };
        let ter_path = self.terrain_root.join(file);
        let text = match std::fs::read_to_string(&ter_path) {
            Ok(text) => text,
            Err(e) => {
                self.record_failure();
                warn!(name = palette_name, path = %ter_path.display(), error = %e,
                      "Night lighting: lit terrain unreadable; this class stays dark");
                return None;
            }
        };
        let lit = parse_lit_terrain(&text, ter_path.parent()?);
        match lit {
            Some(lit) if lit.texture.is_file() => Some(lit),
            Some(lit) => {
                warn!(name = palette_name, texture = %lit.texture.display(),
                      "Night lighting: lit texture missing; this class stays dark");
                None
            }
            None => {
                debug!(
                    name = palette_name,
                    "Night lighting: terrain has no lit texture"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn install(root: &Path, with_lit_file: bool) {
        std::fs::create_dir_all(root.join("terrain12")).unwrap();
        std::fs::create_dir_all(root.join("textures12/urban_FAR")).unwrap();
        std::fs::write(
            root.join("library.txt"),
            "A\n1200\nLIBRARY\nREGION_DEFINE all\nREGION_RECT -180 -90 179 89\nREGION all\n\
             EXPORT lib/g10/terrain10/urban_tmp_sdry3.ter terrain12/urban_tmp_sdry3.ter\n\
             EXPORT lib/g10/terrain10/urban_tmp_sdry_prk.ter terrain12/park_tmp_sdry1.ter\n",
        )
        .unwrap();
        std::fs::write(
            root.join("terrain12/urban_tmp_sdry3.ter"),
            "TEXTURE_LIT ../textures12/urban_FAR/uw5_FAR_LIT.dds\nLIT_FADE_PARAMS 3072 7000 15000\n",
        )
        .unwrap();
        std::fs::write(
            root.join("terrain12/park_tmp_sdry1.ter"),
            "BASE_TEX park.dds\n",
        )
        .unwrap();
        if with_lit_file {
            std::fs::write(root.join("textures12/urban_FAR/uw5_FAR_LIT.dds"), b"DDS ").unwrap();
        }
    }

    #[test]
    fn the_fingerprint_is_stable_for_one_tree() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(art.fingerprint(), art.fingerprint());
        assert_eq!(
            art.fingerprint(),
            LaminarArt::new(temp.path().to_path_buf()).fingerprint()
        );
    }

    #[test]
    fn the_fingerprint_moves_when_a_lit_file_changes() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let before = LaminarArt::new(temp.path().to_path_buf()).fingerprint();
        std::fs::write(
            temp.path().join("textures12/urban_FAR/uw5_FAR_LIT.dds"),
            b"DDS longer",
        )
        .unwrap();
        assert_ne!(
            LaminarArt::new(temp.path().to_path_buf()).fingerprint(),
            before
        );
    }

    #[test]
    fn a_missing_root_still_has_a_fingerprint() {
        let art = LaminarArt::new(PathBuf::from("/nonexistent/1000 world terrain"));
        assert_eq!(art.fingerprint(), art.fingerprint());
    }

    #[test]
    fn an_urban_name_resolves_to_its_lit_terrain() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        let lit = art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .unwrap();
        assert_eq!(lit.ground_scale_m, 3072.0);
        assert!(lit.texture.is_file());
    }

    #[test]
    fn a_park_and_an_unknown_name_are_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry_prk.ter")
            .is_none());
        assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
    }

    #[test]
    fn a_missing_lit_file_is_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), false);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
    }

    /// Runs `f` under a subscriber and returns everything it logged.
    fn logged<F: FnOnce()>(f: F) -> String {
        use std::io::Write;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn an_unresolved_palette_name_warns_once_naming_it() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        let log = logged(|| {
            assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
            assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
        });
        assert_eq!(log.matches("WARN").count(), 1, "{log}");
        assert!(log.contains("lib/g10/terrain10/nope.ter"), "{log}");
    }

    #[test]
    fn an_unreadable_ter_warns_with_name_and_path() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        std::fs::remove_file(temp.path().join("terrain12/urban_tmp_sdry3.ter")).unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        let log = logged(|| {
            assert!(art
                .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
                .is_none());
        });
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("urban_tmp_sdry3.ter"), "{log}");
    }

    #[test]
    fn a_park_logs_no_warning() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        let log = logged(|| {
            assert!(art
                .lit_terrain("lib/g10/terrain10/urban_tmp_sdry_prk.ter")
                .is_none());
        });
        assert!(!log.contains("WARN"), "{log}");
    }

    #[test]
    fn pattern_decodes_the_lit_texture() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let mut dds = crate::dds::DdsHeader::new(4, 4, 1, crate::dds::DdsFormat::BC1).to_bytes();
        dds.extend_from_slice(&[0x00, 0xF8, 0, 0, 0, 0, 0, 0]);
        std::fs::write(
            temp.path().join("textures12/urban_FAR/uw5_FAR_LIT.dds"),
            dds,
        )
        .unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        let p = art
            .pattern("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .unwrap();
        assert_eq!(
            (p.size, p.ground_scale_m, p.rgb[0]),
            (4, 3072.0, [255, 0, 0])
        );
        assert!(art
            .pattern("lib/g10/terrain10/urban_tmp_sdry_prk.ter")
            .is_none());
    }

    #[test]
    fn an_undecodable_lit_texture_has_no_pattern() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true); // writes b"DDS " only
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .pattern("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
    }

    #[test]
    fn only_a_read_failure_counts_as_a_failure() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true); // the lit texture does not decode
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry_prk.ter")
            .is_none());
        assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
        assert!(!art.had_failure(), "unlit classes are not failures");
        assert!(art
            .pattern("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
        assert!(art.had_failure(), "an undecodable lit texture is");
    }

    #[test]
    fn an_unreadable_ter_is_a_failure() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        std::fs::remove_file(temp.path().join("terrain12/urban_tmp_sdry3.ter")).unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
        assert!(art.had_failure());
    }

    #[test]
    fn a_missing_library_is_a_failure() {
        let art = LaminarArt::new(PathBuf::from("/nonexistent/1000 world terrain"));
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
        assert!(art.had_failure());
    }

    #[test]
    fn no_installation_makes_everything_dark() {
        let art = LaminarArt::new(PathBuf::from("/nonexistent/1000 world terrain"));
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
    }

    /// Against the developer's real installation, skipped when absent: every
    /// urban and industrial export resolves; parks and golf stay dark.
    #[test]
    fn the_reference_installation_resolves_its_urban_terrain() {
        let root = PathBuf::from(
            "/media/FlightSim/X-Plane 12-beta/Resources/default scenery/1000 world terrain",
        );
        if !root.join("library.txt").is_file() {
            return;
        }
        let art = LaminarArt::new(root);
        let lit = art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .unwrap();
        assert!(lit
            .texture
            .ends_with("textures12/urban_FAR/uw5_FAR_LIT.dds"));
        assert_eq!(lit.ground_scale_m, 4096.0);
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_cld_dry_prk.ter")
            .is_none());
    }
}
