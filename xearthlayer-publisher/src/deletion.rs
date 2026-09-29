//! Deleting a regional package from a publisher repository.
//!
//! Inspection is split from action: [`plan_deletion`] is pure and answers what
//! would go, [`execute_deletion`] acts on that answer. A dry run is the
//! inspection alone, so it cannot describe something different from what the
//! real run does.
//!
//! Deletion never touches GitHub releases. The publisher has no concept of
//! them, and the assets must outlive the index entry while users on the stable
//! channel are still installing from them.

use std::fs;
use std::path::PathBuf;

use super::library::LibraryManager;
use super::{directory_size, PublishError, PublishResult, Repository};
use xearthlayer_package::PackageType;

/// What deleting one package would remove.
///
/// Produced by [`plan_deletion`], which touches nothing. `bytes_freed` is
/// apparent size, so it reads a little under `du`.
#[derive(Debug, Clone)]
pub struct DeletionPlan {
    /// Region code as the caller gave it.
    pub region: String,

    /// Which package of that region.
    pub package_type: PackageType,

    /// Whether the library index currently advertises this package.
    pub in_library: bool,

    /// The package working directory, if it exists.
    pub package_dir: Option<PathBuf>,

    /// The dist directory holding the archive parts, if it exists.
    pub dist_dir: Option<PathBuf>,

    /// Total apparent size of the directories above.
    pub bytes_freed: u64,
}

/// Reject a region code that could escape the repository root once it is
/// joined into a path.
///
/// Both `package_dir` and `dist_dir` are built by joining the region straight
/// into a path, and `package_mountpoint` only prefixes the first path
/// component it produces: a `..` segment further along survives untouched.
/// `plan_deletion` is the last point in the library that sees the raw region
/// before those paths are built and, eventually, handed to a recursive
/// delete, so it must refuse anything that is not a plain directory name.
///
/// There is no legitimate region code containing a path separator: the code
/// becomes a directory name under Custom Scenery, so rejecting these costs
/// nothing real.
fn validate_region(region: &str) -> PublishResult<()> {
    if region.is_empty() {
        return Err(PublishError::InvalidPath(format!(
            "region '{region}' is invalid: a region code must not be empty"
        )));
    }

    if region.chars().any(std::path::is_separator) {
        return Err(PublishError::InvalidPath(format!(
            "region '{region}' is invalid: a region code must not contain a path separator"
        )));
    }

    if region == "." || region == ".." {
        return Err(PublishError::InvalidPath(format!(
            "region '{region}' is invalid: a region code must not be a path traversal segment"
        )));
    }

    Ok(())
}

/// Work out what deleting a package would remove. Changes nothing.
///
/// A region with no index entry, no package directory and no archives is an
/// error: there is nothing to delete, and the most likely cause is a mistyped
/// region. A package that was built but never released is not an error, because
/// reclaiming its archives is one of the reasons this command exists.
pub fn plan_deletion(
    repo: &Repository,
    region: &str,
    package_type: PackageType,
) -> PublishResult<DeletionPlan> {
    validate_region(region)?;

    let in_library = LibraryManager::open_or_create(repo.root())?.contains(region, package_type);

    let package_dir = repo.package_dir(region, package_type);
    let package_dir = package_dir.is_dir().then_some(package_dir);

    let dist_dir = repo
        .dist_dir()
        .join(region.to_lowercase())
        .join(package_type.folder_suffix());
    let dist_dir = dist_dir.is_dir().then_some(dist_dir);

    if !in_library && package_dir.is_none() && dist_dir.is_none() {
        return Err(PublishError::PackageNotFound {
            region: region.to_string(),
            package_type: package_type.code().to_string(),
        });
    }

    let mut bytes_freed = 0u64;
    for dir in [package_dir.as_ref(), dist_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        bytes_freed = bytes_freed.saturating_add(directory_size(dir)?);
    }

    Ok(DeletionPlan {
        region: region.to_string(),
        package_type,
        in_library,
        package_dir,
        dist_dir,
        bytes_freed,
    })
}

/// Carry out a plan produced by [`plan_deletion`].
///
/// The index entry goes first. If a directory removal then fails, the package
/// is already unadvertised, which is the safe half to have completed: users are
/// not offered a package whose archives are being removed.
pub fn execute_deletion(repo: &Repository, plan: &DeletionPlan) -> PublishResult<()> {
    if plan.in_library {
        let mut library = LibraryManager::open_or_create(repo.root())?;
        library.remove(&plan.region, plan.package_type);
        library.save()?;
    }

    for dir in [plan.package_dir.as_ref(), plan.dist_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        fs::remove_dir_all(dir).map_err(|source| PublishError::WriteFailed {
            path: dir.clone(),
            source,
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A repository with a package directory, a dist directory and a library
    /// entry for `na` ortho.
    ///
    /// The metadata file `add_or_update` checksums lives outside both
    /// counted directories, so it does not perturb `bytes_freed`.
    fn repo_with_released_package() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let package_dir = repo.package_dir("na", PackageType::Ortho);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("payload.bin"), vec![0u8; 512]).unwrap();

        let dist = repo.dist_dir().join("na").join("ortho");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("archive.tar.gz.aa"), vec![0u8; 1024]).unwrap();

        let metadata_path = temp.path().join("meta_source.txt");
        std::fs::write(&metadata_path, "metadata").unwrap();
        let mut library = LibraryManager::open_or_create(repo.root()).unwrap();
        library
            .add_or_update(
                &metadata_path,
                "NA",
                PackageType::Ortho,
                semver::Version::new(1, 0, 0),
                "https://example.com/na/ortho/meta.txt",
            )
            .unwrap();
        library.save().unwrap();

        (temp, repo)
    }

    #[test]
    fn planning_reports_both_directories_and_the_bytes_they_hold() {
        let (_temp, repo) = repo_with_released_package();

        let plan = plan_deletion(&repo, "na", PackageType::Ortho).unwrap();

        assert!(plan.in_library, "the fixture registers a library entry");
        assert!(plan.package_dir.is_some());
        assert!(plan.dist_dir.is_some());
        assert_eq!(plan.bytes_freed, 512 + 1024);
    }

    #[test]
    fn planning_changes_nothing_on_disk() {
        let (_temp, repo) = repo_with_released_package();
        let package_dir = repo.package_dir("na", PackageType::Ortho);

        plan_deletion(&repo, "na", PackageType::Ortho).unwrap();

        assert!(package_dir.exists(), "planning must not delete anything");
    }

    #[test]
    fn planning_a_region_with_nothing_to_delete_is_an_error_naming_it() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let err = plan_deletion(&repo, "zz-typo", PackageType::Ortho)
            .expect_err("a region with no package, no archives and no index entry must error");

        assert!(
            err.to_string().to_lowercase().contains("zz-typo"),
            "the error should name the region: {err}"
        );
    }

    #[test]
    fn planning_rejects_a_region_with_a_path_traversal_segment() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let region = "x/../../../../tmp/evil";
        let err = plan_deletion(&repo, region, PackageType::Ortho)
            .expect_err("a region containing a path separator must be rejected");

        assert!(
            err.to_string().contains(region),
            "the error should name the offending region: {err}"
        );
    }

    #[test]
    fn planning_does_not_touch_a_directory_outside_the_repository_root() {
        let outer = TempDir::new().unwrap();
        let repo_root = outer.path().join("repo");
        let repo = Repository::init(&repo_root).unwrap();

        // Two levels up from `repo/dist` lands on `outer`, then back down
        // into `victim/ortho`: exactly where an unvalidated dist_dir join
        // would resolve for this region.
        let victim_dir = outer.path().join("victim").join("ortho");
        std::fs::create_dir_all(&victim_dir).unwrap();
        std::fs::write(victim_dir.join("do-not-delete.bin"), b"precious").unwrap();

        let region = "../../victim";
        let err = plan_deletion(&repo, region, PackageType::Ortho)
            .expect_err("a region escaping the repository root must be rejected");
        assert!(
            err.to_string().contains(region),
            "the error should name the offending region: {err}"
        );

        assert!(
            victim_dir.join("do-not-delete.bin").exists(),
            "plan_deletion must never touch a path outside the repository root"
        );
    }

    #[test]
    fn planning_accepts_a_region_code_with_hyphens() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let region = "na-usa-mx-central";

        let package_dir = repo.package_dir(region, PackageType::Ortho);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("payload.bin"), vec![0u8; 4]).unwrap();

        let plan = plan_deletion(&repo, region, PackageType::Ortho)
            .expect("a hyphenated region code is a valid region code");

        assert!(plan.package_dir.is_some());
    }

    #[test]
    fn planning_a_built_but_never_released_package_succeeds() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let dist = repo.dist_dir().join("na").join("ortho");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("archive.tar.gz.aa"), vec![0u8; 8]).unwrap();

        let plan = plan_deletion(&repo, "na", PackageType::Ortho).unwrap();

        assert!(!plan.in_library);
        assert!(plan.dist_dir.is_some());
        assert!(plan.package_dir.is_none());
    }

    #[test]
    fn executing_removes_both_directories() {
        let (_temp, repo) = repo_with_released_package();
        let plan = plan_deletion(&repo, "na", PackageType::Ortho).unwrap();
        assert!(plan.in_library, "the fixture registers a library entry");

        execute_deletion(&repo, &plan).unwrap();

        assert!(!repo.package_dir("na", PackageType::Ortho).exists());
        assert!(!repo.dist_dir().join("na").join("ortho").exists());

        let library = LibraryManager::open_or_create(repo.root()).unwrap();
        assert!(
            !library.contains("na", PackageType::Ortho),
            "execute_deletion must remove the library index entry"
        );
    }

    #[test]
    fn executing_leaves_the_other_package_type_alone() {
        let (_temp, repo) = repo_with_released_package();
        let overlay_dir = repo.package_dir("na", PackageType::Overlay);
        std::fs::create_dir_all(&overlay_dir).unwrap();
        std::fs::write(overlay_dir.join("keep.bin"), b"keep").unwrap();

        let plan = plan_deletion(&repo, "na", PackageType::Ortho).unwrap();
        execute_deletion(&repo, &plan).unwrap();

        assert!(
            overlay_dir.exists(),
            "deleting ortho must not touch overlay"
        );
    }
}
