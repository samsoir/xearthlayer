//! Manual smoke test for the macFUSE mount path.
//!
//! Exercises the live fuse3 backend end-to-end: kernel → macFUSE → fuse3 →
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
//! Ignored by default because it needs macFUSE installed and the kext loaded.
//! Run with: `cargo test --test macfuse_smoke -- --ignored --nocapture`

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use xearthlayer::executor::ChannelDdsClient;
use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::runtime::JobRequest;

/// Mount the *production* consolidated ortho union FS over macFUSE and read a
/// real passthrough scenery file back through it. This is the FS X-Plane
/// actually talks to, exercising lookup → getattr → open → read → readdir →
/// release on the live code path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macFUSE installed and kext loaded"]
async fn ortho_union_fs_mounts_and_serves_a_file_through_macfuse() {
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

    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let dds_client = Arc::new(ChannelDdsClient::new(tx));

    let fs = Fuse3OrthoUnionFS::new(index, dds_client, 11_174_016);
    let handle = fs
        .mount_spawned(&mount_str)
        .await
        .expect("ortho union mount_spawned must succeed on macFUSE");

    tokio::time::sleep(Duration::from_millis(500)).await;

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
}
