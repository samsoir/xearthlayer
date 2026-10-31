//! Consolidated ortho union filesystem module.
//!
//! This module provides the infrastructure for merging multiple ortho sources
//! (patches and regional packages) into a single unified virtual filesystem.
//!
//! # Overview
//!
//! XEarthLayer consolidates all ortho sources into a single FUSE mount at
//! `Custom Scenery/zzXEL_ortho/`. This provides:
//!
//! - **Single mount point**: Simpler X-Plane scenery management
//! - **Shared resources**: One DdsHandler, cache, and concurrency limiter
//! - **Clear precedence**: Alphabetical sorting determines priority
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │           OrthoUnionIndexBuilder        │
//! │                                         │
//! │  .with_patches_dir(~/.xearthlayer/...)  │
//! │  .add_package(na_installed)             │
//! │  .add_package(eu_installed)             │
//! │  .build()                               │
//! └────────────────┬────────────────────────┘
//!                  │
//!                  ▼
//! ┌─────────────────────────────────────────┐
//! │           OrthoUnionIndex               │
//! │                                         │
//! │  sources: [_patches/KLAX, eu, na, sa]   │
//! │  files: HashMap<VirtualPath, Source>    │
//! │  directories: HashMap<Path, Entries>    │
//! └─────────────────────────────────────────┘
//! ```
//!
//! # Precedence Rules
//!
//! Sources are sorted alphabetically by `sort_key`:
//!
//! 1. Patches use `_patches/{folder_name}` (underscore sorts first)
//! 2. Packages use `{region}` (e.g., "eu", "na", "sa")
//!
//! First source wins on collision.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
//! use xearthlayer_package::{Package, InstalledPackage, PackageType};
//!
//! let index = OrthoUnionIndexBuilder::new()
//!     .with_patches_dir("~/.xearthlayer/patches")
//!     .add_package(na_installed)
//!     .add_package(eu_installed)
//!     .build()?;
//!
//! // Resolve a file
//! if let Some(source) = index.resolve(Path::new("terrain/12345_67890_ZL16.ter")) {
//!     println!("Found in: {}", source.source_name);
//! }
//! ```

mod builder;
mod cache;
mod index;
mod parallel;
mod progress;
mod source;

pub use builder::OrthoUnionIndexBuilder;
pub use cache::{
    default_cache_path, save_index_cache, try_load_cached_index, IndexCache, IndexCacheKey,
};
pub use index::{DirEntry, FileSource, OrthoUnionIndex};
pub use parallel::{merge_partial_indexes, scan_sources_parallel, PartialIndex};
pub use progress::{IndexBuildPhase, IndexBuildProgress, IndexBuildProgressCallback};
pub use source::{OrthoSource, SourceType};
