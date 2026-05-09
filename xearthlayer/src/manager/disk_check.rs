//! Pre-flight disk-space check for package installs.
//!
//! Runs after `query_sizes()` (when expected part totals are known) and
//! before `download_all()` (so users get a clear failure before any
//! bytes hit the network). See issue #188 for the motivating user
//! report on Bazzite.
//!
//! The provider trait exists so tests can inject low-available stubs;
//! production wires `RealFsInfoProvider`, which delegates to
//! `crate::system::fs_info`.

use std::io;
use std::path::{Path, PathBuf};

use super::error::{ManagerError, ManagerResult};
use crate::system::FilesystemInfo;

/// Abstraction over `statvfs(2)` so callers can stub it in tests.
///
/// Send + Sync because the installer holds a `Box<dyn FsInfoProvider>`
/// that may be moved across thread boundaries during parallel work.
pub trait FsInfoProvider: Send + Sync {
    fn fs_info(&self, path: &Path) -> io::Result<FilesystemInfo>;
}

/// Production implementation backed by `crate::system::fs_info`.
pub struct RealFsInfoProvider;

impl FsInfoProvider for RealFsInfoProvider {
    fn fs_info(&self, path: &Path) -> io::Result<FilesystemInfo> {
        crate::system::fs_info(path)
    }
}

/// Multiplier applied to the sum of part sizes to estimate peak
/// on-disk footprint during install.
///
/// Peak usage = downloaded archive parts (still on disk during
/// extraction) + extracted contents. For our scenery archives the
/// contents are mostly already-compressed BCn DDS textures with low
/// tar.gz compression ratios, so a 2× multiplier covers
/// "compressed + extracted" with a small margin.
pub const SPACE_BUFFER_MULTIPLIER: u64 = 2;

/// Verify that `path` has at least `required_bytes` available, returning
/// `ManagerError::InsufficientDiskSpace` otherwise.
///
/// `path` is what gets `statvfs`'d directly; if it doesn't exist, the
/// caller should pass an existing ancestor (the temp dir is created
/// before this check runs in the installer flow, so this isn't an
/// issue in production).
pub fn check_disk_space(
    provider: &dyn FsInfoProvider,
    path: &Path,
    required_bytes: u64,
) -> ManagerResult<()> {
    let info = provider
        .fs_info(path)
        .map_err(|e| ManagerError::InvalidPath(format!("statvfs {}: {}", path.display(), e)))?;
    if info.available_bytes < required_bytes {
        return Err(ManagerError::InsufficientDiskSpace {
            path: PathBuf::from(path),
            required_bytes,
            available_bytes: info.available_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stub provider returning a fixed FilesystemInfo regardless of path.
    struct StubProvider {
        info: FilesystemInfo,
        last_path: Mutex<Option<PathBuf>>,
    }

    impl StubProvider {
        fn new(total: u64, available: u64) -> Self {
            Self {
                info: FilesystemInfo {
                    total_bytes: total,
                    available_bytes: available,
                },
                last_path: Mutex::new(None),
            }
        }
    }

    impl FsInfoProvider for StubProvider {
        fn fs_info(&self, path: &Path) -> io::Result<FilesystemInfo> {
            *self.last_path.lock().unwrap() = Some(path.to_path_buf());
            Ok(self.info)
        }
    }

    #[test]
    fn returns_ok_when_available_meets_required() {
        let provider = StubProvider::new(100 * 1024 * 1024 * 1024, 50 * 1024 * 1024 * 1024);
        let result = check_disk_space(&provider, Path::new("/tmp"), 10 * 1024 * 1024 * 1024);
        assert!(result.is_ok(), "got {result:?}");
    }

    #[test]
    fn returns_ok_when_available_exactly_equals_required() {
        // Boundary: available == required is success (`<` not `<=`).
        let provider = StubProvider::new(100, 50);
        assert!(check_disk_space(&provider, Path::new("/tmp"), 50).is_ok());
    }

    #[test]
    fn returns_insufficient_disk_space_when_available_below_required() {
        let provider = StubProvider::new(100, 49);
        let result = check_disk_space(&provider, Path::new("/tmp"), 50);
        match result {
            Err(ManagerError::InsufficientDiskSpace {
                path,
                required_bytes,
                available_bytes,
            }) => {
                assert_eq!(path, Path::new("/tmp"));
                assert_eq!(required_bytes, 50);
                assert_eq!(available_bytes, 49);
            }
            other => panic!("expected InsufficientDiskSpace, got {other:?}"),
        }
    }

    #[test]
    fn returns_invalid_path_when_provider_errors() {
        struct ErrorProvider;
        impl FsInfoProvider for ErrorProvider {
            fn fs_info(&self, _path: &Path) -> io::Result<FilesystemInfo> {
                Err(io::Error::other("statvfs failed"))
            }
        }
        let result = check_disk_space(&ErrorProvider, Path::new("/tmp"), 100);
        match result {
            Err(ManagerError::InvalidPath(msg)) => {
                assert!(msg.contains("statvfs"), "got: {msg}");
            }
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[test]
    fn insufficient_disk_space_display_includes_human_readable_sizes() {
        let err = ManagerError::InsufficientDiskSpace {
            path: PathBuf::from("/var/home/user/.xearthlayer/packages"),
            required_bytes: 8_500_000_000,
            available_bytes: 2_100_000_000,
        };
        let display = err.to_string();
        assert!(display.contains("insufficient disk space"));
        assert!(
            display.contains("/var/home/user/.xearthlayer/packages"),
            "got: {display}"
        );
        // Human-readable not raw bytes.
        assert!(
            !display.contains("8500000000"),
            "should be human-readable: {display}"
        );
    }
}
