//! Archive building for package distribution.
//!
//! Creates tar.gz archives from package directories and splits them
//! into manageable parts for distribution.
//!
//! Uses external tools (`tar`, `split`) which are standard on Linux and
//! macOS. Only flags shared by the GNU and BSD variants are used.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use semver::Version;

use super::metadata::calculate_sha256;
use super::{PublishError, PublishResult, RepoConfig};
use xearthlayer_package::{self as package, PackageType};

// Re-export format_size for convenience
pub use crate::size::format_size as format_archive_size;

/// Result of building an archive.
#[derive(Debug, Clone)]
pub struct ArchiveBuildResult {
    /// The base archive filename (e.g., "zzXEL_na_ortho-1.0.0.tar.gz").
    pub archive_name: String,

    /// List of archive parts with their paths and checksums.
    pub parts: Vec<ArchivePart>,

    /// Total size of all parts in bytes.
    pub total_size: u64,
}

/// Information about a single archive part.
#[derive(Debug, Clone)]
pub struct ArchivePart {
    /// Filename of this part (e.g., "zzXEL_na_ortho-1.0.0.tar.gz.aa").
    pub filename: String,

    /// Full path to the part file.
    pub path: PathBuf,

    /// SHA-256 checksum of this part.
    pub checksum: String,

    /// Size of this part in bytes.
    pub size: u64,
}

/// Check if required external tools are available.
pub fn check_required_tools() -> PublishResult<()> {
    check_tool_available("tar", &["--version"])?;
    // BSD split (macOS) has no --version flag; splitting /dev/null is a
    // portable no-op probe (empty input produces no output files).
    check_tool_available("split", &["-b", "1", "/dev/null"])?;
    Ok(())
}

/// Check if a specific tool is available.
fn check_tool_available(tool: &str, args: &[&str]) -> PublishResult<()> {
    let result = Command::new(tool).args(args).output();

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(PublishError::ArchiveFailed(format!(
            "'{}' command failed. Please ensure it is properly installed.",
            tool
        ))),
        Err(e) => Err(PublishError::ArchiveFailed(format!(
            "'{}' command not found: {}. \
             Please install it using your package manager (e.g., 'apt install {}' or 'dnf install {}').",
            tool, e, tool, tool
        ))),
    }
}

/// Generate the archive filename for a package.
///
/// This is a re-export of [`xearthlayer_package::archive_filename`] for convenience.
/// See that function for full documentation.
pub fn archive_filename(region: &str, package_type: PackageType, version: &Version) -> String {
    package::archive_filename(region, package_type, version)
}

/// Build a distributable archive from a package directory.
///
/// # Arguments
///
/// * `package_dir` - Path to the package directory to archive
/// * `dist_dir` - Directory to write archive parts to
/// * `region` - Region code for the package
/// * `package_type` - Type of package (ortho/overlay)
/// * `version` - Package version
/// * `config` - Repository configuration (for part size)
///
/// # Returns
///
/// Information about the built archive and its parts.
pub fn build_archive(
    package_dir: &Path,
    dist_dir: &Path,
    region: &str,
    package_type: PackageType,
    version: &Version,
    config: &RepoConfig,
) -> PublishResult<ArchiveBuildResult> {
    // Check tools are available
    check_required_tools()?;

    // Validate package directory exists
    if !package_dir.exists() || !package_dir.is_dir() {
        return Err(PublishError::InvalidPath(format!(
            "package directory does not exist: {}",
            package_dir.display()
        )));
    }

    // Create dist subdirectory for this package type
    let type_dist_dir = dist_dir
        .join(region.to_lowercase())
        .join(package_type.folder_suffix());
    fs::create_dir_all(&type_dist_dir).map_err(|e| PublishError::CreateDirectoryFailed {
        path: type_dist_dir.clone(),
        source: e,
    })?;

    let archive_name = archive_filename(region, package_type, version);
    let archive_path = type_dist_dir.join(&archive_name);

    // Create tar.gz archive
    create_tar_gz(package_dir, &archive_path)?;

    // Get archive size to determine if splitting is needed
    let archive_size = fs::metadata(&archive_path)
        .map_err(|e| PublishError::ReadFailed {
            path: archive_path.clone(),
            source: e,
        })?
        .len();

    // Split if larger than part size
    let parts = if archive_size > config.part_size {
        split_archive(&archive_path, config.part_size)?
    } else {
        // Single part - rename to add .aa suffix for consistency
        // Note: We can't use with_extension here because the archive name already
        // has .tar.gz extension, and with_extension would only replace .gz
        let mut part_filename = archive_path.as_os_str().to_os_string();
        part_filename.push(".aa");
        let part_path = PathBuf::from(part_filename);
        fs::rename(&archive_path, &part_path).map_err(|e| PublishError::WriteFailed {
            path: part_path.clone(),
            source: e,
        })?;

        let checksum = calculate_sha256(&part_path)?;
        vec![ArchivePart {
            filename: format!("{}.aa", archive_name),
            path: part_path,
            checksum,
            size: archive_size,
        }]
    };

    let total_size = parts.iter().map(|p| p.size).sum();

    Ok(ArchiveBuildResult {
        archive_name,
        parts,
        total_size,
    })
}

/// Create a tar.gz archive of a directory.
fn create_tar_gz(source_dir: &Path, archive_path: &Path) -> PublishResult<()> {
    // Get the directory name for the archive root
    let dir_name = source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| PublishError::InvalidPath("invalid source directory name".to_string()))?;

    // Get the parent directory to run tar from
    let parent_dir = source_dir
        .parent()
        .ok_or_else(|| PublishError::InvalidPath("source directory has no parent".to_string()))?;

    // Convert archive path to absolute since we're running tar from a different directory
    let abs_archive_path = if archive_path.is_absolute() {
        archive_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| PublishError::ArchiveFailed(format!("failed to get cwd: {}", e)))?
            .join(archive_path)
    };

    let output = Command::new("tar")
        .current_dir(parent_dir)
        .args(["-czf", abs_archive_path.to_str().unwrap(), dir_name])
        .output()
        .map_err(|e| PublishError::ArchiveFailed(format!("failed to run tar: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PublishError::ArchiveFailed(format!(
            "tar failed: {}",
            stderr
        )));
    }

    Ok(())
}

/// Split an archive into parts.
fn split_archive(archive_path: &Path, part_size: u64) -> PublishResult<Vec<ArchivePart>> {
    // Generate split prefix (archive path without extension, then re-add it)
    let archive_name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| PublishError::InvalidPath("invalid archive path".to_string()))?;

    let archive_dir = archive_path
        .parent()
        .ok_or_else(|| PublishError::InvalidPath("archive path has no parent".to_string()))?;

    // Split prefix is the full archive name with a dot appended
    let split_prefix = format!("{}.", archive_name);

    let output = Command::new("split")
        .current_dir(archive_dir)
        .args(["-b", &part_size.to_string(), archive_name, &split_prefix])
        .output()
        .map_err(|e| PublishError::ArchiveFailed(format!("failed to run split: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PublishError::ArchiveFailed(format!(
            "split failed: {}",
            stderr
        )));
    }

    // Remove the original archive (we have the split parts now)
    fs::remove_file(archive_path).map_err(|e| PublishError::WriteFailed {
        path: archive_path.to_path_buf(),
        source: e,
    })?;

    // Find all the parts and calculate checksums
    collect_archive_parts(archive_dir, archive_name)
}

/// Collect all archive parts and calculate their checksums.
fn collect_archive_parts(dir: &Path, archive_name: &str) -> PublishResult<Vec<ArchivePart>> {
    let prefix = format!("{}.", archive_name);
    let mut parts = Vec::new();

    let entries = fs::read_dir(dir).map_err(|e| PublishError::ReadFailed {
        path: dir.to_path_buf(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| PublishError::ReadFailed {
            path: dir.to_path_buf(),
            source: e,
        })?;

        let filename = entry.file_name();
        let filename_str = filename.to_string_lossy();

        if filename_str.starts_with(&prefix) {
            let path = entry.path();
            let size = fs::metadata(&path)
                .map_err(|e| PublishError::ReadFailed {
                    path: path.clone(),
                    source: e,
                })?
                .len();

            let checksum = calculate_sha256(&path)?;

            parts.push(ArchivePart {
                filename: filename_str.to_string(),
                path,
                checksum,
                size,
            });
        }
    }

    // Sort by suffix (aa, ab, ac, etc.)
    parts.sort_by(|a, b| a.filename.cmp(&b.filename));

    if parts.is_empty() {
        return Err(PublishError::ArchiveFailed(
            "no archive parts found after splitting".to_string(),
        ));
    }

    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_archive_filename_ortho() {
        let version = Version::new(1, 2, 3);
        let filename = archive_filename("na", PackageType::Ortho, &version);
        assert_eq!(filename, "zzXEL_na_ortho-1.2.3.tar.gz");
    }

    #[test]
    fn test_archive_filename_overlay() {
        let version = Version::new(2, 0, 0);
        let filename = archive_filename("EUR", PackageType::Overlay, &version);
        assert_eq!(filename, "yzXEL_eur_overlay-2.0.0.tar.gz");
    }

    #[test]
    fn test_check_required_tools() {
        // This should pass on Linux with coreutils installed
        let result = check_required_tools();
        assert!(result.is_ok(), "tar and split should be available");
    }

    #[test]
    fn test_check_tool_not_available() {
        let result = check_tool_available("nonexistent_tool_xyz", &["--version"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_build_archive_small_file() {
        let temp = TempDir::new().unwrap();

        // Create a package directory with some content
        let package_dir = temp.path().join("zzXEL_na_ortho");
        fs::create_dir_all(&package_dir).unwrap();

        // Create some test files
        let metadata_path = package_dir.join("xearthlayer_scenery_package.txt");
        File::create(&metadata_path)
            .unwrap()
            .write_all(b"test metadata content")
            .unwrap();

        let terrain_dir = package_dir.join("terrain");
        fs::create_dir_all(&terrain_dir).unwrap();
        File::create(terrain_dir.join("test.ter"))
            .unwrap()
            .write_all(b"test terrain file")
            .unwrap();

        // Create dist directory
        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Build archive with large part size (won't split)
        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        assert_eq!(result.archive_name, "zzXEL_na_ortho-1.0.0.tar.gz");
        assert_eq!(result.parts.len(), 1);
        assert!(result.parts[0].filename.ends_with(".aa"));
        assert!(result.parts[0].path.exists());
        assert!(!result.parts[0].checksum.is_empty());
        assert!(result.total_size > 0);
    }

    #[test]
    fn test_build_archive_splits_large_file() {
        use rand::Rng;

        let temp = TempDir::new().unwrap();

        // Create a package directory with larger content
        let package_dir = temp.path().join("zzXEL_na_ortho");
        fs::create_dir_all(&package_dir).unwrap();

        // Create a file larger than the part size with random data (incompressible)
        let data_path = package_dir.join("large_data.bin");
        let mut file = File::create(&data_path).unwrap();
        let mut rng = rand::rng();

        // Write 25 MB of random data (incompressible, will be split with 10 MB parts)
        let mut chunk = vec![0u8; 1024 * 1024]; // 1 MB chunk
        for _ in 0..25 {
            rng.fill(&mut chunk[..]);
            file.write_all(&chunk).unwrap();
        }
        drop(file);

        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Build with 10 MB part size (minimum allowed)
        let config = RepoConfig::new(10 * 1024 * 1024).unwrap();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        // Should have multiple parts
        assert!(
            result.parts.len() > 1,
            "expected multiple parts, got {}",
            result.parts.len()
        );

        // Parts should be sorted
        let suffixes: Vec<&str> = result
            .parts
            .iter()
            .map(|p| p.filename.rsplit('.').next().unwrap())
            .collect();
        assert!(suffixes.windows(2).all(|w| w[0] <= w[1]));

        // All parts should exist with checksums
        for part in &result.parts {
            assert!(part.path.exists());
            assert!(!part.checksum.is_empty());
            assert_eq!(part.checksum.len(), 64); // SHA-256 hex
        }
    }

    #[test]
    fn test_build_archive_creates_dist_subdirs() {
        let temp = TempDir::new().unwrap();

        let package_dir = temp.path().join("zzXEL_eur_ortho");
        fs::create_dir_all(&package_dir).unwrap();
        File::create(package_dir.join("test.txt"))
            .unwrap()
            .write_all(b"test")
            .unwrap();

        let dist_dir = temp.path().join("dist");
        // Don't create dist_dir - build_archive should create it

        let config = RepoConfig::default();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "eur",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        // Should have created dist/eur/ortho/
        assert!(dist_dir.join("eur").join("ortho").exists());
        assert!(result.parts[0].path.exists());
    }

    #[test]
    fn test_build_archive_invalid_package_dir() {
        let temp = TempDir::new().unwrap();
        let dist_dir = temp.path().join("dist");

        let config = RepoConfig::default();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            Path::new("/nonexistent/path"),
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_archive_part_debug() {
        let part = ArchivePart {
            filename: "test.tar.gz.aa".to_string(),
            path: PathBuf::from("/tmp/test"),
            checksum: "abc123".to_string(),
            size: 1024,
        };
        let debug = format!("{:?}", part);
        assert!(debug.contains("ArchivePart"));
        assert!(debug.contains("test.tar.gz.aa"));
    }

    #[test]
    fn test_archive_build_result_debug() {
        let result = ArchiveBuildResult {
            archive_name: "test.tar.gz".to_string(),
            parts: vec![],
            total_size: 0,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("ArchiveBuildResult"));
    }
}
