//! Compatibility between the in-process extractor and archives built by the
//! system tools the publisher still uses (#274).
//!
//! The publisher builds packages with `tar -czf` and splits them with
//! `split -b`. These tests build fixtures the same way and check that the
//! client's in-process extractor reproduces the source tree exactly.
//!
//! Run with: `cargo test --test package_archive_compat`

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use xearthlayer::manager::{ArchiveExtractor, CancelToken, TarGzExtractor};

/// A package-shaped tree: nested directories, a name with spaces (as in
/// `Earth nav data`), and binary content.
fn write_package(root: &Path) -> PathBuf {
    let pkg = root.join("zzXEL_test_ortho");
    let files: &[(&str, &[u8])] = &[
        ("Earth nav data/+40-080/+45-075.dsf", b"DSF bytes"),
        ("terrain/12345_67890_ZL16.ter", b"A\n800\nTERRAIN\n"),
        (
            "textures/12345_67890_ZL16.dds",
            &[0x44, 0x44, 0x53, 0x20, 0, 255, 7],
        ),
        (
            "xearthlayer_scenery_package.txt",
            b"REGIONAL SCENERY PACKAGE\n",
        ),
    ];
    for (name, data) in files {
        let path = pkg.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    pkg
}

/// Every regular file under `root`, keyed by its path relative to `root`.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Archive `pkg` the way the publisher does: `tar -czf` run from its parent.
fn system_tar_gz(pkg: &Path, archive: &Path) {
    let status = Command::new("tar")
        // macOS bsdtar otherwise stores extended attributes as AppleDouble
        // `._name` entries, files the source tree does not have. GNU tar
        // ignores the variable.
        .env("COPYFILE_DISABLE", "1")
        .current_dir(pkg.parent().unwrap())
        .arg("-czf")
        .arg(archive)
        .arg(pkg.file_name().unwrap())
        .status()
        .expect("system tar is available");
    assert!(status.success(), "tar -czf failed");
}

#[test]
fn an_archive_built_by_system_tar_extracts_to_the_same_tree() {
    let temp = TempDir::new().unwrap();
    let pkg = write_package(&temp.path().join("src"));
    let archive = temp.path().join("pkg.tar.gz");
    system_tar_gz(&pkg, &archive);
    let dest = temp.path().join("dest");

    let files = TarGzExtractor::new().extract(&archive, &dest).unwrap();

    let expected = tree(pkg.parent().unwrap());
    assert_eq!(files, expected.len());
    assert_eq!(tree(&dest), expected);
}

#[test]
fn split_parts_extract_to_the_source_tree() {
    let temp = TempDir::new().unwrap();
    let pkg = write_package(&temp.path().join("src"));
    let parts_dir = temp.path().join("parts");
    fs::create_dir_all(&parts_dir).unwrap();
    let archive = parts_dir.join("pkg.tar.gz");
    system_tar_gz(&pkg, &archive);

    // Split as the publisher does, small enough to force several parts.
    let status = Command::new("split")
        .current_dir(&parts_dir)
        .args(["-b", "64", "pkg.tar.gz", "pkg.tar.gz."])
        .status()
        .expect("system split is available");
    assert!(status.success(), "split failed");
    fs::remove_file(&archive).unwrap();
    let mut parts: Vec<PathBuf> = fs::read_dir(&parts_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    parts.sort();
    assert!(parts.len() > 2, "fixture should span several parts");

    let dest = temp.path().join("dest");
    TarGzExtractor::new()
        .extract_parts(&parts, &dest, &CancelToken::new())
        .unwrap();

    assert_eq!(
        fs::read_dir(&parts_dir).unwrap().count(),
        0,
        "every part is deleted once read"
    );
    assert_eq!(tree(&dest), tree(pkg.parent().unwrap()));
}
