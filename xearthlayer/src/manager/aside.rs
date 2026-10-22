//! Replacing or removing a package folder by renaming it aside first.
//!
//! A rename is atomic on one filesystem, so a package folder is always
//! either the old version or the new one, never neither, whenever the
//! process stops. The slow recursive delete happens after the rename. The
//! aside name starts with a dot, so `LocalPackageStore::list` skips it, and
//! the queue's startup sweep deletes any left by a killed process.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::error::{ManagerError, ManagerResult};

pub(crate) const ASIDE_PREFIX: &str = ".xel-aside-";

/// Where `path` is moved while it is being replaced or removed.
pub(crate) fn aside_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{ASIDE_PREFIX}{name}"))
}

pub(crate) fn is_aside(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with(ASIDE_PREFIX)
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
