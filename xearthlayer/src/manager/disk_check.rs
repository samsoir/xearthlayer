//! Pre-flight disk-space check for package installs.
//!
//! An install batch reads the free space once, after every package's parts
//! are sized and before any bytes hit the network, so users get a clear
//! failure up front (#188). The install queue's `DiskLedger` decides from
//! that figure when each package may download.
//!
//! The provider trait exists so tests can inject low-available stubs;
//! production wires `RealFsInfoProvider`, which delegates to
//! `crate::system::fs_info`.

use std::io;
use std::path::Path;

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
/// Peak usage is at most the downloaded parts plus the extracted tree.
/// Extraction streams the parts and deletes each once it has been read, so
/// the two never hold more than that together.
///
/// **Scenery archives are not incompressible.** The earlier value of 2
/// assumed they were, on the reasoning that BCn texture data does not
/// compress. The payload does not, but a package is millions of small
/// files and gzip compresses the redundancy in tar headers, padding and
/// the `terrain` and `Earth nav data` text files alongside the textures.
/// Measured across every published region, extraction expands the
/// download by 1.49x to 2.12x, so parts plus extracted runs from 2.49x
/// to 3.12x. Every region exceeded 2x and the smallest regions expand
/// most: `af1` is the worst at 3.12x.
///
/// 4 covers the measured worst case with margin. It is deliberately
/// crude and errs high, because refusing an install costs a user a
/// retry while running out of space part way through extraction costs
/// them the whole download. The accurate fix is for the package
/// metadata to carry its uncompressed size so this constant is not
/// needed at all; see issue #260.
pub const SPACE_BUFFER_MULTIPLIER: u64 = 4;

/// Bytes available to an unprivileged user on the filesystem holding `path`.
pub fn available_bytes(provider: &dyn FsInfoProvider, path: &Path) -> ManagerResult<u64> {
    provider
        .fs_info(path)
        .map(|info| info.available_bytes)
        .map_err(|e| ManagerError::InvalidPath(format!("statvfs {}: {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
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
    fn reports_the_bytes_available_on_the_filesystem_holding_the_path() {
        let provider = StubProvider::new(100, 49);

        assert_eq!(available_bytes(&provider, Path::new("/tmp")).unwrap(), 49);
        assert_eq!(
            provider.last_path.lock().unwrap().as_deref(),
            Some(Path::new("/tmp"))
        );
    }

    #[test]
    fn returns_invalid_path_when_provider_errors() {
        struct ErrorProvider;
        impl FsInfoProvider for ErrorProvider {
            fn fs_info(&self, _path: &Path) -> io::Result<FilesystemInfo> {
                Err(io::Error::other("statvfs failed"))
            }
        }
        match available_bytes(&ErrorProvider, Path::new("/tmp")) {
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
