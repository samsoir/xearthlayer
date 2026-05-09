//! Error types for the Package Manager.

use std::io;
use std::path::PathBuf;

/// Result type for manager operations.
pub type ManagerResult<T> = Result<T, ManagerError>;

/// Errors that can occur during package management operations.
#[derive(Debug)]
pub enum ManagerError {
    /// Failed to read a file or directory.
    ReadFailed { path: PathBuf, source: io::Error },

    /// Failed to write a file or directory.
    WriteFailed { path: PathBuf, source: io::Error },

    /// Failed to create a directory.
    CreateDirFailed { path: PathBuf, source: io::Error },

    /// Package not found locally.
    PackageNotFound {
        region: String,
        package_type: String,
    },

    /// Package not found in any configured library.
    PackageNotInLibrary {
        region: String,
        package_type: String,
    },

    /// Failed to fetch library index.
    LibraryFetchFailed { url: String, reason: String },

    /// Failed to parse library index.
    LibraryParseFailed { url: String, reason: String },

    /// Failed to fetch package metadata.
    MetadataFetchFailed { url: String, reason: String },

    /// Failed to parse package metadata.
    MetadataParseFailed { url: String, reason: String },

    /// Failed to download package archive.
    DownloadFailed { url: String, reason: String },

    /// Checksum verification failed.
    ChecksumMismatch {
        filename: String,
        expected: String,
        actual: String,
    },

    /// Archive extraction failed.
    ExtractionFailed { path: PathBuf, reason: String },

    /// Invalid configuration.
    InvalidConfig(String),

    /// Package is already installed.
    AlreadyInstalled {
        region: String,
        package_type: String,
        version: String,
    },

    /// No libraries configured.
    NoLibrariesConfigured,

    /// HTTP request failed.
    HttpError(String),

    /// Network timeout.
    Timeout { url: String, timeout_secs: u64 },

    /// Invalid path provided.
    InvalidPath(String),

    /// Symlink operation failed.
    SymlinkFailed {
        source: PathBuf,
        target: PathBuf,
        reason: String,
    },

    /// Insufficient disk space at the install location for a package
    /// install. Surfaced by the pre-flight check before any download
    /// starts, so users get a clear message instead of mid-stream I/O
    /// failures. See issue #188.
    InsufficientDiskSpace {
        path: PathBuf,
        required_bytes: u64,
        available_bytes: u64,
    },
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFailed { path, source } => {
                write!(f, "failed to read {}: {}", path.display(), source)
            }
            Self::WriteFailed { path, source } => {
                write!(f, "failed to write {}: {}", path.display(), source)
            }
            Self::CreateDirFailed { path, source } => {
                write!(
                    f,
                    "failed to create directory {}: {}",
                    path.display(),
                    source
                )
            }
            Self::PackageNotFound {
                region,
                package_type,
            } => {
                write!(f, "package not found: {} {}", region, package_type)
            }
            Self::PackageNotInLibrary {
                region,
                package_type,
            } => {
                write!(
                    f,
                    "package {} {} not found in any configured library",
                    region, package_type
                )
            }
            Self::LibraryFetchFailed { url, reason } => {
                write!(f, "failed to fetch library index from {}: {}", url, reason)
            }
            Self::LibraryParseFailed { url, reason } => {
                write!(f, "failed to parse library index from {}: {}", url, reason)
            }
            Self::MetadataFetchFailed { url, reason } => {
                write!(
                    f,
                    "failed to fetch package metadata from {}: {}",
                    url, reason
                )
            }
            Self::MetadataParseFailed { url, reason } => {
                write!(
                    f,
                    "failed to parse package metadata from {}: {}",
                    url, reason
                )
            }
            Self::DownloadFailed { url, reason } => {
                write!(f, "failed to download {}: {}", url, reason)
            }
            Self::ChecksumMismatch {
                filename,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "checksum mismatch for {}: expected {}, got {}",
                    filename, expected, actual
                )
            }
            Self::ExtractionFailed { path, reason } => {
                write!(f, "failed to extract {}: {}", path.display(), reason)
            }
            Self::InvalidConfig(msg) => write!(f, "invalid configuration: {}", msg),
            Self::AlreadyInstalled {
                region,
                package_type,
                version,
            } => {
                write!(
                    f,
                    "package {} {} v{} is already installed",
                    region, package_type, version
                )
            }
            Self::NoLibrariesConfigured => write!(f, "no package libraries configured"),
            Self::HttpError(msg) => write!(f, "HTTP error: {}", msg),
            Self::Timeout { url, timeout_secs } => {
                write!(f, "request to {} timed out after {}s", url, timeout_secs)
            }
            Self::InvalidPath(msg) => write!(f, "invalid path: {}", msg),
            Self::SymlinkFailed {
                source,
                target,
                reason,
            } => {
                write!(
                    f,
                    "symlink operation failed ({} -> {}): {}",
                    source.display(),
                    target.display(),
                    reason
                )
            }
            Self::InsufficientDiskSpace {
                path,
                required_bytes,
                available_bytes,
            } => {
                write!(
                    f,
                    "insufficient disk space at {}: required {}, available {}",
                    path.display(),
                    crate::config::format_size(*required_bytes as usize),
                    crate::config::format_size(*available_bytes as usize),
                )
            }
        }
    }
}

impl std::error::Error for ManagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadFailed { source, .. } => Some(source),
            Self::WriteFailed { source, .. } => Some(source),
            Self::CreateDirFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = ManagerError::PackageNotFound {
            region: "na".to_string(),
            package_type: "ortho".to_string(),
        };
        assert_eq!(err.to_string(), "package not found: na ortho");
    }

    #[test]
    fn test_checksum_mismatch_display() {
        let err = ManagerError::ChecksumMismatch {
            filename: "test.tar.gz".to_string(),
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(err.to_string().contains("abc123"));
        assert!(err.to_string().contains("def456"));
    }
}
