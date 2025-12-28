//! X-Plane 12 environment utilities.
//!
//! This module provides utilities for interacting with X-Plane 12 installations,
//! including path detection, resource location, and environment queries.
//!
//! # X-Plane Directory Structure
//!
//! ```text
//! X-Plane 12/
//! ├── Custom Scenery/           # User-installed scenery packs
//! ├── Resources/
//! │   └── default scenery/
//! │       └── default apt dat/
//! │           └── Earth nav data/
//! │               └── apt.dat   # Airport database
//! └── ...
//! ```
//!
//! # Handling Multiple Installations
//!
//! Users may have multiple X-Plane 12 installations (e.g., stable + beta).
//! Use [`XPlaneEnvironment::detect_all()`] to enumerate them:
//!
//! ```ignore
//! use xearthlayer::xplane::XPlaneEnvironment;
//!
//! let installs = XPlaneEnvironment::detect_all();
//! match installs.len() {
//!     0 => println!("No X-Plane 12 found"),
//!     1 => println!("Found: {}", installs[0].installation_path().display()),
//!     n => {
//!         println!("Found {} installations:", n);
//!         for env in &installs {
//!             println!("  - {}", env.installation_path().display());
//!         }
//!     }
//! }
//! ```
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::xplane::XPlaneEnvironment;
//!
//! // Auto-detect X-Plane installation (uses first if multiple exist)
//! let env = XPlaneEnvironment::detect()?;
//! println!("X-Plane 12 at: {}", env.installation_path().display());
//! println!("Custom Scenery: {}", env.custom_scenery_path().display());
//!
//! // Access apt.dat for airport data
//! if let Some(apt_dat) = env.apt_dat_path() {
//!     println!("Airport database: {}", apt_dat.display());
//! }
//! ```

mod detection;
mod paths;

use std::path::{Path, PathBuf};

pub use detection::{
    derive_mountpoint, detect_custom_scenery, detect_scenery_dir, detect_xplane_install,
    detect_xplane_installs, SceneryDetectionResult, XPlanePathError,
};

/// Represents a detected X-Plane 12 installation environment.
///
/// Provides convenient access to various X-Plane directories and resources.
/// Create via [`XPlaneEnvironment::detect()`], [`XPlaneEnvironment::detect_all()`],
/// or [`XPlaneEnvironment::from_path()`].
#[derive(Debug, Clone)]
pub struct XPlaneEnvironment {
    /// Root X-Plane 12 installation directory.
    installation_path: PathBuf,
}

impl XPlaneEnvironment {
    /// Detect X-Plane 12 installation automatically.
    ///
    /// Uses the X-Plane install reference file to find the installation path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No X-Plane 12 installation is found
    /// - Multiple installations are found (user must choose via [`detect_all()`](Self::detect_all))
    ///
    /// # Example
    ///
    /// ```ignore
    /// match XPlaneEnvironment::detect() {
    ///     Ok(env) => println!("Found: {}", env.installation_path().display()),
    ///     Err(XPlanePathError::MultipleInstallations(paths)) => {
    ///         println!("Multiple installations found, please choose:");
    ///         for path in paths {
    ///             println!("  - {}", path.display());
    ///         }
    ///     }
    ///     Err(e) => eprintln!("Error: {}", e),
    /// }
    /// ```
    pub fn detect() -> Result<Self, XPlanePathError> {
        let installations = detect_xplane_installs();

        match installations.len() {
            0 => {
                let reference_path = paths::get_install_reference_path()
                    .unwrap_or_else(|_| PathBuf::from("~/.x-plane/x-plane_install_12.txt"));
                Err(XPlanePathError::InstallFileNotFound(reference_path))
            }
            1 => Ok(Self {
                installation_path: installations.into_iter().next().unwrap(),
            }),
            _ => Err(XPlanePathError::MultipleInstallations(installations)),
        }
    }

    /// Detect all X-Plane 12 installations.
    ///
    /// Returns a vector of all valid X-Plane 12 installations found on the system.
    /// Use this when the user may have multiple installations and needs to choose one.
    ///
    /// # Returns
    ///
    /// A vector of `XPlaneEnvironment` instances, one per valid installation.
    /// Returns an empty vector if no installations are found.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let installs = XPlaneEnvironment::detect_all();
    /// if installs.len() > 1 {
    ///     println!("Multiple X-Plane installations found:");
    ///     for (i, env) in installs.iter().enumerate() {
    ///         println!("  {}. {}", i + 1, env.installation_path().display());
    ///     }
    ///     // Prompt user to choose...
    /// }
    /// ```
    pub fn detect_all() -> Vec<Self> {
        detect_xplane_installs()
            .into_iter()
            .map(|path| Self {
                installation_path: path,
            })
            .collect()
    }

    /// Detect X-Plane installation that contains a specific Custom Scenery path.
    ///
    /// Given a Custom Scenery directory path, finds the X-Plane installation
    /// that contains it. Useful when you know the Custom Scenery path but need
    /// access to other X-Plane resources.
    ///
    /// # Arguments
    ///
    /// * `custom_scenery_path` - Path to a Custom Scenery directory
    ///
    /// # Returns
    ///
    /// The XPlaneEnvironment for the installation containing that Custom Scenery,
    /// or an error if the parent doesn't appear to be a valid X-Plane installation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let custom_scenery = PathBuf::from("/home/user/X-Plane 12/Custom Scenery");
    /// let env = XPlaneEnvironment::from_custom_scenery_path(&custom_scenery)?;
    /// assert!(env.apt_dat_path().is_some());
    /// ```
    pub fn from_custom_scenery_path<P: AsRef<Path>>(
        custom_scenery_path: P,
    ) -> Result<Self, XPlanePathError> {
        let path = custom_scenery_path.as_ref();

        // Custom Scenery is at {X-Plane 12}/Custom Scenery, so parent is the installation
        let installation_path = path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| XPlanePathError::InstallPathNotFound(path.to_path_buf()))?;

        if !installation_path.exists() {
            return Err(XPlanePathError::InstallPathNotFound(installation_path));
        }

        Ok(Self { installation_path })
    }

    /// Create from an explicit X-Plane installation path.
    ///
    /// Use this when you already know the X-Plane path (e.g., from config).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to X-Plane 12 installation directory
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or isn't a valid X-Plane installation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let env = XPlaneEnvironment::from_path("/home/user/X-Plane 12")?;
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, XPlanePathError> {
        let installation_path = path.as_ref().to_path_buf();

        if !installation_path.exists() {
            return Err(XPlanePathError::InstallPathNotFound(installation_path));
        }

        Ok(Self { installation_path })
    }

    /// Get the X-Plane 12 installation root path.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let env = XPlaneEnvironment::detect()?;
    /// // Returns e.g., "/home/user/X-Plane 12"
    /// println!("{}", env.installation_path().display());
    /// ```
    pub fn installation_path(&self) -> &Path {
        &self.installation_path
    }

    /// Get the Custom Scenery directory path.
    ///
    /// Returns `{X-Plane 12}/Custom Scenery`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let env = XPlaneEnvironment::detect()?;
    /// // Returns e.g., "/home/user/X-Plane 12/Custom Scenery"
    /// println!("{}", env.custom_scenery_path().display());
    /// ```
    pub fn custom_scenery_path(&self) -> PathBuf {
        self.installation_path.join("Custom Scenery")
    }

    /// Get the Resources directory path.
    ///
    /// Returns `{X-Plane 12}/Resources`.
    pub fn resources_path(&self) -> PathBuf {
        self.installation_path.join("Resources")
    }

    /// Get the path to apt.dat (airport database).
    ///
    /// Returns `{X-Plane 12}/Resources/default scenery/default apt dat/Earth nav data/apt.dat`
    /// if the file exists, otherwise `None`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let env = XPlaneEnvironment::detect()?;
    /// if let Some(apt_dat) = env.apt_dat_path() {
    ///     let index = AirportIndex::from_file(&apt_dat)?;
    /// }
    /// ```
    pub fn apt_dat_path(&self) -> Option<PathBuf> {
        // X-Plane 12 location: Global Scenery/Global Airports/Earth nav data/apt.dat
        let xp12_path = self
            .installation_path
            .join("Global Scenery")
            .join("Global Airports")
            .join("Earth nav data");

        // Check X-Plane 12 location first
        let xp12_apt = xp12_path.join("apt.dat");
        if xp12_apt.exists() {
            return Some(xp12_apt);
        }

        let xp12_apt_gz = xp12_path.join("apt.dat.gz");
        if xp12_apt_gz.exists() {
            return Some(xp12_apt_gz);
        }

        // Fallback: X-Plane 11 location: Resources/default scenery/default apt dat/Earth nav data/apt.dat
        let xp11_path = self
            .installation_path
            .join("Resources")
            .join("default scenery")
            .join("default apt dat")
            .join("Earth nav data");

        let xp11_apt = xp11_path.join("apt.dat");
        if xp11_apt.exists() {
            return Some(xp11_apt);
        }

        let xp11_apt_gz = xp11_path.join("apt.dat.gz");
        if xp11_apt_gz.exists() {
            return Some(xp11_apt_gz);
        }

        None
    }

    /// Get the path to the Earth nav data directory.
    ///
    /// This directory contains navigation data files like apt.dat, nav.dat, etc.
    /// Returns the X-Plane 12 location if it exists, otherwise falls back to X-Plane 11 location.
    pub fn earth_nav_data_path(&self) -> PathBuf {
        // X-Plane 12 location
        let xp12_path = self
            .installation_path
            .join("Global Scenery")
            .join("Global Airports")
            .join("Earth nav data");

        if xp12_path.exists() {
            return xp12_path;
        }

        // Fallback to X-Plane 11 location
        self.installation_path
            .join("Resources")
            .join("default scenery")
            .join("default apt dat")
            .join("Earth nav data")
    }

    /// Check if Custom Scenery directory exists.
    pub fn has_custom_scenery(&self) -> bool {
        self.custom_scenery_path().exists()
    }

    /// Check if apt.dat exists.
    pub fn has_apt_dat(&self) -> bool {
        self.apt_dat_path().is_some()
    }

    /// Derive a mountpoint path for a scenery pack within Custom Scenery.
    ///
    /// # Arguments
    ///
    /// * `pack_name` - Name of the scenery pack (directory name)
    ///
    /// # Returns
    ///
    /// Full path within Custom Scenery, e.g., `{X-Plane 12}/Custom Scenery/{pack_name}`.
    pub fn mountpoint_for(&self, pack_name: &str) -> PathBuf {
        self.custom_scenery_path().join(pack_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_path_nonexistent() {
        let result = XPlaneEnvironment::from_path("/nonexistent/path");
        assert!(result.is_err());
    }

    #[test]
    fn test_path_construction() {
        // Create a temporary directory structure for testing
        let temp_dir = std::env::temp_dir().join("xearthlayer_test_xplane");
        let _ = std::fs::create_dir_all(&temp_dir);

        if let Ok(env) = XPlaneEnvironment::from_path(&temp_dir) {
            assert_eq!(env.installation_path(), temp_dir.as_path());
            assert_eq!(env.custom_scenery_path(), temp_dir.join("Custom Scenery"));
            assert_eq!(env.resources_path(), temp_dir.join("Resources"));
            assert_eq!(
                env.mountpoint_for("my_pack"),
                temp_dir.join("Custom Scenery").join("my_pack")
            );
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_apt_dat_path_missing() {
        let temp_dir = std::env::temp_dir().join("xearthlayer_test_xplane_apt");
        let _ = std::fs::create_dir_all(&temp_dir);

        if let Ok(env) = XPlaneEnvironment::from_path(&temp_dir) {
            // apt.dat doesn't exist in our temp dir
            assert!(env.apt_dat_path().is_none());
            assert!(!env.has_apt_dat());
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
