//! Test doubles for the packages command handlers.
//!
//! The handlers depend only on the three traits in `traits`, so a fake of each
//! is enough to drive any handler with no network and no filesystem. The
//! fakes record what was asked of them, so tests assert on outcomes (which
//! packages were installed or removed, which prompts were shown) rather than
//! on call sequences.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use semver::Version;
use xearthlayer::manager::{
    InstallResult, InstalledPackage, LocalPackageStore, ManagerError, ManagerResult, PackageInfo,
    PackageStatus,
};
use xearthlayer_package::{LibraryEntry, PackageLibrary, PackageMetadata, PackageType};

use super::traits::{Output, PackageManagerService, UserInteraction};
use crate::error::CliError;

/// Output that keeps every line, for asserting on what the user saw.
#[derive(Default)]
pub struct RecordingOutput {
    lines: Mutex<Vec<String>>,
}

impl RecordingOutput {
    pub fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

impl Output for RecordingOutput {
    fn println(&self, message: &str) {
        self.lines.lock().unwrap().push(message.to_string());
    }

    fn print(&self, message: &str) {
        self.println(message);
    }
}

/// Interaction that gives one fixed answer to every prompt and records them.
pub struct ScriptedInteraction {
    answer: bool,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedInteraction {
    pub fn answering(answer: bool) -> Self {
        Self {
            answer,
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl UserInteraction for ScriptedInteraction {
    fn confirm(&self, message: &str) -> bool {
        self.prompts.lock().unwrap().push(message.to_string());
        self.answer
    }

    fn read_line(&self) -> Option<String> {
        None
    }
}

/// A package manager backed by in-memory fixtures.
#[derive(Default)]
pub struct FakeManager {
    library: PackageLibrary,
    installed: Vec<InstalledPackage>,
    statuses: Vec<(PackageInfo, PackageStatus)>,
    /// "TITLE (type)" of packages whose install fails.
    failing: Vec<String>,
    /// Each `install_packages` call, as "TITLE (type)" per package.
    batches: Mutex<Vec<Vec<String>>>,
    removals: Mutex<Vec<String>>,
    /// Titles whose removal is refused, as when the queue lock is held.
    refused_removals: Vec<String>,
    /// The pid of a running `xearthlayer run`, when there is one.
    running: Option<u32>,
    /// Checking for updates fails.
    failing_update_check: bool,
    /// Listing the installed packages panics.
    panicking_list: bool,
    /// Creating a store first waits for a message on this, once, like a
    /// slow filesystem.
    held_store: Mutex<Option<Receiver<()>>>,
}

impl FakeManager {
    /// Offer `titles` in the library, one package of `package_type` each.
    pub fn with_library(mut self, package_type: PackageType, titles: &[&str]) -> Self {
        for title in titles {
            self.library.entries.push(LibraryEntry::new(
                "checksum",
                package_type,
                *title,
                Version::new(1, 0, 0),
                metadata_url(title, package_type),
            ));
        }
        self
    }

    /// Have `titles` installed as `package_type`.
    pub fn with_installed(mut self, package_type: PackageType, titles: &[&str]) -> Self {
        for title in titles {
            self.installed.push(installed(title, package_type));
        }
        self
    }

    /// Hold the next `create_store` until `release` is sent to or dropped.
    pub fn holding_create_store(self, release: Receiver<()>) -> Self {
        *self.held_store.lock().unwrap() = Some(release);
        self
    }

    /// Report another `xearthlayer run` serving packages as process `pid`.
    pub fn with_running_instance(mut self, pid: u32) -> Self {
        self.running = Some(pid);
        self
    }

    /// Report `title` as installed at 1.0.0 with 2.0.0 available.
    pub fn with_update(self, package_type: PackageType, title: &str) -> Self {
        self.with_library(package_type, &[title]).with_status(
            package_type,
            title,
            PackageStatus::UpdateAvailable {
                installed: Version::new(1, 0, 0),
                available: Version::new(2, 0, 0),
            },
        )
    }

    /// Report `title` with exactly `status` when updates are checked.
    pub fn with_status(
        mut self,
        package_type: PackageType,
        title: &str,
        status: PackageStatus,
    ) -> Self {
        self.statuses.push((
            PackageInfo {
                region: title.to_string(),
                package_type,
                status: status.clone(),
                library_entry: None,
            },
            status,
        ));
        self
    }

    /// Make checking for updates fail, as an unreadable install folder does.
    pub fn with_failing_update_check(mut self) -> Self {
        self.failing_update_check = true;
        self
    }

    /// Make listing the installed packages panic, as a bug in a read would.
    pub fn panicking_on_list(mut self) -> Self {
        self.panicking_list = true;
        self
    }

    /// Make installing `title` as `package_type` fail.
    pub fn failing_on(mut self, title: &str, package_type: PackageType) -> Self {
        self.failing.push(format!("{title} ({package_type})"));
        self
    }

    /// Make removing `title` fail with `QueueBusy`, as a held queue lock does.
    pub fn refusing_removal_of(mut self, title: &str) -> Self {
        self.refused_removals.push(title.to_string());
        self
    }

    /// Titles installed, in order, including failed attempts.
    pub fn installs(&self) -> Vec<String> {
        self.batches()
            .into_iter()
            .flatten()
            .map(|p| p.split(" (").next().unwrap().to_string())
            .collect()
    }

    /// Each batch installed, as "TITLE (type)" per package.
    pub fn batches(&self) -> Vec<Vec<String>> {
        self.batches.lock().unwrap().clone()
    }

    /// Titles removed, in order.
    pub fn removals(&self) -> Vec<String> {
        self.removals.lock().unwrap().clone()
    }
}

impl PackageManagerService for FakeManager {
    fn create_store(&self, install_dir: &Path) -> LocalPackageStore {
        let held = self.held_store.lock().unwrap().take();
        if let Some(release) = held {
            let _ = release.recv();
        }
        LocalPackageStore::new(install_dir)
    }

    fn fetch_library(&self, _url: &str) -> ManagerResult<PackageLibrary> {
        Ok(self.library.clone())
    }

    fn fetch_metadata(&self, url: &str) -> ManagerResult<PackageMetadata> {
        let entry = self
            .library
            .entries
            .iter()
            .find(|e| e.metadata_url == url)
            .expect("metadata requested for an entry the library offers");
        Ok(metadata(&entry.title, entry.package_type))
    }

    fn try_check_updates(
        &self,
        _store: &LocalPackageStore,
        _library: &PackageLibrary,
    ) -> ManagerResult<Vec<(PackageInfo, PackageStatus)>> {
        if self.failing_update_check {
            return Err(ManagerError::ReadFailed {
                path: install_dir(),
                source: std::io::Error::other("unreadable"),
            });
        }
        Ok(self.statuses.clone())
    }

    fn install_packages(
        &self,
        packages: &[PackageMetadata],
        install_dir: &Path,
        _temp_dir: &Path,
        _concurrent_downloads: usize,
    ) -> Vec<Result<InstallResult, CliError>> {
        self.batches.lock().unwrap().push(
            packages
                .iter()
                .map(|m| format!("{} ({})", m.title, m.package_type))
                .collect(),
        );
        packages
            .iter()
            .map(|metadata| {
                let key = format!("{} ({})", metadata.title, metadata.package_type);
                if self.failing.contains(&key) {
                    return Err(CliError::Packages(format!("{key} failed")));
                }
                Ok(InstallResult {
                    region: metadata.title.clone(),
                    package_type: metadata.package_type,
                    version: metadata.package_version.clone(),
                    install_path: install_dir.join(&metadata.title),
                    bytes_downloaded: 0,
                    files_extracted: 0,
                })
            })
            .collect()
    }

    fn remove_package(
        &self,
        _store: &LocalPackageStore,
        region: &str,
        _package_type: PackageType,
        temp_dir: &Path,
    ) -> ManagerResult<()> {
        if self.refused_removals.iter().any(|t| t == region) {
            return Err(ManagerError::QueueBusy {
                path: temp_dir.join(".queue.lock"),
            });
        }
        self.removals.lock().unwrap().push(region.to_string());
        Ok(())
    }

    fn list_packages(&self, _store: &LocalPackageStore) -> ManagerResult<Vec<InstalledPackage>> {
        assert!(
            !self.panicking_list,
            "listing the installed packages blew up"
        );
        Ok(self.installed.clone())
    }

    fn running_instance(&self) -> Option<u32> {
        self.running
    }

    fn instance_lock(&self) -> PathBuf {
        PathBuf::from(INSTANCE_LOCK)
    }
}

/// Where the fake's instance lock is.
pub const INSTANCE_LOCK: &str = "/nonexistent/state/xearthlayer.lock";

/// The refusal a `FakeManager` running as `pid` gives.
pub fn run_active(pid: u32) -> crate::commands::packages::actions::RunActive {
    crate::commands::packages::actions::RunActive {
        pid,
        lock: PathBuf::from(INSTANCE_LOCK),
    }
}

pub fn install_dir() -> PathBuf {
    PathBuf::from("/nonexistent/packages")
}

fn metadata_url(title: &str, package_type: PackageType) -> String {
    format!("https://example.invalid/{title}/{package_type}")
}

fn metadata(title: &str, package_type: PackageType) -> PackageMetadata {
    PackageMetadata {
        spec_version: Version::new(1, 0, 0),
        title: title.to_string(),
        package_version: Version::new(1, 0, 0),
        published_at: chrono::Utc::now(),
        package_type,
        mountpoint: title.to_string(),
        filename: format!("{title}.tar.gz"),
        parts: Vec::new(),
    }
}

fn installed(title: &str, package_type: PackageType) -> InstalledPackage {
    InstalledPackage {
        metadata: metadata(title, package_type),
        path: install_dir().join(title),
        size_bytes: 1024,
    }
}
