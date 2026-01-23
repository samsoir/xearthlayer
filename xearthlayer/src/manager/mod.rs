//! Package Manager for discovering, downloading, and managing XEarthLayer scenery packages.
//!
//! This module provides the client-side package management functionality, complementing
//! the [`publisher`](crate::publisher) module which handles package creation.
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

mod cache;
mod client;
mod config;
mod download;
mod error;
mod extractor;
mod installer;
mod local;
mod mounts;
mod symlinks;
mod traits;
mod updates;

pub use cache::{CacheStats, CachedLibraryClient};
pub use client::HttpLibraryClient;
pub use config::ManagerConfig;
pub use download::{DownloadState, MultiPartDownloader, MultiPartProgressCallback};
pub use error::{ManagerError, ManagerResult};
pub use extractor::{check_required_tools, ShellExtractor};
pub use installer::{InstallProgressCallback, InstallResult, InstallStage, PackageInstaller};
pub use local::{InstalledPackage, LocalPackageStore, MountStatus};
pub use mounts::{
    ActiveMount, CacheBridges, ConsolidatedOrthoMountResult, MountManager, ServiceBuilder,
};
pub use symlinks::{
    consolidated_overlay_exists, create_consolidated_overlay, create_overlay_symlink,
    overlay_symlink_exists, overlay_symlink_path, remove_consolidated_overlay,
    remove_overlay_symlink, ConsolidatedOverlayResult, CONSOLIDATED_OVERLAY_NAME,
};
pub use traits::{ArchiveExtractor, LibraryClient, PackageDownloader, ProgressCallback};
pub use updates::{PackageInfo, PackageStatus, UpdateChecker};
