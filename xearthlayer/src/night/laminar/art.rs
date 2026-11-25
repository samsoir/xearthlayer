//! Palette names to the user's Laminar lit terrain.
//!
//! Every answer is remembered for the session, and each file is loaded once
//! however many callers ask for it at the same moment. Two kinds of failure
//! are told apart. A file that reads but has nothing usable (a `.ter`
//! without lit lines, a lit texture that is not square DXT1) makes its class
//! dark: a property of the files the lit fingerprint covers, so payloads
//! drawn with it may be kept on disk. A file that cannot be read is retried
//! with the recipe policy ([`with_retries`]); one that fails every attempt is
//! given up on for the session, and a payload whose lookups met it is told
//! so through an [`ArtRecord`]: a later session may read the file and light
//! the class, under the same fingerprint.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::library::LibraryIndex;
use super::pattern::{decode_dxt1, LitPattern};
use super::ter::{parse_lit_terrain, LitTerrain};
use crate::night::retry::{with_retries, READ_RETRIES, READ_RETRY_DELAY};
use crate::night::FileIdentity;

/// Reads of the art files: the disk in production, a seam for tests to
/// inject I/O errors. Blocking.
pub trait ArtFiles: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
}

/// The art files on disk.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiskArtFiles;

impl ArtFiles for DiskArtFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// What the art lookups made for one payload met: the first art file among
/// them given up on after I/O errors, if any. Such a payload may differ
/// from a later session's under the same fingerprint, so it is not kept on
/// disk. Shared by the generator's threads.
#[derive(Debug, Default)]
pub struct ArtRecord {
    given_up: OnceLock<Arc<Path>>,
}

impl ArtRecord {
    /// The first file given up on that a lookup met, if any.
    pub fn given_up(&self) -> Option<&Path> {
        self.given_up.get().map(|path| &**path)
    }

    /// [`Self::given_up`], owned.
    pub fn into_given_up(self) -> Option<Arc<Path>> {
        self.given_up.into_inner()
    }

    fn note<T>(&self, lookup: &Lookup<T>) {
        if let Lookup::GivenUp(path) = lookup {
            // The first file met is the one named.
            let _ = self.given_up.set(Arc::clone(path));
        }
    }
}

/// One remembered art answer.
#[derive(Debug)]
enum Lookup<T> {
    Found(Arc<T>),
    /// Dark by the content of the art files (deterministic under the lit
    /// fingerprint): nothing exported under the name, no lit lines, a lit
    /// texture that is missing or does not decode.
    Dark,
    /// Dark because this file was given up on after I/O errors.
    GivenUp(Arc<Path>),
}

// Derived `Clone` would ask for `T: Clone`.
impl<T> Clone for Lookup<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Found(t) => Self::Found(Arc::clone(t)),
            Self::Dark => Self::Dark,
            Self::GivenUp(path) => Self::GivenUp(Arc::clone(path)),
        }
    }
}

impl<T> Lookup<T> {
    fn found(self) -> Option<Arc<T>> {
        match self {
            Self::Found(t) => Some(t),
            Self::Dark | Self::GivenUp(_) => None,
        }
    }
}

/// Answers by key, each computed once: concurrent first callers of one key
/// wait for a single `init`, while other keys go on.
type OnceMap<T> = DashMap<String, Arc<OnceLock<Lookup<T>>>>;

fn once<T>(map: &OnceMap<T>, key: &str, init: impl FnOnce() -> Lookup<T>) -> Lookup<T> {
    // The cell is cloned out so no map shard stays locked during `init`.
    let cell = match map.get(key) {
        Some(cell) => Arc::clone(&cell),
        None => Arc::clone(&map.entry(key.to_string()).or_default()),
    };
    cell.get_or_init(init).clone()
}

/// The lit art of one X-Plane installation (`1000 world terrain`).
pub struct LaminarArt {
    terrain_root: PathBuf,
    files: Arc<dyn ArtFiles>,
    /// The library index, or the `library.txt` given up on.
    library: OnceLock<Result<LibraryIndex, Arc<Path>>>,
    resolved: OnceMap<LitTerrain>,
    /// Decoded patterns by `texture|ground scale`; Laminar has nine textures.
    patterns: OnceMap<LitPattern>,
    fingerprint: OnceLock<[u8; 32]>,
}

impl LaminarArt {
    /// `terrain_root` is `<install>/Resources/default scenery/1000 world terrain`.
    pub fn new(terrain_root: PathBuf) -> Self {
        Self::with_files(terrain_root, Arc::new(DiskArtFiles))
    }

    /// As [`Self::new`], reading the art files through `files`.
    pub fn with_files(terrain_root: PathBuf, files: Arc<dyn ArtFiles>) -> Self {
        Self {
            terrain_root,
            files,
            library: OnceLock::new(),
            resolved: DashMap::new(),
            patterns: DashMap::new(),
            fingerprint: OnceLock::new(),
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

    /// The decoded lit pattern a palette name stands for, or `None` (dark).
    ///
    /// Blocking file I/O and decoding on first use of a texture.
    pub fn pattern(&self, palette_name: &str) -> Option<Arc<LitPattern>> {
        self.pattern_lookup(palette_name).found()
    }

    /// [`Self::pattern`], noting in `record` a file given up on that the
    /// lookup met: what the lit generator's lookups for one payload use.
    pub fn pattern_recorded(
        &self,
        palette_name: &str,
        record: &ArtRecord,
    ) -> Option<Arc<LitPattern>> {
        let lookup = self.pattern_lookup(palette_name);
        record.note(&lookup);
        lookup.found()
    }

    fn pattern_lookup(&self, palette_name: &str) -> Lookup<LitPattern> {
        let lit = match self.terrain_lookup(palette_name) {
            Lookup::Found(lit) => lit,
            Lookup::Dark => return Lookup::Dark,
            Lookup::GivenUp(path) => return Lookup::GivenUp(path),
        };
        let key = format!("{}|{}", lit.texture.display(), lit.ground_scale_m);
        once(&self.patterns, &key, || self.load_pattern(&lit))
    }

    fn load_pattern(&self, lit: &LitTerrain) -> Lookup<LitPattern> {
        let read = with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
            self.files.read(&lit.texture)
        });
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(texture = %lit.texture.display(), error = %e,
                      "Night lighting: lit texture unreadable after retries; this class stays dark this session");
                return Lookup::GivenUp(Arc::from(lit.texture.as_path()));
            }
        };
        match decode_dxt1(&bytes, lit.ground_scale_m) {
            Some(pattern) => Lookup::Found(Arc::new(pattern)),
            None => {
                warn!(texture = %lit.texture.display(),
                      "Night lighting: lit texture is not a readable DXT1 DDS; this class stays dark");
                Lookup::Dark
            }
        }
    }

    /// The lit terrain a recipe palette name stands for, or `None` (dark).
    ///
    /// Blocking file I/O on first use of a name: call from a blocking context.
    pub fn lit_terrain(&self, palette_name: &str) -> Option<Arc<LitTerrain>> {
        self.terrain_lookup(palette_name).found()
    }

    fn terrain_lookup(&self, palette_name: &str) -> Lookup<LitTerrain> {
        once(&self.resolved, palette_name, || self.resolve(palette_name))
    }

    /// The library index, or the `library.txt` given up on.
    fn library(&self) -> Result<&LibraryIndex, &Arc<Path>> {
        self.library
            .get_or_init(|| {
                let path = self.terrain_root.join("library.txt");
                let read = with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
                    self.files.read_to_string(&path)
                });
                match read {
                    Ok(text) => Ok(LibraryIndex::parse(&text)),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e,
                              "Night lighting: Laminar library unreadable after retries; every lit texture stays dark this session");
                        Err(Arc::from(path.as_path()))
                    }
                }
            })
            .as_ref()
    }

    fn resolve(&self, palette_name: &str) -> Lookup<LitTerrain> {
        let library = match self.library() {
            Ok(library) => library,
            Err(path) => return Lookup::GivenUp(Arc::clone(path)),
        };
        let Some(file) = library.resolve(palette_name) else {
            warn!(
                name = palette_name,
                "Night lighting: palette name not in the Laminar library; this class stays dark"
            );
            return Lookup::Dark;
        };
        let ter_path = self.terrain_root.join(file);
        let read = with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
            self.files.read_to_string(&ter_path)
        });
        let text = match read {
            Ok(text) => text,
            Err(e) => {
                warn!(name = palette_name, path = %ter_path.display(), error = %e,
                      "Night lighting: lit terrain unreadable after retries; this class stays dark this session");
                return Lookup::GivenUp(Arc::from(ter_path.as_path()));
            }
        };
        let Some(ter_dir) = ter_path.parent() else {
            return Lookup::Dark;
        };
        match parse_lit_terrain(&text, ter_dir) {
            Some(lit) if lit.texture.is_file() => Lookup::Found(Arc::new(lit)),
            Some(lit) => {
                warn!(name = palette_name, texture = %lit.texture.display(),
                      "Night lighting: lit texture missing; this class stays dark");
                Lookup::Dark
            }
            None => {
                debug!(
                    name = palette_name,
                    "Night lighting: terrain has no lit texture"
                );
                Lookup::Dark
            }
        }
    }
}

/// Art file reads for tests: the disk, except that the first `failures`
/// reads of one file fail with an I/O error; every read of that file is
/// counted, and each successful one takes `delay` longer.
#[cfg(test)]
pub(crate) struct FlakyArtFiles {
    path: PathBuf,
    failures: std::sync::atomic::AtomicUsize,
    reads: std::sync::atomic::AtomicUsize,
    delay: std::time::Duration,
}

#[cfg(test)]
impl FlakyArtFiles {
    /// The first `failures` reads of `path` fail (`usize::MAX`: every one).
    pub(crate) fn new(path: PathBuf, failures: usize) -> Self {
        Self {
            path,
            failures: failures.into(),
            reads: 0.into(),
            delay: std::time::Duration::ZERO,
        }
    }

    /// Each successful read of the file takes `delay` longer.
    pub(crate) fn slowed(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Reads of the file so far, failed ones included.
    pub(crate) fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn attempt(&self, path: &Path) -> io::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        if path != self.path {
            return Ok(());
        }
        self.reads.fetch_add(1, SeqCst);
        if self
            .failures
            .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(io::Error::other("injected read failure"));
        }
        std::thread::sleep(self.delay);
        Ok(())
    }
}

#[cfg(test)]
impl ArtFiles for FlakyArtFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.attempt(path)?;
        std::fs::read(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.attempt(path)?;
        std::fs::read_to_string(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::test_log::logged;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const URBAN: &str = "lib/g10/terrain10/urban_tmp_sdry3.ter";
    const PARK: &str = "lib/g10/terrain10/urban_tmp_sdry_prk.ter";

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
            std::fs::write(lit_file(root), b"DDS ").unwrap();
        }
    }

    fn lit_file(root: &Path) -> PathBuf {
        root.join("textures12/urban_FAR/uw5_FAR_LIT.dds")
    }

    /// `install`, with a lit texture that decodes (4 x 4 solid red).
    fn install_decodable(root: &Path) {
        install(root, true);
        let mut dds = crate::dds::DdsHeader::new(4, 4, 1, crate::dds::DdsFormat::BC1).to_bytes();
        dds.extend_from_slice(&[0x00, 0xF8, 0, 0, 0, 0, 0, 0]);
        std::fs::write(lit_file(root), dds).unwrap();
    }

    /// The lit texture's path as the art resolves it (through `terrain12/..`).
    fn resolved_lit_file(art: &LaminarArt) -> PathBuf {
        art.lit_terrain(URBAN).unwrap().texture.clone()
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
        std::fs::write(lit_file(temp.path()), b"DDS longer").unwrap();
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
        let lit = art.lit_terrain(URBAN).unwrap();
        assert_eq!(lit.ground_scale_m, 3072.0);
        assert!(lit.texture.is_file());
    }

    #[test]
    fn a_park_and_an_unknown_name_are_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art.lit_terrain(PARK).is_none());
        assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
    }

    #[test]
    fn a_missing_lit_file_is_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), false);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art.lit_terrain(URBAN).is_none());
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
            assert!(art.lit_terrain(URBAN).is_none());
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
            assert!(art.lit_terrain(PARK).is_none());
        });
        assert!(!log.contains("WARN"), "{log}");
    }

    #[test]
    fn pattern_decodes_the_lit_texture() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let art = LaminarArt::new(temp.path().to_path_buf());
        let p = art.pattern(URBAN).unwrap();
        assert_eq!(
            (p.size, p.ground_scale_m, p.rgb[0]),
            (4, 3072.0, [255, 0, 0])
        );
        assert!(art.pattern(PARK).is_none());
    }

    #[test]
    fn an_undecodable_lit_texture_has_no_pattern() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true); // writes b"DDS " only
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art.pattern(URBAN).is_none());
    }

    #[test]
    fn dark_content_is_not_given_up_on() {
        // Unlit, unknown and undecodable are properties of the files the
        // fingerprint covers: the same every session.
        let temp = TempDir::new().unwrap();
        install(temp.path(), true); // the lit texture does not decode
        let art = LaminarArt::new(temp.path().to_path_buf());
        let record = ArtRecord::default();
        for name in [PARK, "lib/g10/terrain10/nope.ter", URBAN] {
            assert!(art.pattern_recorded(name, &record).is_none(), "{name}");
        }
        assert_eq!(record.given_up(), None);
    }

    #[test]
    fn an_unreadable_ter_is_given_up_on() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let ter = temp.path().join("terrain12/urban_tmp_sdry3.ter");
        std::fs::remove_file(&ter).unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        let record = ArtRecord::default();
        assert!(art.pattern_recorded(URBAN, &record).is_none());
        assert_eq!(record.given_up(), Some(ter.as_path()));
    }

    #[test]
    fn a_missing_library_is_given_up_on() {
        let root = PathBuf::from("/nonexistent/1000 world terrain");
        let art = LaminarArt::new(root.clone());
        let record = ArtRecord::default();
        assert!(art.pattern_recorded(URBAN, &record).is_none());
        assert_eq!(record.given_up(), Some(root.join("library.txt").as_path()));
    }

    #[test]
    fn a_lit_texture_whose_first_read_fails_is_read_again_and_lit() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = resolved_lit_file(&LaminarArt::new(temp.path().to_path_buf()));
        let files = Arc::new(FlakyArtFiles::new(path, 1));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        let record = ArtRecord::default();
        assert!(art.pattern_recorded(URBAN, &record).is_some(), "lit");
        assert_eq!(files.reads(), 2, "one retry");
        assert_eq!(record.given_up(), None);
    }

    #[test]
    fn a_lit_texture_unreadable_on_every_attempt_is_given_up_on_once() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = resolved_lit_file(&LaminarArt::new(temp.path().to_path_buf()));
        let files = Arc::new(FlakyArtFiles::new(path.clone(), usize::MAX));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        let log = logged(|| {
            let record = ArtRecord::default();
            assert!(art.pattern_recorded(URBAN, &record).is_none(), "dark");
            assert_eq!(record.given_up(), Some(path.as_path()));
            // Remembered for the session: no more reads, still recorded.
            let later = ArtRecord::default();
            assert!(art.pattern_recorded(URBAN, &later).is_none());
            assert_eq!(later.into_given_up().as_deref(), Some(path.as_path()));
        });
        assert_eq!(files.reads(), 1 + READ_RETRIES as usize);
        assert_eq!(log.matches("WARN").count(), 1, "{log}");
        assert!(log.contains("uw5_FAR_LIT.dds"), "{log}");
        // The probe's view of the class is unaffected: its terrain resolved.
        assert!(art.lit_terrain(URBAN).is_some());
    }

    #[test]
    fn a_given_up_file_is_recorded_only_by_the_lookups_that_meet_it() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = resolved_lit_file(&LaminarArt::new(temp.path().to_path_buf()));
        let art = LaminarArt::with_files(
            temp.path().to_path_buf(),
            Arc::new(FlakyArtFiles::new(path, usize::MAX)),
        );
        assert!(art.pattern(URBAN).is_none());
        let park = ArtRecord::default();
        assert!(art.pattern_recorded(PARK, &park).is_none());
        assert_eq!(park.given_up(), None);
    }

    #[test]
    fn concurrent_first_calls_decode_the_lit_texture_once() {
        const CALLERS: usize = 8;
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = resolved_lit_file(&LaminarArt::new(temp.path().to_path_buf()));
        // Slow enough that every caller arrives while the first one reads.
        let files = Arc::new(FlakyArtFiles::new(path, 0).slowed(Duration::from_millis(50)));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        let barrier = std::sync::Barrier::new(CALLERS);
        let patterns: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..CALLERS)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        art.pattern(URBAN).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(files.reads(), 1, "one read and decode");
        assert!(patterns.iter().all(|p| Arc::ptr_eq(p, &patterns[0])));
    }

    #[test]
    fn no_installation_makes_everything_dark() {
        let art = LaminarArt::new(PathBuf::from("/nonexistent/1000 world terrain"));
        assert!(art.lit_terrain(URBAN).is_none());
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
        let lit = art.lit_terrain(URBAN).unwrap();
        assert!(lit
            .texture
            .ends_with("textures12/urban_FAR/uw5_FAR_LIT.dds"));
        assert_eq!(lit.ground_scale_m, 4096.0);
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_cld_dry_prk.ter")
            .is_none());
    }

    /// `pattern` cold (one read and decode of a real lit texture) and warm
    /// (a remembered answer), printed. Run with
    /// `XEL_LAMINAR_ART=<install>/Resources/default scenery/1000 world terrain
    /// cargo test --release -p xearthlayer pattern_cold_and_warm -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing: release build and a real Laminar installation"]
    fn pattern_cold_and_warm() {
        let Some(root) = std::env::var_os("XEL_LAMINAR_ART") else {
            eprintln!("XEL_LAMINAR_ART is not set: skipped (point it at `1000 world terrain`)");
            return;
        };
        let art = LaminarArt::new(PathBuf::from(root));
        // The library and the .ter first, so cold times the texture alone.
        let lit = art.lit_terrain(URBAN).expect("urban terrain resolves");
        let started = Instant::now();
        let pattern = art.pattern(URBAN).expect("the lit texture decodes");
        let cold = started.elapsed();
        const WARM_CALLS: u32 = 10_000;
        let started = Instant::now();
        for _ in 0..WARM_CALLS {
            std::hint::black_box(art.pattern(std::hint::black_box(URBAN)));
        }
        let warm = started.elapsed() / WARM_CALLS;
        eprintln!(
            "pattern {} ({} px): cold {cold:?} (read and decode), warm {warm:?} per call",
            lit.texture.display(),
            pattern.size
        );
    }
}
