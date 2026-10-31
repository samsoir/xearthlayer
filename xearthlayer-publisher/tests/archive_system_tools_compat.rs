//! Compatibility between the in-process archive builder and the system
//! tools it replaced (#274).
//!
//! Packages used to be built with `tar -czf` and cut with `split -b`, and
//! users with older clients still extract them with system `tar`. These tests
//! hold the in-process builder to both tools.
//!
//! Run with: `cargo test --test archive_system_tools_compat`

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use rand::Rng;
use semver::Version;
use tempfile::TempDir;
use xearthlayer_package::PackageType;
use xearthlayer_publisher::{build_archive, ArchiveBuildResult, RepoConfig};

/// The smallest part size a repository accepts.
const PART_SIZE: u64 = 10 * 1024 * 1024;

/// A package tree with nested directories, a name with spaces (as in
/// `Earth nav data`) and `random_mib` MiB of incompressible data, so the
/// archive spans several parts.
fn write_package(root: &Path, random_mib: usize) -> PathBuf {
    let pkg = root.join("zzXEL_na_ortho");
    let files: [(&str, &[u8]); 2] = [
        ("Earth nav data/+40-080/+45-075.dsf", b"DSF bytes"),
        ("terrain/12345_67890_ZL16.ter", b"A\n800\nTERRAIN\n"),
    ];
    for (name, data) in files {
        let path = pkg.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    let textures = pkg.join("textures");
    fs::create_dir_all(&textures).unwrap();
    let mut file = fs::File::create(textures.join("water_mask.png")).unwrap();
    let mut chunk = vec![0u8; 1024 * 1024];
    let mut rng = rand::rng();
    for _ in 0..random_mib {
        rng.fill(&mut chunk[..]);
        file.write_all(&chunk).unwrap();
    }
    pkg
}

fn build(pkg: &Path, dist: &Path) -> ArchiveBuildResult {
    build_archive(
        pkg,
        dist,
        "na",
        PackageType::Ortho,
        &Version::new(1, 0, 0),
        &RepoConfig::new(PART_SIZE).unwrap(),
    )
    .unwrap()
}

/// Concatenate the parts, as a client reassembles them.
fn reassemble(result: &ArchiveBuildResult, archive: &Path) {
    let mut out = fs::File::create(archive).unwrap();
    for part in &result.parts {
        out.write_all(&fs::read(&part.path).unwrap()).unwrap();
    }
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

#[test]
fn an_archive_built_in_process_extracts_with_system_tar() {
    let temp = TempDir::new().unwrap();
    let pkg = write_package(&temp.path().join("src"), 1);
    let result = build(&pkg, &temp.path().join("dist"));
    let archive = temp.path().join("reassembled.tar.gz");
    reassemble(&result, &archive);
    let dest = temp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&dest)
        .status()
        .expect("system tar is available");

    assert!(status.success(), "tar -xzf failed");
    assert_eq!(tree(&dest), tree(pkg.parent().unwrap()));
}

#[test]
fn parts_are_cut_exactly_where_split_cuts_them() {
    let temp = TempDir::new().unwrap();
    let pkg = write_package(&temp.path().join("src"), 25);
    let result = build(&pkg, &temp.path().join("dist"));
    assert!(result.parts.len() > 2, "fixture should span several parts");

    // Cut the same archive with system split.
    let split_dir = temp.path().join("split");
    fs::create_dir_all(&split_dir).unwrap();
    reassemble(&result, &split_dir.join(&result.archive_name));
    let status = Command::new("split")
        .current_dir(&split_dir)
        .arg("-b")
        .arg(PART_SIZE.to_string())
        .arg(&result.archive_name)
        .arg(format!("{}.", result.archive_name))
        .status()
        .expect("system split is available");
    assert!(status.success(), "split failed");
    fs::remove_file(split_dir.join(&result.archive_name)).unwrap();

    let mut split_parts: Vec<PathBuf> = fs::read_dir(&split_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    split_parts.sort();
    assert_eq!(split_parts.len(), result.parts.len());
    for (ours, theirs) in result.parts.iter().zip(&split_parts) {
        assert_eq!(
            ours.filename,
            theirs.file_name().unwrap().to_string_lossy(),
            "part names differ"
        );
        assert!(
            fs::read(&ours.path).unwrap() == fs::read(theirs).unwrap(),
            "{} differs from split's",
            ours.filename
        );
    }
}
