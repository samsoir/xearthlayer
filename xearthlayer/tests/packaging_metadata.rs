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

fn arch_pkgbuild() -> String {
    let path = repo_root().join("pkg/arch/PKGBUILD");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
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
