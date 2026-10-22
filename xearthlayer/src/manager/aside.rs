//! Replacing or removing a package folder by renaming it first.
//!
//! A rename is atomic on one filesystem, so a package folder is always
//! either the old version or the new one, never neither, whenever the
//! process stops. The new version is first staged beside the install path
//! (the temp directory may be on another filesystem, so staging may copy),
//! then swapped in with two renames in the same parent: the old version is
//! renamed aside, then the staged one into place. The slow recursive delete
//! of the old version happens after both renames.
//!
//! A remove renames the package straight to its removed name and deletes it
//! there. That delete can take minutes, and a tree it was part way through
//! is never a package again, so a removed folder is never renamed back.
//!
//! Aside, staging and removed names start with a dot, so
//! `LocalPackageStore::list` skips them. A killed process can leave any of
//! them behind, and the queue's startup sweep settles them: a staging or
//! removed folder is always deleted; an aside whose package folder is absent
//! (killed between a swap's two renames) is renamed back, since it is then
//! the only copy; an aside whose package folder exists is deleted.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::error::{ManagerError, ManagerResult};

pub(crate) const ASIDE_PREFIX: &str = ".xel-aside-";
pub(crate) const STAGING_PREFIX: &str = ".xel-staging-";
pub(crate) const REMOVED_PREFIX: &str = ".xel-removed-";

/// Where `path` is moved while it is being replaced.
pub(crate) fn aside_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{ASIDE_PREFIX}{name}"))
}

/// Where a new version of `path` is staged before it is swapped in.
pub(crate) fn staging_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{STAGING_PREFIX}{name}"))
}

/// Where `path` is moved to be deleted when it is removed.
pub(crate) fn removed_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{REMOVED_PREFIX}{name}"))
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
    [ASIDE_PREFIX, STAGING_PREFIX, REMOVED_PREFIX]
        .iter()
        .any(|prefix| name.starts_with(prefix))
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

/// Rename the package at `path` to its removed name, ready to delete. A
/// removed folder left by an earlier run is deleted first, since a rename
/// cannot replace a folder that is not empty. The rename's error is returned
/// as is, so the caller can tell a package that has gone from a failure.
pub(crate) fn set_removed(path: &Path) -> std::io::Result<PathBuf> {
    let removed = removed_path(path);
    if removed.exists() {
        discard(&removed);
    }
    fs::rename(path, &removed)?;
    Ok(removed)
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
            warn!(
                path = %path.display(),
                aside = %aside.display(),
                error = %e,
                "could not restore the previous package version; it stays on disk at the aside path"
            );
        }
    }
}

/// Delete a set-aside, staging or removed folder. Best effort: the startup
/// sweep retries.
pub(crate) fn discard(folder: &Path) {
    if let Err(e) = fs::remove_dir_all(folder) {
        warn!(path = %folder.display(), error = %e, "could not delete a replaced or removed package");
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
    fn set_removed_replaces_a_stale_removed_folder() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("zzXEL_eu_ortho");
        folder_with(&path, "current.txt", "current");
        folder_with(&removed_path(&path), "stale.txt", "stale");

        let removed = set_removed(&path).unwrap();

        assert_eq!(removed, removed_path(&path));
        assert!(!path.exists());
        assert!(removed.join("current.txt").exists());
        assert!(!removed.join("stale.txt").exists());
    }

    #[test]
    fn set_removed_on_a_missing_path_is_not_found() {
        let temp = TempDir::new().unwrap();
        let error = set_removed(&temp.path().join("zzXEL_eu_ortho")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn transient_names_are_every_prefix_and_not_packages() {
        assert!(is_transient(OsStr::new(".xel-aside-zzXEL_eu_ortho")));
        assert!(is_transient(OsStr::new(".xel-staging-zzXEL_eu_ortho")));
        assert!(is_transient(OsStr::new(".xel-removed-zzXEL_eu_ortho")));
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
        assert!(is_transient(
            removed_path(Path::new("/x/zzXEL_eu_ortho"))
                .file_name()
                .unwrap()
        ));
    }
}
