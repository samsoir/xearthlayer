//! Manual smoke test for the macFUSE mount path.
//!
//! Exercises the live fuse3 backend end-to-end: FSKit → macFUSE → fuse3 →
//! `Fuse3OrthoUnionFS`, the filesystem X-Plane actually talks to. Mounts it
//! over a temp directory, reads a file back through the mountpoint, and
//! unmounts.
//!
//! The companion test that mounted `Fuse3PassthroughFS` was dropped when #233
//! deleted that filesystem as unreachable; the union mount is now the only
//! live path.
//!
//! Run this after bumping the pinned fuse3 fork revision or changing the
//! mount/session code.
//!
//! Every mount goes through macFUSE's FSKit backend, the only one
//! XEarthLayer uses on macOS, so the tests need macFUSE 5.3.3+ with
//! "macFUSE (local)" turned on under File System Extensions. Each test also
//! checks the mount table for the `fskit` flag, so it cannot pass on any
//! other transport.
//!
//! Ignored by default because they need macFUSE installed. `make
//! verify-macos` runs them with:
//! `cargo test --test macfuse_smoke -- --ignored --nocapture --test-threads=1`
//!
//! Run them one at a time: one mount at a time, so a failure or a hang is
//! attributable to one test.
//!
//! Run it in a terminal on the Mac itself, not over SSH. macOS treats a
//! macFUSE mount as a network volume and attributes an SSH session to
//! `/usr/libexec/sshd-keygen-wrapper`, which has no permission to open files
//! on one: the mount succeeds and the first read through it fails with
//! `Operation not permitted` (EPERM). The filesystem never returns EPERM
//! itself, so that error always means macOS refused the caller. To run over
//! SSH anyway, give `sshd-keygen-wrapper` Full Disk Access under System
//! Settings > Privacy & Security.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use xearthlayer::executor::ChannelDdsClient;
use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::runtime::JobRequest;
use xearthlayer::system::is_path_mounted;

/// Mount the *production* consolidated ortho union FS over macFUSE and read a
/// real passthrough scenery file back through it. This is the FS X-Plane
/// actually talks to, exercising lookup → getattr → open → read → readdir →
/// release on the live code path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macFUSE 5.3.3+ with its File System Extension on"]
async fn ortho_union_fs_mounts_and_serves_a_file() {
    // A patches dir holding one patch with a known scenery file.
    let patches = tempfile::tempdir().expect("patches tempdir");
    let patch_dir = patches.path().join("TestPatch");
    let dsf_rel = "Earth nav data/+30-120/+33-119.dsf";
    let dsf_contents = b"fake dsf payload for macfuse spike";
    std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
    std::fs::write(patch_dir.join(dsf_rel), dsf_contents).unwrap();
    std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();
    std::fs::write(patch_dir.join("terrain/test.ter"), b"fake terrain").unwrap();

    let index = OrthoUnionIndexBuilder::new()
        .with_patches_dir(patches.path())
        .build()
        .expect("build ortho union index");
    assert!(index.file_count() > 0, "index should have scanned files");

    let mountpoint = tempfile::tempdir().expect("mountpoint tempdir");
    let mount_str = mountpoint.path().to_str().unwrap().to_string();
    let in_table = mount_table_path(mountpoint.path());

    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let dds_client = Arc::new(ChannelDdsClient::new(tx));

    let fs = Fuse3OrthoUnionFS::new(index, dds_client, 11_174_016);
    let handle = fs
        .mount_spawned(&mount_str)
        .await
        .expect("ortho union mount_spawned must succeed on macFUSE");

    wait_for_mount_table(&in_table, true).await;
    let flags = mount_flags(&in_table);

    // Read the real scenery file back through the union mount.
    let mounted = mountpoint.path().join(dsf_rel);
    let read_back = std::fs::read(&mounted).expect("read scenery file through union mount");
    assert_eq!(
        read_back, dsf_contents,
        "file read through ortho union mount must match source"
    );

    // Root listing should expose the patch's top-level directory.
    let names: Vec<String> = std::fs::read_dir(mountpoint.path())
        .expect("readdir union mountpoint")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "Earth nav data"),
        "names: {names:?}"
    );

    handle.unmount().await.expect("union unmount must succeed");
    wait_for_mount_table(&in_table, false).await;
    assert_fskit_mount(&flags);
}

/// #305: Finder shows a mounted volume's name in place of its mount-point
/// folder. Without a `volname` the mount appears in Custom Scenery as
/// `macFUSE Volume 0 (<process>)` rather than `zzXEL_ortho`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macFUSE 5.3.3+ with its File System Extension on"]
async fn ortho_union_fs_volume_is_named_after_its_mount_point_folder() {
    let patches = tempfile::tempdir().expect("patches tempdir");
    std::fs::create_dir_all(patches.path().join("TestPatch/terrain")).unwrap();
    std::fs::write(patches.path().join("TestPatch/terrain/test.ter"), b"t").unwrap();
    let index = OrthoUnionIndexBuilder::new()
        .with_patches_dir(patches.path())
        .build()
        .expect("build ortho union index");

    let scenery = tempfile::tempdir().expect("custom scenery tempdir");
    let mountpoint = scenery.path().join("zzXEL_ortho");
    std::fs::create_dir(&mountpoint).unwrap();
    let in_table = mount_table_path(&mountpoint);

    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let fs = Fuse3OrthoUnionFS::new(index, Arc::new(ChannelDdsClient::new(tx)), 11_174_016);
    let handle = fs
        .mount_spawned(mountpoint.to_str().unwrap())
        .await
        .expect("ortho union mount_spawned must succeed on macFUSE");

    wait_for_mount_table(&in_table, true).await;
    let name = volume_name(&mountpoint);
    let flags = mount_flags(&in_table);
    handle.unmount().await.expect("union unmount must succeed");
    wait_for_mount_table(&in_table, false).await;

    assert_fskit_mount(&flags);
    assert_eq!(name, "zzXEL_ortho");
}

/// Asserts the volume was served through FSKit, from its mount-table flags.
/// An FSKit volume carries the `fskit` flag (macFUSE 5.5.0, macOS 26.6):
///   macfuse://<UUID> on <dir> (macfuse, nodev, nosuid, noowners, noatime,
///   fskit, mounted by <user>)
fn assert_fskit_mount(flags: &[String]) {
    assert!(
        flags.iter().any(|flag| flag == "fskit"),
        "not an FSKit mount: {flags:?}"
    );
}

/// How long macOS may take to list or drop a mount before a test fails.
const MOUNT_TABLE_DEADLINE: Duration = Duration::from_secs(10);

/// The path the mount table names for the mount point `path`: the resolved
/// path (`/private/var/...`, not the `/var/...` a temp dir is handed out as).
/// `is_path_mounted` compares paths exactly, so polling the unresolved path
/// would report "not mounted" at every moment.
fn mount_table_path(path: &Path) -> PathBuf {
    path.canonicalize().expect("canonicalize mount point")
}

/// Waits until the mount table lists `path` (`mounted`) or no longer does.
///
/// fuse3 can return from a mount before the volume is listed in the mount
/// table, and returns from an unmount before its detached unmount task has
/// run. Without the second wait
/// the test runtime could shut down first and leave a dead mount under
/// `$TMPDIR` (#349).
async fn wait_for_mount_table(path: &Path, mounted: bool) {
    let deadline = Instant::now() + MOUNT_TABLE_DEADLINE;
    while is_path_mounted(path) != mounted {
        assert!(
            Instant::now() < deadline,
            "{} still {} the mount table after {:?}",
            path.display(),
            if mounted { "missing from" } else { "in" },
            MOUNT_TABLE_DEADLINE
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The flags `mount` lists for the volume mounted on `dir`, a path as the
/// mount table names it (see `mount_table_path`). Read while the volume is
/// mounted; panics if `dir` is not in the mount table.
fn mount_flags(dir: &Path) -> Vec<String> {
    let needle = format!(" on {} (", dir.display());
    let out = std::process::Command::new("/sbin/mount")
        .output()
        .expect("run mount");
    let table = String::from_utf8_lossy(&out.stdout);
    let line = table
        .lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("{} not in the mount table:\n{table}", dir.display()));
    line.rsplit_once(" (")
        .map(|(_, flags)| flags.trim_end_matches(')'))
        .unwrap_or_default()
        .split(", ")
        .map(str::to_owned)
        .collect()
}

/// The volume name Finder shows for `path`, read with `getattrlist`
/// (`ATTR_VOL_NAME`), which is what Foundation's `volumeNameKey` reads.
fn volume_name(path: &Path) -> String {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_NAME,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // u32 total length, then the attrreference_t, then the name it points at.
    let mut buf = [0u8; 1024];
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&mut request as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
        )
    };
    assert_eq!(rc, 0, "getattrlist: {}", std::io::Error::last_os_error());

    const REF_AT: usize = 4;
    let offset = i32::from_ne_bytes(buf[REF_AT..REF_AT + 4].try_into().unwrap()) as usize;
    let name = &buf[REF_AT + offset..];
    CStr::from_bytes_until_nul(name)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
