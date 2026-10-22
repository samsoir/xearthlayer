//! Package installer for downloading and installing packages.
//!
//! Installing a package has two halves with different bottlenecks:
//!
//! 1. **Download** (network bound): fetch every archive part into the
//!    package's own temp directory, verifying checksums as parts arrive.
//! 2. **Install** (disk bound): extract the parts into a staging folder beside
//!    the install path, then swap it in.
//!
//! [`PackageInstaller`] does the work of each stage as the [`Stages`] of an
//! [`InstallQueue`](super::InstallQueue), which downloads packages one after
//! another while installing them in order (#292).

use std::ffi::OsStr;
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
use super::queue::{Capacity, InstallContext, Stages};

/// Timeout for downloading a single archive part.
const PART_TIMEOUT: Duration = Duration::from_secs(300);

/// Parts downloaded in parallel unless `with_parallel_downloads` says otherwise.
const DEFAULT_PARALLEL_DOWNLOADS: usize = 4;

/// The file every published package carries at its top, naming its version.
const PACKAGE_METADATA_FILE: &str = "xearthlayer_scenery_package.txt";

/// Start of the name of each package's temp directory.
pub(crate) const INSTALL_TEMP_PREFIX: &str = "install_";

/// Stages of installing a package whose parts have downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// Extracting the archive parts into the staging folder.
    Extracting,
    /// Swapping the staged package into place.
    Installing,
}

impl InstallStage {
    /// Get a human-readable name for the stage.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Extracting => "Extracting",
            Self::Installing => "Installing",
        }
    }
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
pub struct PreparedPackage {
    metadata: PackageMetadata,
    temp: InstallTempGuard,
    state: DownloadState,
}

/// A package whose parts are all on disk with verified checksums.
pub struct DownloadedPackage {
    metadata: PackageMetadata,
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

    /// The device and free bytes of the filesystem holding `folder`,
    /// creating the folder first so a fresh install can be measured.
    fn filesystem_of(&self, folder: &Path) -> ManagerResult<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;

        fs::create_dir_all(folder).map_err(|e| ManagerError::CreateDirFailed {
            path: folder.to_path_buf(),
            source: e,
        })?;
        let device = fs::metadata(folder)
            .map_err(|e| ManagerError::ReadFailed {
                path: folder.to_path_buf(),
                source: e,
            })?
            .dev();
        let free = available_bytes(self.fs_info_provider.as_ref(), folder)?;
        Ok((device, free))
    }
}

impl Stages for PackageInstaller {
    type Prepared = PreparedPackage;
    type Downloaded = DownloadedPackage;

    fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    fn packages_root(&self) -> &Path {
        self.store.root()
    }

    fn capacity(&self) -> ManagerResult<Capacity> {
        let temp = self.filesystem_of(&self.temp_dir)?;
        let packages = self.filesystem_of(self.store.root())?;
        Ok(Capacity::from_filesystems(temp, packages))
    }

    /// Validate `metadata`, create its temp directory and size its parts.
    fn prepare(&self, metadata: &PackageMetadata) -> ManagerResult<PreparedPackage> {
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
            "{INSTALL_TEMP_PREFIX}{}_{}_{}",
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

        Ok(PreparedPackage {
            metadata: metadata.clone(),
            temp,
            state,
        })
    }

    fn download_size(&self, prepared: &PreparedPackage) -> u64 {
        prepared.state.total_size
    }

    /// Download every part of a prepared package.
    fn download(
        &self,
        prepared: PreparedPackage,
        cancel: &CancelToken,
        progress: DownloadProgressCallback,
    ) -> ManagerResult<DownloadedPackage> {
        let PreparedPackage {
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
        self.transfer
            .download_all(&mut state, Some(progress), cancel)?;
        info!(
            region = %metadata.title,
            bytes_downloaded = state.bytes_downloaded,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "download phase completed"
        );

        Ok(DownloadedPackage {
            metadata,
            temp,
            parts: state.destinations,
            bytes_downloaded: state.bytes_downloaded,
        })
    }

    /// Extract and install a downloaded package. Its temp directory is
    /// removed when this returns, whatever the outcome.
    fn install(
        &self,
        downloaded: DownloadedPackage,
        ctx: &dyn InstallContext,
    ) -> ManagerResult<InstallResult> {
        let DownloadedPackage {
            metadata,
            temp: _temp,
            parts,
            bytes_downloaded,
        } = downloaded;
        let region = &metadata.title;
        let package_type = metadata.package_type;

        ctx.stage(InstallStage::Extracting);
        let install_path = self.store.install_path(region, package_type);
        let staged = stage(&install_path)?;
        let files_extracted =
            TarGzExtractor::new().extract_parts(&parts, staged.path(), ctx.cancel_token())?;
        info!(region = %region, files_extracted, "extraction completed");
        let source = package_source(staged.path(), install_path.file_name())?;
        // An archive that is empty or was cut short must not replace a
        // working install: refuse before anything is set aside. `staged`
        // drops and the old version is untouched.
        if !source.join(PACKAGE_METADATA_FILE).is_file() {
            return Err(ManagerError::ExtractionFailed {
                path: staged.path().to_path_buf(),
                reason: format!("the archive holds no package (no {PACKAGE_METADATA_FILE})"),
            });
        }
        if !ctx.begin_swap() {
            // `staged` drops here, which removes its folder.
            return Err(ManagerError::Cancelled);
        }
        ctx.stage(InstallStage::Installing);
        let replaced = swap_in(staged, &source, &install_path)?;
        // The renames are done; deleting the old version may be abandoned.
        ctx.end_swap();
        if let Some(old) = replaced {
            aside::discard(&old);
        }

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

    fn remove(&self, region: &str, package_type: PackageType) -> ManagerResult<()> {
        self.store.remove(region, package_type)
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

/// Create an empty staging folder beside `install_path`, and the packages
/// root if it is missing, ready to extract into.
///
/// Extracting beside the install path writes the tree once, on the
/// filesystem it ends up on, and makes the swap a same-filesystem rename.
/// A staging folder left by an earlier run is deleted first; if it cannot be
/// deleted the install stops rather than mix its leftovers into the new
/// version.
fn stage(install_path: &Path) -> ManagerResult<aside::Staged> {
    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent).map_err(|e| ManagerError::CreateDirFailed {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let staging = aside::staging_path(install_path);
    if staging.exists() {
        aside::discard(&staging);
        if staging.exists() {
            return Err(ManagerError::WriteFailed {
                path: staging,
                source: std::io::Error::other(
                    "a folder left by an earlier install could not be deleted",
                ),
            });
        }
    }
    fs::create_dir(&staging).map_err(|e| ManagerError::CreateDirFailed {
        path: staging.clone(),
        source: e,
    })?;
    Ok(aside::Staged::new(staging))
}

/// The folder inside `staged` that is the package.
///
/// The archive should contain a single top-level directory, the package
/// folder, named like the install path. A directory of that name is the
/// package whatever else sits beside it: Finder writes a `.DS_Store` into
/// any folder it displays, including the staging folder mid-install (#303).
/// Failing that, a lone directory is the package; an archive with files at
/// its top level makes `staged` itself the package.
fn package_source(staged: &Path, package_name: Option<&OsStr>) -> ManagerResult<PathBuf> {
    let entries: Vec<_> = fs::read_dir(staged)
        .map_err(|e| ManagerError::ReadFailed {
            path: staged.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .collect();

    // `file_type` does not follow symlinks, so a link named like the package
    // is never chosen: it could point anywhere.
    let is_dir = |e: &fs::DirEntry| e.file_type().is_ok_and(|t| t.is_dir());
    let named_package = entries
        .iter()
        .find(|e| Some(e.file_name().as_os_str()) == package_name && is_dir(e));
    let source = match named_package {
        Some(entry) => entry.path(),
        None if entries.len() == 1 && is_dir(&entries[0]) => entries[0].path(),
        None => staged.to_path_buf(),
    };
    Ok(source)
}

/// Swap the package at `source`, inside `staged`, into `install_path`,
/// replacing any older version so that one of the two is always in place.
///
/// Returns where the replaced version was set aside, if there was one. The
/// caller discards it: that delete is slow and need not hold up the swap.
/// `staged` drops on return and removes what is left of the staging folder
/// (nothing, or files that sat beside the package folder).
fn swap_in(
    staged: aside::Staged,
    source: &Path,
    install_path: &Path,
) -> ManagerResult<Option<PathBuf>> {
    let replaced = aside::set_aside(install_path)?;
    if let Err(e) = fs::rename(source, install_path) {
        aside::restore(install_path, replaced.as_deref());
        return Err(ManagerError::WriteFailed {
            path: install_path.to_path_buf(),
            source: e,
        });
    }
    drop(staged);
    Ok(replaced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;
    use xearthlayer_package::ArchivePart;

    use crate::manager::queue::{
        InstallContext, InstallQueue, JobEvent, JobId, QueueObserver, Stages, LOCK_FILE_NAME,
    };
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

    /// Records the events each job passes through as "<event> <region>".
    #[derive(Default)]
    struct Recorder(Mutex<(HashMap<JobId, String>, Vec<String>)>);

    impl Recorder {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().1.clone()
        }
    }

    impl QueueObserver for Recorder {
        fn on_event(&self, id: JobId, event: &JobEvent<'_>) {
            let mut recorded = self.0.lock().unwrap();
            let (regions, events) = &mut *recorded;
            let name = match event {
                JobEvent::Queued(summary) => {
                    regions.insert(id, summary.region.clone());
                    return;
                }
                JobEvent::Preparing => "download",
                JobEvent::Installing(stage) => stage.name(),
                JobEvent::Finished(_) => "Finished",
                _ => return,
            };
            let region = regions.get(&id).cloned().unwrap_or_default();
            events.push(format!("{name} {region}"));
        }
    }

    /// Install `batch` through a queue, one result per package.
    fn install_all(
        installer: PackageInstaller,
        batch: &[PackageMetadata],
        recorder: Arc<Recorder>,
    ) -> Vec<ManagerResult<InstallResult>> {
        InstallQueue::start(installer, recorder)
            .unwrap()
            .install_batch(batch.to_vec())
    }

    /// An install context driven by the test rather than a queue.
    struct TestContext {
        token: CancelToken,
        allow_swap: bool,
    }

    impl InstallContext for TestContext {
        fn stage(&self, _stage: InstallStage) {}

        fn cancel_token(&self) -> &CancelToken {
            &self.token
        }

        fn begin_swap(&self) -> bool {
            self.allow_swap
        }

        fn end_swap(&self) {}
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
        package_at(title, semver::Version::new(1, 0, 0), transfer)
    }

    /// `package` at `version`, which its archive's metadata file records.
    fn package_at(
        title: &str,
        version: semver::Version,
        transfer: &mut LocalTransfer,
    ) -> PackageMetadata {
        package_archive(title, version, transfer, true)
    }

    /// `package_at`, with the archive's entries inside the package folder
    /// when `in_folder` is set, or at the archive's root when not.
    fn package_archive(
        title: &str,
        version: semver::Version,
        transfer: &mut LocalTransfer,
        in_folder: bool,
    ) -> PackageMetadata {
        let mountpoint = xearthlayer_package::package_mountpoint(title, PackageType::Ortho);
        let url = format!("test://{title}");
        let filename = format!("{mountpoint}-{version}.tar.gz");
        let metadata = PackageMetadata {
            spec_version: semver::Version::new(1, 0, 0),
            title: title.to_string(),
            package_version: version,
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
            let entry = if in_folder {
                format!("{mountpoint}/{name}")
            } else {
                name.to_string()
            };
            builder
                .append_data(&mut header, entry, body.as_bytes())
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

        let results = install_all(installer(&temp, transfer, u64::MAX), &batch, Arc::default());

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
        let recorder = Arc::new(Recorder::default());

        install_all(
            installer(&temp, transfer, u64::MAX),
            &batch,
            Arc::clone(&recorder),
        );

        let events = recorder.events();
        for title in ["AF-NORTH", "AF-SOUTH"] {
            let at = |event: &str| {
                let event = format!("{event} {title}");
                events
                    .iter()
                    .position(|e| *e == event)
                    .unwrap_or_else(|| panic!("no '{event}' in {events:?}"))
            };
            assert!(at("download") < at("Extracting"));
            assert!(at("Extracting") < at("Installing"));
            assert!(at("Installing") < at("Finished"));
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

        let results = install_all(installer(&temp, transfer, u64::MAX), &batch, Arc::default());

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

        let results = install_all(
            installer(&temp, transfer, u64::MAX),
            &[hostile, south],
            Arc::default(),
        );

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
        let first = install_all(
            installer(&temp, transfer.clone(), u64::MAX),
            std::slice::from_ref(&north),
            Arc::default(),
        );
        assert!(first[0].is_ok(), "{:?}", first[0]);

        let again = install_all(
            installer(&temp, transfer, u64::MAX),
            &[north],
            Arc::default(),
        );

        assert!(
            matches!(again[0], Err(ManagerError::AlreadyInstalled { .. })),
            "{:?}",
            again[0]
        );
        assert_eq!(downloaded.lock().unwrap().len(), 1);
    }

    /// The installed package's files, to compare before and after.
    fn snapshot(install_path: &Path) -> Vec<Vec<u8>> {
        ["terrain/tile.ter", "xearthlayer_scenery_package.txt"]
            .iter()
            .map(|name| fs::read(install_path.join(name)).unwrap())
            .collect()
    }

    /// Install v1 of AF-NORTH through a queue, then download v2 of it
    /// without installing it. Returns the v2 download and v1's install path.
    fn v1_installed_and_v2_downloaded(
        temp: &TempDir,
    ) -> (PackageInstaller, DownloadedPackage, PathBuf) {
        let mut transfer = LocalTransfer::default();
        let v1 = package("AF-NORTH", &mut transfer);
        let first = install_all(
            installer(temp, transfer.clone(), u64::MAX),
            &[v1],
            Arc::default(),
        );
        let install_path = first[0].as_ref().unwrap().install_path.clone();

        let v2 = package_at("AF-NORTH", semver::Version::new(2, 0, 0), &mut transfer);
        let installer = installer(temp, transfer, u64::MAX);
        let prepared = Stages::prepare(&installer, &v2).unwrap();
        let downloaded = installer
            .download(prepared, &CancelToken::new(), Box::new(|_| {}))
            .unwrap();
        (installer, downloaded, install_path)
    }

    /// Spec section 3: cancelling before the swap leaves the old version untouched.
    #[test]
    fn a_cancelled_install_leaves_the_installed_version_byte_for_byte() {
        let temp = TempDir::new().unwrap();
        let (installer, downloaded, install_path) = v1_installed_and_v2_downloaded(&temp);
        let v1 = snapshot(&install_path);
        let token = CancelToken::new();
        token.cancel();
        let context = TestContext {
            token,
            allow_swap: true,
        };

        let result = installer.install(downloaded, &context);

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
        assert_eq!(snapshot(&install_path), v1);
        assert_eq!(
            fs::read_to_string(install_path.join("terrain/tile.ter")).unwrap(),
            "terrain for AF-NORTH"
        );
        assert!(!aside::staging_path(&install_path).exists());
        let leftovers: Vec<_> = fs::read_dir(temp.path().join("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != LOCK_FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The installer calls `begin_swap` and honours a refusal.
    #[test]
    fn an_install_refused_its_swap_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let (installer, downloaded, install_path) = v1_installed_and_v2_downloaded(&temp);
        let v1 = snapshot(&install_path);
        let context = TestContext {
            token: CancelToken::new(),
            allow_swap: false,
        };

        let result = installer.install(downloaded, &context);

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
        assert_eq!(snapshot(&install_path), v1);
        assert!(!aside::staging_path(&install_path).exists());
        assert!(!aside::aside_path(&install_path).exists());
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

        // One at a time needs its tree, 3 * 50, plus its 50 of parts: 200.
        let results = install_all(installer(&temp, transfer, 199), &batch, Arc::default());

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
        let leftovers: Vec<_> = fs::read_dir(temp.path().join("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != LOCK_FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_batch_whose_second_tree_the_disk_cannot_hold_installs_only_the_first() {
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

        // The first tree stays on the disk: 150 installed, plus the second
        // package's 150 tree and 50 of parts, is 350.
        let results = install_all(installer(&temp, transfer, 200), &batch, Arc::default());

        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok(), "{:?}", results[0]);
        match &results[1] {
            Err(ManagerError::InsufficientDiskSpace {
                required_bytes,
                available_bytes,
                ..
            }) => {
                assert_eq!((*required_bytes, *available_bytes), (350, 200));
            }
            other => panic!("expected InsufficientDiskSpace, got {other:?}"),
        }
        assert_eq!(*downloaded.lock().unwrap(), ["test://AF-NORTH"]);
    }

    #[test]
    fn capacity_is_shared_for_folders_on_one_filesystem() {
        let temp = TempDir::new().unwrap();
        let installer = installer(&temp, LocalTransfer::default(), 1_234);

        assert_eq!(
            installer.capacity().unwrap(),
            Capacity::Shared { free: 1_234 }
        );
        assert!(installer.temp_dir().is_dir());
        assert!(installer.packages_root().is_dir());
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

        install_all(installer(&temp, transfer, u64::MAX), &batch, Arc::default());

        let leftovers: Vec<_> = fs::read_dir(temp.path().join("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != LOCK_FILE_NAME)
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

        let results = install_all(
            installer(&temp, transfer, u64::MAX),
            &[hostile],
            Arc::default(),
        );

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

        let results = install_all(
            installer(&temp, transfer, u64::MAX),
            &[hostile],
            Arc::default(),
        );

        assert!(
            matches!(results[0], Err(ManagerError::InvalidPath(_))),
            "{:?}",
            results[0]
        );
        assert!(downloaded.lock().unwrap().is_empty());
        assert!(!target.exists());
    }

    /// #303: Finder writes a `.DS_Store` into any folder it displays, so a
    /// Finder window on the staging folder during an install puts one beside
    /// the package folder. The package folder must still be the source, not
    /// the staging folder, or the package would be nested inside a folder of
    /// its own name, where nothing finds it. macOS's own files, but nothing
    /// here needs macOS to reproduce.
    #[test]
    fn a_finder_ds_store_beside_the_package_folder_is_not_the_package_source() {
        let temp = TempDir::new().unwrap();
        let mountpoint = "zzXEL_af-north_ortho";
        let staged = temp.path().join(".xel-staging-zzXEL_af-north_ortho");
        fs::create_dir_all(staged.join(mountpoint).join("terrain")).unwrap();
        fs::write(staged.join(".DS_Store"), "finder").unwrap();
        fs::write(staged.join(format!("._{mountpoint}")), "appledouble").unwrap();

        let source = package_source(&staged, Some(OsStr::new(mountpoint))).unwrap();

        assert_eq!(source, staged.join(mountpoint));
    }

    #[test]
    fn a_lone_folder_of_another_name_is_the_package_source() {
        let temp = TempDir::new().unwrap();
        let staged = temp.path().join("staged");
        fs::create_dir_all(staged.join("renamed_package")).unwrap();

        let source = package_source(&staged, Some(OsStr::new("zzXEL_af-north_ortho"))).unwrap();

        assert_eq!(source, staged.join("renamed_package"));
    }

    #[test]
    fn files_at_the_top_level_make_the_staging_folder_the_package_source() {
        let temp = TempDir::new().unwrap();
        let staged = temp.path().join("staged");
        fs::create_dir_all(staged.join("terrain")).unwrap();
        fs::write(staged.join("xearthlayer_scenery_package.txt"), "meta").unwrap();

        let source = package_source(&staged, Some(OsStr::new("zzXEL_af-north_ortho"))).unwrap();

        assert_eq!(source, staged);
    }

    #[test]
    fn staging_creates_an_empty_folder_beside_the_install_path() {
        let temp = TempDir::new().unwrap();
        // Neither the packages root nor the install path exists yet.
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");

        let staged = stage(&install_path).unwrap();

        assert_eq!(staged.path(), aside::staging_path(&install_path));
        assert!(staged.path().is_dir());
        assert_eq!(fs::read_dir(staged.path()).unwrap().count(), 0);
        assert!(!install_path.exists());
    }

    #[test]
    fn staging_replaces_a_stale_staging_folder_with_an_empty_one() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        let stale = aside::staging_path(&install_path);
        fs::create_dir_all(stale.join("terrain")).unwrap();
        fs::write(stale.join("terrain/tile.ter"), "stale").unwrap();

        let staged = stage(&install_path).unwrap();

        assert_eq!(fs::read_dir(staged.path()).unwrap().count(), 0);
    }

    /// A stale staging folder that cannot be deleted must stop the install,
    /// the same rule `LocalPackageStore::remove` applies to a stale aside:
    /// extracting into it would mix its leftovers into the new version.
    #[test]
    fn a_stale_staging_folder_that_cannot_be_deleted_stops_the_stage() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();
        // A read-only folder inside the staging folder keeps its files
        // undeletable.
        let locked = aside::staging_path(&install_path).join("terrain");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("tile.ter"), "stale").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let deletable = fs::remove_file(locked.join("tile.ter")).is_ok();

        let result = if deletable {
            None // running as root: permissions cannot induce the failure
        } else {
            Some(stage(&install_path))
        };

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let Some(result) = result else { return };
        assert!(
            matches!(result, Err(ManagerError::WriteFailed { .. })),
            "{result:?}"
        );
        assert_eq!(
            fs::read_to_string(install_path.join("old.txt")).unwrap(),
            "old"
        );
    }

    #[test]
    fn a_swap_replaces_the_old_version_and_hands_back_its_aside_to_discard() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();

        let staged = stage(&install_path).unwrap();
        let staged_path = staged.path().to_path_buf();
        fs::create_dir_all(staged_path.join("zzXEL_af-north_ortho")).unwrap();
        fs::write(staged_path.join("zzXEL_af-north_ortho/new.txt"), "new").unwrap();
        let source = package_source(&staged_path, install_path.file_name()).unwrap();
        assert!(install_path.join("old.txt").exists());
        let replaced = swap_in(staged, &source, &install_path).unwrap();

        assert!(install_path.join("new.txt").exists());
        assert!(!install_path.join("old.txt").exists());
        assert!(!staged_path.exists());
        let old = replaced.expect("the old version was set aside");
        assert_eq!(old, aside::aside_path(&install_path));
        assert_eq!(fs::read_to_string(old.join("old.txt")).unwrap(), "old");
        aside::discard(&old);
        assert!(!aside::aside_path(&install_path).exists());
    }

    /// Extraction is where an install now fails, after staging and before
    /// the swap: the old version stays and the staging folder goes.
    #[test]
    fn an_extraction_that_fails_leaves_the_old_version_and_no_staging_folder() {
        let temp = TempDir::new().unwrap();
        let (installer, downloaded, install_path) = v1_installed_and_v2_downloaded(&temp);
        let v1 = snapshot(&install_path);
        fs::write(&downloaded.parts[0], b"this is not a gzip stream").unwrap();
        let context = TestContext {
            token: CancelToken::new(),
            allow_swap: true,
        };

        let result = installer.install(downloaded, &context);

        assert!(
            matches!(result, Err(ManagerError::ExtractionFailed { .. })),
            "{result:?}"
        );
        assert_eq!(snapshot(&install_path), v1);
        assert!(!aside::staging_path(&install_path).exists());
        assert!(!aside::aside_path(&install_path).exists());
    }

    /// Replace the downloaded part with a valid archive holding only `names`.
    fn replace_archive_with(downloaded: &DownloadedPackage, names: &[&str]) {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for name in names {
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &b"text"[..])
                .unwrap();
        }
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        fs::write(&downloaded.parts[0], bytes).unwrap();
    }

    /// An archive that holds no package must not replace a working install
    /// with an empty folder: the old version stays, the staging folder goes.
    #[test]
    fn an_archive_without_package_metadata_leaves_the_old_version_untouched() {
        let temp = TempDir::new().unwrap();
        let (installer, downloaded, install_path) = v1_installed_and_v2_downloaded(&temp);
        let v1 = snapshot(&install_path);
        replace_archive_with(&downloaded, &["README"]);
        let context = TestContext {
            token: CancelToken::new(),
            allow_swap: true,
        };

        let result = installer.install(downloaded, &context);

        match result {
            Err(ManagerError::ExtractionFailed { path, reason }) => {
                assert_eq!(path, aside::staging_path(&install_path));
                assert!(reason.contains("holds no package"), "{reason}");
            }
            other => panic!("expected ExtractionFailed, got {other:?}"),
        }
        assert_eq!(snapshot(&install_path), v1);
        assert!(!aside::staging_path(&install_path).exists());
        assert!(!aside::aside_path(&install_path).exists());
    }

    /// A top-level symlink named like the package is not a package folder,
    /// whatever it points at: nothing outside staging may be renamed into
    /// the install path.
    #[test]
    fn a_symlink_named_like_the_package_is_not_the_package_source() {
        let temp = TempDir::new().unwrap();
        let mountpoint = "zzXEL_af-north_ortho";
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let staged = temp.path().join("staged");
        fs::create_dir_all(&staged).unwrap();
        std::os::unix::fs::symlink(&elsewhere, staged.join(mountpoint)).unwrap();
        fs::write(staged.join("README"), "text").unwrap();

        let source = package_source(&staged, Some(OsStr::new(mountpoint))).unwrap();

        assert_eq!(source, staged);
    }

    /// A lone symlink is no more a package folder than a named one.
    #[test]
    fn a_lone_symlink_to_a_folder_is_not_the_package_source() {
        let temp = TempDir::new().unwrap();
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let staged = temp.path().join("staged");
        fs::create_dir_all(&staged).unwrap();
        std::os::unix::fs::symlink(&elsewhere, staged.join("renamed_package")).unwrap();

        let source = package_source(&staged, Some(OsStr::new("zzXEL_af-north_ortho"))).unwrap();

        assert_eq!(source, staged);
    }

    #[test]
    fn a_swap_whose_rename_fails_puts_the_old_version_back() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();
        // The old version is already set aside when this rename fails.
        let missing = temp.path().join("Custom Scenery/.xel-staging-missing");
        let staged = aside::Staged::new(missing.clone());

        assert!(swap_in(staged, &missing, &install_path).is_err());

        assert_eq!(
            fs::read_to_string(install_path.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!aside::aside_path(&install_path).exists());
    }

    /// #303 through the swap: Finder's files beside the package folder in
    /// staging neither nest the package nor reach the install path, and they
    /// leave with the staging folder.
    #[test]
    fn a_staged_install_with_finder_files_beside_the_package_does_not_nest_it() {
        let temp = TempDir::new().unwrap();
        let mountpoint = "zzXEL_af-north_ortho";
        let install_path = temp.path().join("Custom Scenery").join(mountpoint);
        let staged = stage(&install_path).unwrap();
        let staged_path = staged.path().to_path_buf();
        let package_dir = staged_path.join(mountpoint);
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(package_dir.join("xearthlayer_scenery_package.txt"), "meta").unwrap();
        fs::write(staged_path.join(".DS_Store"), "finder").unwrap();
        fs::write(staged_path.join(format!("._{mountpoint}")), "appledouble").unwrap();

        let source = package_source(&staged_path, install_path.file_name()).unwrap();
        let replaced = swap_in(staged, &source, &install_path).unwrap();

        assert_eq!(replaced, None, "nothing was installed before");
        assert_eq!(
            fs::read_to_string(install_path.join("xearthlayer_scenery_package.txt")).unwrap(),
            "meta"
        );
        assert!(
            !install_path.join(mountpoint).exists(),
            "package was nested"
        );
        assert!(!install_path.join(".DS_Store").exists());
        assert!(!staged_path.exists());
    }

    #[test]
    fn a_staged_package_dropped_without_swapping_is_removed() {
        let temp = TempDir::new().unwrap();
        let install_path = temp.path().join("Custom Scenery/zzXEL_af-north_ortho");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("old.txt"), "old").unwrap();

        let staged = stage(&install_path).unwrap();
        let staged_path = staged.path().to_path_buf();
        fs::create_dir_all(staged_path.join("zzXEL_af-north_ortho")).unwrap();
        fs::write(staged_path.join("zzXEL_af-north_ortho/new.txt"), "new").unwrap();
        drop(staged);

        assert!(!staged_path.exists());
        assert_eq!(
            fs::read_to_string(install_path.join("old.txt")).unwrap(),
            "old"
        );
    }

    /// What `PeekingContext::begin_swap` saw: whether staging held the new
    /// version's tree, and whether the package's temp directory was empty.
    type Peek = (bool, bool);

    /// An install context that looks at the disk at the moment the swap is
    /// claimed, then allows it.
    struct PeekingContext {
        token: CancelToken,
        install_path: PathBuf,
        package_temp: PathBuf,
        new_metadata: String,
        seen: RefCell<Option<Peek>>,
    }

    impl InstallContext for PeekingContext {
        fn stage(&self, _stage: InstallStage) {}

        fn cancel_token(&self) -> &CancelToken {
            &self.token
        }

        fn begin_swap(&self) -> bool {
            let staging = aside::staging_path(&self.install_path);
            let package = [
                staging.clone(),
                staging.join(self.install_path.file_name().unwrap()),
            ]
            .into_iter()
            .find(|root| root.join("terrain/tile.ter").is_file());
            let staged_is_new = package.is_some_and(|root| {
                fs::read_to_string(root.join("xearthlayer_scenery_package.txt")).ok()
                    == Some(self.new_metadata.clone())
            });
            let temp_is_empty = fs::read_dir(&self.package_temp)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            *self.seen.borrow_mut() = Some((staged_is_new, temp_is_empty));
            true
        }

        fn end_swap(&self) {}
    }

    /// The names in the packages root that only exist mid-replacement.
    fn transient_folders(packages_root: &Path) -> Vec<std::ffi::OsString> {
        fs::read_dir(packages_root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| aside::is_transient(name))
            .collect()
    }

    /// The tree is written once, beside its destination: at the moment the
    /// swap is claimed it is complete inside the staging folder and temp holds
    /// no extracted tree.
    #[test]
    fn an_install_extracts_into_staging_beside_the_install_path() {
        let temp = TempDir::new().unwrap();
        let (installer, downloaded, install_path) = v1_installed_and_v2_downloaded(&temp);
        let new_metadata = xearthlayer_package::serialize_package_metadata(&downloaded.metadata);
        let parts = downloaded.parts.clone();
        let context = PeekingContext {
            token: CancelToken::new(),
            install_path: install_path.clone(),
            package_temp: parts[0].parent().unwrap().to_path_buf(),
            new_metadata: new_metadata.clone(),
            seen: RefCell::new(None),
        };

        installer.install(downloaded, &context).unwrap();

        let (staged_is_new, temp_is_empty) = context.seen.borrow().expect("the swap was claimed");
        assert!(staged_is_new, "staging held the new version at the swap");
        assert!(
            temp_is_empty,
            "temp held no extracted tree, archive or part at the swap"
        );
        assert!(parts.iter().all(|part| !part.exists()));
        assert_eq!(
            fs::read_to_string(install_path.join("xearthlayer_scenery_package.txt")).unwrap(),
            new_metadata
        );
        assert!(install_path.join("terrain/tile.ter").is_file());
        let left = transient_folders(install_path.parent().unwrap());
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn an_archive_with_its_files_at_the_top_level_installs() {
        let temp = TempDir::new().unwrap();
        let mut transfer = LocalTransfer::default();
        let flat = package_archive(
            "AF-NORTH",
            semver::Version::new(1, 0, 0),
            &mut transfer,
            false,
        );

        let results = install_all(
            installer(&temp, transfer, u64::MAX),
            &[flat],
            Arc::default(),
        );

        let result = results[0].as_ref().unwrap();
        assert_eq!(result.files_extracted, 2);
        let install_path = &result.install_path;
        assert_eq!(
            fs::read_to_string(install_path.join("terrain/tile.ter")).unwrap(),
            "terrain for AF-NORTH"
        );
        assert!(install_path
            .join("xearthlayer_scenery_package.txt")
            .is_file());
        assert!(!install_path
            .join(install_path.file_name().unwrap())
            .exists());
        let left = transient_folders(install_path.parent().unwrap());
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn test_install_stage_name() {
        assert_eq!(InstallStage::Extracting.name(), "Extracting");
        assert_eq!(InstallStage::Installing.name(), "Installing");
    }
}
