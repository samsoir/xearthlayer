//! Trait definitions for Package Manager abstractions.
//!
//! These traits enable dependency injection and testing of the manager components.

use std::path::Path;

use crate::package::{PackageLibrary, PackageMetadata};

use super::ManagerResult;

/// Client for fetching package library indexes and metadata.
///
/// This trait abstracts HTTP fetching to enable testing without network access.
pub trait LibraryClient: Send + Sync {
    /// Fetch a package library index from a URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the library index file
    ///
    /// # Returns
    ///
    /// The parsed `PackageLibrary` on success.
    fn fetch_library(&self, url: &str) -> ManagerResult<PackageLibrary>;

    /// Fetch package metadata from a URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the package metadata file
    ///
    /// # Returns
    ///
    /// The parsed `PackageMetadata` on success.
    fn fetch_metadata(&self, url: &str) -> ManagerResult<PackageMetadata>;
}

/// Progress callback for download operations.
pub type ProgressCallback = Box<dyn Fn(u64, u64) + Send + Sync>;

/// Download progress information.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Will be used in Phase 6 implementation
pub struct DownloadProgress {
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Total bytes to download (if known).
    pub total: Option<u64>,
    /// Current download speed in bytes per second.
    pub speed: u64,
    /// Estimated time remaining in seconds.
    pub eta: Option<u64>,
}

/// Downloader for package archives.
///
/// This trait abstracts archive downloading to enable testing and different
/// download strategies (sequential, parallel, resumable).
pub trait PackageDownloader: Send + Sync {
    /// Download a file from a URL to a local path.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to download from
    /// * `dest` - The destination path for the downloaded file
    /// * `expected_checksum` - Optional SHA-256 checksum to verify
    ///
    /// # Returns
    ///
    /// The number of bytes downloaded on success.
    fn download(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
    ) -> ManagerResult<u64>;

    /// Download a file with progress reporting.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to download from
    /// * `dest` - The destination path for the downloaded file
    /// * `expected_checksum` - Optional SHA-256 checksum to verify
    /// * `on_progress` - Callback for progress updates
    ///
    /// # Returns
    ///
    /// The number of bytes downloaded on success.
    fn download_with_progress(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
        on_progress: ProgressCallback,
    ) -> ManagerResult<u64>;
}

/// Extractor for package archives.
///
/// This trait abstracts archive extraction to support different formats
/// and enable testing.
#[allow(dead_code)] // Will be used in Phase 6 implementation
pub trait ArchiveExtractor: Send + Sync {
    /// Extract an archive to a destination directory.
    ///
    /// # Arguments
    ///
    /// * `archive_path` - Path to the archive file
    /// * `dest_dir` - Directory to extract to
    ///
    /// # Returns
    ///
    /// The number of files extracted on success.
    fn extract(&self, archive_path: &Path, dest_dir: &Path) -> ManagerResult<usize>;

    /// List contents of an archive without extracting.
    ///
    /// # Arguments
    ///
    /// * `archive_path` - Path to the archive file
    ///
    /// # Returns
    ///
    /// List of file paths within the archive.
    fn list_contents(&self, archive_path: &Path) -> ManagerResult<Vec<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Mock library client for testing.
    #[allow(dead_code)] // Will be used in Phase 6 implementation tests
    struct MockLibraryClient {
        library_response: Option<PackageLibrary>,
        metadata_response: Option<PackageMetadata>,
    }

    impl LibraryClient for MockLibraryClient {
        fn fetch_library(&self, _url: &str) -> ManagerResult<PackageLibrary> {
            self.library_response.clone().ok_or_else(|| {
                super::super::ManagerError::LibraryFetchFailed {
                    url: "mock://test".to_string(),
                    reason: "no mock response".to_string(),
                }
            })
        }

        fn fetch_metadata(&self, _url: &str) -> ManagerResult<PackageMetadata> {
            self.metadata_response.clone().ok_or_else(|| {
                super::super::ManagerError::MetadataFetchFailed {
                    url: "mock://test".to_string(),
                    reason: "no mock response".to_string(),
                }
            })
        }
    }

    /// Mock downloader for testing.
    struct MockDownloader {
        bytes_to_return: u64,
    }

    impl PackageDownloader for MockDownloader {
        fn download(
            &self,
            _url: &str,
            _dest: &Path,
            _expected_checksum: Option<&str>,
        ) -> ManagerResult<u64> {
            Ok(self.bytes_to_return)
        }

        fn download_with_progress(
            &self,
            _url: &str,
            _dest: &Path,
            _expected_checksum: Option<&str>,
            on_progress: ProgressCallback,
        ) -> ManagerResult<u64> {
            // Simulate progress callbacks
            on_progress(self.bytes_to_return / 2, self.bytes_to_return);
            on_progress(self.bytes_to_return, self.bytes_to_return);
            Ok(self.bytes_to_return)
        }
    }

    #[test]
    fn test_mock_downloader() {
        let downloader = MockDownloader {
            bytes_to_return: 1024,
        };

        let result = downloader.download("http://test", Path::new("/tmp/test"), None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1024);
    }

    #[test]
    fn test_progress_callback() {
        let downloader = MockDownloader {
            bytes_to_return: 1000,
        };

        let progress = Arc::new(AtomicU64::new(0));
        let progress_clone = progress.clone();

        let callback: ProgressCallback = Box::new(move |downloaded, _total| {
            progress_clone.store(downloaded, Ordering::SeqCst);
        });

        let result = downloader.download_with_progress(
            "http://test",
            Path::new("/tmp/test"),
            None,
            callback,
        );

        assert!(result.is_ok());
        assert_eq!(progress.load(Ordering::SeqCst), 1000);
    }
}
