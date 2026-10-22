//! Replacing or removing a package folder by renaming it aside first.
//!
//! A rename is atomic on one filesystem, so a package folder is always
//! either the old version or the new one, never neither, whenever the
//! process stops. The new version is first staged beside the install path
//! (the temp directory may be on another filesystem, so staging may copy),
//! then swapped in with two renames in the same parent. The slow recursive
//! delete happens after the rename.
//!
//! Aside and staging names start with a dot, so `LocalPackageStore::list`
//! skips them. A killed process can leave either behind, and the queue's
//! startup sweep settles them: a staging folder is always deleted; an aside
//! whose package folder is absent (killed between the two renames) is renamed
//! back, since it is then the only copy; an aside whose package folder exists
//! is deleted.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::error::{ManagerError, ManagerResult};

pub(crate) const ASIDE_PREFIX: &str = ".xel-aside-";
pub(crate) const STAGING_PREFIX: &str = ".xel-staging-";

/// Where `path` is moved while it is being replaced or removed.
pub(crate) fn aside_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{ASIDE_PREFIX}{name}"))
}

/// Where a new version of `path` is staged before it is swapped in.
pub(crate) fn staging_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{STAGING_PREFIX}{name}"))
}

/// A package staged beside its install path, not yet swapped in.
///
/// Dropping it without swapping (a cancelled install, an error) deletes the
/// staging folder, best effort. After a successful rename the folder is gone
/// and the drop does nothing.
#[derive(Debug)]
pub(crate) struct Staged {
    path: PathBuf,
}

impl Staged {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.path.exists() {
            discard(&self.path);
        }
    }
}

/// True for the names of folders that exist only mid-replacement.
pub(crate) fn is_transient(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with(ASIDE_PREFIX) || name.starts_with(STAGING_PREFIX)
}

/// Rename `path` aside if it exists, returning where it went. An aside left
/// by an earlier run is deleted first.
pub(crate) fn set_aside(path: &Path) -> ManagerResult<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let aside = aside_path(path);
    if aside.exists() {
        discard(&aside);
    }
    fs::rename(path, &aside).map_err(|e| ManagerError::WriteFailed {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(Some(aside))
}

/// Undo a failed replacement: drop whatever reached `path` and rename the
/// aside back. Best effort; a failure is logged and the sweep finishes it.
pub(crate) fn restore(path: &Path, aside: Option<&Path>) {
    if path.exists() {
        if let Err(e) = fs::remove_dir_all(path) {
            warn!(path = %path.display(), error = %e, "could not remove a partial package");
        }
    }
    if let Some(aside) = aside {
        if let Err(e) = fs::rename(aside, path) {
            warn!(path = %path.display(), error = %e, "could not restore the previous package version");
        }
    }
}

/// Delete an aside folder. Best effort: the startup sweep retries.
pub(crate) fn discard(aside: &Path) {
    if let Err(e) = fs::remove_dir_all(aside) {
        warn!(path = %aside.display(), error = %e, "could not delete a replaced package");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn folder_with(path: &Path, file: &str, content: &str) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join(file), content).unwrap();
    }

    #[test]
    fn set_aside_on_a_missing_path_returns_none() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("zzXEL_eu_ortho");

        assert_eq!(set_aside(&path).unwrap(), None);
    }

    #[test]
    fn set_aside_replaces_a_stale_aside() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("zzXEL_eu_ortho");
        folder_with(&path, "current.txt", "current");
        folder_with(&aside_path(&path), "stale.txt", "stale");

        let aside = set_aside(&path).unwrap().unwrap();

        assert_eq!(aside, aside_path(&path));
        assert!(!path.exists());
        assert!(aside.join("current.txt").exists());
        assert!(!aside.join("stale.txt").exists());
    }

    #[test]
    fn restore_removes_a_partial_tree_and_renames_the_aside_back() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("zzXEL_eu_ortho");
        let aside = aside_path(&path);
        folder_with(&aside, "old.txt", "old");
        folder_with(&path, "partial.txt", "partial");

        restore(&path, Some(&aside));

        assert_eq!(fs::read_to_string(path.join("old.txt")).unwrap(), "old");
        assert!(!path.join("partial.txt").exists());
        assert!(!aside.exists());
    }

    #[test]
    fn restore_without_an_aside_removes_the_partial_tree() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("zzXEL_eu_ortho");
        folder_with(&path, "partial.txt", "partial");

        restore(&path, None);

        assert!(!path.exists());
        assert!(!aside_path(&path).exists());
    }

    #[test]
    fn transient_names_are_both_prefixes_and_not_packages() {
        assert!(is_transient(OsStr::new(".xel-aside-zzXEL_eu_ortho")));
        assert!(is_transient(OsStr::new(".xel-staging-zzXEL_eu_ortho")));
        assert!(!is_transient(OsStr::new("zzXEL_eu_ortho")));
        assert!(is_transient(
            aside_path(Path::new("/x/zzXEL_eu_ortho"))
                .file_name()
                .unwrap()
        ));
        assert!(is_transient(
            staging_path(Path::new("/x/zzXEL_eu_ortho"))
                .file_name()
                .unwrap()
        ));
    }
}
