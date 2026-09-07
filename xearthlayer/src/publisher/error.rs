//! Error types for the publisher module.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Result type for publisher operations.
pub type PublishResult<T> = Result<T, PublishError>;

/// Errors that can occur during publishing operations.
#[derive(Debug)]
pub enum PublishError {
    /// Repository already exists at the specified path.
    RepositoryExists(PathBuf),

    /// No repository found at the specified path.
    RepositoryNotFound(PathBuf),

    /// Repository marker file is invalid or corrupted.
    InvalidRepository(String),

    /// Failed to create directory.
    CreateDirectoryFailed { path: PathBuf, source: io::Error },

    /// Failed to read file.
    ReadFailed { path: PathBuf, source: io::Error },

    /// Failed to write file.
    WriteFailed { path: PathBuf, source: io::Error },

    /// Invalid path provided.
    InvalidPath(String),

    /// Package not found in repository.
    PackageNotFound {
        region: String,
        package_type: String,
    },

    /// Package already exists in repository.
    PackageExists {
        region: String,
        package_type: String,
    },

    /// Invalid Ortho4XP source directory.
    InvalidSource(String),

    /// No valid tiles found in source.
    NoTilesFound(PathBuf),

    /// Invalid version string.
    InvalidVersion(String),

    /// Archive building failed.
    ArchiveFailed(String),

    /// Checksum verification failed.
    ChecksumMismatch {
        file: PathBuf,
        expected: String,
        actual: String,
    },

    /// URL configuration error.
    InvalidUrl(String),

    /// Release validation failed.
    ReleaseValidation(String),

    /// Region metadata file not found.
    RegionMetadataNotFound(PathBuf),

    /// Region metadata file could not be parsed.
    InvalidRegionMetadata { path: PathBuf, message: String },

    /// A region's colour could not be resolved to RGB.
    UnknownRegionColor { region: String, color: String },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::RepositoryExists(path) => {
                write!(f, "repository already exists at {}", path.display())
            }
            PublishError::RepositoryNotFound(path) => {
                write!(f, "no repository found at {}", path.display())
            }
            PublishError::InvalidRepository(msg) => {
                write!(f, "invalid repository: {}", msg)
            }
            PublishError::CreateDirectoryFailed { path, source } => {
                write!(
                    f,
                    "failed to create directory {}: {}",
                    path.display(),
                    source
                )
            }
            PublishError::ReadFailed { path, source } => {
                write!(f, "failed to read {}: {}", path.display(), source)
            }
            PublishError::WriteFailed { path, source } => {
                write!(f, "failed to write {}: {}", path.display(), source)
            }
            PublishError::InvalidPath(msg) => {
                write!(f, "invalid path: {}", msg)
            }
            PublishError::PackageNotFound {
                region,
                package_type,
            } => {
                write!(f, "package not found: {} {}", region, package_type)
            }
            PublishError::PackageExists {
                region,
                package_type,
            } => {
                write!(f, "package already exists: {} {}", region, package_type)
            }
            PublishError::InvalidSource(msg) => {
                write!(f, "invalid source: {}", msg)
            }
            PublishError::NoTilesFound(path) => {
                write!(f, "no valid tiles found in {}", path.display())
            }
            PublishError::InvalidVersion(msg) => {
                write!(f, "invalid version: {}", msg)
            }
            PublishError::ArchiveFailed(msg) => {
                write!(f, "archive failed: {}", msg)
            }
            PublishError::ChecksumMismatch {
                file,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "checksum mismatch for {}: expected {}, got {}",
                    file.display(),
                    expected,
                    actual
                )
            }
            PublishError::InvalidUrl(msg) => {
                write!(f, "invalid URL: {}", msg)
            }
            PublishError::ReleaseValidation(msg) => {
                write!(f, "release validation failed: {}", msg)
            }
            PublishError::RegionMetadataNotFound(path) => {
                write!(
                    f,
                    "region_metadata.json not found at {}. Pass --metadata to override.",
                    path.display()
                )
            }
            PublishError::InvalidRegionMetadata { path, message } => {
                write!(f, "failed to parse {}: {}", path.display(), message)
            }
            PublishError::UnknownRegionColor { region, color } => {
                write!(
                    f,
                    "region \"{}\" has unknown color \"{}\". Use a CSS color name or hex (#rrggbb).",
                    region, color
                )
            }
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PublishError::CreateDirectoryFailed { source, .. } => Some(source),
            PublishError::ReadFailed { source, .. } => Some(source),
            PublishError::WriteFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_repository_exists_display() {
        let err = PublishError::RepositoryExists(PathBuf::from("/test/path"));
        assert!(err.to_string().contains("/test/path"));
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn test_repository_not_found_display() {
        let err = PublishError::RepositoryNotFound(PathBuf::from("/test/path"));
        assert!(err.to_string().contains("/test/path"));
        assert!(err.to_string().contains("no repository found"));
    }

    #[test]
    fn test_package_not_found_display() {
        let err = PublishError::PackageNotFound {
            region: "na".to_string(),
            package_type: "ortho".to_string(),
        };
        assert!(err.to_string().contains("na"));
        assert!(err.to_string().contains("ortho"));
    }

    #[test]
    fn test_checksum_mismatch_display() {
        let err = PublishError::ChecksumMismatch {
            file: PathBuf::from("test.tar.gz"),
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("def456"));
    }

    #[test]
    fn test_error_source_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err = PublishError::ReadFailed {
            path: PathBuf::from("/test"),
            source: io_err,
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_none() {
        let err = PublishError::InvalidVersion("bad".to_string());
        assert!(err.source().is_none());
    }
}
