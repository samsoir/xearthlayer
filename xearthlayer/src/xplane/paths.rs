//! X-Plane path utilities.
//!
//! Internal helpers for constructing X-Plane-related paths.

use std::path::PathBuf;

use super::detection::XPlanePathError;

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
}
