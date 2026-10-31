//! Platform-abstracted FUSE mount-table operations.
//!
//! Linux unmounts FUSE filesystems with `fusermount3`/`fusermount` and exposes
//! the mount table as `/proc/mounts`, while macOS (macFUSE) uses the BSD
//! `umount(8)` and `mount(8)`. Everything that needs to unmount a mount or ask
//! "is this path mounted?" shares the same platform logic, so it lives here
//! once: the panic handler, the spawned-mount Drop fallback, and the package
//! manager's status display.

use std::path::Path;
use std::process::Command;

use tracing::{debug, warn};

/// Unmount the FUSE filesystem at `mountpoint`.
///
/// `force` requests the most aggressive unmount the platform offers — Linux's
/// lazy `-uz` (detach now, clean up when idle) or macOS's `-f` (force). It is
/// used as an escalation when a graceful unmount fails because the mount is
/// still busy.
///
/// Returns `true` if the mountpoint ended up unmounted, including the benign
/// case where it was already not mounted.
pub fn unmount_fuse(mountpoint: &Path, force: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        // macFUSE unmounts via BSD umount(8). This helper is only reached as a
        // fallback from the panic handler and the Drop path, where the daemon
        // may be wedged and we just want the mount gone — so we always force
        // (`-f`) to detach immediately rather than risk blocking. The normal,
        // clean teardown goes through the async `MountHandle::unmount`, not
        // this path; `force` is therefore advisory on macOS.
        let _ = force;
        run_unmount(Command::new("umount").arg("-f").arg(mountpoint), mountpoint)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let flag = if force { "-uz" } else { "-u" };
        // Prefer fusermount3 (libfuse3), fall back to fusermount (libfuse2).
        let mut cmd = Command::new("fusermount3");
        cmd.arg(flag).arg(mountpoint);
        if run_unmount(&mut cmd, mountpoint) {
            return true;
        }
        let mut fallback = Command::new("fusermount");
        fallback.arg(flag).arg(mountpoint);
        run_unmount(&mut fallback, mountpoint)
    }
}

/// Whether `path` is currently a mountpoint, per the OS mount table.
///
/// Platform-specific because the mount table lives in different places: Linux
/// exposes it as `/proc/mounts`; macOS has no such file and must be queried via
/// `mount(8)`. A wrong answer here is not cosmetic — callers gate real `umount`
/// escalation (`SpawnedMountHandle`) and the user-facing package mount status on
/// it, so a Linux-only implementation silently mis-reports every macOS mount.
///
/// This answers "is the kernel mount-table entry present", which is also `true`
/// for a wedged macFUSE mount (daemon dead, entry lingering) — exactly the
/// zombie the unmount fallback must still act on. Use [`is_stale_fuse_mount`] to
/// distinguish a healthy mount from a wedged one.
pub fn is_path_mounted(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    #[cfg(target_os = "macos")]
    {
        match Command::new("/sbin/mount").output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                mount_output_contains(&stdout, &path_str)
            }
            Err(_) => false,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        match std::fs::read_to_string("/proc/mounts") {
            Ok(mounts) => proc_mounts_contains(&mounts, &path_str),
            Err(_) => false,
        }
    }
}

/// Whether `/proc/mounts` content lists `path` as a mountpoint.
///
/// Pure function (no I/O) so the parsing is unit-testable. Each line is
/// `device mountpoint fstype opts ...`; we match field 2.
#[cfg(not(target_os = "macos"))]
fn proc_mounts_contains(mounts: &str, path: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next(); // device
        fields.next() == Some(path)
    })
}

/// Whether BSD `mount(8)` output lists `path` as a mountpoint.
///
/// Pure function (no I/O) so the parsing is unit-testable. Lines look like
/// `device on /mount/point (fstype, opts...)`; the mountpoint sits between
/// " on " and the trailing " (". Splitting on the last " (" keeps mountpoints
/// that contain spaces (e.g. `X-Plane 12`) intact.
#[cfg(target_os = "macos")]
fn mount_output_contains(output: &str, path: &str) -> bool {
    output.lines().any(|line| {
        line.split_once(" on ")
            .and_then(|(_, rest)| rest.rsplit_once(" (").map(|(mp, _)| mp))
            .map(|mp| mp == path)
            .unwrap_or(false)
    })
}

/// Detect a stale FUSE mount at `mountpoint`.
///
/// A FUSE mount whose daemon died without unmounting (crash, SIGKILL,
/// system sleep) stays in the kernel mount table, but every operation on
/// it fails — on macOS/macFUSE with `ENXIO` ("Device not configured"),
/// on Linux with `ENOTCONN` ("Transport endpoint is not connected").
/// Mounting over such a corpse fails, so it must be cleared first.
///
/// A healthy mount, an ordinary directory, or a missing path are all
/// reported as not stale.
///
/// Detection uses `opendir(2)` (via `read_dir`), not `stat(2)`: on macOS
/// the kernel serves cached attributes for a dead macFUSE mountpoint, so
/// `stat` succeeds while any real operation — including reading the
/// directory — fails with `ENXIO` (verified empirically against a killed
/// mount).
pub fn is_stale_fuse_mount(mountpoint: &Path) -> bool {
    match std::fs::read_dir(mountpoint) {
        Ok(_) => false,
        Err(e) => is_stale_mount_error(&e),
    }
}

/// Classify an I/O error from `stat(2)` on a mountpoint as "stale FUSE
/// mount" or not. Extracted as a pure function for testability.
fn is_stale_mount_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENXIO) | Some(libc::ENOTCONN)
    )
}

/// Detect and clear a stale FUSE mount at `mountpoint`.
///
/// Returns `true` if a stale mount was found and successfully unmounted,
/// `false` if the mountpoint was healthy (nothing to do). Logs a warning
/// if a stale mount is found but cannot be cleared.
pub fn recover_stale_fuse_mount(mountpoint: &Path) -> bool {
    if !is_stale_fuse_mount(mountpoint) {
        return false;
    }
    warn!(
        mountpoint = %mountpoint.display(),
        "Stale FUSE mount detected from a previous run; force-unmounting"
    );
    let cleared = unmount_fuse(mountpoint, true);
    if !cleared {
        warn!(
            mountpoint = %mountpoint.display(),
            "Could not clear stale FUSE mount; mounting will likely fail. \
             Unmount manually with: umount <mountpoint>"
        );
    }
    cleared
}

/// Run one unmount command, treating "already unmounted" as success.
fn run_unmount(cmd: &mut Command, mountpoint: &Path) -> bool {
    match cmd.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // An already-detached mount is the outcome we want, so a "not
            // mounted"/"not currently mounted" error still counts as success.
            if stderr.contains("not mounted")
                || stderr.contains("not currently mounted")
                || stderr.contains("not found")
            {
                true
            } else {
                debug!(
                    mountpoint = %mountpoint.display(),
                    stderr = %stderr,
                    "unmount command failed"
                );
                false
            }
        }
        Err(e) => {
            warn!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "failed to run unmount command"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmounting a path that isn't a mountpoint must be treated as success
    /// (the desired end state — nothing mounted there — already holds). This
    /// also exercises the real platform unmount binary and its stderr parsing
    /// without needing a live FUSE mount, so it is safe in CI.
    #[test]
    fn unmount_of_non_mountpoint_reports_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            unmount_fuse(dir.path(), false),
            "non-mountpoint should be reported as already unmounted"
        );
    }

    /// The stale-mount errno classification: ENXIO (macFUSE) and ENOTCONN
    /// (Linux FUSE) mean a dead mount; anything else does not. A real dead
    /// mount cannot be created in CI, so the policy is tested via the pure
    /// classifier.
    #[test]
    fn stale_mount_error_classification() {
        let stale_macos = std::io::Error::from_raw_os_error(libc::ENXIO);
        let stale_linux = std::io::Error::from_raw_os_error(libc::ENOTCONN);
        let missing = std::io::Error::from_raw_os_error(libc::ENOENT);
        let denied = std::io::Error::from_raw_os_error(libc::EACCES);

        assert!(is_stale_mount_error(&stale_macos), "ENXIO is stale");
        assert!(is_stale_mount_error(&stale_linux), "ENOTCONN is stale");
        assert!(!is_stale_mount_error(&missing), "ENOENT is not stale");
        assert!(!is_stale_mount_error(&denied), "EACCES is not stale");
    }

    /// A healthy directory is not a stale mount.
    #[test]
    fn healthy_directory_is_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_stale_fuse_mount(dir.path()));
    }

    /// A missing path is not a stale mount (nothing to recover).
    #[test]
    fn missing_path_is_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        assert!(!is_stale_fuse_mount(&missing));
    }

    /// Recovery on a healthy mountpoint is a no-op that reports false.
    #[test]
    fn recover_on_healthy_mountpoint_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!recover_stale_fuse_mount(dir.path()));
    }

    /// A freshly created directory is not a mountpoint. Exercises the real
    /// platform mount-table query end to end without needing a live mount.
    #[test]
    fn ordinary_directory_is_not_mounted() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_path_mounted(dir.path()));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn proc_mounts_contains_detects_mountpoint() {
        let mounts = "proc /proc proc rw,nosuid 0 0\n\
                      fuse.xel /mnt/zzXEL_ortho fuse rw 0 0\n\
                      /dev/sda1 / ext4 rw 0 0\n";
        assert!(proc_mounts_contains(mounts, "/mnt/zzXEL_ortho"));
        assert!(proc_mounts_contains(mounts, "/"));
        assert!(!proc_mounts_contains(mounts, "/mnt/not_mounted"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_output_contains_detects_macfuse_mountpoint() {
        // Real macFUSE line shape, including a mountpoint that contains spaces.
        let output = "/dev/disk1s1 on / (apfs, local, journaled)\n\
                      xearthlayer@macfuse0 on /Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho (macfuse, nodev, nosuid, synchronous, mounted by me)\n";
        assert!(mount_output_contains(
            output,
            "/Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho"
        ));
        assert!(mount_output_contains(output, "/"));
        assert!(!mount_output_contains(output, "/Users/me/elsewhere"));
    }
}
