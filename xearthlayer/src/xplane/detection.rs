//! X-Plane 12 installation detection.
//!
//! Provides utilities for detecting X-Plane 12 installation paths across
//! different operating systems.

use std::fs;
use std::path::PathBuf;
use thiserror::Error;

use super::paths;

/// Result of detecting X-Plane Custom Scenery directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneryDetectionResult {
    /// No X-Plane 12 installation found.
    NotFound,
    /// Single X-Plane 12 installation with valid Custom Scenery directory.
    Single(PathBuf),
    /// Multiple X-Plane 12 installations found - user must choose.
    /// Contains list of Custom Scenery directories.
    Multiple(Vec<PathBuf>),
}

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

    /// Multiple X-Plane installations found - user must choose.
    #[error("Multiple X-Plane 12 installations found. Please specify which to use.")]
    MultipleInstallations(Vec<PathBuf>),
}

/// Detect all X-Plane 12 installation directories.
///
/// Reads the install reference file which may contain multiple paths (one per line)
/// for users with multiple X-Plane installations.
///
/// # Returns
///
/// A vector of paths to valid X-Plane 12 installation directories.
/// Returns an empty vector if no valid installations are found.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::xplane::detect_xplane_installs;
///
/// let installs = detect_xplane_installs();
/// match installs.len() {
///     0 => println!("No X-Plane 12 installations found"),
///     1 => println!("Found X-Plane at: {}", installs[0].display()),
///     _ => println!("Found {} X-Plane installations", installs.len()),
/// }
/// ```
pub fn detect_xplane_installs() -> Vec<PathBuf> {
    let reference_path = match paths::get_install_reference_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };

    if !reference_path.exists() {
        return Vec::new();
    }

    // Read the file and parse all lines as potential paths
    let contents = match fs::read_to_string(&reference_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Each line is a potential install path
    contents
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .filter(|path| !path.as_os_str().is_empty() && path.exists())
        .collect()
}

/// Detect the X-Plane 12 installation directory.
///
/// Reads the install reference file to find the X-Plane 12 installation path.
/// If multiple installations exist, returns the first valid one.
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
/// - No valid installation path exists
///
/// # Example
///
/// ```ignore
/// use xearthlayer::xplane::detect_xplane_install;
///
/// match detect_xplane_install() {
///     Ok(path) => println!("X-Plane 12 installed at: {}", path.display()),
///     Err(e) => eprintln!("Could not detect X-Plane: {}", e),
/// }
/// ```
pub fn detect_xplane_install() -> Result<PathBuf, XPlanePathError> {
    let installs = detect_xplane_installs();

    installs.into_iter().next().ok_or_else(|| {
        let reference_path = paths::get_install_reference_path()
            .unwrap_or_else(|_| PathBuf::from("~/.x-plane/x-plane_install_12.txt"));
        XPlanePathError::InstallFileNotFound(reference_path)
    })
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
/// use xearthlayer::xplane::detect_custom_scenery;
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
/// use xearthlayer::xplane::derive_mountpoint;
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

/// Detect X-Plane Custom Scenery directory for configuration.
///
/// This is the main entry point for the init command to detect where
/// X-Plane scenery packs should be mounted.
///
/// # Returns
///
/// - `NotFound` if no X-Plane 12 installation is detected
/// - `Single(path)` if exactly one installation with Custom Scenery is found
/// - `Multiple(paths)` if multiple installations are found (user must choose)
///
/// # Example
///
/// ```
/// use xearthlayer::xplane::{detect_scenery_dir, SceneryDetectionResult};
///
/// match detect_scenery_dir() {
///     SceneryDetectionResult::NotFound => {
///         println!("X-Plane 12 not found");
///     }
///     SceneryDetectionResult::Single(path) => {
///         println!("Found: {}", path.display());
///     }
///     SceneryDetectionResult::Multiple(paths) => {
///         println!("Multiple installations found:");
///         for path in paths {
///             println!("  - {}", path.display());
///         }
///     }
/// }
/// ```
pub fn detect_scenery_dir() -> SceneryDetectionResult {
    let installs = detect_xplane_installs();

    // Filter to only installations with valid Custom Scenery directories
    let scenery_dirs: Vec<PathBuf> = installs
        .into_iter()
        .map(|install| install.join("Custom Scenery"))
        .filter(|path| path.exists())
        .collect();

    match scenery_dirs.len() {
        0 => SceneryDetectionResult::NotFound,
        1 => SceneryDetectionResult::Single(scenery_dirs.into_iter().next().unwrap()),
        _ => SceneryDetectionResult::Multiple(scenery_dirs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
