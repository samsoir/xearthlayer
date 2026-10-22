//! Package installer for downloading and installing packages.
//!
//! Installing a package has two halves with different bottlenecks:
//!
//! 1. **Download** (network bound): fetch every archive part into the
//!    package's own temp directory, verifying checksums as parts arrive.
//! 2. **Install** (disk bound): reassemble the parts, extract the archive and
//!    move the extracted tree into the package store.
//!
//! [`PackageInstaller::install_all`] runs a batch through the [`pipeline`], so
//! one package installs while the next one downloads (#292). Installing a
//! single package is a batch of one.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::info;

use xearthlayer_package::{PackageMetadata, PackageType};

use super::aside;
use super::cancel::CancelToken;
use super::disk_check::{available_bytes, FsInfoProvider, RealFsInfoProvider};
use super::download::{DownloadProgressCallback, DownloadState, MultiPartDownloader};
use super::error::{ManagerError, ManagerResult};
use super::extractor::TarGzExtractor;
use super::install_guard::InstallTempGuard;
use super::local::LocalPackageStore;
use super::pipeline;
use super::traits::ArchiveExtractor;

/// Timeout for downloading a single archive part.
const PART_TIMEOUT: Duration = Duration::from_secs(300);

/// Parts downloaded in parallel unless `with_parallel_downloads` says otherwise.
const DEFAULT_PARALLEL_DOWNLOADS: usize = 4;

/// Stages of installing a package whose parts have downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// Joining the downloaded parts into one archive.
    Reassembling,
    /// Extracting archive contents.
    Extracting,
    /// Moving the extracted tree into the package store.
    Installing,
    /// Installation complete.
    Complete,
}

impl InstallStage {
    /// Get a human-readable name for the stage.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Reassembling => "Reassembling",
            Self::Extracting => "Extracting",
            Self::Installing => "Installing",
            Self::Complete => "Complete",
        }
    }
}

/// Receives progress from [`PackageInstaller::install_all`].
///
/// A package's download reports from the calling thread while an earlier
/// package installs on a worker thread, so both methods can be called at the
/// same time.
pub trait InstallObserver: Send + Sync {
    /// `package` is about to download. The returned callback, if any,
    /// receives its per-part progress.
    fn download_started(&self, package: &PackageMetadata) -> Option<DownloadProgressCallback>;

    /// `package`'s install has reached `stage`.
    fn install_stage(&self, package: &PackageMetadata, stage: InstallStage);
}

/// The network half of a download: sizing and fetching archive parts.
///
/// A seam rather than a plugin point: production always uses
/// [`MultiPartDownloader`], and tests substitute local files.
pub(crate) trait PartTransfer: Send + Sync {
    /// Record each part's size, and their total, before anything downloads.
    fn query_sizes(&self, state: &mut DownloadState);

    /// Download every part to its destination, verifying checksums.
    fn download_all(
        &self,
        state: &mut DownloadState,
        on_progress: Option<DownloadProgressCallback>,
        cancel: &CancelToken,
    ) -> ManagerResult<()>;
}

impl PartTransfer for MultiPartDownloader {
    fn query_sizes(&self, state: &mut DownloadState) {
        MultiPartDownloader::query_sizes(self, state);
    }

    fn download_all(
        &self,
        state: &mut DownloadState,
        on_progress: Option<DownloadProgressCallback>,
        cancel: &CancelToken,
    ) -> ManagerResult<()> {
        MultiPartDownloader::download_all(self, state, on_progress, cancel)
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

/// A package ready to download: validated, its temp directory created and
/// its parts sized.
struct Prepared<'a> {
    metadata: &'a PackageMetadata,
    temp: InstallTempGuard,
    state: DownloadState,
}

/// A package whose parts are all on disk with verified checksums.
struct Downloaded<'a> {
    metadata: &'a PackageMetadata,
    temp: InstallTempGuard,
    parts: Vec<PathBuf>,
    bytes_downloaded: u64,
}

/// Package installer.
///
/// Handles the complete installation workflow including downloading,
/// verification, extraction, and installation.
pub struct PackageInstaller {
    /// Local package store.
    store: LocalPackageStore,
    /// Directory for temporary download files.
    temp_dir: PathBuf,
    /// Fetches archive parts.
    transfer: Box<dyn PartTransfer>,
    /// Filesystem info provider for the pre-flight disk-space check.
    /// Defaults to `RealFsInfoProvider`; tests can substitute a stub
    /// via `with_fs_info_provider`.
    fs_info_provider: Box<dyn FsInfoProvider>,
}

impl PackageInstaller {
    /// Create a new package installer.
    ///
    /// # Arguments
    ///
    /// * `store` - Local package store for installation
    /// * `temp_dir` - Directory for temporary files during installation
    pub fn new(store: LocalPackageStore, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            store,
            temp_dir: temp_dir.into(),
            transfer: Box::new(MultiPartDownloader::with_settings(
                PART_TIMEOUT,
                DEFAULT_PARALLEL_DOWNLOADS,
            )),
            fs_info_provider: Box::new(RealFsInfoProvider),
        }
    }

    /// Override the filesystem-info provider used by the pre-flight
    /// disk-space check. Production callers don't need this; tests use
    /// it to inject low-available stubs.
    #[cfg(test)]
    pub fn with_fs_info_provider(mut self, provider: Box<dyn FsInfoProvider>) -> Self {
        self.fs_info_provider = provider;
        self
    }

    /// Serve archive parts from somewhere other than the network.
    #[cfg(test)]
    fn with_transfer(mut self, transfer: Box<dyn PartTransfer>) -> Self {
        self.transfer = transfer;
        self
    }

    /// Set the number of parts downloaded in parallel.
    pub fn with_parallel_downloads(mut self, count: usize) -> Self {
        self.transfer = Box::new(MultiPartDownloader::with_settings(
            PART_TIMEOUT,
            count.max(1),
        ));
        self
    }

    /// Install every package in `packages`, returning one result per package
    /// in the same order.
    ///
    /// Each package is validated and sized first, then the peak disk usage of
    /// the whole batch is checked once, before anything downloads. Packages
    /// then install while the next one downloads, or one at a time when only
    /// that fits (see [`pipeline::schedule_for`]). A package that fails at any
    /// point fails alone.
    pub fn install_all(
        &self,
        packages: &[PackageMetadata],
        observer: &dyn InstallObserver,
    ) -> Vec<ManagerResult<InstallResult>> {
        let mut ready = Vec::new();
        let refusals: Vec<Option<ManagerError>> = packages
            .iter()
            .map(|metadata| match self.prepare(metadata) {
                Ok(prepared) => {
                    ready.push(prepared);
                    None
                }
                Err(e) => Some(e),
            })
            .collect();

        let mut installed = self.run_ready(ready, observer).into_iter();
        refusals
            .into_iter()
            .map(|refusal| match refusal {
                None => installed.next().expect("one result per prepared package"),
                Some(e) => Err(e),
            })
            .collect()
    }

    /// Check the batch's disk peak, then download and install every prepared
    /// package. Results are in `ready` order.
    fn run_ready(
        &self,
        ready: Vec<Prepared<'_>>,
        observer: &dyn InstallObserver,
    ) -> Vec<ManagerResult<InstallResult>> {
        if ready.is_empty() {
            return Vec::new();
        }

        let refuse_all =
            |reason: &dyn Fn() -> ManagerError| ready.iter().map(|_| Err(reason())).collect();
        let available = match available_bytes(self.fs_info_provider.as_ref(), &self.temp_dir) {
            Ok(available) => available,
            Err(e) => {
                let reason = e.to_string();
                return refuse_all(&|| ManagerError::InvalidPath(reason.clone()));
            }
        };
        let sizes: Vec<u64> = ready.iter().map(|p| p.state.total_size).collect();
        let schedule = match pipeline::schedule_for(&sizes, available) {
            Ok(schedule) => schedule,
            Err(required_bytes) => {
                return refuse_all(&|| ManagerError::InsufficientDiskSpace {
                    path: self.temp_dir.clone(),
                    required_bytes,
                    available_bytes: available,
                });
            }
        };

        info!(
            packages = ready.len(),
            ?schedule,
            available_bytes = available,
            "batch install started"
        );
        pipeline::run(
            ready,
            schedule,
            |prepared| self.download(prepared, observer),
            |downloaded| self.install(downloaded, observer),
        )
    }

    /// Validate `metadata`, create its temp directory and size its parts.
    fn prepare<'a>(&self, metadata: &'a PackageMetadata) -> ManagerResult<Prepared<'a>> {
        let region = &metadata.title;
        // The title becomes the install directory name, and metadata reaches
        // us over HTTP. The parser refuses an unusable title, but
        // PackageMetadata is publicly constructible, so the write path checks
        // for itself rather than trusting its caller.
        xearthlayer_package::validate_region(region)
            .map_err(|e| ManagerError::InvalidPath(e.to_string()))?;
        // The archive and part names are joined onto the temp directory, and
        // arrive by the same route as the title.
        validate_file_name(&metadata.filename)?;
        for part in &metadata.parts {
            validate_file_name(&part.filename)?;
        }
        let package_type = metadata.package_type;
        let version = &metadata.package_version;

        if self.store.is_installed(region, package_type) {
            let existing = self.store.get(region, package_type)?;
            if existing.version() == version {
                return Err(ManagerError::AlreadyInstalled {
                    region: region.to_string(),
                    package_type: package_type.to_string(),
                    version: version.to_string(),
                });
            }
        }

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
        // From here every exit, including a panic, removes the temp
        // directory: the guard travels with the package until it is
        // installed or dropped. See issue #187.
        let temp = InstallTempGuard::new(install_temp.clone());

        let mut state = DownloadState::new(
            metadata.parts.iter().map(|p| p.url.clone()).collect(),
            metadata.parts.iter().map(|p| p.checksum.clone()).collect(),
            metadata
                .parts
                .iter()
                .map(|p| install_temp.join(&p.filename))
                .collect(),
        );
        self.transfer.query_sizes(&mut state);

        Ok(Prepared {
            metadata,
            temp,
            state,
        })
    }

    /// Download every part of a prepared package.
    fn download<'a>(
        &self,
        prepared: Prepared<'a>,
        observer: &dyn InstallObserver,
    ) -> ManagerResult<Downloaded<'a>> {
        let Prepared {
            metadata,
            temp,
            mut state,
        } = prepared;

        info!(
            region = %metadata.title,
            parts = metadata.parts.len(),
            total_bytes = state.total_size,
            "download phase started"
        );
        let started = Instant::now();
        self.transfer.download_all(
            &mut state,
            observer.download_started(metadata),
            &CancelToken::new(),
        )?;
        info!(
            region = %metadata.title,
            bytes_downloaded = state.bytes_downloaded,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "download phase completed"
        );

        Ok(Downloaded {
            metadata,
            temp,
            parts: state.destinations,
            bytes_downloaded: state.bytes_downloaded,
        })
    }

    /// Reassemble, extract and install a downloaded package. Its temp
    /// directory is removed when this returns, whatever the outcome.
    fn install(
        &self,
        downloaded: Downloaded<'_>,
        observer: &dyn InstallObserver,
    ) -> ManagerResult<InstallResult> {
        let Downloaded {
            metadata,
            temp,
            parts,
            bytes_downloaded,
        } = downloaded;
        let report = |stage| observer.install_stage(metadata, stage);
        let region = &metadata.title;
        let package_type = metadata.package_type;

        report(InstallStage::Reassembling);
        let extractor = TarGzExtractor::new();
        let archive = temp.path().join(&metadata.filename);
        extractor.reassemble(&parts, &archive, &CancelToken::new())?;
        // Free the parts before extracting rather than at cleanup. They are
        // redundant now and extraction is where peak disk usage lives, so
        // holding them through it costs a full extra copy of the download
        // (#260).
        let freed_bytes = free_downloaded_parts(&parts);
        info!(region = %region, freed_bytes, "reassembly completed, archive parts freed");

        report(InstallStage::Extracting);
        let extract_dir = temp.path().join("extracted");
        let files_extracted = extractor.extract(&archive, &extract_dir)?;
        info!(region = %region, files_extracted, "extraction completed");

        report(InstallStage::Installing);
        let install_path = self.store.install_path(region, package_type);
        swap_in(&extract_dir, &install_path)?;
        report(InstallStage::Complete);

        info!(
            region = %region,
            package_type = %package_type,
            version = %metadata.package_version,
            install_path = %install_path.display(),
            bytes_downloaded,
            files_extracted,
            "package install completed"
        );
        Ok(InstallResult {
            region: region.to_string(),
            package_type,
            version: metadata.package_version.clone(),
            install_path,
            bytes_downloaded,
            files_extracted,
        })
    }
}

/// Refuse a metadata-supplied file name unless it is a single plain path
/// component. Anything else, joined onto a directory, can land outside it:
/// `..` climbs out, and an absolute name replaces the directory outright.
fn validate_file_name(name: &str) -> ManagerResult<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(ManagerError::InvalidPath(format!(
            "unsafe file name in package metadata: {name:?}"
        ))),
    }
}

/// Move the extracted package to `install_path`, replacing any older
/// version so that one of the two is always in place.
fn swap_in(extract_dir: &Path, install_path: &Path) -> ManagerResult<()> {
    let replaced = aside::set_aside(install_path)?;
    if let Err(e) = move_extracted_contents(extract_dir, install_path) {
        aside::restore(install_path, replaced.as_deref());
        return Err(e);
    }
    if let Some(old) = replaced {
        aside::discard(&old);
    }
    Ok(())
}

/// Move extracted contents to the install path.
///
/// The archive should contain a single top-level directory, the package
/// folder, named like the install path. A directory of that name is the
/// package whatever else sits beside it: Finder writes a `.DS_Store` into
/// any folder it displays, including `extracted/` mid-install (#303).
fn move_extracted_contents(extract_dir: &Path, install_path: &Path) -> ManagerResult<()> {
    let entries: Vec<_> = fs::read_dir(extract_dir)
        .map_err(|e| ManagerError::ReadFailed {
            path: extract_dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .collect();

    let named_package = entries
        .iter()
        .find(|e| Some(e.file_name().as_os_str()) == install_path.file_name() && e.path().is_dir());
    let package_folder = match named_package {
        Some(entry) => Some(entry),
        None if entries.len() == 1 && entries[0].path().is_dir() => Some(&entries[0]),
        None => None,
    };

    if let Some(package_folder) = package_folder {
        let source = package_folder.path();

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

/// Remove the downloaded archive parts, returning the bytes reclaimed.
///
/// Called once reassembly has succeeded, when the parts are redundant: their
/// checksums were verified during download and their contents are now in the
/// reassembled archive. Leaving them in place meant the temp filesystem held
/// the parts, the archive and the extracted tree at the same time, which is a
/// whole extra copy of the download at the point of peak usage (#260).
///
/// Best effort. A part that cannot be removed is logged and skipped: the
/// archive is already built, and `InstallTempGuard` removes whatever survives
/// when the install finishes or fails. Failing an otherwise good install
/// because a file could not be unlinked would be the wrong trade.
fn free_downloaded_parts(parts: &[PathBuf]) -> u64 {
    let mut freed = 0u64;
    for part in parts {
        let size = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(part) {
            Ok(()) => freed += size,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                info!(part = %part.display(), error = %e, "could not free archive part");
            }
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;
    use xearthlayer_package::ArchivePart;

    use crate::system::FilesystemInfo;

    /// Serves archive parts from memory instead of the network.
    #[derive(Clone, Default)]
    struct LocalTransfer {
        /// Bytes served for each part URL.
        parts: HashMap<String, Vec<u8>>,
        /// Size reported for every part when set, otherwise its real size.
        declared_size: Option<u64>,
        /// URLs whose download fails.
        failing: Vec<String>,
        /// URLs downloaded, in order, shared with the test.
        downloaded: Arc<Mutex<Vec<String>>>,
    }

    impl PartTransfer for LocalTransfer {
        fn query_sizes(&self, state: &mut DownloadState) {
            state.total_size = state
                .urls
                .iter()
                .map(|url| {
                    let real = self.parts.get(url).map_or(0, |p| p.len() as u64);
                    self.declared_size.unwrap_or(real)
                })
                .sum();
        }

        fn download_all(
            &self,
            state: &mut DownloadState,
            _on_progress: Option<DownloadProgressCallback>,
            _cancel: &CancelToken,
        ) -> ManagerResult<()> {
            for (url, dest) in state.urls.iter().zip(&state.destinations) {
                if self.failing.contains(url) {
                    return Err(ManagerError::DownloadFailed {
                        url: url.clone(),
                        reason: "connection reset".to_string(),
                    });
                }
                let bytes = &self.parts[url];
                fs::write(dest, bytes).unwrap();
                state.bytes_downloaded += bytes.len() as u64;
                self.downloaded.lock().unwrap().push(url.clone());
            }
            Ok(())
        }
    }

    /// Records every observer event as "<event> <title>".
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Recorder {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl InstallObserver for Recorder {
        fn download_started(&self, package: &PackageMetadata) -> Option<DownloadProgressCallback> {
            self.0
                .lock()
                .unwrap()
                .push(format!("download {}", package.title));
            None
        }

        fn install_stage(&self, package: &PackageMetadata, stage: InstallStage) {
            self.0
                .lock()
                .unwrap()
                .push(format!("{} {}", stage.name(), package.title));
        }
    }

    /// Reports a fixed amount of free space.
    struct FreeSpace(u64);

    impl FsInfoProvider for FreeSpace {
        fn fs_info(&self, _path: &Path) -> io::Result<FilesystemInfo> {
            Ok(FilesystemInfo {
                total_bytes: u64::MAX,
                available_bytes: self.0,
            })
        }
    }

    /// Add a one-part ortho package for `title` to `transfer`. Like a
    /// published package, its archive holds the package's metadata file
    /// alongside its content, `<mountpoint>/terrain/tile.ter`.
    fn package(title: &str, transfer: &mut LocalTransfer) -> PackageMetadata {
        let mountpoint = xearthlayer_package::package_mountpoint(title, PackageType::Ortho);
        let url = format!("test://{title}");
        let filename = format!("{mountpoint}-1.0.0.tar.gz");
        let metadata = PackageMetadata {
            spec_version: semver::Version::new(1, 0, 0),
            title: title.to_string(),
            package_version: semver::Version::new(1, 0, 0),
            published_at: chrono::Utc::now(),
            package_type: PackageType::Ortho,
            mountpoint: mountpoint.clone(),
            parts: vec![ArchivePart::new(
                "unused",
                format!("{filename}.aa"),
                url.clone(),
            )],
            filename,
        };

        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        let files = [
            (
                "xearthlayer_scenery_package.txt",
                xearthlayer_package::serialize_package_metadata(&metadata),
            ),
            ("terrain/tile.ter", format!("terrain for {title}")),
        ];
        for (name, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{mountpoint}/{name}"), body.as_bytes())
                .unwrap();
        }
        transfer
            .parts
            .insert(url, builder.into_inner().unwrap().finish().unwrap());
        metadata
    }

    fn installer(temp: &TempDir, transfer: LocalTransfer, free: u64) -> PackageInstaller {
        PackageInstaller::new(
            LocalPackageStore::new(temp.path().join("Custom Scenery")),
            temp.path().join("tmp"),
        )
        .with_transfer(Box::new(transfer))
        .with_fs_info_provider(Box::new(FreeSpace(free)))
    }

    #[test]
    fn a_batch_installs_every_package() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];

        let results =
            installer(&temp, transfer, u64::MAX).install_all(&batch, &Recorder::default());

        assert_eq!(results.len(), 2);
        for (result, title) in results.iter().zip(["AF-NORTH", "AF-SOUTH"]) {
            let result = result.as_ref().unwrap();
            assert_eq!(result.region, title);
            assert_eq!(result.files_extracted, 2);
            assert_eq!(
                fs::read_to_string(result.install_path.join("terrain/tile.ter")).unwrap(),
                format!("terrain for {title}")
            );
        }
    }

    #[test]
    fn the_observer_hears_each_download_and_every_install_stage() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];
        let recorder = Recorder::default();

        installer(&temp, transfer, u64::MAX).install_all(&batch, &recorder);

        let events = recorder.events();
        for title in ["AF-NORTH", "AF-SOUTH"] {
            let at = |event: &str| {
                let event = format!("{event} {title}");
                events
                    .iter()
                    .position(|e| *e == event)
                    .unwrap_or_else(|| panic!("no '{event}' in {events:?}"))
            };
            assert!(at("download") < at("Reassembling"));
            assert!(at("Reassembling") < at("Extracting"));
            assert!(at("Extracting") < at("Installing"));
            assert!(at("Installing") < at("Complete"));
        }
    }

    #[test]
    fn a_failed_download_does_not_stop_the_batch() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];
        transfer.failing.push("test://AF-NORTH".to_string());

        let results =
            installer(&temp, transfer, u64::MAX).install_all(&batch, &Recorder::default());

        assert!(
            matches!(results[0], Err(ManagerError::DownloadFailed { .. })),
            "{:?}",
            results[0]
        );
        assert!(results[1].is_ok(), "{:?}", results[1]);
    }

    /// A hostile package metadata file could name a title that escapes the
    /// Custom Scenery directory once `package_mountpoint` joins it into a
    /// path, because that function prefixes only the first path component.
    /// The metadata parser now refuses such a title, but `PackageMetadata` is
    /// publicly constructible, so the install path itself is checked too.
    #[test]
    fn a_title_that_escapes_the_scenery_directory_is_refused_and_the_batch_continues() {
        let temp = TempDir::new().unwrap();
        let victim = temp.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), b"keep").unwrap();
        let mut transfer = LocalTransfer::default();
        let mut hostile = package("AF-NORTH", &mut transfer);
        hostile.title = "../victim".to_string();
        let south = package("AF-SOUTH", &mut transfer);
        let downloaded = Arc::clone(&transfer.downloaded);

        let results = installer(&temp, transfer, u64::MAX)
            .install_all(&[hostile, south], &Recorder::default());

        assert!(
            matches!(results[0], Err(ManagerError::InvalidPath(_))),
            "{:?}",
            results[0]
        );
        assert!(results[1].is_ok(), "{:?}", results[1]);
        assert!(victim.join("keep.txt").exists());
        assert_eq!(*downloaded.lock().unwrap(), ["test://AF-SOUTH"]);
    }

    #[test]
    fn an_installed_version_is_refused_without_downloading_it_again() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let north = package("AF-NORTH", &mut transfer);
        let downloaded = Arc::clone(&transfer.downloaded);
        let first = installer(&temp, transfer.clone(), u64::MAX)
            .install_all(std::slice::from_ref(&north), &Recorder::default());
        assert!(first[0].is_ok(), "{:?}", first[0]);

        let again =
            installer(&temp, transfer, u64::MAX).install_all(&[north], &Recorder::default());

        assert!(
            matches!(again[0], Err(ManagerError::AlreadyInstalled { .. })),
            "{:?}",
            again[0]
        );
        assert_eq!(downloaded.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_batch_that_cannot_install_even_one_package_at_a_time_downloads_nothing() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer {
            declared_size: Some(50),
            ..LocalTransfer::default()
        };
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];
        let downloaded = Arc::clone(&transfer.downloaded);

        // One at a time needs 4 * 50 = 200.
        let results = installer(&temp, transfer, 199).install_all(&batch, &Recorder::default());

        for result in &results {
            match result {
                Err(ManagerError::InsufficientDiskSpace {
                    required_bytes,
                    available_bytes,
                    ..
                }) => {
                    assert_eq!((*required_bytes, *available_bytes), (200, 199));
                }
                other => panic!("expected InsufficientDiskSpace, got {other:?}"),
            }
        }
        assert!(downloaded.lock().unwrap().is_empty());
        assert_eq!(fs::read_dir(temp.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn a_batch_that_only_fits_one_package_at_a_time_still_installs() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer {
            declared_size: Some(50),
            ..LocalTransfer::default()
        };
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];

        // Overlapped needs 4 * 50 + 50 = 250; one at a time needs 200.
        let results = installer(&temp, transfer, 200).install_all(&batch, &Recorder::default());

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
    }

    #[test]
    fn nothing_is_left_in_the_temp_directory() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let batch = [
            package("AF-NORTH", &mut transfer),
            package("AF-SOUTH", &mut transfer),
        ];
        transfer.failing.push("test://AF-SOUTH".to_string());

        installer(&temp, transfer, u64::MAX).install_all(&batch, &Recorder::default());

        let leftovers: Vec<_> = fs::read_dir(temp.path().join("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Part and archive file names come from metadata fetched over HTTP and
    /// are joined onto the package's temp directory, so `../x` would write
    /// outside it and an absolute name would replace the directory entirely.
    /// The checksums come from the same file, so they cannot catch it.
    #[test]
    fn only_a_single_plain_path_component_is_a_valid_file_name() {
        assert!(validate_file_name("zzXEL_af-north_ortho-1.0.0.tar.gz.aa").is_ok());
        for hostile in [
            "",
            ".",
            "..",
            "../x.aa",
            "a/../../x.aa",
            "sub/x.aa",
            "/etc/x.aa",
        ] {
            assert!(
                matches!(
                    validate_file_name(hostile),
                    Err(ManagerError::InvalidPath(_))
                ),
                "{hostile:?} must be refused"
            );
        }
    }

    #[test]
    fn a_part_file_name_that_escapes_the_temp_directory_is_refused_before_downloading() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let mut hostile = package("AF-NORTH", &mut transfer);
        hostile.parts[0].filename = "../../escaped.aa".to_string();
        let downloaded = Arc::clone(&transfer.downloaded);

        let results =
            installer(&temp, transfer, u64::MAX).install_all(&[hostile], &Recorder::default());

        assert!(
            matches!(results[0], Err(ManagerError::InvalidPath(_))),
            "{:?}",
            results[0]
        );
        assert!(downloaded.lock().unwrap().is_empty());
        assert!(!temp.path().join("escaped.aa").exists());
    }

    #[test]
    fn an_archive_file_name_that_escapes_the_temp_directory_is_refused_before_downloading() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let mut hostile = package("AF-NORTH", &mut transfer);
        let target = temp.path().join("escaped.tar.gz");
        hostile.filename = target.to_string_lossy().into_owned();
        let downloaded = Arc::clone(&transfer.downloaded);

        let results =
            installer(&temp, transfer, u64::MAX).install_all(&[hostile], &Recorder::default());

        assert!(
            matches!(results[0], Err(ManagerError::InvalidPath(_))),
            "{:?}",
            results[0]
        );
        assert!(downloaded.lock().unwrap().is_empty());
        assert!(!target.exists());
    }

    /// #303: Finder writes a `.DS_Store` into any folder it displays, so a
    /// Finder window on the temp directory during an install puts one beside
    /// the package folder in `extracted/`. The package must still land at the
    /// install path, not nested inside a folder of its own name, where nothing
    /// finds it. macOS's own files, but nothing here needs macOS to reproduce.
    #[test]
    fn a_finder_ds_store_beside_the_package_folder_does_not_nest_the_package() {
        let temp = TempDir::new().unwrap();
        let mountpoint = "zzXEL_af-north_ortho";
        let extract_dir = temp.path().join("extracted");
        let package_dir = extract_dir.join(mountpoint);
        fs::create_dir_all(package_dir.join("terrain")).unwrap();
        fs::write(package_dir.join("xearthlayer_scenery_package.txt"), "meta").unwrap();
        fs::write(package_dir.join("terrain/tile.ter"), "terrain").unwrap();
        fs::write(extract_dir.join(".DS_Store"), "finder").unwrap();
        fs::write(extract_dir.join(format!("._{mountpoint}")), "appledouble").unwrap();
        let install_path = temp.path().join("Custom Scenery").join(mountpoint);

        move_extracted_contents(&extract_dir, &install_path).unwrap();

        assert_eq!(
            fs::read_to_string(install_path.join("xearthlayer_scenery_package.txt")).unwrap(),
            "meta"
        );
        assert!(install_path.join("terrain/tile.ter").exists());
        assert!(
            !install_path.join(mountpoint).exists(),
            "package was nested"
        );
    }

    #[test]
    fn a_swap_replaces_the_old_version_and_leaves_nothing_aside() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();
        let extract_dir = temp.path().join("extracted");
        fs::create_dir_all(extract_dir.join("zzXEL_af-north_ortho")).unwrap();
        fs::write(extract_dir.join("zzXEL_af-north_ortho/new.txt"), "new").unwrap();

        swap_in(&extract_dir, &install_path).unwrap();

        assert!(install_path.join("new.txt").exists());
        assert!(!install_path.join("old.txt").exists());
        assert!(!crate::manager::aside::aside_path(&install_path).exists());
    }

    #[test]
    fn a_swap_that_fails_part_way_puts_the_old_version_back() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();
        // A missing extract directory makes the move fail after the old
        // version is already set aside.
        let extract_dir = temp.path().join("missing");

        assert!(swap_in(&extract_dir, &install_path).is_err());

        assert_eq!(
            fs::read_to_string(install_path.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!crate::manager::aside::aside_path(&install_path).exists());
    }

    #[test]
    fn test_install_stage_name() {
        assert_eq!(InstallStage::Reassembling.name(), "Reassembling");
        assert_eq!(InstallStage::Extracting.name(), "Extracting");
        assert_eq!(InstallStage::Installing.name(), "Installing");
        assert_eq!(InstallStage::Complete.name(), "Complete");
    }

    /// #260: the parts stay on disk through reassembly and extraction, so the
    /// temp filesystem briefly holds the parts, the reassembled archive AND the
    /// extracted tree. Freeing the parts as soon as the archive exists removes
    /// a whole copy of the download from the peak.
    #[test]
    fn free_downloaded_parts_removes_every_part_and_reports_bytes_freed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut parts = Vec::new();
        for (i, len) in [10usize, 20, 30].iter().enumerate() {
            let path = temp.path().join(format!("archive.tar.gz.a{i}"));
            std::fs::write(&path, vec![b'x'; *len]).expect("write part");
            parts.push(path);
        }

        let freed = free_downloaded_parts(&parts);

        assert_eq!(freed, 60, "should report the total bytes reclaimed");
        for part in &parts {
            assert!(
                !part.exists(),
                "{} should have been removed",
                part.display()
            );
        }
    }

    /// A part that is already gone must not fail the install: the archive is
    /// built and verified by this point, and the temp guard removes whatever
    /// survives. Reclaiming space is best effort.
    #[test]
    fn free_downloaded_parts_tolerates_a_missing_part() {
        let temp = tempfile::tempdir().expect("tempdir");
        let present = temp.path().join("archive.tar.gz.aa");
        std::fs::write(&present, vec![b'x'; 5]).expect("write part");
        let absent = temp.path().join("archive.tar.gz.ab");

        let freed = free_downloaded_parts(&[present.clone(), absent]);

        assert_eq!(freed, 5, "counts only what it actually removed");
        assert!(!present.exists());
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
