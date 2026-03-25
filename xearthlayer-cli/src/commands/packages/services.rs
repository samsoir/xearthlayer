//! Concrete implementations of the service traits.
//!
//! These implementations wrap the actual xearthlayer manager functions,
//! adapting them to the trait interfaces used by handlers.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use super::traits::{Output, PackageManagerService, ProgressCallback, UserInteraction};
use crate::error::CliError;
use xearthlayer::manager::{
    DownloadProgress, DownloadProgressCallback, HttpLibraryClient, InstallResult, InstalledPackage,
    LibraryClient, LocalPackageStore, ManagerResult, PackageInfo, PackageInstaller, PackageStatus,
    PartState, UpdateChecker,
};
use xearthlayer::package::{PackageLibrary, PackageMetadata, PackageType};

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

    fn progress_done(&self) {
        // Print newline to preserve the final progress state
        println!();
    }

    fn create_progress_callback(&self) -> ProgressCallback {
        let bar = Arc::new(Mutex::new(ProgressBar::new(100)));

        // Configure the initial bar style (progress bar for determinate stages)
        {
            let b = bar.lock().unwrap();
            b.set_style(
                ProgressStyle::with_template(
                    "{spinner:.cyan} {prefix:.bold} [{bar:30.cyan/dim}] {percent:>3}% {msg}",
                )
                .unwrap()
                .progress_chars("##-"),
            );
        }

        Box::new(move |stage, progress, message| {
            let b = bar.lock().unwrap();

            if stage == xearthlayer::manager::InstallStage::Complete {
                b.finish_and_clear();
                return;
            }

            if stage.is_indeterminate() {
                b.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold} {msg}").unwrap(),
                );
                b.set_prefix(stage.name().to_string());
                b.set_message(message.to_string());
                b.enable_steady_tick(std::time::Duration::from_millis(100));
            } else {
                b.disable_steady_tick();
                b.set_style(
                    ProgressStyle::with_template(
                        "{spinner:.cyan} {prefix:.bold} [{bar:30.cyan/dim}] {percent:>3}% {msg}",
                    )
                    .unwrap()
                    .progress_chars("##-"),
                );
                b.set_length(100);
                b.set_position((progress * 100.0).min(100.0) as u64);
                b.set_prefix(stage.name().to_string());
                b.set_message(message.to_string());
            }
        })
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

    fn install_package(
        &self,
        metadata: &PackageMetadata,
        install_dir: &Path,
        temp_dir: &Path,
        concurrent_downloads: usize,
        on_progress: Option<ProgressCallback>,
    ) -> Result<InstallResult, CliError> {
        let store = LocalPackageStore::new(install_dir);

        // Wrap the aggregate callback to skip Downloading stage — per-part bars handle it
        let on_progress: Option<ProgressCallback> = on_progress.map(|cb| {
            let wrapped: ProgressCallback = Box::new(move |stage, progress, message| {
                if stage == xearthlayer::manager::InstallStage::Downloading {
                    return; // Per-part bars handle this
                }
                cb(stage, progress, message);
            });
            wrapped
        });

        // Create per-part progress bars
        let mp = MultiProgress::new();
        let part_style = ProgressStyle::with_template(
            "  {prefix:.dim} [{bar:25.cyan/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
        )
        .unwrap()
        .progress_chars("##-");
        let done_style = ProgressStyle::with_template("  {prefix:.green} {msg:.green}").unwrap();
        let fail_style = ProgressStyle::with_template("  {prefix:.red} {msg:.red}").unwrap();
        let retry_style = ProgressStyle::with_template("  {prefix:.yellow} {msg:.yellow}").unwrap();

        let queued_style = ProgressStyle::with_template("  {prefix:.dim} {msg:.dim}").unwrap();
        let bars: Vec<ProgressBar> = metadata
            .parts
            .iter()
            .map(|part| {
                let bar = mp.add(ProgressBar::new(0));
                bar.set_style(queued_style.clone());
                bar.set_prefix(part.filename.clone());
                bar.set_message("(queued)");
                bar
            })
            .collect();

        // Footer bar for aggregate progress
        let footer = mp.add(ProgressBar::new(0));
        footer.set_style(
            ProgressStyle::with_template(
                "\n  Total [{bar:25.green/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
            )
            .unwrap()
            .progress_chars("##-"),
        );

        let bars = Arc::new(bars);
        let footer = Arc::new(footer);
        let part_style = Arc::new(part_style);
        let done_style_arc = Arc::new(done_style);
        let fail_style_arc = Arc::new(fail_style);
        let retry_style_arc = Arc::new(retry_style);

        let download_cb: DownloadProgressCallback = Box::new({
            let bars = Arc::clone(&bars);
            let footer = Arc::clone(&footer);
            let part_style = Arc::clone(&part_style);
            let done_style = Arc::clone(&done_style_arc);
            let fail_style = Arc::clone(&fail_style_arc);
            let retry_style = Arc::clone(&retry_style_arc);
            move |progress: &DownloadProgress| {
                for part in &progress.parts {
                    if part.index >= bars.len() {
                        continue;
                    }
                    let bar = &bars[part.index];
                    // Skip already-finished bars to avoid duplicate rendering
                    if bar.is_finished() {
                        continue;
                    }
                    match &part.state {
                        PartState::Queued => {}
                        PartState::Downloading => {
                            if let Some(total) = part.total_bytes {
                                bar.set_length(total);
                            }
                            bar.set_style((*part_style).clone());
                            bar.set_position(part.bytes_downloaded);
                        }
                        PartState::Done => {
                            bar.set_style((*done_style).clone());
                            bar.finish_with_message("[done]");
                        }
                        PartState::Failed { reason, .. } => {
                            bar.set_style((*fail_style).clone());
                            bar.abandon_with_message(format!("(failed: {})", reason));
                        }
                        PartState::Retrying { attempt } => {
                            bar.set_style((*retry_style).clone());
                            bar.set_message(format!("(retry {}/3)", attempt));
                        }
                    }
                }
                if let Some(total) = progress.total_bytes {
                    footer.set_length(total);
                }
                footer.set_position(progress.total_bytes_downloaded);
            }
        });

        let installer = PackageInstaller::new(self.client.clone(), store, temp_dir)
            .with_parallel_downloads(concurrent_downloads)
            .with_download_progress(download_cb);

        let result = installer
            .install_from_metadata(metadata, on_progress)
            .map_err(|e| CliError::Packages(e.to_string()));

        // Clean up progress bars
        footer.finish_and_clear();
        for bar in bars.iter() {
            bar.finish_and_clear();
        }

        result
    }

    fn remove_package(
        &self,
        store: &LocalPackageStore,
        region: &str,
        package_type: PackageType,
    ) -> ManagerResult<()> {
        store.remove(region, package_type)
    }

    fn get_package(
        &self,
        store: &LocalPackageStore,
        region: &str,
        package_type: PackageType,
    ) -> ManagerResult<InstalledPackage> {
        store.get(region, package_type)
    }

    fn list_packages(&self, store: &LocalPackageStore) -> ManagerResult<Vec<InstalledPackage>> {
        store.list()
    }
}
