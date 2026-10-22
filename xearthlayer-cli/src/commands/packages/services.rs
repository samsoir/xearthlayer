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
    HttpLibraryClient, InstallQueue, InstallResult, InstalledPackage, LibraryClient,
    LocalPackageStore, ManagerResult, PackageInfo, PackageInstaller, PackageStatus, UpdateChecker,
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
    ) -> ManagerResult<()> {
        store.remove(region, package_type)
    }

    fn list_packages(&self, store: &LocalPackageStore) -> ManagerResult<Vec<InstalledPackage>> {
        store.list()
    }
}
