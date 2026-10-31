//! HTTP-based file downloader with resume support.
//!
//! This module provides the core HTTP download functionality including:
//! - Resumable downloads via HTTP Range requests
//! - Progress callbacks for UI updates
//! - Checksum verification

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::Client;

use super::checksum::{calculate_file_checksum, verify_checksum};
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::traits::{PackageDownloader, ProgressCallback};

/// Default timeout for HTTP requests in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Buffer size for reading/writing during downloads (64KB).
const BUFFER_SIZE: usize = 64 * 1024;

/// HTTP-based package downloader.
///
/// Implements the `PackageDownloader` trait with support for:
/// - Range requests for resuming downloads
/// - SHA-256 checksum verification
/// - Progress reporting
#[derive(Debug)]
pub struct HttpDownloader {
    client: Client,
    pub(crate) timeout: Duration,
}

impl Default for HttpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpDownloader {
    /// Create a new HTTP downloader with default settings.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Create a new HTTP downloader with custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self { client, timeout }
    }

    /// Get the file size from a URL via HEAD request.
    ///
    /// Returns 0 if the size cannot be determined.
    pub fn get_file_size(&self, url: &str) -> u64 {
        self.client
            .head(url)
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| {
                r.headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .unwrap_or(0)
    }

    /// Download a file with resumption support.
    ///
    /// If the destination file already exists and is a partial download,
    /// this will resume from where it left off (if the server supports Range requests).
    fn download_with_resume(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
        progress: Option<ProgressCallback>,
        should_stop: &dyn Fn() -> bool,
    ) -> ManagerResult<u64> {
        // Check existing file for resume
        let existing_size = if dest.exists() {
            dest.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        // Get total size and check resume support
        let (total_size, supports_range) = self.query_file_info(url)?;

        // Check if already complete
        if let Some(size) = self.check_existing_download(
            dest,
            existing_size,
            total_size,
            expected_checksum,
            progress.as_ref(),
        )? {
            return Ok(size);
        }

        // Open file for writing (append or create)
        let (start_byte, file) =
            self.prepare_destination(dest, existing_size, total_size, supports_range)?;

        // Download the content
        self.stream_download(
            url,
            file,
            dest,
            start_byte,
            total_size,
            progress,
            should_stop,
        )?;

        // Verify checksum if provided
        if let Some(expected) = expected_checksum {
            verify_checksum(dest, expected)?;
        }

        Ok(total_size.max(start_byte))
    }

    /// Download like `download_with_progress`, stopping with
    /// [`ManagerError::Cancelled`] at the next buffer once `should_stop`
    /// returns true.
    pub fn download_cancellable(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
        on_progress: ProgressCallback,
        should_stop: &dyn Fn() -> bool,
    ) -> ManagerResult<u64> {
        self.download_with_resume(url, dest, expected_checksum, Some(on_progress), should_stop)
    }

    /// Query file info via HEAD request.
    fn query_file_info(&self, url: &str) -> ManagerResult<(u64, bool)> {
        let head_response =
            self.client
                .head(url)
                .send()
                .map_err(|e| ManagerError::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?;

        if !head_response.status().is_success() {
            return Err(ManagerError::DownloadFailed {
                url: url.to_string(),
                reason: format!("HEAD request failed with status {}", head_response.status()),
            });
        }

        let total_size = head_response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let supports_range = head_response
            .headers()
            .get("accept-ranges")
            .map(|v| v.to_str().unwrap_or("") == "bytes")
            .unwrap_or(false);

        Ok((total_size, supports_range))
    }

    /// Check if an existing download is already complete.
    fn check_existing_download(
        &self,
        dest: &Path,
        existing_size: u64,
        total_size: u64,
        expected_checksum: Option<&str>,
        progress: Option<&ProgressCallback>,
    ) -> ManagerResult<Option<u64>> {
        if existing_size != total_size || total_size == 0 {
            return Ok(None);
        }

        // File size matches - verify checksum if provided
        if let Some(expected) = expected_checksum {
            let actual = calculate_file_checksum(dest)?;
            if actual != expected {
                // Checksum mismatch - need to re-download
                fs::remove_file(dest).ok();
                return Ok(None);
            }
        }

        // Already downloaded and verified
        if let Some(cb) = progress {
            cb(total_size, total_size);
        }
        Ok(Some(total_size))
    }

    /// Prepare the destination file for writing.
    fn prepare_destination(
        &self,
        dest: &Path,
        existing_size: u64,
        total_size: u64,
        supports_range: bool,
    ) -> ManagerResult<(u64, File)> {
        if existing_size > 0 && supports_range && existing_size < total_size {
            // Resume from existing position
            let file = OpenOptions::new().append(true).open(dest).map_err(|e| {
                ManagerError::WriteFailed {
                    path: dest.to_path_buf(),
                    source: e,
                }
            })?;
            Ok((existing_size, file))
        } else {
            // Start fresh
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| ManagerError::CreateDirFailed {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            let file = File::create(dest).map_err(|e| ManagerError::WriteFailed {
                path: dest.to_path_buf(),
                source: e,
            })?;
            Ok((0, file))
        }
    }

    /// Stream the download to the destination file.
    #[allow(clippy::too_many_arguments)]
    fn stream_download(
        &self,
        url: &str,
        file: File,
        dest: &Path,
        start_byte: u64,
        total_size: u64,
        progress: Option<ProgressCallback>,
        should_stop: &dyn Fn() -> bool,
    ) -> ManagerResult<u64> {
        // Build request with optional Range header
        let mut request = self.client.get(url);
        if start_byte > 0 {
            request = request.header("Range", format!("bytes={}-", start_byte));
        }

        let mut response = request.send().map_err(|e| {
            if e.is_timeout() {
                ManagerError::Timeout {
                    url: url.to_string(),
                    timeout_secs: self.timeout.as_secs(),
                }
            } else {
                ManagerError::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                }
            }
        })?;

        // Check response status (200 OK or 206 Partial Content)
        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            return Err(ManagerError::DownloadFailed {
                url: url.to_string(),
                reason: format!("GET request failed with status {}", status),
            });
        }

        // Stream to file
        let mut writer = BufWriter::new(file);
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut downloaded = start_byte;

        loop {
            if should_stop() {
                return Err(ManagerError::Cancelled);
            }
            let bytes_read =
                response
                    .read(&mut buffer)
                    .map_err(|e| ManagerError::DownloadFailed {
                        url: url.to_string(),
                        reason: format!("Read error: {}", e),
                    })?;

            if bytes_read == 0 {
                break;
            }

            writer
                .write_all(&buffer[..bytes_read])
                .map_err(|e| ManagerError::WriteFailed {
                    path: dest.to_path_buf(),
                    source: e,
                })?;

            downloaded += bytes_read as u64;

            if let Some(ref cb) = progress {
                cb(downloaded, total_size);
            }
        }

        writer.flush().map_err(|e| ManagerError::WriteFailed {
            path: dest.to_path_buf(),
            source: e,
        })?;

        Ok(downloaded)
    }
}

impl PackageDownloader for HttpDownloader {
    fn download(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
    ) -> ManagerResult<u64> {
        self.download_with_resume(url, dest, expected_checksum, None, &|| false)
    }

    fn download_with_progress(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: Option<&str>,
        on_progress: ProgressCallback,
    ) -> ManagerResult<u64> {
        self.download_with_resume(url, dest, expected_checksum, Some(on_progress), &|| false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_downloader_default() {
        let downloader = HttpDownloader::default();
        assert_eq!(downloader.timeout.as_secs(), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn test_http_downloader_with_timeout() {
        let downloader = HttpDownloader::with_timeout(Duration::from_secs(60));
        assert_eq!(downloader.timeout.as_secs(), 60);
    }

    #[test]
    fn test_http_downloader_new() {
        let downloader = HttpDownloader::new();
        assert_eq!(downloader.timeout.as_secs(), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn a_stop_request_ends_a_download_within_one_buffer() {
        let url = format!(
            "{}/part.aa",
            crate::manager::download::test_server::slow_server()
        );
        let temp = tempfile::TempDir::new().unwrap();
        let dest = temp.path().join("part.aa");
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&started);
        let progress: ProgressCallback = Box::new(move |downloaded, _| {
            if downloaded > 0 {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let begun = std::time::Instant::now();

        let result =
            HttpDownloader::new().download_cancellable(&url, &dest, None, progress, &|| {
                started.load(std::sync::atomic::Ordering::SeqCst)
            });

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
        assert!(begun.elapsed() < std::time::Duration::from_secs(5));
    }
}
