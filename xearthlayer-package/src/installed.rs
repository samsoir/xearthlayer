//! Installed package type with filesystem context.
//!
//! The [`InstalledPackage`] struct extends [`Package`] with installation-specific
//! information using composition.

use std::ops::Deref;
use std::path::{Path, PathBuf};

use super::core::Package;

/// An installed scenery package.
///
/// Uses composition to extend [`Package`] with installation context:
/// - Filesystem path where the package is installed
/// - Enabled state for the UI toggle feature
///
/// # Composition Pattern
///
/// `InstalledPackage` contains a `Package` rather than inheriting from it.
/// The [`Deref`] implementation allows transparent access to `Package` fields.
///
/// # Example
///
/// ```
/// use semver::Version;
/// use xearthlayer_package::{Package, InstalledPackage, PackageType};
///
/// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
/// let installed = InstalledPackage::new(package, "/path/to/na_ortho");
///
/// // Access Package fields via Deref
/// assert_eq!(installed.region, "na");
/// assert!(installed.is_ortho());
///
/// // Access InstalledPackage fields directly
/// assert!(installed.enabled);
/// assert_eq!(installed.path.to_str().unwrap(), "/path/to/na_ortho");
/// ```
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    /// Core package identity (composition).
    pub package: Package,

    /// Filesystem path to the installed package directory.
    pub path: PathBuf,

    /// Whether this package is enabled for mounting.
    ///
    /// Disabled packages are not mounted by XEarthLayer. This allows
    /// users to temporarily disable packages via the UI without uninstalling.
    pub enabled: bool,
}

impl InstalledPackage {
    /// Create a new enabled installed package.
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, InstalledPackage, PackageType};
    ///
    /// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let installed = InstalledPackage::new(package, "/path/to/package");
    ///
    /// assert!(installed.enabled);
    /// ```
    pub fn new(package: Package, path: impl Into<PathBuf>) -> Self {
        Self {
            package,
            path: path.into(),
            enabled: true,
        }
    }

    /// Create a disabled installed package.
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, InstalledPackage, PackageType};
    ///
    /// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let installed = InstalledPackage::new_disabled(package, "/path/to/package");
    ///
    /// assert!(!installed.enabled);
    /// ```
    pub fn new_disabled(package: Package, path: impl Into<PathBuf>) -> Self {
        Self {
            package,
            path: path.into(),
            enabled: false,
        }
    }

    /// Set the enabled state (builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use semver::Version;
    /// use xearthlayer_package::{Package, InstalledPackage, PackageType};
    ///
    /// let package = Package::new("na", PackageType::Ortho, Version::new(1, 0, 0));
    /// let installed = InstalledPackage::new(package, "/path")
    ///     .with_enabled(false);
    ///
    /// assert!(!installed.enabled);
    /// ```
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Get the path to the installed package.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Check if the package is enabled for mounting.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable this package.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

/// Deref to Package for convenient access to base fields.
///
/// This allows `installed.region` instead of `installed.package.region`.
impl Deref for InstalledPackage {
    type Target = Package;

    fn deref(&self) -> &Self::Target {
        &self.package
    }
}

/// Convert InstalledPackage back to Package (drops installation context).
impl From<InstalledPackage> for Package {
    fn from(installed: InstalledPackage) -> Self {
        installed.package
    }
}

/// Convert a reference to InstalledPackage to a reference to Package.
impl AsRef<Package> for InstalledPackage {
    fn as_ref(&self) -> &Package {
        &self.package
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;
    use crate::PackageType;

    fn test_package() -> Package {
        Package::new("na", PackageType::Ortho, Version::new(1, 0, 0))
    }

    #[test]
    fn test_installed_package_new() {
        let installed = InstalledPackage::new(test_package(), "/test/path");

        assert_eq!(installed.package.region, "na");
        assert_eq!(installed.path, PathBuf::from("/test/path"));
        assert!(installed.enabled);
    }

    #[test]
    fn test_installed_package_new_disabled() {
        let installed = InstalledPackage::new_disabled(test_package(), "/test/path");

        assert!(!installed.enabled);
    }

    #[test]
    fn test_installed_package_with_enabled() {
        let installed = InstalledPackage::new(test_package(), "/test/path").with_enabled(false);

        assert!(!installed.enabled);
    }

    #[test]
    fn test_installed_package_set_enabled() {
        let mut installed = InstalledPackage::new(test_package(), "/test/path");
        assert!(installed.enabled);

        installed.set_enabled(false);
        assert!(!installed.enabled);

        installed.set_enabled(true);
        assert!(installed.enabled);
    }

    #[test]
    fn test_installed_package_deref() {
        let installed = InstalledPackage::new(test_package(), "/test/path");

        // Access Package fields via Deref
        assert_eq!(installed.region, "na");
        assert!(installed.is_ortho());
        assert_eq!(installed.folder_name(), "zzXEL_na_ortho");
    }

    #[test]
    fn test_installed_package_into_package() {
        let installed = InstalledPackage::new(test_package(), "/test/path");
        let package: Package = installed.into();

        assert_eq!(package.region, "na");
        assert_eq!(package.package_type, PackageType::Ortho);
    }

    #[test]
    fn test_installed_package_as_ref() {
        let installed = InstalledPackage::new(test_package(), "/test/path");
        let package_ref: &Package = installed.as_ref();

        assert_eq!(package_ref.region, "na");
    }

    #[test]
    fn test_installed_package_path_methods() {
        let installed = InstalledPackage::new(test_package(), "/test/path");

        assert_eq!(installed.path(), Path::new("/test/path"));
        assert!(installed.is_enabled());
    }

    #[test]
    fn test_installed_package_clone() {
        let installed = InstalledPackage::new(test_package(), "/test/path");
        let cloned = installed.clone();

        assert_eq!(installed.package, cloned.package);
        assert_eq!(installed.path, cloned.path);
        assert_eq!(installed.enabled, cloned.enabled);
    }

    #[test]
    fn test_installed_package_debug() {
        let installed = InstalledPackage::new(test_package(), "/test/path");
        let debug = format!("{:?}", installed);

        assert!(debug.contains("InstalledPackage"));
        assert!(debug.contains("na"));
        assert!(debug.contains("/test/path"));
    }
}
