//! Package installer for downloading and installing packages.
//!
//! This module orchestrates the full installation workflow:
//! 1. Fetch package metadata from library
//! 2. Download all archive parts
//! 3. Verify checksums
//! 4. Reassemble and extract archive
//! 5. Install to package directory
//! 6. Clean up temporary files

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::package::{PackageMetadata, PackageType};

use super::download::{
    DownloadProgress, DownloadProgressCallback, DownloadState, MultiPartDownloader, PartState,
};
use super::error::{ManagerError, ManagerResult};
use super::extractor::ShellExtractor;
use super::local::LocalPackageStore;
use super::traits::{ArchiveExtractor, LibraryClient};

/// Progress callback for installation operations.
///
/// # Arguments
///
/// * `stage` - Current installation stage
/// * `progress` - Progress within the stage (0.0 - 1.0)
/// * `message` - Human-readable message
pub type InstallProgressCallback = Box<dyn Fn(InstallStage, f64, &str) + Send + Sync>;

/// Installation stages for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// Fetching package metadata.
    FetchingMetadata,
    /// Downloading archive parts.
    Downloading,
    /// Verifying checksums.
    Verifying,
    /// Reassembling split archive.
    Reassembling,
    /// Extracting archive contents.
    Extracting,
    /// Installing to final location.
    Installing,
    /// Cleaning up temporary files.
    Cleanup,
    /// Installation complete.
    Complete,
}

impl InstallStage {
    /// Get a human-readable name for the stage.
    pub fn name(&self) -> &'static str {
        match self {
            Self::FetchingMetadata => "Fetching metadata",
            Self::Downloading => "Downloading",
            Self::Verifying => "Verifying",
            Self::Reassembling => "Reassembling",
            Self::Extracting => "Extracting",
            Self::Installing => "Installing",
            Self::Cleanup => "Cleaning up",
            Self::Complete => "Complete",
        }
    }

    /// Whether this stage has indeterminate progress (no meaningful percentage).
    ///
    /// Indeterminate stages should show a spinner or similar indicator
    /// rather than a percentage progress bar.
    pub fn is_indeterminate(&self) -> bool {
        matches!(
            self,
            Self::FetchingMetadata
                | Self::Verifying
                | Self::Reassembling
                | Self::Extracting
                | Self::Installing
                | Self::Cleanup
        )
    }
}

/// Result of a package installation.
#[derive(Debug, Clone)]
pub struct InstallResult {
    /// Region of the installed package.
    pub region: String,
    /// Type of the installed package.
    pub package_type: PackageType,
    /// Version of the installed package.
    pub version: semver::Version,
    /// Path where the package was installed.
    pub install_path: PathBuf,
    /// Total bytes downloaded.
    pub bytes_downloaded: u64,
    /// Number of files extracted.
    pub files_extracted: usize,
}

/// Package installer.
///
/// Handles the complete installation workflow including downloading,
/// verification, extraction, and installation.
pub struct PackageInstaller<C: LibraryClient> {
    /// Library client for fetching metadata.
    client: C,
    /// Local package store.
    store: LocalPackageStore,
    /// Directory for temporary download files.
    temp_dir: PathBuf,
    /// Number of parallel downloads.
    parallel_downloads: usize,
    /// Optional per-part download progress callback.
    /// When set, bypasses the aggregate InstallProgressCallback bridge.
    on_download_progress: Option<Arc<DownloadProgressCallback>>,
}

impl<C: LibraryClient> PackageInstaller<C> {
    /// Create a new package installer.
    ///
    /// # Arguments
    ///
    /// * `client` - Library client for fetching metadata
    /// * `store` - Local package store for installation
    /// * `temp_dir` - Directory for temporary files during installation
    pub fn new(client: C, store: LocalPackageStore, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            client,
            store,
            temp_dir: temp_dir.into(),
            parallel_downloads: 4,
            on_download_progress: None,
        }
    }

    /// Set the number of parallel downloads.
    pub fn with_parallel_downloads(mut self, count: usize) -> Self {
        self.parallel_downloads = count.max(1);
        self
    }

    /// Set a per-part download progress callback.
    ///
    /// When set, this callback receives `DownloadProgress` snapshots with
    /// per-part state, bypassing the aggregate `InstallProgressCallback`
    /// bridge for the download stage. Use this for multi-bar UIs.
    pub fn with_download_progress(mut self, cb: DownloadProgressCallback) -> Self {
        self.on_download_progress = Some(Arc::new(cb));
        self
    }

    /// Get the local package store.
    pub fn store(&self) -> &LocalPackageStore {
        &self.store
    }

    /// Install a package from a metadata URL.
    ///
    /// # Arguments
    ///
    /// * `metadata_url` - URL to the package metadata file
    /// * `on_progress` - Optional progress callback
    ///
    /// # Returns
    ///
    /// Information about the installed package.
    pub fn install_from_url(
        &self,
        metadata_url: &str,
        on_progress: Option<InstallProgressCallback>,
    ) -> ManagerResult<InstallResult> {
        // Report progress helper
        let report = |stage: InstallStage, progress: f64, message: &str| {
            if let Some(ref cb) = on_progress {
                cb(stage, progress, message);
            }
        };

        // Stage 1: Fetch metadata
        report(
            InstallStage::FetchingMetadata,
            0.0,
            "Fetching package metadata...",
        );
        let metadata = self.client.fetch_metadata(metadata_url)?;
        report(InstallStage::FetchingMetadata, 1.0, "Metadata fetched");

        // Install using the fetched metadata
        self.install_from_metadata(&metadata, on_progress)
    }

    /// Install a package from already-fetched metadata.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The package metadata
    /// * `on_progress` - Optional progress callback
    ///
    /// # Returns
    ///
    /// Information about the installed package.
    pub fn install_from_metadata(
        &self,
        metadata: &PackageMetadata,
        on_progress: Option<InstallProgressCallback>,
    ) -> ManagerResult<InstallResult> {
        let region = &metadata.title;
        let package_type = metadata.package_type;
        let version = metadata.package_version.clone();

        // Wrap callback in Arc for sharing with download progress
        let on_progress = on_progress.map(Arc::new);

        // Report progress helper
        let report = |stage: InstallStage, progress: f64, message: &str| {
            if let Some(ref cb) = on_progress {
                cb(stage, progress, message);
            }
        };

        // Check if already installed
        if self.store.is_installed(region, package_type) {
            let existing = self.store.get(region, package_type)?;
            if existing.version() == &version {
                return Err(ManagerError::AlreadyInstalled {
                    region: region.to_string(),
                    package_type: package_type.to_string(),
                    version: version.to_string(),
                });
            }
        }

        // Create temp directory for this installation
        let install_temp = self.temp_dir.join(format!(
            "install_{}_{}_{}",
            region.to_lowercase(),
            package_type,
            version
        ));
        fs::create_dir_all(&install_temp).map_err(|e| ManagerError::CreateDirFailed {
            path: install_temp.clone(),
            source: e,
        })?;

        // Prepare download state
        let urls: Vec<String> = metadata.parts.iter().map(|p| p.url.clone()).collect();
        let checksums: Vec<String> = metadata.parts.iter().map(|p| p.checksum.clone()).collect();
        let destinations: Vec<PathBuf> = metadata
            .parts
            .iter()
            .map(|p| install_temp.join(&p.filename))
            .collect();

        // Stage 2: Download all parts
        let mut download_state = DownloadState::new(urls, checksums, destinations.clone());
        let downloader = MultiPartDownloader::with_settings(
            std::time::Duration::from_secs(300),
            self.parallel_downloads,
        );

        // Query sizes via HEAD requests for accurate progress
        report(
            InstallStage::Downloading,
            0.0,
            &format!("Preparing {} parts...", metadata.parts.len()),
        );
        downloader.query_sizes(&mut download_state);

        // Use per-part callback if set, otherwise bridge to aggregate InstallProgressCallback
        let download_progress: Option<DownloadProgressCallback> =
            if let Some(ref dp) = self.on_download_progress {
                Some(Box::new({
                    let dp = Arc::clone(dp);
                    move |progress: &DownloadProgress| dp(progress)
                }))
            } else {
                on_progress.as_ref().map(|cb| {
                    let cb = Arc::clone(cb);
                    let boxed: DownloadProgressCallback =
                        Box::new(move |progress: &DownloadProgress| {
                            let ratio = match progress.total_bytes {
                                Some(total) if total > 0 => {
                                    (progress.total_bytes_downloaded as f64 / total as f64).min(1.0)
                                }
                                _ => {
                                    let done = progress
                                        .parts
                                        .iter()
                                        .filter(|p| matches!(p.state, PartState::Done))
                                        .count();
                                    if progress.parts.is_empty() {
                                        0.0
                                    } else {
                                        done as f64 / progress.parts.len() as f64
                                    }
                                }
                            };
                            let message = format!(
                                "{:.1} / {:.1} MB",
                                progress.total_bytes_downloaded as f64 / 1_048_576.0,
                                progress.total_bytes.unwrap_or(0) as f64 / 1_048_576.0,
                            );
                            cb(InstallStage::Downloading, ratio, &message);
                        });
                    boxed
                })
            };

        downloader.download_all(&mut download_state, download_progress)?;

        let bytes_downloaded = download_state.bytes_downloaded;
        report(
            InstallStage::Downloading,
            1.0,
            &format!("Downloaded {} bytes", bytes_downloaded),
        );

        // Stage 3: Verify (checksums already verified during download)
        report(
            InstallStage::Verifying,
            1.0,
            "Checksums verified during download",
        );

        // Stage 4: Reassemble archive
        report(InstallStage::Reassembling, 0.0, "Reassembling archive...");
        let archive_path = install_temp.join(&metadata.filename);
        let extractor = ShellExtractor::new();
        extractor.reassemble(&destinations, &archive_path)?;
        report(InstallStage::Reassembling, 1.0, "Archive reassembled");

        // Stage 5: Extract archive
        report(InstallStage::Extracting, 0.0, "Extracting archive...");
        let extract_dir = install_temp.join("extracted");
        let files_extracted = extractor.extract(&archive_path, &extract_dir)?;
        report(
            InstallStage::Extracting,
            1.0,
            &format!("Extracted {} files", files_extracted),
        );

        // Stage 6: Install to final location
        report(InstallStage::Installing, 0.0, "Installing package...");
        let install_path = self.store.install_path(region, package_type);

        // Remove existing package if present
        if install_path.exists() {
            fs::remove_dir_all(&install_path).map_err(|e| ManagerError::WriteFailed {
                path: install_path.clone(),
                source: e,
            })?;
        }

        // Move extracted contents to install path
        // The extracted directory should contain the package folder
        self.move_extracted_contents(&extract_dir, &install_path)?;
        report(InstallStage::Installing, 1.0, "Package installed");

        // Stage 7: Cleanup
        report(InstallStage::Cleanup, 0.0, "Cleaning up temporary files...");
        fs::remove_dir_all(&install_temp).ok(); // Best effort cleanup
        report(InstallStage::Cleanup, 1.0, "Cleanup complete");

        report(InstallStage::Complete, 1.0, "Installation complete");

        Ok(InstallResult {
            region: region.to_string(),
            package_type,
            version,
            install_path,
            bytes_downloaded,
            files_extracted,
        })
    }

    /// Move extracted contents to the install path.
    fn move_extracted_contents(
        &self,
        extract_dir: &Path,
        install_path: &Path,
    ) -> ManagerResult<()> {
        // The archive should contain a single top-level directory (the package folder)
        // We need to move its contents to the install path

        let entries: Vec<_> = fs::read_dir(extract_dir)
            .map_err(|e| ManagerError::ReadFailed {
                path: extract_dir.to_path_buf(),
                source: e,
            })?
            .filter_map(|e| e.ok())
            .collect();

        if entries.len() == 1 && entries[0].path().is_dir() {
            // Single directory - this is the package folder, rename it
            let source = entries[0].path();

            // Create parent directory
            if let Some(parent) = install_path.parent() {
                fs::create_dir_all(parent).map_err(|e| ManagerError::CreateDirFailed {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }

            // Try rename first (fast path if same filesystem)
            if fs::rename(&source, install_path).is_err() {
                // Fall back to recursive copy
                copy_dir_recursive(&source, install_path)?;
            }
        } else {
            // Multiple entries or files at root - move them all
            if let Some(parent) = install_path.parent() {
                fs::create_dir_all(parent).map_err(|e| ManagerError::CreateDirFailed {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }

            fs::create_dir_all(install_path).map_err(|e| ManagerError::CreateDirFailed {
                path: install_path.to_path_buf(),
                source: e,
            })?;

            for entry in entries {
                let source = entry.path();
                let dest = install_path.join(entry.file_name());

                if fs::rename(&source, &dest).is_err() {
                    if source.is_dir() {
                        copy_dir_recursive(&source, &dest)?;
                    } else {
                        fs::copy(&source, &dest).map_err(|e| ManagerError::WriteFailed {
                            path: dest,
                            source: e,
                        })?;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Recursively copy a directory.
fn copy_dir_recursive(source: &Path, dest: &Path) -> ManagerResult<()> {
    fs::create_dir_all(dest).map_err(|e| ManagerError::CreateDirFailed {
        path: dest.to_path_buf(),
        source: e,
    })?;

    for entry in fs::read_dir(source).map_err(|e| ManagerError::ReadFailed {
        path: source.to_path_buf(),
        source: e,
    })? {
        let entry = entry.map_err(|e| ManagerError::ReadFailed {
            path: source.to_path_buf(),
            source: e,
        })?;

        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else {
            fs::copy(&source_path, &dest_path).map_err(|e| ManagerError::WriteFailed {
                path: dest_path,
                source: e,
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_stage_name() {
        assert_eq!(InstallStage::FetchingMetadata.name(), "Fetching metadata");
        assert_eq!(InstallStage::Downloading.name(), "Downloading");
        assert_eq!(InstallStage::Verifying.name(), "Verifying");
        assert_eq!(InstallStage::Reassembling.name(), "Reassembling");
        assert_eq!(InstallStage::Extracting.name(), "Extracting");
        assert_eq!(InstallStage::Installing.name(), "Installing");
        assert_eq!(InstallStage::Cleanup.name(), "Cleaning up");
        assert_eq!(InstallStage::Complete.name(), "Complete");
    }

    #[test]
    fn test_install_stage_equality() {
        assert_eq!(InstallStage::Downloading, InstallStage::Downloading);
        assert_ne!(InstallStage::Downloading, InstallStage::Extracting);
    }

    #[test]
    fn test_install_stage_is_indeterminate() {
        // Indeterminate stages - no meaningful percentage
        assert!(InstallStage::FetchingMetadata.is_indeterminate());
        assert!(InstallStage::Verifying.is_indeterminate());
        assert!(InstallStage::Reassembling.is_indeterminate());
        assert!(InstallStage::Extracting.is_indeterminate());
        assert!(InstallStage::Installing.is_indeterminate());
        assert!(InstallStage::Cleanup.is_indeterminate());

        // Determinate stages - show percentage progress
        assert!(!InstallStage::Downloading.is_indeterminate());
        assert!(!InstallStage::Complete.is_indeterminate());
    }

    #[test]
    fn test_copy_dir_recursive() {
        use tempfile::TempDir;

        let source_temp = TempDir::new().unwrap();
        let dest_temp = TempDir::new().unwrap();

        // Create source structure
        fs::write(source_temp.path().join("file1.txt"), "hello").unwrap();
        let subdir = source_temp.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("file2.txt"), "world").unwrap();

        let dest = dest_temp.path().join("copied");
        copy_dir_recursive(source_temp.path(), &dest).unwrap();

        // Verify
        assert!(dest.join("file1.txt").exists());
        assert!(dest.join("subdir").is_dir());
        assert!(dest.join("subdir/file2.txt").exists());

        let content = fs::read_to_string(dest.join("file1.txt")).unwrap();
        assert_eq!(content, "hello");
    }
}
