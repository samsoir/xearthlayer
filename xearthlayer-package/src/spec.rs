//! Specification version policy for the package library and metadata formats.
//!
//! Both `xearthlayer_package_library.txt` and `xearthlayer_scenery_package.txt`
//! carry a semantic specification version on line 2. Semantic versioning as
//! documented in `docs/dev/scenery-packages.md`: a MAJOR bump means an
//! incompatible layout, a MINOR or PATCH bump does not. So the parser refuses
//! a major it does not know and accepts anything at or below the one it does.
//!
//! The field was in the format from the start; enforcement was deferred until
//! a format change was on the horizon. It is here so that when a version 2
//! is published, a client that predates it fails with an instruction rather
//! than by misreading the file.

use semver::Version;

/// The specification version this build writes.
pub const CURRENT_SPEC_VERSION: &str = "1.0.0";

/// The highest specification major this build can read.
pub const SUPPORTED_SPEC_MAJOR: u64 = 1;

/// True if a file at `version` can be read by this build.
pub fn is_supported_spec_major(version: &Version) -> bool {
    version.major <= SUPPORTED_SPEC_MAJOR
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

    #[test]
    fn current_spec_version_is_within_supported_major() {
        let current = Version::parse(CURRENT_SPEC_VERSION).unwrap();
        assert_eq!(current.major, SUPPORTED_SPEC_MAJOR);
    }

    #[test]
    fn same_major_any_minor_or_patch_is_supported() {
        assert!(is_supported_spec_major(&Version::new(1, 0, 0)));
        assert!(is_supported_spec_major(&Version::new(1, 9, 3)));
    }

    #[test]
    fn lower_major_is_supported() {
        assert!(is_supported_spec_major(&Version::new(0, 9, 0)));
    }

    #[test]
    fn higher_major_is_refused() {
        assert!(!is_supported_spec_major(&Version::new(2, 0, 0)));
        assert!(!is_supported_spec_major(&Version::new(3, 1, 0)));
    }
}
