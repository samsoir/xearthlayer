//! X-Plane path utilities.
//!
//! Internal helpers for constructing X-Plane-related paths.

use std::path::PathBuf;

use super::detection::XPlanePathError;

/// Well-known X-Plane subdirectories.
#[allow(dead_code)]
pub mod subdirs {
    /// Custom Scenery directory name.
    pub const CUSTOM_SCENERY: &str = "Custom Scenery";
    /// Resources directory name.
    pub const RESOURCES: &str = "Resources";
    /// Default scenery directory (inside Resources).
    pub const DEFAULT_SCENERY: &str = "default scenery";
    /// Default apt dat directory (inside default scenery).
    pub const DEFAULT_APT_DAT: &str = "default apt dat";
    /// Earth nav data directory.
    pub const EARTH_NAV_DATA: &str = "Earth nav data";
    /// Airport database filename.
    pub const APT_DAT: &str = "apt.dat";
}

/// Get the path to the X-Plane install reference file.
///
/// The location varies by OS:
/// - Linux: `~/.x-plane/x-plane_install_12.txt`
/// - macOS: `~/.x-plane/x-plane_install_12.txt`
/// - Windows: `%LOCALAPPDATA%\x-plane\x-plane_install_12.txt`
pub fn get_install_reference_path() -> Result<PathBuf, XPlanePathError> {
    #[cfg(target_os = "windows")]
    {
        // Windows uses LOCALAPPDATA
        let local_app_data =
            std::env::var("LOCALAPPDATA").map_err(|_| XPlanePathError::NoHomeDirectory)?;
        Ok(PathBuf::from(local_app_data)
            .join("x-plane")
            .join("x-plane_install_12.txt"))
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Linux and macOS use ~/.x-plane
        let home = dirs::home_dir().ok_or(XPlanePathError::NoHomeDirectory)?;
        Ok(home.join(".x-plane").join("x-plane_install_12.txt"))
    }
}

/// Construct the path to apt.dat from an X-Plane installation root.
#[allow(dead_code)]
pub fn apt_dat_path(xplane_root: &std::path::Path) -> PathBuf {
    xplane_root
        .join(subdirs::RESOURCES)
        .join(subdirs::DEFAULT_SCENERY)
        .join(subdirs::DEFAULT_APT_DAT)
        .join(subdirs::EARTH_NAV_DATA)
        .join(subdirs::APT_DAT)
}

/// Construct the path to Custom Scenery from an X-Plane installation root.
#[allow(dead_code)]
pub fn custom_scenery_path(xplane_root: &std::path::Path) -> PathBuf {
    xplane_root.join(subdirs::CUSTOM_SCENERY)
}

/// Construct the path to Resources from an X-Plane installation root.
#[allow(dead_code)]
pub fn resources_path(xplane_root: &std::path::Path) -> PathBuf {
    xplane_root.join(subdirs::RESOURCES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_install_reference_path() {
        // Should not panic and return a valid path structure
        let result = get_install_reference_path();
        assert!(result.is_ok());

        let path = result.unwrap();
        assert!(path.to_string_lossy().contains("x-plane_install_12.txt"));
    }

    #[test]
    fn test_apt_dat_path_construction() {
        let root = PathBuf::from("/home/user/X-Plane 12");
        let path = apt_dat_path(&root);

        assert!(path.to_string_lossy().contains("Resources"));
        assert!(path.to_string_lossy().contains("default scenery"));
        assert!(path.to_string_lossy().ends_with("apt.dat"));
    }

    #[test]
    fn test_custom_scenery_path_construction() {
        let root = PathBuf::from("/home/user/X-Plane 12");
        let path = custom_scenery_path(&root);

        assert_eq!(path, PathBuf::from("/home/user/X-Plane 12/Custom Scenery"));
    }
}
