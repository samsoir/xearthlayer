//! Filesystem capacity and host-OS classification helpers.
//!
//! These helpers underpin disk-space-aware diagnostics and pre-flight
//! validation in the package installer. They abstract over `statvfs(3)`
//! and host-class detection so both the diagnostics report and the
//! installer can ask the same questions in the same way.
//!
//! # Atomic / immutable host detection
//!
//! Atomic / ostree-based distros (Bazzite, Silverblue, Kinoite, SteamOS,
//! Fedora Atomic) deploy the rootfs as a content-addressable image sized
//! exactly to its content. `df /` on these systems reports `100%` as the
//! steady state, which is normal and not a problem. Detection here lets
//! the diagnostics report annotate that fact rather than alarm the user.
//!
//! Detection uses the well-known marker file `/run/ostree-booted`, which
//! the ostree systemd unit creates at boot when the rootfs was deployed
//! by ostree. This is the same signal `rpm-ostree` and `bootc` themselves
//! use, so it stays correct as the ecosystem evolves.

use nix::sys::statvfs::statvfs;
use std::io;
use std::path::Path;

/// Marker file created by ostree's systemd unit on hosts where the
/// rootfs is an ostree deployment.
const OSTREE_BOOTED_MARKER: &str = "/run/ostree-booted";

/// Capacity information for a filesystem mounted at or above a given path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemInfo {
    /// Total size of the filesystem in bytes.
    pub total_bytes: u64,
    /// Bytes available to a non-privileged process.
    ///
    /// This is `f_bavail * f_frsize` rather than `f_bfree * f_frsize`,
    /// matching what `df` reports under the `Avail` column. The
    /// distinction matters on filesystems that reserve a fraction of
    /// blocks for root (ext4 default 5%); we always report what the
    /// user can actually use.
    pub available_bytes: u64,
}

/// Query capacity information for the filesystem containing `path`.
///
/// Returns an `io::Error` if `path` doesn't exist or `statvfs(2)` fails.
pub fn fs_info(path: &Path) -> io::Result<FilesystemInfo> {
    let stat = statvfs(path).map_err(|errno| io::Error::from_raw_os_error(errno as i32))?;
    // f_frsize is the fragment (allocation) size; on Linux it equals
    // f_bsize for almost all filesystems, but the standard says capacity
    // calculations should use f_frsize.
    let frsize = stat.fragment_size();
    let total_bytes = stat.blocks().saturating_mul(frsize);
    let available_bytes = stat.blocks_available().saturating_mul(frsize);
    Ok(FilesystemInfo {
        total_bytes,
        available_bytes,
    })
}

/// Returns `true` if the host appears to be an ostree-based atomic
/// distribution (Bazzite, Silverblue, Kinoite, SteamOS, Fedora Atomic,
/// etc.) where the rootfs is intentionally sized to its content.
pub fn is_immutable_os() -> bool {
    Path::new(OSTREE_BOOTED_MARKER).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_info_returns_nonzero_capacity_for_tmp() {
        // /tmp is guaranteed to exist on any test host.
        let info = fs_info(Path::new("/tmp")).expect("statvfs /tmp must succeed");
        assert!(
            info.total_bytes > 0,
            "/tmp filesystem should report nonzero total bytes"
        );
        // Don't assert on available_bytes > 0; CI runners can theoretically
        // run with /tmp full. Just verify the field is populated and sane.
        assert!(
            info.available_bytes <= info.total_bytes,
            "available bytes ({}) must not exceed total ({})",
            info.available_bytes,
            info.total_bytes
        );
    }

    #[test]
    fn fs_info_errors_for_nonexistent_path() {
        let result = fs_info(Path::new("/this/path/does/not/exist/xearthlayer-test"));
        assert!(
            result.is_err(),
            "fs_info on a nonexistent path must return Err, got {result:?}"
        );
    }

    #[test]
    fn is_immutable_os_does_not_panic() {
        // We can't deterministically assert true or false because tests
        // could in theory run on either host class. Just verify the
        // function is callable and returns a bool.
        let _ = is_immutable_os();
    }
}
