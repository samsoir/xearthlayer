//! RAII cleanup of an install's temporary directory.
//!
//! [`InstallTempGuard`] wraps the per-install temp directory created by
//! `PackageInstaller::install_from_metadata`. On drop, the directory is
//! recursively removed. This catches cleanup on every exit path:
//!
//! - Early `Err` from a download failure (issue #187: previously, the
//!   temp directory leaked on hard download failures).
//! - Mid-install errors from extraction, checksum mismatch, or move.
//! - The success path (the explicit Stage 7 cleanup at end of
//!   `install_from_metadata` is no longer needed and is removed in
//!   the same change).
//!
//! Cleanup is unconditional and idempotent: removing an already-deleted
//! directory is silently ignored.

use std::path::PathBuf;

/// RAII guard that wipes a directory tree when dropped.
///
/// Construct after the temp directory has been created; let it drop
/// when the install scope exits.
pub struct InstallTempGuard {
    path: PathBuf,
}

impl InstallTempGuard {
    /// Construct a guard for `path`. The path is removed (recursively)
    /// when the guard is dropped.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for InstallTempGuard {
    fn drop(&mut self) {
        // Best-effort recursive cleanup; ignore errors because this
        // runs on the unwinding path where surfacing a fresh error
        // would mask the original failure.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn drop_removes_the_directory_tree() {
        let temp = TempDir::new().unwrap();
        let install_temp = temp.path().join("install_eu_ortho_0.1.1");
        fs::create_dir_all(install_temp.join("nested/parts")).unwrap();
        fs::write(install_temp.join("nested/parts/part.aa"), b"junk").unwrap();
        assert!(install_temp.exists());

        {
            let _guard = InstallTempGuard::new(install_temp.clone());
            // guard dropped at end of this block
        }

        assert!(!install_temp.exists(), "guard drop must wipe the tree");
    }

    #[test]
    fn drop_is_silent_when_path_does_not_exist() {
        let temp = TempDir::new().unwrap();
        let nonexistent = temp.path().join("never-created");

        // Just constructing and dropping must not panic when the path
        // is absent (e.g., earlier failure removed it already).
        let _guard = InstallTempGuard::new(nonexistent);
    }

    #[test]
    fn drop_is_silent_when_already_partially_removed() {
        // Simulates: an Err path that already cleaned a subdirectory,
        // then the guard runs to clean the parent.
        let temp = TempDir::new().unwrap();
        let install_temp = temp.path().join("install_eu");
        fs::create_dir_all(&install_temp).unwrap();
        fs::write(install_temp.join("a"), b"x").unwrap();

        // Pre-emptively remove a child to simulate prior cleanup.
        fs::remove_file(install_temp.join("a")).unwrap();

        let _guard = InstallTempGuard::new(install_temp.clone());
        drop(_guard);

        assert!(!install_temp.exists());
    }
}
