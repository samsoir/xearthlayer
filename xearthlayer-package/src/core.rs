//! Core package identity type.
//!
//! The [`Package`] struct represents the essential identity of a scenery package,
//! shared across all contexts: publisher, package manager, and runtime.

use std::fmt;

use semver::Version;

use super::naming::package_mountpoint;
use super::types::PackageType;

/// Core package identity.
///
/// This is the base type representing a scenery package. It contains
/// the essential identifying information used across all contexts:
/// - **Publisher**: creating and versioning packages
/// - **Package Manager**: downloading and installing
/// - **Runtime**: mounting and serving to X-Plane
/// - **Library Index**: listing available packages
///
/// # Example
///
/// ```
/// use semver::Version;
/// use xearthlayer_package::{Package, PackageType};
///
/// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
///
/// assert_eq!(package.region, "na");
/// assert!(package.is_ortho());
/// assert_eq!(package.folder_name(), "zzXEL_na_ortho");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Region code (e.g., "na", "eu", "sa").
    ///
    /// Region codes are always stored in lowercase.
    pub region: String,

    /// Package type (Ortho or Overlay).
    pub package_type: PackageType,

    /// Package version using semantic versioning.
    pub version: Version,
}

impl Package {
    /// Create a new package.
    ///
    /// The region code is normalized to lowercase.
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, PackageType};
    ///
    /// let package = Package::new("NA", PackageType::Ortho, Version::new(1, 0, 0));
    /// assert_eq!(package.region, "na"); // Normalized to lowercase
    /// ```
    pub fn new(region: impl Into<String>, package_type: PackageType, version: Version) -> Self {
        Self {
            region: region.into().to_lowercase(),
            package_type,
            version,
        }
    }

    /// Check if this is an ortho package.
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, PackageType};
    ///
    /// let ortho = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let overlay = Package::new("na", PackageType::Overlay, Version::new(1, 0, 0));
    ///
    /// assert!(ortho.is_ortho());
    /// assert!(!overlay.is_ortho());
    /// ```
    pub fn is_ortho(&self) -> bool {
        self.package_type == PackageType::Ortho
    }

    /// Check if this is an overlay package.
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, PackageType};
    ///
    /// let ortho = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let overlay = Package::new("na", PackageType::Overlay, Version::new(1, 0, 0));
    ///
    /// assert!(!ortho.is_overlay());
    /// assert!(overlay.is_overlay());
    /// ```
    pub fn is_overlay(&self) -> bool {
        self.package_type == PackageType::Overlay
    }

    /// Get the canonical folder name for this package.
    ///
    /// This is the directory name used in X-Plane's Custom Scenery folder.
    /// Format: `{sort_prefix}XEL_{region}_{type_suffix}`
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, PackageType};
    ///
    /// let ortho = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let overlay = Package::new("eu", PackageType::Overlay, Version::new(1, 0, 0));
    ///
    /// assert_eq!(ortho.folder_name(), "zzXEL_na_ortho");
    /// assert_eq!(overlay.folder_name(), "yzXEL_eu_overlay");
    /// ```
    pub fn folder_name(&self) -> String {
        package_mountpoint(&self.region, self.package_type)
    }
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} v{}", self.region, self.package_type, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_new() {
        let pkg = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));

        assert_eq!(pkg.region, "na");
        assert_eq!(pkg.package_type, PackageType::Ortho);
        assert_eq!(pkg.version, Version::new(1, 0, 0));
    }

    #[test]
    fn test_package_normalizes_region() {
        let pkg = Package::new("NA", PackageType::Ortho, Version::new(1, 0, 0));
        assert_eq!(pkg.region, "na");

        let pkg2 = Package::new("Eu", PackageType::Overlay, Version::new(1, 0, 0));
        assert_eq!(pkg2.region, "eu");
    }

    #[test]
    fn test_package_is_ortho() {
        let ortho = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
        let overlay = Package::new("na", PackageType::Overlay, Version::new(1, 0, 0));

        assert!(ortho.is_ortho());
        assert!(!ortho.is_overlay());
        assert!(!overlay.is_ortho());
        assert!(overlay.is_overlay());
    }

    #[test]
    fn test_package_folder_name() {
        let ortho = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
        let overlay = Package::new("eu", PackageType::Overlay, Version::new(2, 0, 0));

        assert_eq!(ortho.folder_name(), "zzXEL_na_ortho");
        assert_eq!(overlay.folder_name(), "yzXEL_eu_overlay");
    }

    #[test]
    fn test_package_display() {
        let pkg = Package::new("na", PackageType::Ortho, Version::new(1, 2, 3));
        assert_eq!(format!("{}", pkg), "na ortho v1.2.3");
    }

    #[test]
    fn test_package_equality() {
        let pkg1 = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
        let pkg2 = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
        let pkg3 = Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0));
        let pkg4 = Package::new("na", PackageType::Ortho, Version::new(2, 0, 0));

        assert_eq!(pkg1, pkg2);
        assert_ne!(pkg1, pkg3); // Different region
        assert_ne!(pkg1, pkg4); // Different version
    }

    #[test]
    fn test_package_clone() {
        let pkg = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
        let cloned = pkg.clone();

        assert_eq!(pkg, cloned);
    }
}
