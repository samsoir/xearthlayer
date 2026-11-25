//! Palette names to the user's Laminar lit terrain.
//!
//! Every answer is remembered for the session, and each file is loaded once
//! however many callers ask for it at the same moment.
//!
//! A lit payload may be written to the lit disk tier only when it is a
//! function of what the lit fingerprint covers ([`LaminarArt::fingerprint`]),
//! so every answer here says whether it is. A file that is missing, or reads
//! but has nothing usable (not UTF-8, a `.ter` without lit lines, a lit
//! texture that is not square DXT1), makes its class dark: when the walk
//! covers the file, that is the same every session, and the answer is
//! settled. Two things leave an answer in doubt ([`ArtDoubt`]): a file that
//! fails with any other I/O error on every attempt of the recipe policy
//! (`retry::with_retries`) is given up on for the session, and a file the walk
//! does not cover (outside the root, reached through a symlink, or named
//! unlike the files the walk hashes) may change without changing the
//! fingerprint. A payload whose lookups met a doubt is told so through an
//! [`ArtRecord`] and is kept in memory only.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::library::LibraryIndex;
use super::pattern::{decode_dxt1, LitPattern};
use super::ter::{parse_lit_terrain, LitTerrain};
use crate::night::retry::{with_retries, READ_RETRIES, READ_RETRY_DELAY};
use crate::night::FileIdentity;

/// Access to the art files: the disk in production, a seam for tests to
/// inject I/O errors. Blocking.
pub trait ArtFiles: Send + Sync {
    /// The file's bytes.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Whether `path` is a regular file (symlinks followed); `NotFound` when
    /// nothing is there.
    fn is_file(&self, path: &Path) -> io::Result<bool>;
}

/// The art files on disk.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiskArtFiles;

impl ArtFiles for DiskArtFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn is_file(&self, path: &Path) -> io::Result<bool> {
        std::fs::metadata(path).map(|meta| meta.is_file())
    }
}

/// Why an art answer may differ from a later session's under the same
/// fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtDoubtKind {
    /// The file failed with an I/O error on every attempt.
    GivenUp,
    /// The fingerprint walk does not cover the file.
    Uncovered,
}

impl ArtDoubtKind {
    /// For the log.
    pub fn reason(self) -> &'static str {
        match self {
            Self::GivenUp => "given up on after I/O errors",
            Self::Uncovered => "not covered by the lit fingerprint",
        }
    }
}

/// An art file whose answer may differ next session, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtDoubt {
    pub file: Arc<Path>,
    pub kind: ArtDoubtKind,
}

impl ArtDoubt {
    fn given_up(file: &Path) -> Self {
        Self {
            file: Arc::from(file),
            kind: ArtDoubtKind::GivenUp,
        }
    }

    fn uncovered(file: &Path) -> Self {
        Self {
            file: Arc::from(file),
            kind: ArtDoubtKind::Uncovered,
        }
    }
}

/// What the art lookups made for one payload met: the first [`ArtDoubt`]
/// among them, if any. A payload with one is not kept on disk. Shared by
/// the generator's threads.
#[derive(Debug, Default)]
pub struct ArtRecord {
    doubt: OnceLock<ArtDoubt>,
}

impl ArtRecord {
    /// The first doubt a lookup met, if any.
    #[cfg(test)]
    pub fn doubt(&self) -> Option<&ArtDoubt> {
        self.doubt.get()
    }

    /// The first doubt a lookup met, if any, owned.
    pub fn into_doubt(self) -> Option<ArtDoubt> {
        self.doubt.into_inner()
    }

    fn note(&self, doubt: &Option<ArtDoubt>) {
        if let Some(doubt) = doubt {
            // The first doubt met is the one named.
            let _ = self.doubt.set(doubt.clone());
        }
    }
}

/// One remembered art answer: the value (`None` is dark) and whether it is
/// in doubt.
#[derive(Debug)]
struct Lookup<T> {
    value: Option<Arc<T>>,
    doubt: Option<ArtDoubt>,
}

// Derived `Clone` would ask for `T: Clone`.
impl<T> Clone for Lookup<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            doubt: self.doubt.clone(),
        }
    }
}

impl<T> Lookup<T> {
    fn new(value: Option<T>, doubt: Option<ArtDoubt>) -> Self {
        Self {
            value: value.map(Arc::new),
            doubt,
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

/// The names of the files the fingerprint walk hashes.
fn fingerprinted_name(name: &str) -> bool {
    name == "library.txt" || name.ends_with(".ter") || name.ends_with("_LIT.dds")
}

/// `path` with `.` and `..` resolved lexically, so the path read is the
/// path checked against the walk. A `..` with nothing to climb stays.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A read's outcome after the retry policy.
enum Read {
    Bytes(Vec<u8>),
    /// Nothing there; not retried.
    Missing,
    /// Every attempt failed.
    Failed(io::Error),
}

/// The lit art of one X-Plane installation (`1000 world terrain`).
pub struct LaminarArt {
    terrain_root: PathBuf,
    /// `terrain_root`, normalized: the prefix of every path resolved.
    root_lexical: PathBuf,
    /// `terrain_root` with every symlink resolved, `None` when it is absent.
    root_real: OnceLock<Option<PathBuf>>,
    files: Arc<dyn ArtFiles>,
    library: OnceLock<Lookup<LibraryIndex>>,
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

    /// As [`Self::new`], reaching the art files through `files`.
    pub fn with_files(terrain_root: PathBuf, files: Arc<dyn ArtFiles>) -> Self {
        Self {
            root_lexical: normalize(&terrain_root),
            terrain_root,
            root_real: OnceLock::new(),
            files,
            library: OnceLock::new(),
            resolved: DashMap::new(),
            patterns: DashMap::new(),
            fingerprint: OnceLock::new(),
        }
    }

    /// Digest of the art a lit texture can draw on: the library, every
    /// `.ter` and every `_LIT.dds` under the terrain root, by relative path,
    /// size and modification time. Symlinks are not followed (a symlinked
    /// file counts by the link's own identity, a symlinked folder is not
    /// entered); answers that depend on such files are in doubt instead. A
    /// missing root has a fingerprint too.
    ///
    /// Blocking directory walk (~30 ms on a real install), done once per
    /// session and before the first art file is read, so no answer is drawn
    /// from a file older than the walk.
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
                    if fingerprinted_name(&name) {
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

    /// Whether the fingerprint walk covers `path` (normalized), present or
    /// not: it lies under the root, has a name the walk hashes, and no
    /// component below the root is a symlink, so the walk lists it where it
    /// is, or lists the folder it would appear in. Blocking `stat`s.
    fn covered(&self, path: &Path) -> bool {
        let Ok(mut relative) = path.strip_prefix(&self.root_lexical) else {
            return false;
        };
        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(fingerprinted_name);
        let root = self
            .root_real
            .get_or_init(|| std::fs::canonicalize(&self.terrain_root).ok());
        let (true, Some(root)) = (named, root) else {
            return false;
        };
        // The deepest part of the path that exists must resolve to itself.
        loop {
            match std::fs::canonicalize(self.root_lexical.join(relative)) {
                Ok(real) => return real == root.join(relative),
                Err(e) if e.kind() == io::ErrorKind::NotFound => match relative.parent() {
                    Some(parent) => relative = parent,
                    None => return false,
                },
                Err(_) => return false,
            }
        }
    }

    /// `doubt`, or else an `Uncovered` doubt when the walk does not cover
    /// `path`.
    fn or_uncovered(&self, doubt: Option<ArtDoubt>, path: &Path) -> Option<ArtDoubt> {
        doubt.or_else(|| (!self.covered(path)).then(|| ArtDoubt::uncovered(path)))
    }

    /// Reads `path` under the retry policy; a missing file is not retried.
    fn read(&self, path: &Path) -> Read {
        let read = with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
            match self.files.read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        });
        match read {
            Ok(Some(bytes)) => Read::Bytes(bytes),
            Ok(None) => Read::Missing,
            Err(e) => Read::Failed(e),
        }
    }

    /// Whether `path` is a file, under the retry policy; a missing one is
    /// not retried.
    fn is_file(&self, path: &Path) -> io::Result<bool> {
        with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
            match self.files.is_file(path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                other => other,
            }
        })
    }

    /// The decoded lit pattern a palette name stands for, or `None` (dark).
    /// Generation uses [`Self::pattern_recorded`], which also says whether
    /// the answer may be kept on disk.
    ///
    /// Blocking file I/O and decoding on first use of a texture.
    #[cfg(test)]
    pub fn pattern(&self, palette_name: &str) -> Option<Arc<LitPattern>> {
        self.pattern_lookup(palette_name).value
    }

    /// The decoded lit pattern a palette name stands for, or `None` (dark),
    /// noting in `record` a doubt the lookup met: what the lit generator's
    /// lookups for one payload use.
    ///
    /// Blocking file I/O and decoding on first use of a texture.
    pub fn pattern_recorded(
        &self,
        palette_name: &str,
        record: &ArtRecord,
    ) -> Option<Arc<LitPattern>> {
        let lookup = self.pattern_lookup(palette_name);
        record.note(&lookup.doubt);
        lookup.value
    }

    fn pattern_lookup(&self, palette_name: &str) -> Lookup<LitPattern> {
        let terrain = self.terrain_lookup(palette_name);
        let Some(lit) = terrain.value else {
            return Lookup::new(None, terrain.doubt);
        };
        let key = format!("{}|{}", lit.texture.display(), lit.ground_scale_m);
        let pattern = once(&self.patterns, &key, || self.load_pattern(&lit));
        Lookup {
            value: pattern.value,
            doubt: terrain.doubt.or(pattern.doubt),
        }
    }

    /// The texture's pattern; its doubt is only its own read's (whether the
    /// walk covers it is the terrain's answer).
    fn load_pattern(&self, lit: &LitTerrain) -> Lookup<LitPattern> {
        let texture = &lit.texture;
        let bytes = match self.read(texture) {
            Read::Bytes(bytes) => bytes,
            Read::Missing => {
                warn!(texture = %texture.display(),
                      "Night lighting: lit texture missing; this class stays dark");
                return Lookup::new(None, None);
            }
            Read::Failed(e) => {
                warn!(texture = %texture.display(), error = %e,
                      "Night lighting: lit texture unreadable after retries; this class stays dark this session");
                return Lookup::new(None, Some(ArtDoubt::given_up(texture)));
            }
        };
        let pattern = decode_dxt1(&bytes, lit.ground_scale_m);
        if pattern.is_none() {
            warn!(texture = %texture.display(),
                  "Night lighting: lit texture is not a readable DXT1 DDS; this class stays dark");
        }
        Lookup::new(pattern, None)
    }

    /// The lit terrain a recipe palette name stands for, or `None` (dark).
    /// It does not say whether the answer is in doubt: the probe, its one
    /// caller outside tests, decides only a size, which is never persisted.
    ///
    /// Blocking file I/O on first use of a name: call from a blocking context.
    pub fn lit_terrain(&self, palette_name: &str) -> Option<Arc<LitTerrain>> {
        self.terrain_lookup(palette_name).value
    }

    fn terrain_lookup(&self, palette_name: &str) -> Lookup<LitTerrain> {
        once(&self.resolved, palette_name, || self.resolve(palette_name))
    }

    /// Text of an art file: `Err` is the dark (or given up) answer to give.
    fn text<T>(&self, path: &Path, what: &str) -> Result<String, Lookup<T>> {
        match self.read(path) {
            Read::Bytes(bytes) => String::from_utf8(bytes).map_err(|_| {
                warn!(path = %path.display(), file = what,
                      "Night lighting: Laminar art file is not UTF-8; it is treated as empty");
                Lookup::new(None, self.or_uncovered(None, path))
            }),
            Read::Missing => {
                warn!(path = %path.display(), file = what,
                      "Night lighting: Laminar art file missing; it is treated as empty");
                Err(Lookup::new(None, self.or_uncovered(None, path)))
            }
            Read::Failed(e) => {
                warn!(path = %path.display(), file = what, error = %e,
                      "Night lighting: Laminar art file unreadable after retries; it is treated as empty this session");
                Err(Lookup::new(None, Some(ArtDoubt::given_up(path))))
            }
        }
    }

    fn library(&self) -> &Lookup<LibraryIndex> {
        self.library.get_or_init(|| {
            // The walk before the first read: a file replaced after a read
            // and before the walk would file the answer under the new
            // file's identity.
            self.fingerprint();
            let path = self.root_lexical.join("library.txt");
            match self.text(&path, "library") {
                Ok(text) => Lookup::new(
                    Some(LibraryIndex::parse(&text)),
                    self.or_uncovered(None, &path),
                ),
                Err(dark) => dark,
            }
        })
    }

    fn resolve(&self, palette_name: &str) -> Lookup<LitTerrain> {
        let library = self.library();
        let Some(index) = &library.value else {
            return Lookup::new(None, library.doubt.clone());
        };
        let Some(file) = index.resolve(palette_name) else {
            warn!(
                name = palette_name,
                "Night lighting: palette name not in the Laminar library; this class stays dark"
            );
            return Lookup::new(None, library.doubt.clone());
        };
        let ter_path = normalize(&self.root_lexical.join(file));
        let text = match self.text::<LitTerrain>(&ter_path, "terrain") {
            Ok(text) => text,
            Err(dark) => return Lookup::new(None, library.doubt.clone().or(dark.doubt)),
        };
        let doubt = self.or_uncovered(library.doubt.clone(), &ter_path);
        let Some(lit) = ter_path
            .parent()
            .and_then(|dir| parse_lit_terrain(&text, dir))
        else {
            debug!(
                name = palette_name,
                "Night lighting: terrain has no lit texture"
            );
            return Lookup::new(None, doubt);
        };
        let lit = LitTerrain {
            texture: normalize(&lit.texture),
            ..lit
        };
        let doubt = self.or_uncovered(doubt, &lit.texture);
        match self.is_file(&lit.texture) {
            Ok(true) => Lookup::new(Some(lit), doubt),
            Ok(false) => {
                warn!(name = palette_name, texture = %lit.texture.display(),
                      "Night lighting: lit texture missing; this class stays dark");
                Lookup::new(None, doubt)
            }
            Err(e) => {
                warn!(name = palette_name, texture = %lit.texture.display(), error = %e,
                      "Night lighting: lit texture unreadable after retries; this class stays dark this session");
                Lookup::new(None, doubt.or(Some(ArtDoubt::given_up(&lit.texture))))
            }
        }
    }
}

/// Art file access for tests: the disk, except that the first
/// `read_failures` reads and `stat_failures` `is_file`s of one file fail
/// with an I/O error. Every read of that file is counted, and each
/// successful one takes `delay` longer.
#[cfg(test)]
pub(crate) struct FlakyArtFiles {
    path: PathBuf,
    read_failures: std::sync::atomic::AtomicUsize,
    stat_failures: std::sync::atomic::AtomicUsize,
    reads: std::sync::atomic::AtomicUsize,
    delay: std::time::Duration,
}

#[cfg(test)]
impl FlakyArtFiles {
    /// The first `read_failures` reads of `path` fail (`usize::MAX`: every one).
    pub(crate) fn new(path: PathBuf, read_failures: usize) -> Self {
        Self {
            path,
            read_failures: read_failures.into(),
            stat_failures: 0.into(),
            reads: 0.into(),
            delay: std::time::Duration::ZERO,
        }
    }

    /// The first `failures` `is_file`s of the file fail too.
    pub(crate) fn failing_stats(self, failures: usize) -> Self {
        self.stat_failures
            .store(failures, std::sync::atomic::Ordering::SeqCst);
        self
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

    fn injected(failures: &std::sync::atomic::AtomicUsize) -> io::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        match failures.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)) {
            Ok(_) => Err(io::Error::other("injected I/O failure")),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
impl ArtFiles for FlakyArtFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if path == self.path {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self::injected(&self.read_failures)?;
            std::thread::sleep(self.delay);
        }
        std::fs::read(path)
    }

    fn is_file(&self, path: &Path) -> io::Result<bool> {
        if path == self.path {
            Self::injected(&self.stat_failures)?;
        }
        DiskArtFiles.is_file(path)
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

    /// The digest of one tree, pinned: what the walk hashes may not change,
    /// or every user's lit disk tier would be regenerated.
    #[test]
    fn the_fingerprint_of_a_fixed_tree_is_pinned() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        std::fs::write(temp.path().join("terrain12/notes.txt"), b"not art").unwrap();
        let when = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for file in [
            "library.txt",
            "terrain12/urban_tmp_sdry3.ter",
            "terrain12/park_tmp_sdry1.ter",
            "terrain12/notes.txt",
            "textures12/urban_FAR/uw5_FAR_LIT.dds",
        ] {
            std::fs::File::options()
                .write(true)
                .open(temp.path().join(file))
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        let hex: String = LaminarArt::new(temp.path().to_path_buf())
            .fingerprint()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "e181ab1beb3b44a347692a743cc4620f11da0a10507c4b5eeff69ca952f2d1bd"
        );
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
            (p.size(), p.ground_scale_m, p.rgb[0]),
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

    /// The doubt `record` holds, as (file, kind).
    fn doubt(record: &ArtRecord) -> Option<(PathBuf, ArtDoubtKind)> {
        record.doubt().map(|d| (d.file.to_path_buf(), d.kind))
    }

    /// The doubt of one fresh lookup of `name`, and whether it was lit.
    fn lookup(art: &LaminarArt, name: &str) -> (bool, Option<(PathBuf, ArtDoubtKind)>) {
        let record = ArtRecord::default();
        let lit = art.pattern_recorded(name, &record).is_some();
        (lit, doubt(&record))
    }

    fn ter_file(root: &Path) -> PathBuf {
        root.join("terrain12/urban_tmp_sdry3.ter")
    }

    #[test]
    fn dark_content_is_settled() {
        // Unlit, unknown and undecodable are properties of the files the
        // fingerprint covers: the same every session.
        let temp = TempDir::new().unwrap();
        install(temp.path(), true); // the lit texture does not decode
        let art = LaminarArt::new(temp.path().to_path_buf());
        for name in [PARK, "lib/g10/terrain10/nope.ter", URBAN] {
            assert_eq!(lookup(&art, name), (false, None), "{name}");
        }
    }

    #[test]
    fn a_missing_file_under_the_root_is_dark_and_settled() {
        // Whether a file the walk would hash exists is in the fingerprint.
        let temp = TempDir::new().unwrap();
        install(temp.path(), false); // no lit texture
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(lookup(&art, URBAN), (false, None), "missing texture");

        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        std::fs::remove_file(ter_file(temp.path())).unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(lookup(&art, URBAN), (false, None), "missing .ter");

        let temp = TempDir::new().unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(lookup(&art, URBAN), (false, None), "missing library");
    }

    #[test]
    fn a_missing_file_takes_no_retries() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        std::fs::remove_file(ter_file(temp.path())).unwrap();
        let files = Arc::new(FlakyArtFiles::new(ter_file(temp.path()), 0));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        assert!(art.lit_terrain(URBAN).is_none());
        assert_eq!(files.reads(), 1);
    }

    #[test]
    fn no_installation_is_in_doubt() {
        // Without a root nothing is walked, so nothing is covered.
        let root = PathBuf::from("/nonexistent/1000 world terrain");
        let art = LaminarArt::new(root.clone());
        assert_eq!(
            lookup(&art, URBAN),
            (
                false,
                Some((root.join("library.txt"), ArtDoubtKind::Uncovered))
            )
        );
    }

    #[test]
    fn files_that_are_not_utf8_are_dark_and_settled() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        std::fs::write(ter_file(temp.path()), b"TEXTURE_LIT \xff\xfe\n").unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(lookup(&art, URBAN), (false, None), ".ter");

        std::fs::write(temp.path().join("library.txt"), b"A\n\xff\n").unwrap();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert_eq!(lookup(&art, URBAN), (false, None), "library");
    }

    #[test]
    fn a_library_and_a_ter_whose_first_read_fails_are_read_again() {
        for file in ["library.txt", "terrain12/urban_tmp_sdry3.ter"] {
            let temp = TempDir::new().unwrap();
            install_decodable(temp.path());
            let files = Arc::new(FlakyArtFiles::new(temp.path().join(file), 1));
            let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
            assert_eq!(lookup(&art, URBAN), (true, None), "{file}");
            assert_eq!(files.reads(), 2, "{file}: one retry");
        }
    }

    #[test]
    fn an_unreadable_ter_is_given_up_on() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let ter = ter_file(temp.path());
        let files = Arc::new(FlakyArtFiles::new(ter.clone(), usize::MAX));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        assert_eq!(
            lookup(&art, URBAN),
            (false, Some((ter, ArtDoubtKind::GivenUp)))
        );
        assert_eq!(files.reads(), 1 + READ_RETRIES as usize);
        assert_eq!(lookup(&art, PARK), (false, None), "another .ter");
    }

    #[test]
    fn a_library_given_up_on_is_noted_by_every_later_lookup() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let library = temp.path().join("library.txt");
        let files = Arc::new(FlakyArtFiles::new(library.clone(), usize::MAX));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        let given_up = Some((library, ArtDoubtKind::GivenUp));
        assert_eq!(lookup(&art, URBAN), (false, given_up.clone()));
        assert_eq!(lookup(&art, URBAN), (false, given_up.clone()), "cached");
        assert_eq!(lookup(&art, PARK), (false, given_up), "another name");
        assert_eq!(files.reads(), 1 + READ_RETRIES as usize, "read once");
    }

    #[test]
    fn a_lit_texture_whose_first_read_fails_is_read_again_and_lit() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let files = Arc::new(FlakyArtFiles::new(lit_file(temp.path()), 1));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        assert_eq!(lookup(&art, URBAN), (true, None));
        assert_eq!(files.reads(), 2, "one retry");
    }

    #[test]
    fn a_lit_texture_unreadable_on_every_attempt_is_given_up_on_once() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = lit_file(temp.path());
        let files = Arc::new(FlakyArtFiles::new(path.clone(), usize::MAX));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files.clone());
        let given_up = Some((path, ArtDoubtKind::GivenUp));
        let log = logged(|| {
            assert_eq!(lookup(&art, URBAN), (false, given_up.clone()));
            // Remembered for the session: no more reads, still noted.
            assert_eq!(lookup(&art, URBAN), (false, given_up));
        });
        assert_eq!(files.reads(), 1 + READ_RETRIES as usize);
        assert_eq!(log.matches("WARN").count(), 1, "{log}");
        assert!(log.contains("uw5_FAR_LIT.dds"), "{log}");
        // The probe's view of the class is unaffected: its terrain resolved.
        assert!(art.lit_terrain(URBAN).is_some());
        assert_eq!(lookup(&art, PARK), (false, None), "other lookups");
    }

    #[test]
    fn a_lit_texture_stat_that_fails_once_is_retried() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let files = Arc::new(FlakyArtFiles::new(lit_file(temp.path()), 0).failing_stats(1));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files);
        assert_eq!(lookup(&art, URBAN), (true, None));
    }

    #[test]
    fn a_lit_texture_stat_failing_every_attempt_is_given_up_on() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let path = lit_file(temp.path());
        let files = Arc::new(FlakyArtFiles::new(path.clone(), 0).failing_stats(usize::MAX));
        let art = LaminarArt::with_files(temp.path().to_path_buf(), files);
        assert_eq!(
            lookup(&art, URBAN),
            (false, Some((path, ArtDoubtKind::GivenUp)))
        );
        assert!(art.lit_terrain(URBAN).is_none(), "dark for the probe too");
    }

    /// `install_decodable` under `temp/root`, with the urban `.ter`
    /// pointing at `texture` (relative to `terrain12`).
    fn install_pointing_at(temp: &TempDir, texture: &str) -> PathBuf {
        let root = temp.path().join("root");
        install_decodable(&root);
        std::fs::write(
            ter_file(&root),
            format!("TEXTURE_LIT {texture}\nLIT_FADE_PARAMS 3072 7000 15000\n"),
        )
        .unwrap();
        root
    }

    #[test]
    fn a_lit_texture_outside_the_root_is_in_doubt() {
        let temp = TempDir::new().unwrap();
        let root = install_pointing_at(&temp, "../../outside/x_LIT.dds");
        let outside = temp.path().join("outside/x_LIT.dds");
        let art = LaminarArt::new(root.clone());
        let uncovered = Some((outside.clone(), ArtDoubtKind::Uncovered));
        assert_eq!(lookup(&art, URBAN), (false, uncovered.clone()), "missing");

        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::copy(lit_file(&root), &outside).unwrap();
        let art = LaminarArt::new(root);
        assert_eq!(lookup(&art, URBAN), (true, uncovered), "lit");
    }

    #[test]
    fn a_lit_texture_the_walk_does_not_hash_is_in_doubt() {
        let temp = TempDir::new().unwrap();
        let root = install_pointing_at(&temp, "../textures12/urban_FAR/night.dds");
        let texture = root.join("textures12/urban_FAR/night.dds");
        std::fs::copy(lit_file(&root), &texture).unwrap();
        let art = LaminarArt::new(root);
        assert_eq!(
            lookup(&art, URBAN),
            (true, Some((texture, ArtDoubtKind::Uncovered)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_lit_texture_in_a_symlinked_folder_is_in_doubt() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        install_decodable(&root);
        let elsewhere = temp.path().join("elsewhere");
        std::fs::rename(root.join("textures12"), &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("textures12")).unwrap();
        let art = LaminarArt::new(root.clone());
        assert_eq!(
            lookup(&art, URBAN),
            (true, Some((lit_file(&root), ArtDoubtKind::Uncovered)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_covered() {
        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real");
        install_decodable(&real);
        let root = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &root).unwrap();
        assert_eq!(lookup(&LaminarArt::new(root), URBAN), (true, None));
    }

    #[test]
    fn the_walk_runs_before_the_first_art_read() {
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        let before = LaminarArt::new(temp.path().to_path_buf()).fingerprint();
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art.lit_terrain(URBAN).is_some());
        std::fs::write(
            ter_file(temp.path()),
            "TEXTURE_LIT ../textures12/urban_FAR/uw5_FAR_LIT.dds\nLIT_FADE_PARAMS 4096 1 2\n",
        )
        .unwrap();
        assert_eq!(art.fingerprint(), before, "walked before the read");
        assert_ne!(
            LaminarArt::new(temp.path().to_path_buf()).fingerprint(),
            before
        );
    }

    #[test]
    fn concurrent_first_calls_decode_the_lit_texture_once() {
        const CALLERS: usize = 8;
        let temp = TempDir::new().unwrap();
        install_decodable(temp.path());
        // Slow enough that every caller arrives while the first one reads.
        let files = Arc::new(
            FlakyArtFiles::new(lit_file(temp.path()), 0).slowed(Duration::from_millis(50)),
        );
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

    /// Every global `.ter` export of the installation `XEL_LAMINAR_ART`
    /// names is settled (no doubt), so the lit disk tier is written on a
    /// stock install; prints the counts and the fingerprint.
    #[test]
    #[ignore = "needs a real Laminar installation"]
    fn a_real_installation_is_settled() {
        let Some(root) = std::env::var_os("XEL_LAMINAR_ART") else {
            eprintln!("XEL_LAMINAR_ART is not set: skipped (point it at `1000 world terrain`)");
            return;
        };
        let root = PathBuf::from(root);
        let text = std::fs::read_to_string(root.join("library.txt")).unwrap();
        let mut names: Vec<&str> = text
            .lines()
            .filter_map(|line| {
                let tokens: Vec<&str> = line.split_whitespace().collect();
                match tokens.as_slice() {
                    [kind, name, ..] if kind.starts_with("EXPORT") && name.ends_with(".ter") => {
                        Some(*name)
                    }
                    [kind, _, name, ..] if kind.starts_with("EXPORT") && name.ends_with(".ter") => {
                        Some(*name)
                    }
                    _ => None,
                }
            })
            .collect();
        names.sort_unstable();
        names.dedup();
        let art = LaminarArt::new(root);
        let (mut lit, mut doubts) = (0, Vec::new());
        for name in &names {
            let (is_lit, doubt) = lookup(&art, name);
            lit += usize::from(is_lit);
            doubts.extend(doubt.map(|d| (name.to_string(), d)));
        }
        let hex: String = art
            .fingerprint()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        eprintln!(
            "{} names, {lit} lit, {} in doubt; fingerprint {hex}",
            names.len(),
            doubts.len()
        );
        assert!(lit > 0);
        assert!(doubts.is_empty(), "{doubts:?}");
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
