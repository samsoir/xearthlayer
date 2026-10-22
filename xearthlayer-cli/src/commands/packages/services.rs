//! Concrete implementations of the service traits.
//!
//! These implementations wrap the actual xearthlayer manager functions,
//! adapting them to the trait interfaces used by handlers.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use super::progress::ConsoleQueueObserver;
use super::traits::{Output, PackageManagerService, UserInteraction};
use crate::error::CliError;
use xearthlayer::manager::{
    HttpLibraryClient, InstallQueue, InstallResult, InstalledPackage, Job, JobOutcome,
    LibraryClient, LocalPackageStore, ManagerError, ManagerResult, PackageInfo, PackageInstaller,
    PackageStatus, UpdateChecker,
};
use xearthlayer_package::{PackageLibrary, PackageMetadata, PackageType};

// ============================================================================
// Console Output Implementation
// ============================================================================

/// Standard console output implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConsoleOutput;

impl ConsoleOutput {
    /// Create a new console output.
    pub fn new() -> Self {
        Self
    }
}

impl Output for ConsoleOutput {
    fn println(&self, message: &str) {
        println!("{}", message);
    }

    fn print(&self, message: &str) {
        print!("{}", message);
        io::stdout().flush().ok();
    }
}

// ============================================================================
// Console User Interaction Implementation
// ============================================================================

/// Standard console user interaction implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConsoleInteraction;

impl ConsoleInteraction {
    /// Create a new console interaction.
    pub fn new() -> Self {
        Self
    }
}

impl UserInteraction for ConsoleInteraction {
    fn confirm(&self, message: &str) -> bool {
        print!("{} [y/N]: ", message);
        io::stdout().flush().ok();

        let mut input = String::new();
        if io::stdin().lock().read_line(&mut input).is_err() {
            return false;
        }

        let input = input.trim().to_lowercase();
        input == "y" || input == "yes"
    }

    fn read_line(&self) -> Option<String> {
        let mut input = String::new();
        io::stdin().lock().read_line(&mut input).ok()?;
        Some(input.trim().to_string())
    }
}

// ============================================================================
// Default Package Manager Service Implementation
// ============================================================================

/// Default implementation of the package manager service.
///
/// This wraps the actual xearthlayer manager functions.
#[derive(Debug, Clone, Default)]
pub struct DefaultPackageManagerService {
    client: HttpLibraryClient,
}

impl DefaultPackageManagerService {
    /// Create a new default package manager service.
    pub fn new() -> Self {
        Self {
            client: HttpLibraryClient::new(),
        }
    }
}

impl PackageManagerService for DefaultPackageManagerService {
    fn create_store(&self, install_dir: &Path) -> LocalPackageStore {
        LocalPackageStore::new(install_dir)
    }

    fn fetch_library(&self, url: &str) -> ManagerResult<PackageLibrary> {
        self.client.fetch_library(url)
    }

    fn fetch_metadata(&self, url: &str) -> ManagerResult<PackageMetadata> {
        self.client.fetch_metadata(url)
    }

    fn check_updates(
        &self,
        store: &LocalPackageStore,
        library: &PackageLibrary,
    ) -> Vec<(PackageInfo, PackageStatus)> {
        let checker = UpdateChecker::new(store, &self.client);
        match checker.list_all(library) {
            Ok(infos) => infos
                .into_iter()
                .map(|info| {
                    let status = info.status.clone();
                    (info, status)
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn install_packages(
        &self,
        packages: &[PackageMetadata],
        install_dir: &Path,
        temp_dir: &Path,
        concurrent_downloads: usize,
    ) -> Vec<Result<InstallResult, CliError>> {
        let observer = Arc::new(ConsoleQueueObserver::default());
        let installer = PackageInstaller::new(LocalPackageStore::new(install_dir), temp_dir)
            .with_parallel_downloads(concurrent_downloads);
        let queue = match InstallQueue::start(installer, observer.clone()) {
            Ok(queue) => queue,
            Err(e) => {
                // Every package gets the reason, e.g. another operation holds the lock.
                let reason = e.to_string();
                return packages
                    .iter()
                    .map(|_| Err(CliError::Packages(reason.clone())))
                    .collect();
            }
        };
        let results = queue.install_batch(packages.to_vec());
        drop(queue);
        observer.finish();

        results
            .into_iter()
            .map(|r| r.map_err(|e| CliError::Packages(e.to_string())))
            .collect()
    }

    fn remove_package(
        &self,
        store: &LocalPackageStore,
        region: &str,
        package_type: PackageType,
        temp_dir: &Path,
    ) -> ManagerResult<()> {
        // Through the queue, so a remove takes the same one-operation-at-a-time
        // lock as an install and uses the rename-aside removal.
        let observer = Arc::new(ConsoleQueueObserver::default());
        let installer = PackageInstaller::new(LocalPackageStore::new(store.root()), temp_dir);
        let queue = InstallQueue::start(installer, observer.clone())?;
        let id = queue.submit(Job::Remove {
            region: region.to_string(),
            package_type,
        })?;
        let outcome = queue.wait_for(&[id]).remove(0);
        drop(queue);
        observer.finish();
        match outcome {
            JobOutcome::Removed => Ok(()),
            JobOutcome::Cancelled => Err(ManagerError::Cancelled),
            JobOutcome::Failed(e) => Err(e),
            JobOutcome::Installed(_) => unreachable!("a remove never ends as an install"),
        }
    }

    fn list_packages(&self, store: &LocalPackageStore) -> ManagerResult<Vec<InstalledPackage>> {
        store.list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer_package::package_mountpoint;

    fn install_fake_package(root: &Path, region: &str) -> std::path::PathBuf {
        let folder = package_mountpoint(region, PackageType::Ortho);
        let dir = root.join(&folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dummy.dsf"), "dsf").unwrap();
        std::fs::write(
            dir.join("xearthlayer_scenery_package.txt"),
            format!(
                "REGIONAL SCENERY PACKAGE\n1.0.0\n{}  1.0.0\n2024-01-01T00:00:00Z\nZ\n{folder}\n\
                 test.tar.gz\n1\n\nabc123  test.tar.gz  http://example.com/test.tar.gz\n",
                region.to_uppercase()
            ),
        )
        .unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn removing_an_installed_package_deletes_it_and_leaves_no_aside_folder() {
        let packages = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let dir = install_fake_package(packages.path(), "eu");
        let service = DefaultPackageManagerService::new();
        let store = service.create_store(packages.path());

        service
            .remove_package(&store, "eu", PackageType::Ortho, temp.path())
            .unwrap();

        assert!(!dir.exists());
        assert_eq!(entries(packages.path()), Vec::<String>::new());
    }

    #[test]
    fn removing_a_package_that_is_not_installed_reports_it_as_not_found() {
        let packages = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let service = DefaultPackageManagerService::new();
        let store = service.create_store(packages.path());

        let err = service
            .remove_package(&store, "eu", PackageType::Ortho, temp.path())
            .unwrap_err();

        assert!(
            matches!(err, ManagerError::PackageNotFound { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_remove_refused_by_the_queue_lock_leaves_the_package_installed() {
        let packages = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let dir = install_fake_package(packages.path(), "eu");
        let service = DefaultPackageManagerService::new();
        let store = service.create_store(packages.path());
        // Another operation owns the temp directory's queue lock.
        let _holder = InstallQueue::start(
            PackageInstaller::new(LocalPackageStore::new(packages.path()), temp.path()),
            Arc::new(ConsoleQueueObserver::default()),
        )
        .unwrap();

        let err = service
            .remove_package(&store, "eu", PackageType::Ortho, temp.path())
            .unwrap_err();

        assert!(matches!(err, ManagerError::QueueBusy { .. }), "{err:?}");
        assert!(err.to_string().contains("another package operation"));
        assert!(dir.join("xearthlayer_scenery_package.txt").exists());
    }
}
