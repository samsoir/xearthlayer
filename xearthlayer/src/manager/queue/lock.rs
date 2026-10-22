//! One package operation at a time per temp directory, and cleanup of what
//! a killed process left behind.
//!
//! The lock is a `flock` on a file in the temp directory, held for the
//! queue's life. The kernel drops it when the process exits however it
//! exits, so a stale lock cannot outlive its owner. Holding it is also what
//! makes the sweep safe: no other process can be using the folders it
//! deletes.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tracing::{info, warn};

use crate::manager::aside;
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::installer::INSTALL_TEMP_PREFIX;

pub(crate) const LOCK_FILE_NAME: &str = ".queue.lock";

/// Held while a queue runs. Dropping it releases the lock.
pub(crate) struct QueueLock {
    _lock: Flock<File>,
}

impl QueueLock {
    pub(crate) fn acquire(temp_dir: &Path) -> ManagerResult<Self> {
        fs::create_dir_all(temp_dir).map_err(|e| ManagerError::CreateDirFailed {
            path: temp_dir.to_path_buf(),
            source: e,
        })?;
        let path = temp_dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| ManagerError::WriteFailed {
                path: path.clone(),
                source: e,
            })?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err((_, Errno::EWOULDBLOCK)) => Err(ManagerError::QueueBusy { path }),
            Err((_, errno)) => Err(ManagerError::WriteFailed {
                path,
                source: std::io::Error::from(errno),
            }),
        }
    }
}

/// Clean up after a run that was killed. Call only while holding the lock.
/// Install temp directories and staging folders are deleted. An aside
/// folder is renamed back when its package folder is absent (the run died
/// between the two renames of a swap, so the aside is the only copy) and
/// deleted otherwise. Returns how many leftovers it handled.
pub(crate) fn sweep(temp_dir: &Path, packages_root: &Path) -> usize {
    let mut handled = 0;
    let deletable = entries(temp_dir, |name| name.starts_with(INSTALL_TEMP_PREFIX))
        .chain(entries(packages_root, |name| {
            name.starts_with(aside::STAGING_PREFIX)
        }));
    for path in deletable {
        handled += remove(&path) as usize;
    }
    for aside_dir in entries(packages_root, |name| name.starts_with(aside::ASIDE_PREFIX)) {
        let name = aside_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let package = packages_root.join(&name[aside::ASIDE_PREFIX.len()..]);
        if package.exists() {
            handled += remove(&aside_dir) as usize;
        } else {
            match fs::rename(&aside_dir, &package) {
                Ok(()) => {
                    info!(
                        path = %package.display(),
                        "restored a package an interrupted swap had set aside"
                    );
                    handled += 1;
                }
                Err(e) => warn!(
                    path = %aside_dir.display(),
                    error = %e,
                    "could not restore a set-aside package"
                ),
            }
        }
    }
    handled
}

fn remove(path: &Path) -> bool {
    match fs::remove_dir_all(path) {
        Ok(()) => {
            info!(path = %path.display(), "removed a leftover from an interrupted package operation");
            true
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not remove a leftover");
            false
        }
    }
}

fn entries(dir: &Path, matches: impl Fn(&str) -> bool) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(move |e| e.path().is_dir() && matches(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
}
