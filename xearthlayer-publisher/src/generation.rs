//! Creating library generations from libraries that already exist (#333).
//!
//! [`upgrade`] turns a repository from before generations into one: its root
//! library becomes a generation. [`import`] adds a generation from library
//! files kept elsewhere, such as another branch of the repository. Both copy
//! the library files and leave the originals where they are, because the root
//! copy is what clients from before generations keep reading, and both move
//! the working folders the copied library lists from the root `packages/` and
//! `dist/` into the generation. A working folder the library does not list is
//! left in place and reported. Moves are renames, so nothing is copied twice
//! and a move onto an existing folder is refused rather than merged.

use std::fs;
use std::path::{Path, PathBuf};

use xearthlayer_package::{self as package, PackageType};

use crate::library::LibraryManager;
use crate::repository::{
    create_working_dirs, is_generation_name, Marker, Repository, LIBRARY_FILE, REGION_METADATA_FILE,
};
use crate::{PublishError, PublishResult};

/// The files that make up a library besides its working folders, in the
/// order they are copied. Only the index is required.
pub const LIBRARY_FILES: [&str; 4] = [
    LIBRARY_FILE,
    REGION_METADATA_FILE,
    "coverage.png",
    "coverage.geojson",
];

const PACKAGES_DIR: &str = "packages";
const DIST_DIR: &str = "dist";

/// What an upgrade or import did, or with `dry_run`, would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationReport {
    /// The generation created.
    pub generation: String,
    /// Library files copied into it: (from, to).
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// Working folders moved into it: (from, to).
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Working folders left at the root because the library does not list them.
    pub left: Vec<PathBuf>,
    /// Nothing was changed.
    pub dry_run: bool,
}

/// Turn the repository at `root`, from before generations, into format
/// 2.0.0, its root library becoming generation `root_as`, which is made
/// current. The root library files stay, unchanged.
pub fn upgrade(root: &Path, root_as: &str, dry_run: bool) -> PublishResult<GenerationReport> {
    let marker = Marker::read(root)?;
    if marker.version.major >= 2 {
        return Err(PublishError::AlreadyUpgraded(root.to_path_buf()));
    }
    let report = plan(root, root, root_as, dry_run)?;
    if !dry_run {
        carry_out(&report, root)?;
        Marker::write(root, marker.created_at, root_as)?;
    }
    Ok(report)
}

/// Add generation `generation` to `repo` from the library files in `from`,
/// moving the root working folders its index lists. Does not make it current.
pub fn import(
    repo: &Repository,
    generation: &str,
    from: &Path,
    dry_run: bool,
) -> PublishResult<GenerationReport> {
    let report = plan(repo.root(), from, generation, dry_run)?;
    if !dry_run {
        carry_out(&report, repo.root())?;
    }
    Ok(report)
}

/// Work out the copies and moves without touching anything, so a dry run
/// and a real one agree, and a conflict stops before the first change.
fn plan(
    root: &Path,
    source: &Path,
    generation: &str,
    dry_run: bool,
) -> PublishResult<GenerationReport> {
    if !is_generation_name(generation) {
        return Err(PublishError::InvalidGeneration(generation.to_string()));
    }
    let target = root.join(generation);
    if target.exists() {
        return Err(PublishError::GenerationExists(generation.to_string()));
    }
    if !source.join(LIBRARY_FILE).is_file() {
        return Err(PublishError::InvalidRepository(format!(
            "no library index ({LIBRARY_FILE}) in {}",
            source.display()
        )));
    }

    let copied = LIBRARY_FILES
        .iter()
        .map(|name| (source.join(name), target.join(name)))
        .filter(|(from, _)| from.is_file())
        .collect();

    let listed: Vec<(String, PackageType)> = LibraryManager::open(source)?
        .entries()
        .iter()
        .map(|e| (e.title.clone(), e.package_type))
        .collect();
    let mut moved = Vec::new();
    for (title, package_type) in &listed {
        let folder = package::package_mountpoint(title, *package_type);
        let archive = Path::new(&title.to_lowercase()).join(package_type.folder_suffix());
        for relative in [
            Path::new(PACKAGES_DIR).join(&folder),
            Path::new(DIST_DIR).join(&archive),
        ] {
            let from = root.join(&relative);
            if from.exists() {
                let to = target.join(&relative);
                moved.push((from, to));
            }
        }
    }

    let moving: Vec<&PathBuf> = moved.iter().map(|(from, _)| from).collect();
    let mut left = Vec::new();
    let packages = root.join(PACKAGES_DIR);
    left.extend(
        children(&packages)
            .into_iter()
            .filter(|p| !moving.contains(&p)),
    );
    // An archive folder is dist/<region>/<type>; a region folder is left when
    // any type under it is.
    for region in children(&root.join(DIST_DIR)) {
        for archive in children(&region) {
            if !moving.contains(&&archive) {
                left.push(archive);
            }
        }
    }
    left.sort();

    Ok(GenerationReport {
        generation: generation.to_string(),
        copied,
        moved,
        left,
        dry_run,
    })
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    found.sort();
    found
}

fn carry_out(report: &GenerationReport, root: &Path) -> PublishResult<()> {
    let target = root.join(&report.generation);
    create_working_dirs(&target)?;
    for (from, to) in &report.copied {
        fs::copy(from, to).map_err(|e| PublishError::WriteFailed {
            path: to.clone(),
            source: e,
        })?;
    }
    for (from, to) in &report.moved {
        if to.exists() {
            return Err(PublishError::MoveTargetExists(to.clone()));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| PublishError::CreateDirectoryFailed {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        fs::rename(from, to).map_err(|e| PublishError::WriteFailed {
            path: to.clone(),
            source: e,
        })?;
        // dist/<region>/ empties once its last type has moved.
        if let Some(region) = from
            .parent()
            .filter(|p| p.parent() == Some(&root.join(DIST_DIR)))
        {
            remove_if_empty(region);
        }
    }
    remove_if_empty(&root.join(PACKAGES_DIR));
    remove_if_empty(&root.join(DIST_DIR));
    Ok(())
}

/// Remove `dir` if it is an empty directory; anything else is left alone.
fn remove_if_empty(dir: &Path) {
    let _ = fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use tempfile::TempDir;

    const MARKER: &str = ".xearthlayer-repo";

    /// A library index listing `packages` as (title, type), with a release
    /// asset URL each, as published libraries do.
    fn library_with(dir: &Path, packages: &[(&str, PackageType)]) {
        fs::create_dir_all(dir).unwrap();
        let metadata = dir.join("fixture-metadata.txt");
        fs::write(&metadata, "metadata").unwrap();
        let mut manager = LibraryManager::open_or_create(dir).unwrap();
        for (title, package_type) in packages {
            manager
                .add_or_update(
                    &metadata,
                    title,
                    *package_type,
                    Version::new(0, 1, 0),
                    &format!("https://example.com/releases/download/{title}-v0.1.0/meta.txt"),
                )
                .unwrap();
        }
        manager.save().unwrap();
        fs::remove_file(&metadata).unwrap();
    }

    /// A repository from before generations: marker 1.0.0, root library
    /// listing `listed`, working folders for those and for `unlisted`.
    fn legacy(root: &Path, listed: &[(&str, PackageType)], unlisted: &[(&str, PackageType)]) {
        fs::write(
            root.join(MARKER),
            "XEARTHLAYER PACKAGE REPOSITORY\n1.0.0\n2025-01-01T00:00:00Z\n",
        )
        .unwrap();
        library_with(root, listed);
        fs::write(root.join(REGION_METADATA_FILE), "{\"regions\":{}}\n").unwrap();
        fs::write(root.join("coverage.png"), b"png bytes").unwrap();
        fs::create_dir_all(root.join("staging")).unwrap();
        for (title, package_type) in listed.iter().chain(unlisted) {
            let folder = root
                .join(PACKAGES_DIR)
                .join(package::package_mountpoint(title, *package_type));
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("file.dsf"), title.as_bytes()).unwrap();
            let archive = root
                .join(DIST_DIR)
                .join(title.to_lowercase())
                .join(package_type.folder_suffix());
            fs::create_dir_all(&archive).unwrap();
            fs::write(archive.join("part.aa"), title.as_bytes()).unwrap();
        }
    }

    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        LIBRARY_FILES
            .iter()
            .filter(|n| root.join(n).is_file())
            .map(|n| (n.to_string(), fs::read(root.join(n)).unwrap()))
            .collect()
    }

    const V1: &[(&str, PackageType)] = &[("NA", PackageType::Ortho), ("NA", PackageType::Overlay)];
    const V2: &[(&str, PackageType)] = &[("NA-CANADA-WEST", PackageType::Ortho)];

    #[test]
    fn upgrade_copies_the_root_library_and_leaves_it_byte_for_byte() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, V2);
        let before = snapshot(root);

        let report = upgrade(root, "v1", false).unwrap();

        assert_eq!(snapshot(root), before, "the frozen root is untouched");
        assert_eq!(snapshot(&root.join("v1")), before, "and copied whole");
        assert_eq!(report.copied.len(), 3, "index, metadata, coverage.png");

        let repo = Repository::open(root).unwrap();
        assert_eq!(repo.generation(), "v1");
        assert_eq!(repo.version().major, 2);
        assert!(
            root.join("staging").is_dir(),
            "the shared work area is left alone"
        );
    }

    #[test]
    fn after_an_upgrade_a_deletion_changes_the_generation_and_never_the_root() {
        // The root library is what clients from before generations read:
        // publishing in a generation must leave it exactly as it was.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, &[]);
        upgrade(root, "v1", false).unwrap();
        let root_before = snapshot(root);
        let repo = Repository::open(root).unwrap();

        let plan = crate::plan_deletion(&repo, "NA", PackageType::Ortho).unwrap();
        crate::execute_deletion(&repo, &plan).unwrap();

        assert_eq!(snapshot(root), root_before);
        let v1 = LibraryManager::open(&repo.library_root()).unwrap();
        assert!(!v1.contains("NA", PackageType::Ortho));
        assert!(v1.contains("NA", PackageType::Overlay));
    }

    #[test]
    fn upgrade_keeps_the_repository_settings() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, &[]);
        let config = crate::RepoConfig::new(64 * 1024 * 1024).unwrap();
        crate::write_config(root, &config).unwrap();

        upgrade(root, "v1", false).unwrap();

        assert_eq!(crate::read_config(root).unwrap(), config);
    }

    #[test]
    fn upgrade_moves_only_the_working_folders_its_library_lists() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, V2);

        let report = upgrade(root, "v1", false).unwrap();
        let repo = Repository::open(root).unwrap();

        for (title, t) in V1 {
            assert!(
                repo.package_dir(title, *t).join("file.dsf").is_file(),
                "{title} {t}"
            );
            assert!(repo
                .dist_dir()
                .join(title.to_lowercase())
                .join(t.folder_suffix())
                .is_dir());
        }
        assert_eq!(report.moved.len(), 4, "two packages, two archive folders");
        // The other world's folders stay at the root, reported.
        let canada = root.join(PACKAGES_DIR).join("zzXEL_na-canada-west_ortho");
        assert!(canada.is_dir());
        assert!(report.left.contains(&canada), "{:?}", report.left);
        assert!(report
            .left
            .contains(&root.join(DIST_DIR).join("na-canada-west").join("ortho")));
        assert!(
            !root.join(DIST_DIR).join("na").exists(),
            "an emptied region folder goes"
        );
    }

    #[test]
    fn a_dry_run_reports_the_same_plan_and_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, V2);

        let dry = upgrade(root, "v1", true).unwrap();
        assert!(dry.dry_run);
        assert!(!root.join("v1").exists());
        assert!(matches!(
            Repository::open(root),
            Err(PublishError::UpgradeRequired(_))
        ));

        let real = upgrade(root, "v1", false).unwrap();
        assert_eq!(
            (dry.copied, dry.moved, dry.left),
            (real.copied, real.moved, real.left)
        );
    }

    #[test]
    fn upgrading_twice_or_onto_an_existing_generation_is_refused() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, &[]);
        fs::create_dir_all(root.join("v1")).unwrap();
        assert!(matches!(
            upgrade(root, "v1", false),
            Err(PublishError::GenerationExists(_))
        ));
        assert!(matches!(
            upgrade(root, "1", false),
            Err(PublishError::InvalidGeneration(_))
        ));

        fs::remove_dir(root.join("v1")).unwrap();
        upgrade(root, "v1", false).unwrap();
        assert!(matches!(
            upgrade(root, "v1", false),
            Err(PublishError::AlreadyUpgraded(_))
        ));
    }

    #[test]
    fn import_adds_a_generation_and_takes_its_working_folders_from_the_root() {
        // The promotion: upgrade main's root to v1, then bring the staging
        // branch's library in as v2. Its working folders were left at the
        // root by the upgrade, because v1 does not list them.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        legacy(root, V1, V2);
        upgrade(root, "v1", false).unwrap();
        let staging = temp.path().join("from-staging");
        library_with(&staging, V2);
        fs::write(staging.join("coverage.png"), b"v2 map").unwrap();
        let root_before = snapshot(root);

        let repo = Repository::open(root).unwrap();
        let report = import(&repo, "v2", &staging, false).unwrap();

        assert_eq!(report.moved.len(), 2);
        assert!(report.left.is_empty(), "{:?}", report.left);
        assert_eq!(snapshot(root), root_before, "the frozen root is untouched");
        let v2 = repo.at_generation("v2").unwrap();
        assert!(v2
            .package_dir("NA-CANADA-WEST", PackageType::Ortho)
            .is_dir());
        assert_eq!(fs::read(root.join("v2/coverage.png")).unwrap(), b"v2 map");
        assert!(
            !root.join(PACKAGES_DIR).exists(),
            "the root working folders are empty now"
        );
        assert_eq!(
            Repository::open(root).unwrap().generation(),
            "v1",
            "importing does not switch"
        );
    }
}
