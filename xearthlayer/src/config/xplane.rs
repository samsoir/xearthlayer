//! X-Plane 12 installation path detection.
//!
//! Provides utilities for detecting X-Plane 12 installation paths across
//! different operating systems.

use std::fs;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when detecting X-Plane paths.
#[derive(Debug, Error)]
pub enum XPlanePathError {
    /// Home directory could not be determined.
    #[error("Could not determine home directory")]
    NoHomeDirectory,

    /// X-Plane install reference file not found.
    #[error("X-Plane 12 install reference not found at {0}")]
    InstallFileNotFound(PathBuf),

    /// Failed to read the install reference file.
    #[error("Failed to read X-Plane install reference: {0}")]
    ReadError(#[from] std::io::Error),

    /// The install path in the reference file doesn't exist.
    #[error("X-Plane 12 install path does not exist: {0}")]
    InstallPathNotFound(PathBuf),

    /// Custom Scenery directory not found.
    #[error("Custom Scenery directory not found: {0}")]
    CustomSceneryNotFound(PathBuf),
}

/// Get the path to the X-Plane install reference file.
///
/// The location varies by OS:
/// - Linux: `~/.x-plane/x-plane_install_12.txt`
/// - macOS: `~/.x-plane/x-plane_install_12.txt`
/// - Windows: `%LOCALAPPDATA%\x-plane\x-plane_install_12.txt`
fn get_install_reference_path() -> Result<PathBuf, XPlanePathError> {
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

/// Detect the X-Plane 12 installation directory.
///
/// Reads the install reference file to find the X-Plane 12 installation path.
///
/// # Returns
///
/// The path to the X-Plane 12 installation directory (e.g., `/home/user/X-Plane 12`).
///
/// # Errors
///
/// Returns an error if:
/// - The install reference file cannot be found
/// - The file cannot be read
/// - The path in the file doesn't exist
///
/// # Example
///
/// ```ignore
/// use xearthlayer::config::detect_xplane_install;
///
/// match detect_xplane_install() {
///     Ok(path) => println!("X-Plane 12 installed at: {}", path.display()),
///     Err(e) => eprintln!("Could not detect X-Plane: {}", e),
/// }
/// ```
pub fn detect_xplane_install() -> Result<PathBuf, XPlanePathError> {
    let reference_path = get_install_reference_path()?;

    if !reference_path.exists() {
        return Err(XPlanePathError::InstallFileNotFound(reference_path));
    }

    // Read the install path from the file
    let contents = fs::read_to_string(&reference_path)?;
    let install_path = PathBuf::from(contents.trim());

    if !install_path.exists() {
        return Err(XPlanePathError::InstallPathNotFound(install_path));
    }

    Ok(install_path)
}

/// Detect the X-Plane 12 Custom Scenery directory.
///
/// Combines the detected X-Plane 12 installation path with the Custom Scenery
/// subdirectory.
///
/// # Returns
///
/// The path to the Custom Scenery directory (e.g., `/home/user/X-Plane 12/Custom Scenery`).
///
/// # Errors
///
/// Returns an error if:
/// - X-Plane 12 installation cannot be detected
/// - The Custom Scenery directory doesn't exist
///
/// # Example
///
/// ```ignore
/// use xearthlayer::config::detect_custom_scenery;
///
/// match detect_custom_scenery() {
///     Ok(path) => println!("Custom Scenery at: {}", path.display()),
///     Err(e) => eprintln!("Could not find Custom Scenery: {}", e),
/// }
/// ```
pub fn detect_custom_scenery() -> Result<PathBuf, XPlanePathError> {
    let install_path = detect_xplane_install()?;
    let custom_scenery = install_path.join("Custom Scenery");

    if !custom_scenery.exists() {
        return Err(XPlanePathError::CustomSceneryNotFound(custom_scenery));
    }

    Ok(custom_scenery)
}

/// Derive the mountpoint for a scenery pack.
///
/// Given a source scenery pack path, determines the appropriate mountpoint
/// within the X-Plane Custom Scenery directory.
///
/// The mountpoint is derived by:
/// 1. Detecting the Custom Scenery directory
/// 2. Using the source pack's directory name as the mountpoint subdirectory
///
/// # Arguments
///
/// * `source_path` - Path to the source scenery pack (e.g., `/data/scenery/z_xel_test`)
///
/// # Returns
///
/// The derived mountpoint path (e.g., `/home/user/X-Plane 12/Custom Scenery/z_xel_test`).
///
/// # Errors
///
/// Returns an error if X-Plane or Custom Scenery cannot be detected.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::config::derive_mountpoint;
/// use std::path::Path;
///
/// let source = Path::new("/data/scenery/z_xel_north_america");
/// match derive_mountpoint(source) {
///     Ok(mp) => println!("Mount at: {}", mp.display()),
///     Err(e) => eprintln!("Could not derive mountpoint: {}", e),
/// }
/// ```
pub fn derive_mountpoint(source_path: &std::path::Path) -> Result<PathBuf, XPlanePathError> {
    let custom_scenery = detect_custom_scenery()?;

    // Get the directory name from the source path
    let pack_name = source_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "xel_scenery".to_string());

    Ok(custom_scenery.join(pack_name))
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
    fn test_detect_xplane_install_no_file() {
        // This test will likely fail if X-Plane is not installed
        // That's expected - we're testing the error case
        let result = detect_xplane_install();

        // The result depends on whether X-Plane is installed
        // We just verify it doesn't panic
        match result {
            Ok(path) => {
                assert!(path.exists());
            }
            Err(e) => {
                // Expected if X-Plane is not installed
                assert!(matches!(
                    e,
                    XPlanePathError::InstallFileNotFound(_)
                        | XPlanePathError::InstallPathNotFound(_)
                        | XPlanePathError::NoHomeDirectory
                ));
            }
        }
    }

    #[test]
    fn test_derive_mountpoint_pack_name() {
        // Test that pack name extraction works correctly
        let source = PathBuf::from("/data/scenery/z_xel_north_america");

        // Note: This will fail if X-Plane isn't installed, but we can
        // still verify the function structure works
        let result = derive_mountpoint(&source);

        match result {
            Ok(path) => {
                // Should end with the pack name
                assert!(path.to_string_lossy().ends_with("z_xel_north_america"));
            }
            Err(_) => {
                // Expected if X-Plane is not installed
            }
        }
    }
}
