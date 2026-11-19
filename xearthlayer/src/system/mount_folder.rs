//! The folder the ortho volume mounts on, and the drive that holds it (#369).
//!
//! macFUSE's FSKit mount is performed by `fskitd`, which refuses a mount
//! folder the caller does not own. Two drive states defeat that check, and
//! both only show up on external drives:
//!
//! - **Ownership ignored** ("Owners: Disabled" in `diskutil info`, "Ignore
//!   ownership on this volume" in Finder). Every file reads as owned by uid 99,
//!   so no folder on the drive can be mounted on.
//! - **A folder created while ownership was ignored.** APFS stores uid 99 on
//!   disk, and `fskitd` keeps refusing the folder after ownership is turned
//!   back on, although `stat` then reports the user. A fresh folder carries
//!   the user's uid, so recreating an empty mount folder clears it.
//!
//! The kext checked neither, so installs that mounted for years can fail on
//! their first FSKit run. The user sees only macFUSE's `os error 35`.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// `MNT_IGNORE_OWNERSHIP` from macOS `<sys/mount.h>` (also spelled
/// `MNT_NOOWNERS`): the filesystem ignores ownership on its objects.
pub const MNT_IGNORE_OWNERSHIP: u64 = 0x0020_0000;

/// Whether `statfs` flags say the filesystem ignores ownership.
pub fn ignores_ownership(f_flags: u64) -> bool {
    f_flags & MNT_IGNORE_OWNERSHIP != 0
}

/// Whether the filesystem holding `path` ignores ownership.
///
/// Always `false` off macOS, where the flag does not exist and no FSKit mount
/// is made.
pub fn volume_ignores_ownership(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `c_path` is NUL terminated and `buf` is a `statfs` sized
        // out-parameter that the call fills on success.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statfs returned 0, so it initialised `buf`.
        let flags = unsafe { buf.assume_init() }.f_flags;
        Ok(ignores_ownership(u64::from(flags)))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(false)
    }
}

/// The drive to name in advice about `path`: `/Volumes/<name>` for a path on
/// an external drive, `None` otherwise.
pub fn external_volume_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match (components.next(), components.next(), components.next()) {
        (Some(std::path::Component::RootDir), Some(volumes), Some(name))
            if volumes.as_os_str() == "Volumes" =>
        {
            Some(Path::new("/Volumes").join(name.as_os_str()))
        }
        _ => None,
    }
}

/// What [`prepare_mount_folder`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountFolder {
    /// The folder did not exist and was created.
    Created,
    /// The folder was empty and was deleted and created again.
    Recreated,
    /// The folder was left as it was: not empty, a mount point, or
    /// recreation was not asked for.
    Kept,
}

/// Make sure the mount folder exists, recreating it when it is empty.
///
/// With `recreate_empty`, an existing folder that is empty and is not itself a
/// mount point is deleted and created again, so it carries the current user's
/// ownership (see the module docs). `remove_dir` only deletes an empty folder,
/// so a folder that gains a file between the check and the delete is kept.
/// A folder holding anything is never touched.
pub fn prepare_mount_folder(path: &Path, recreate_empty: bool) -> io::Result<MountFolder> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        return Ok(MountFolder::Created);
    }
    if !recreate_empty || is_mount_point(path) || !is_empty_dir(path) {
        return Ok(MountFolder::Kept);
    }
    match std::fs::remove_dir(path) {
        Ok(()) => {
            std::fs::create_dir(path)?;
            Ok(MountFolder::Recreated)
        }
        // Not empty after all, or not ours to delete: mount on it as it is.
        Err(_) => Ok(MountFolder::Kept),
    }
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// A folder on a different device from its parent is a mount point.
fn is_mount_point(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(folder), Ok(parent)) => folder.dev() != parent.dev(),
        // Unreadable: leave it alone.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn the_ignore_ownership_flag_is_read_from_statfs_flags() {
        assert!(ignores_ownership(MNT_IGNORE_OWNERSHIP));
        assert!(ignores_ownership(MNT_IGNORE_OWNERSHIP | 0x1));
        assert!(!ignores_ownership(0x1 | 0x0010_0000));
        assert!(!ignores_ownership(0));
    }

    #[test]
    fn the_temp_volume_does_not_ignore_ownership() {
        // On macOS this runs the real statfs call: the temp folder is on the
        // system volume, which always honours ownership.
        let dir = tempfile::tempdir().unwrap();
        assert!(!volume_ignores_ownership(dir.path()).unwrap());
    }

    #[test]
    fn a_missing_path_is_an_error_on_macos() {
        let result = volume_ignores_ownership(Path::new("/definitely/not/here"));
        assert_eq!(result.is_err(), cfg!(target_os = "macos"));
    }

    #[test]
    fn the_external_volume_root_is_the_folder_under_volumes() {
        assert_eq!(
            external_volume_root(Path::new(
                "/Volumes/Davinci Resolve Media/X-Plane 12/Custom Scenery"
            )),
            Some(PathBuf::from("/Volumes/Davinci Resolve Media"))
        );
        assert_eq!(
            external_volume_root(Path::new("/Users/sam/X-Plane 12/Custom Scenery")),
            None
        );
        assert_eq!(external_volume_root(Path::new("/Volumes")), None);
        assert_eq!(external_volume_root(Path::new("Volumes/x/y")), None);
    }

    #[test]
    fn a_missing_mount_folder_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("zzXEL_ortho");
        assert_eq!(
            prepare_mount_folder(&folder, true).unwrap(),
            MountFolder::Created
        );
        assert!(folder.is_dir());
    }

    #[test]
    fn an_empty_mount_folder_is_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("zzXEL_ortho");
        std::fs::create_dir(&folder).unwrap();
        // Mark the old folder: owner read and execute only. A folder made by
        // create_dir always has owner write (no sane umask removes it), so the
        // bit tells the new folder from the old one.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).unwrap();

        assert_eq!(
            prepare_mount_folder(&folder, true).unwrap(),
            MountFolder::Recreated
        );
        let mode = std::fs::metadata(&folder).unwrap().mode();
        assert!(folder.is_dir());
        assert_ne!(mode & 0o200, 0, "expected a new folder, got mode {mode:o}");
    }

    #[test]
    fn a_mount_folder_holding_anything_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("zzXEL_ortho");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep.txt"), b"x").unwrap();

        assert_eq!(
            prepare_mount_folder(&folder, true).unwrap(),
            MountFolder::Kept
        );
        assert!(folder.join("keep.txt").exists());
    }

    #[test]
    fn an_empty_mount_folder_is_kept_when_recreation_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("zzXEL_ortho");
        std::fs::create_dir(&folder).unwrap();

        assert_eq!(
            prepare_mount_folder(&folder, false).unwrap(),
            MountFolder::Kept
        );
        assert!(folder.is_dir());
    }

    #[test]
    fn a_folder_on_the_same_device_as_its_parent_is_not_a_mount_point() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("zzXEL_ortho");
        std::fs::create_dir(&folder).unwrap();
        assert!(!is_mount_point(&folder));
    }
}
