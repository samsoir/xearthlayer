//! Package Manager for discovering, downloading, and managing XEarthLayer scenery packages.
//!
//! This module provides the client-side package management functionality, complementing
//! the `xearthlayer-publisher` crate, which handles package creation.
//!
//! # Overview
//!
//! The Manager handles:
//! - Discovering locally installed packages
//! - Fetching package library indexes from remote sources
//! - Downloading and installing packages
//! - Managing package updates and versions
//! - Verifying package integrity via checksums
//!
//! # Architecture
//!
//! The manager uses trait-based abstractions for testability:
//!
//! - [`LibraryClient`] - Fetches library indexes and package metadata
//! - [`PackageDownloader`] - Downloads and verifies package archives
//! - [`LocalPackageStore`] - Manages installed packages on disk
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::manager::{PackageManager, ManagerConfig};
//!
//! let config = ManagerConfig::default();
//! let manager = PackageManager::new(config)?;
//!
//! // List available packages from configured libraries
//! let available = manager.list_available().await?;
//!
//! // Install a package
//! manager.install("na", PackageType::Ortho).await?;
//!
//! // List installed packages
//! let installed = manager.list_installed()?;
//! ```

mod aside;
mod cache;
mod cancel;
mod client;
mod config;
mod disk_check;
mod download;
mod error;
mod extractor;
mod install_guard;
mod installer;
mod local;
mod mounts;
mod queue;
mod symlinks;
mod traits;
mod updates;

pub use cache::{CacheStats, CachedLibraryClient};
pub use cancel::CancelToken;
pub use client::HttpLibraryClient;
pub use config::ManagerConfig;
pub use disk_check::{available_bytes, FsInfoProvider, RealFsInfoProvider, TREE_MULTIPLIER};
pub use download::{
    DownloadProgress, DownloadProgressCallback, DownloadState, MultiPartDownloader, PartState,
};
pub use error::{ManagerError, ManagerResult};
pub use extractor::TarGzExtractor;
pub use installer::{InstallResult, InstallStage, PackageInstaller};
pub use local::{InstalledPackage, LocalPackageStore};
pub use mounts::{ActiveMount, ConsolidatedOrthoMountResult, MountManager, ServiceBuilder};
pub use queue::{
    CancelResult, Canceller, Capacity, InstallContext, InstallQueue, Job, JobEvent, JobId, JobKind,
    JobOutcome, JobSummary, QueueControl, QueueObserver, Stages,
};
pub use symlinks::{
    consolidated_overlay_exists, create_consolidated_overlay, create_consolidated_overlay_except,
    remove_consolidated_overlay, remove_overlay_symlink, ConsolidatedOverlayResult,
    CONSOLIDATED_OVERLAY_NAME,
};
pub use traits::{ArchiveExtractor, LibraryClient, PackageDownloader, ProgressCallback};
pub use updates::{PackageInfo, PackageStatus, UpdateChecker};
