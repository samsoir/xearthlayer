//! Finds, loads and caches night recipes for 1 degree tiles.
//!
//! The union index hides a package's `night/` folder from X-Plane, so the
//! store reads recipes through each ortho source's own path, in the union's
//! priority order (patches before regional packages).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashSet;
use moka::sync::Cache;
use tracing::warn;

use super::generator::LedRegion;
use super::recipe::{parse_recipe, Recipe};
use crate::ortho_union::{OrthoSource, OrthoUnionIndex};

/// Byte budget for decoded recipes (about 1.5 MB each).
const RECIPE_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Larger files are rejected unread. Real recipes are tens of kilobytes.
const MAX_RECIPE_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub use xearthlayer_package::night::{block_name, tile_name};

/// Where a source keeps a tile's recipe: `<root>/night/<block>/<tile>.oxn`.
pub fn recipe_path(root: &Path, lat: i32, lon: i32) -> PathBuf {
    root.join(xearthlayer_package::night::PACKAGE_DIR)
        .join(xearthlayer_package::night::recipe_relative_path(lat, lon))
}

/// Size and modification time of a recipe file: what changes when a recipe
/// is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub len: u64,
    pub mtime_nanos: u128,
}

impl FileIdentity {
    /// The identity `meta` describes; an unknown mtime counts as zero.
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            mtime_nanos: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos()),
        }
    }
}

/// What a tile's load found: the recipe (`None` for absent or rejected),
/// the identity of the file it was read from (`None` when there was none),
/// and the region of the source that file came from (`Unknown` when there
/// was none).
#[derive(Clone)]
struct Loaded {
    recipe: Option<Arc<Recipe>>,
    identity: Option<FileIdentity>,
    region: LedRegion,
}

/// The LED region of an ortho source: a regional package's code, or
/// `Unknown` for a patch.
fn source_region(source: &OrthoSource) -> LedRegion {
    if source.source_type.is_regional_package() {
        LedRegion::from_code(&source.sort_key)
    } else {
        LedRegion::Unknown
    }
}

/// Recipes per tile, loaded lazily and evicted by size. Absence is cached.
pub struct RecipeStore {
    roots: Vec<(PathBuf, LedRegion)>,
    cache: Cache<(i32, i32), Loaded>,
    warned: DashSet<PathBuf>,
}

impl RecipeStore {
    /// `roots` in priority order: the first source with a recipe wins.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self::with_regions(
            roots
                .into_iter()
                .map(|root| (root, LedRegion::Unknown))
                .collect(),
        )
    }

    /// Like `new`, with the LED region of each root.
    pub fn with_regions(roots: Vec<(PathBuf, LedRegion)>) -> Self {
        let cache = Cache::builder()
            .max_capacity(RECIPE_CACHE_BYTES)
            .weigher(|_, v: &Loaded| match &v.recipe {
                Some(r) => (r.class.cells.len() + r.brightness.cells.len())
                    .try_into()
                    .unwrap_or(u32::MAX),
                None => 1,
            })
            .build();
        Self {
            roots,
            cache,
            warned: DashSet::new(),
        }
    }

    /// Every source of the union index, in its priority order.
    pub fn from_index(index: &OrthoUnionIndex) -> Self {
        Self::with_regions(
            index
                .sources()
                .iter()
                .map(|s| (s.source_path.clone(), source_region(s)))
                .collect(),
        )
    }

    /// The recipe for the tile whose south-west corner is `(lat, lon)`.
    ///
    /// Blocking file I/O on a cache miss: call from a blocking context.
    /// A transient I/O failure is not cached, so the next call retries.
    pub fn get(&self, lat: i32, lon: i32) -> Option<Arc<Recipe>> {
        self.get_checked(lat, lon).unwrap_or(None)
    }

    /// The recipe, or the I/O error that stopped it being read. A transient
    /// error is not cached, so calling again retries.
    pub fn get_checked(&self, lat: i32, lon: i32) -> std::io::Result<Option<Arc<Recipe>>> {
        Ok(self.loaded(lat, lon)?.recipe)
    }

    /// Identity of the recipe file the cached recipe was read from, or `None`
    /// when the tile has none. Loads through the cache when not yet loaded,
    /// so it always describes the content `get` returns; a file replaced
    /// after loading does not change it. `Err` (not cached) when the load
    /// failed. Blocking file I/O on a cache miss.
    pub fn identity(&self, lat: i32, lon: i32) -> std::io::Result<Option<FileIdentity>> {
        Ok(self.loaded(lat, lon)?.identity)
    }

    /// The recipe and the region of the source it was read from, from one
    /// cached load, so the two always describe the same file.
    pub fn get_with_region(
        &self,
        lat: i32,
        lon: i32,
    ) -> std::io::Result<(Option<Arc<Recipe>>, LedRegion)> {
        let l = self.loaded(lat, lon)?;
        Ok((l.recipe, l.region))
    }

    /// `identity` with the region, from one cached load.
    pub fn identity_with_region(
        &self,
        lat: i32,
        lon: i32,
    ) -> std::io::Result<(Option<FileIdentity>, LedRegion)> {
        let l = self.loaded(lat, lon)?;
        Ok((l.identity, l.region))
    }

    fn loaded(&self, lat: i32, lon: i32) -> std::io::Result<Loaded> {
        self.cache
            .try_get_with((lat, lon), || self.load(lat, lon))
            .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))
    }

    /// The file a tile's recipe is read from, and its source's region: the
    /// first source that has one.
    fn resolve(&self, lat: i32, lon: i32) -> Option<(PathBuf, LedRegion)> {
        self.roots
            .iter()
            .map(|(root, region)| (recipe_path(root, lat, lon), *region))
            .find(|(p, _)| p.is_file())
    }

    /// No recipe (cached) for an absent or rejected file; `Err` (not cached)
    /// when the file could not be read, or changed while it was read. The
    /// identity is taken before and after the read, so it is the identity of
    /// the bytes parsed.
    fn load(&self, lat: i32, lon: i32) -> Result<Loaded, std::io::Error> {
        let Some((path, region)) = self.resolve(lat, lon) else {
            return Ok(Loaded {
                recipe: None,
                identity: None,
                region: LedRegion::Unknown,
            });
        };
        let (identity, read) = match identified_read(&path) {
            Ok(read) => read,
            Err(e) => {
                self.warn_once(&path, &e.to_string());
                return Err(e);
            }
        };
        let rejected = Loaded {
            recipe: None,
            identity: Some(identity),
            region,
        };
        let Some(bytes) = read else {
            self.warn_once(&path, "file exceeds the recipe size ceiling");
            return Ok(rejected);
        };
        let recipe = match parse_recipe(&bytes) {
            Ok(recipe) if recipe.tile != tile_name(lat, lon) => {
                self.warn_once(
                    &path,
                    &format!(
                        "recipe is for tile {}, expected {}",
                        recipe.tile,
                        tile_name(lat, lon)
                    ),
                );
                return Ok(rejected);
            }
            Ok(recipe) => recipe,
            Err(e) => {
                self.warn_once(&path, &e.to_string());
                return Ok(rejected);
            }
        };
        Ok(Loaded {
            recipe: Some(Arc::new(recipe)),
            identity: Some(identity),
            region,
        })
    }

    fn warn_once(&self, path: &Path, reason: &str) {
        if self.warned.insert(path.to_path_buf()) {
            warn!(path = %path.display(), %reason, "Night recipe unusable; tile stays dark");
        }
    }
}

/// The file's identity and `read_bounded` bytes, or an error when the file
/// changed during the read (its identity before and after differ): a
/// transient state, retried by the caller.
fn identified_read(path: &Path) -> std::io::Result<(FileIdentity, Option<Vec<u8>>)> {
    let before = FileIdentity::from_metadata(&std::fs::metadata(path)?);
    let read = read_bounded(path)?;
    let after = FileIdentity::from_metadata(&std::fs::metadata(path)?);
    if before != after {
        return Err(std::io::Error::other(
            "recipe file changed while being read",
        ));
    }
    Ok((after, read))
}

/// The file's bytes, or `None` when it is larger than the ceiling. Reads at
/// most one byte past the ceiling, whatever the file claims.
fn read_bounded(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_RECIPE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_RECIPE_FILE_BYTES).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");
    const EMPTY: &[u8] = include_bytes!("../../tests/fixtures/night/empty.oxn");

    #[test]
    fn a_recipe_carries_the_region_of_the_root_it_was_read_from() {
        let (na, eu) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        for root in [&na, &eu] {
            let p = recipe_path(root.path(), 37, -123);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, URBAN).unwrap();
        }
        let store = RecipeStore::with_regions(vec![
            (na.path().to_path_buf(), LedRegion::Na),
            (eu.path().to_path_buf(), LedRegion::Eu),
        ]);
        let (recipe, region) = store.get_with_region(37, -123).unwrap();
        assert!(recipe.is_some());
        assert_eq!(region, LedRegion::Na, "the first root wins");
        let (identity, region) = store.identity_with_region(37, -123).unwrap();
        assert!(identity.is_some());
        assert_eq!(region, LedRegion::Na);
    }

    #[test]
    fn no_recipe_has_no_region() {
        let temp = TempDir::new().unwrap();
        let store = RecipeStore::with_regions(vec![(temp.path().to_path_buf(), LedRegion::Eu)]);
        let (recipe, region) = store.get_with_region(37, -123).unwrap();
        assert!(recipe.is_none());
        assert_eq!(region, LedRegion::Unknown);
    }

    #[test]
    fn plain_roots_are_unknown() {
        let temp = TempDir::new().unwrap();
        let p = recipe_path(temp.path(), 37, -123);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, URBAN).unwrap();
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert_eq!(
            store.get_with_region(37, -123).unwrap().1,
            LedRegion::Unknown
        );
    }

    #[test]
    fn packages_give_their_code_and_patches_none() {
        let pkg = OrthoSource::new_package("NA-USA-MX-CENTRAL", "/p/na");
        let patch = OrthoSource::new_patch("KLAX_Mesh", "/p/klax");
        assert_eq!(source_region(&pkg), LedRegion::Na);
        assert_eq!(source_region(&patch), LedRegion::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_recipe_is_an_error_and_not_cached() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let path = recipe_path(temp.path(), 37, -123);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, URBAN).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        if std::fs::read(&path).is_ok() {
            return; // running as root: permissions do not apply
        }
        assert!(store.get_checked(37, -123).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            store.get_checked(37, -123).unwrap().is_some(),
            "retried, not cached"
        );
    }

    fn put(root: &Path, lat: i32, lon: i32, bytes: &[u8]) {
        let path = recipe_path(root, lat, lon);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn identity_len(store: &RecipeStore) -> u64 {
        store.identity(37, -123).unwrap().unwrap().len
    }

    #[test]
    fn identity_is_none_without_a_recipe() {
        let temp = TempDir::new().unwrap();
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert_eq!(store.identity(37, -123).unwrap(), None);
    }

    #[test]
    fn identity_follows_the_first_source_with_the_file() {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        put(b.path(), 37, -123, URBAN);
        assert_eq!(
            identity_len(&RecipeStore::new(roots.clone())),
            URBAN.len() as u64
        );
        put(a.path(), 37, -123, EMPTY);
        assert_eq!(
            identity_len(&RecipeStore::new(roots)),
            EMPTY.len() as u64,
            "the first source now has it"
        );
    }

    #[test]
    fn identity_changes_when_the_file_is_rewritten() {
        let temp = TempDir::new().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        put(temp.path(), 37, -123, URBAN);
        let before = RecipeStore::new(roots.clone()).identity(37, -123).unwrap();
        put(temp.path(), 37, -123, EMPTY);
        assert_ne!(RecipeStore::new(roots).identity(37, -123).unwrap(), before);
    }

    #[test]
    fn a_rejected_recipe_keeps_its_identity() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, b"not a recipe");
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
        assert_eq!(identity_len(&store), 12);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_recipe_has_no_identity() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let path = recipe_path(temp.path(), 37, -123);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.identity(37, -123).is_err());
    }

    #[test]
    fn identity_is_the_loaded_content_after_the_file_changes() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_some());
        put(temp.path(), 37, -123, EMPTY);
        assert_eq!(
            identity_len(&store),
            URBAN.len() as u64,
            "the recipe in use is still urban.oxn"
        );
    }

    #[test]
    fn names_follow_ortho4xp() {
        assert_eq!(tile_name(37, -123), "+37-123");
        assert_eq!(block_name(37, -123), "+30-130");
        assert_eq!(tile_name(-12, 45), "-12+045");
        assert_eq!(block_name(-12, 45), "-20+040");
        assert_eq!(
            recipe_path(Path::new("/pkg"), 37, -123),
            PathBuf::from("/pkg/night/+30-130/+37-123.oxn")
        );
    }

    #[test]
    fn finds_a_recipe_in_a_source() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert_eq!(store.get(37, -123).unwrap().tile, "+37-123");
    }

    #[test]
    fn a_tile_without_a_recipe_is_none() {
        let temp = TempDir::new().unwrap();
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[test]
    fn the_first_source_in_priority_order_wins() {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        put(a.path(), 37, -123, EMPTY);
        put(b.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![a.path().to_path_buf(), b.path().to_path_buf()]);
        assert_eq!(
            store.get(37, -123).unwrap().palette.len(),
            1,
            "empty.oxn from the first source"
        );
    }

    #[test]
    fn a_rejected_recipe_is_a_dark_tile() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, b"not a recipe");
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[test]
    fn a_recipe_for_another_tile_is_rejected() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 38, -123, URBAN); // urban.oxn is tile +37-123
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(38, -123).is_none());
    }

    #[test]
    fn a_file_over_the_ceiling_is_rejected_unread() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(recipe_path(temp.path(), 37, -123))
            .unwrap();
        file.set_len(MAX_RECIPE_FILE_BYTES + 1).unwrap(); // sparse
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn an_io_error_is_not_cached() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let path = recipe_path(temp.path(), 37, -123);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none(), "unreadable now");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.get(37, -123).is_some(), "the failure was not cached");
    }

    #[test]
    fn results_are_cached_including_absence() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_some());
        assert!(store.get(38, -123).is_none());
        std::fs::remove_file(recipe_path(temp.path(), 37, -123)).unwrap();
        put(temp.path(), 38, -123, URBAN);
        assert!(store.get(37, -123).is_some(), "served from cache");
        assert!(store.get(38, -123).is_none(), "absence is cached too");
    }

    /// R2: the union index never shows `night/` to X-Plane, yet the store,
    /// reading through the source path, still finds the recipe.
    #[test]
    fn the_union_index_hides_night_but_from_index_finds_it() {
        use crate::ortho_union::OrthoUnionIndexBuilder;
        use semver::Version;
        use xearthlayer_package::{InstalledPackage, Package, PackageType};

        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+30-130")).unwrap();
        put(&pkg_dir, 37, -123, URBAN);
        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        assert!(!index.is_directory(Path::new("night")));
        assert!(index
            .resolve(Path::new("night/+30-130/+37-123.oxn"))
            .is_none());
        assert_eq!(
            RecipeStore::from_index(&index).get(37, -123).unwrap().tile,
            "+37-123"
        );
    }
}
