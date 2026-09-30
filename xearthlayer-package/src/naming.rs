//! Centralized package naming conventions.
//!
//! This module is the single source of truth for all XEarthLayer package naming:
//! - Mountpoint/folder names (e.g., `zzXEL_na_ortho`)
//! - Archive filenames (e.g., `zzXEL_na_ortho-1.0.0.tar.gz`)
//! - Archive part filenames (e.g., `zzXEL_na_ortho-1.0.0.tar.gz.aa`)
//!
//! All other modules should use these functions rather than constructing names directly.
//! This ensures consistency across the publisher and manager components.

use semver::Version;

use super::PackageType;

/// Why a region code was refused.
///
/// Carries the offending code so every caller's message names it without
/// having to re-quote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRegion {
    /// The region code as the caller supplied it.
    pub region: String,
    /// What was wrong with it.
    pub reason: &'static str,
}

impl std::fmt::Display for InvalidRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "region '{}' is invalid: {}", self.region, self.reason)
    }
}

impl std::error::Error for InvalidRegion {}

/// Reject a region code that could escape its parent once joined into a path.
///
/// Every path derived from a region is built by joining it in, and
/// [`package_mountpoint`] prefixes only the FIRST path component it produces,
/// so a `..` segment further along survives untouched. A region of
/// `x/../../../../tmp/evil` therefore yields a mountpoint that resolves outside
/// the directory it was joined to. That directory is the publisher's repository
/// on one side and the user's Custom Scenery folder on the other, and the
/// region reaching the latter arrives in package metadata fetched over HTTP.
///
/// Call this wherever an untrusted region enters, which means the parsers in
/// this crate and the publisher's path-building entry points. Validating at
/// those boundaries covers every downstream consumer, which is why
/// [`package_mountpoint`] itself stays infallible.
///
/// There is no legitimate region code containing a path separator: the code
/// becomes a directory name under Custom Scenery, so rejecting these costs
/// nothing real. A dot pair inside a name, such as `foo..bar`, has no traversal
/// power and is accepted.
pub fn validate_region(region: &str) -> Result<(), InvalidRegion> {
    let reject = |reason| {
        Err(InvalidRegion {
            region: region.to_string(),
            reason,
        })
    };

    if region.is_empty() {
        return reject("a region code must not be empty");
    }

    if region.chars().any(std::path::is_separator) {
        return reject("a region code must not contain a path separator");
    }

    if region == "." || region == ".." {
        return reject("a region code must not be a path traversal segment");
    }

    Ok(())
}

/// Generate the mountpoint (folder) name for a package.
///
/// This is the directory name used in X-Plane's Custom Scenery folder.
/// The naming convention ensures proper load order:
/// - `zz` prefix for ortho (loads last, lowest priority)
/// - `yz` prefix for overlay (loads before ortho)
///
/// # Format
///
/// `{sort_prefix}XEL_{region}_{type_suffix}`
///
/// # Examples
///
/// ```
/// use xearthlayer_package::{PackageType, package_mountpoint};
///
/// assert_eq!(package_mountpoint("na", PackageType::Ortho), "zzXEL_na_ortho");
/// assert_eq!(package_mountpoint("EU", PackageType::Ortho), "zzXEL_eu_ortho");
/// assert_eq!(package_mountpoint("na", PackageType::Overlay), "yzXEL_na_overlay");
/// ```
pub fn package_mountpoint(region: &str, package_type: PackageType) -> String {
    format!(
        "{}XEL_{}_{}",
        package_type.sort_prefix(),
        region.to_lowercase(),
        package_type.folder_suffix()
    )
}

/// Generate the base archive filename for a package.
///
/// This is the filename used for distributable archives (before splitting).
///
/// # Format
///
/// `{sort_prefix}XEL_{region}_{type_suffix}-{version}.tar.gz`
///
/// # Examples
///
/// ```
/// use semver::Version;
/// use xearthlayer_package::{PackageType, archive_filename};
///
/// assert_eq!(
///     archive_filename("na", PackageType::Ortho, &Version::new(1, 0, 0)),
///     "zzXEL_na_ortho-1.0.0.tar.gz"
/// );
/// assert_eq!(
///     archive_filename("eu", PackageType::Overlay, &Version::new(2, 1, 0)),
///     "yzXEL_eu_overlay-2.1.0.tar.gz"
/// );
/// ```
pub fn archive_filename(region: &str, package_type: PackageType, version: &Version) -> String {
    format!(
        "{}-{}.tar.gz",
        package_mountpoint(region, package_type),
        version
    )
}

/// Generate the filename for an archive part.
///
/// Large archives are split into parts with letter suffixes (aa, ab, ac, ...).
///
/// # Format
///
/// `{archive_filename}.{part_suffix}`
///
/// # Examples
///
/// ```
/// use semver::Version;
/// use xearthlayer_package::{PackageType, archive_part_filename};
///
/// assert_eq!(
///     archive_part_filename("na", PackageType::Ortho, &Version::new(1, 0, 0), "aa"),
///     "zzXEL_na_ortho-1.0.0.tar.gz.aa"
/// );
/// ```
pub fn archive_part_filename(
    region: &str,
    package_type: PackageType,
    version: &Version,
    part_suffix: &str,
) -> String {
    format!(
        "{}.{}",
        archive_filename(region, package_type, version),
        part_suffix
    )
}

/// Update the version in an existing archive filename.
///
/// This preserves the package naming while changing the version number.
/// Handles both base filenames and part filenames.
///
/// # Examples
///
/// ```
/// use semver::Version;
/// use xearthlayer_package::update_archive_version;
///
/// assert_eq!(
///     update_archive_version("zzXEL_na_ortho-1.0.0.tar.gz", &Version::new(2, 0, 0)),
///     "zzXEL_na_ortho-2.0.0.tar.gz"
/// );
/// assert_eq!(
///     update_archive_version("zzXEL_na_ortho-1.0.0.tar.gz.aa", &Version::new(2, 0, 0)),
///     "zzXEL_na_ortho-2.0.0.tar.gz.aa"
/// );
/// ```
pub fn update_archive_version(filename: &str, version: &Version) -> String {
    // Find the version pattern in the filename (after last '-' before '.tar')
    if let Some(dash_pos) = filename.rfind('-') {
        if let Some(ext_pos) = filename[dash_pos..].find(".tar") {
            let prefix = &filename[..dash_pos];
            let extension = &filename[dash_pos + ext_pos..];
            return format!("{}-{}{}", prefix, version, extension);
        }
    }
    // Fallback: return original if pattern not found
    filename.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_mountpoint_ortho() {
        assert_eq!(
            package_mountpoint("na", PackageType::Ortho),
            "zzXEL_na_ortho"
        );
        assert_eq!(
            package_mountpoint("eu", PackageType::Ortho),
            "zzXEL_eu_ortho"
        );
        assert_eq!(
            package_mountpoint("sa", PackageType::Ortho),
            "zzXEL_sa_ortho"
        );
    }

    #[test]
    fn test_package_mountpoint_overlay() {
        assert_eq!(
            package_mountpoint("na", PackageType::Overlay),
            "yzXEL_na_overlay"
        );
        assert_eq!(
            package_mountpoint("eu", PackageType::Overlay),
            "yzXEL_eu_overlay"
        );
    }

    #[test]
    fn test_package_mountpoint_normalizes_region() {
        assert_eq!(
            package_mountpoint("NA", PackageType::Ortho),
            "zzXEL_na_ortho"
        );
        assert_eq!(
            package_mountpoint("Eu", PackageType::Ortho),
            "zzXEL_eu_ortho"
        );
    }

    #[test]
    fn test_archive_filename_ortho() {
        assert_eq!(
            archive_filename("na", PackageType::Ortho, &Version::new(1, 0, 0)),
            "zzXEL_na_ortho-1.0.0.tar.gz"
        );
        assert_eq!(
            archive_filename("eu", PackageType::Ortho, &Version::new(2, 1, 3)),
            "zzXEL_eu_ortho-2.1.3.tar.gz"
        );
    }

    #[test]
    fn test_archive_filename_overlay() {
        assert_eq!(
            archive_filename("na", PackageType::Overlay, &Version::new(1, 0, 0)),
            "yzXEL_na_overlay-1.0.0.tar.gz"
        );
    }

    #[test]
    fn test_archive_part_filename() {
        assert_eq!(
            archive_part_filename("na", PackageType::Ortho, &Version::new(1, 0, 0), "aa"),
            "zzXEL_na_ortho-1.0.0.tar.gz.aa"
        );
        assert_eq!(
            archive_part_filename("na", PackageType::Ortho, &Version::new(1, 0, 0), "ab"),
            "zzXEL_na_ortho-1.0.0.tar.gz.ab"
        );
        assert_eq!(
            archive_part_filename("eu", PackageType::Overlay, &Version::new(2, 0, 0), "zz"),
            "yzXEL_eu_overlay-2.0.0.tar.gz.zz"
        );
    }

    #[test]
    fn test_update_archive_version_base() {
        assert_eq!(
            update_archive_version("zzXEL_na_ortho-1.0.0.tar.gz", &Version::new(2, 0, 0)),
            "zzXEL_na_ortho-2.0.0.tar.gz"
        );
        assert_eq!(
            update_archive_version("yzXEL_eu_overlay-1.2.3.tar.gz", &Version::new(1, 3, 0)),
            "yzXEL_eu_overlay-1.3.0.tar.gz"
        );
    }

    #[test]
    fn test_update_archive_version_part() {
        assert_eq!(
            update_archive_version("zzXEL_na_ortho-1.0.0.tar.gz.aa", &Version::new(2, 0, 0)),
            "zzXEL_na_ortho-2.0.0.tar.gz.aa"
        );
        assert_eq!(
            update_archive_version("zzXEL_na_ortho-1.0.0.tar.gz.ab", &Version::new(2, 0, 0)),
            "zzXEL_na_ortho-2.0.0.tar.gz.ab"
        );
    }

    #[test]
    fn test_update_archive_version_fallback() {
        // Should return original if pattern not found
        assert_eq!(
            update_archive_version("invalid_filename.txt", &Version::new(2, 0, 0)),
            "invalid_filename.txt"
        );
    }

    #[test]
    fn test_naming_consistency() {
        // Verify that archive filename starts with mountpoint
        let mountpoint = package_mountpoint("na", PackageType::Ortho);
        let archive = archive_filename("na", PackageType::Ortho, &Version::new(1, 0, 0));
        assert!(archive.starts_with(&mountpoint));
    }

    // ---- region validation ----

    #[test]
    fn a_plain_region_code_is_valid() {
        for code in ["na", "NA", "eu2", "na-usa-mx-central", "AS1"] {
            assert!(
                validate_region(code).is_ok(),
                "{code} is a real region code and must be accepted"
            );
        }
    }

    #[test]
    fn an_empty_region_is_rejected() {
        assert!(validate_region("").is_err());
    }

    #[test]
    fn a_region_containing_a_path_separator_is_rejected() {
        let err = validate_region("x/../../../../tmp/evil")
            .expect_err("a region with a separator must not be accepted");

        assert!(
            err.to_string().contains("x/../../../../tmp/evil"),
            "the error should name the offending region: {err}"
        );
    }

    #[test]
    fn an_absolute_region_is_rejected() {
        assert!(validate_region("/etc").is_err());
    }

    #[test]
    fn a_traversal_segment_is_rejected() {
        assert!(validate_region("..").is_err());
        assert!(validate_region(".").is_err());
    }

    #[test]
    fn a_dot_inside_a_name_is_not_a_traversal_and_is_accepted() {
        assert!(
            validate_region("foo..bar").is_ok(),
            "a dot pair inside a name has no traversal power"
        );
    }
}
