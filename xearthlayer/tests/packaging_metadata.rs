//! Consistency checks between the workspace manifest and the packaging files.
//!
//! `pkg/arch/PKGBUILD` is the single source the release tooling templates from,
//! so anything hand-maintained inside it drifts silently: `pkgver` sat at
//! `0.2.0` for fourteen releases, and `options=(!lto)` went missing from a
//! duplicate copy and broke every Arch-family build (issue #222). These tests
//! run under `make pre-commit`, so drift fails before it can ship.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate directory has a parent")
        .to_path_buf()
}

fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn arch_pkgbuild() -> String {
    read_repo_file("pkg/arch/PKGBUILD")
}

fn rpm_spec() -> String {
    read_repo_file("pkg/rpm/xearthlayer.spec")
}

/// Arch forbids `-` in `pkgver`, so a preview version packages as the release
/// it will become: `0.4.7-alpha.3` -> `0.4.7`.
fn release_version(version: &str) -> &str {
    version.split('-').next().unwrap_or(version)
}

fn declared_pkgver(pkgbuild: &str) -> &str {
    pkgbuild
        .lines()
        .find_map(|line| line.strip_prefix("pkgver="))
        .expect("PKGBUILD declares pkgver")
}

#[test]
fn pkgbuild_pkgver_tracks_the_workspace_version() {
    let pkgbuild = arch_pkgbuild();
    let expected = release_version(env!("CARGO_PKG_VERSION"));

    assert_eq!(
        declared_pkgver(&pkgbuild),
        expected,
        "pkg/arch/PKGBUILD pkgver has drifted from the workspace version \
         ({}). Bump both together.",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn pkgbuild_pkgver_is_valid_for_arch() {
    let pkgbuild = arch_pkgbuild();
    let pkgver = declared_pkgver(&pkgbuild);

    assert!(
        !pkgver.contains('-'),
        "pkgver may not contain a hyphen, got {pkgver:?}"
    );
}

#[test]
fn pkgbuild_disables_lto() {
    // Regression guard for #222: Arch ships OPTIONS=(... lto ...), the cc crate
    // picks -flto=auto up from CFLAGS, and ring's C objects then carry no
    // machine code for lld to link.
    let pkgbuild = arch_pkgbuild();

    assert!(
        pkgbuild.lines().any(|line| line.trim() == "options=(!lto)"),
        "pkg/arch/PKGBUILD lost options=(!lto); every Arch-family build will \
         fail to link ring. See issue #222."
    );
}

// The publisher ships as its own package on every channel (#284). These pin
// the split so a packaging edit cannot quietly drop the second binary.

#[test]
fn pkgbuild_splits_the_publish_binary_into_its_own_package() {
    let pkgbuild = arch_pkgbuild();

    assert!(
        pkgbuild.lines().any(|line| line == "pkgbase=xearthlayer"),
        "PKGBUILD must declare pkgbase for a split package"
    );
    assert!(
        pkgbuild
            .lines()
            .any(|line| line == "pkgname=('xearthlayer' 'xearthlayer-publish')"),
        "PKGBUILD must produce both xearthlayer and xearthlayer-publish"
    );
    assert!(
        pkgbuild.contains("package_xearthlayer-publish()"),
        "PKGBUILD needs a package function for xearthlayer-publish"
    );
    assert!(
        pkgbuild.contains(r#"install -Dm755 "target/release/xearthlayer-publish""#),
        "package_xearthlayer-publish must install the binary"
    );
}

#[test]
fn rpm_spec_carries_the_publish_subpackage() {
    let spec = rpm_spec();

    assert!(
        spec.lines()
            .any(|line| line.starts_with("%package") && line.ends_with("publish")),
        "spec must declare the publish subpackage"
    );
    assert!(
        spec.lines()
            .any(|line| line.starts_with("%files") && line.ends_with("publish")),
        "spec must list files for the publish subpackage"
    );
    assert!(
        spec.contains("%{_bindir}/xearthlayer-publish"),
        "the publish subpackage must ship the binary"
    );
}

#[test]
fn rpm_spec_version_tracks_the_workspace_version() {
    let spec = rpm_spec();
    let expected = release_version(env!("CARGO_PKG_VERSION"));
    let declared = spec
        .lines()
        .find_map(|line| line.strip_prefix("Version:"))
        .map(str::trim)
        .expect("spec declares Version:");

    assert_eq!(
        declared, expected,
        "pkg/rpm/xearthlayer.spec Version has drifted from the workspace version"
    );
}

#[test]
fn debian_metadata_packages_the_publish_binary_separately() {
    let manifest = read_repo_file("xearthlayer-publish/Cargo.toml");
    let deb = manifest
        .split("[package.metadata.deb]")
        .nth(1)
        .expect("xearthlayer-publish declares cargo-deb metadata");

    assert!(
        deb.lines()
            .any(|line| line.trim() == r#"name = "xearthlayer-publish""#),
        "the Debian package must be named xearthlayer-publish"
    );
    assert!(
        deb.contains(r#"["target/release/xearthlayer-publish", "usr/bin/", "755"]"#),
        "the Debian package must install the binary"
    );
    assert!(
        !deb.contains("libfuse"),
        "publishing does not mount anything and must not depend on FUSE"
    );
}
