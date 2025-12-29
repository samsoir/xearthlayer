//! Package Publisher for creating distributable XEarthLayer Scenery Packages.
//!
//! This module provides tools for creating, building, and managing scenery
//! packages from various scenery sources. It enables anyone to create and host
//! their own scenery libraries.
//!
//! # Overview
//!
//! The Publisher workflow:
//! 1. Initialize a repository (`init`)
//! 2. Scan scenery source with a processor (`scan`)
//! 3. Process tiles into packages (`process`)
//! 4. Build distributable archives (`build`)
//! 5. Configure download URLs (`urls`)
//! 6. Publish to library index (`release`)
//!
//! # Scenery Processors
//!
//! Different scenery creation tools produce output in different formats.
//! The [`SceneryProcessor`] trait abstracts this, with implementations for:
//!
//! - [`Ortho4XPProcessor`] - Processes Ortho4XP tile output (X-Plane 12)
//! - [`OverlayProcessor`] - Processes Ortho4XP overlay output (X-Plane 12)
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::publisher::{Repository, SceneryProcessor, Ortho4XPProcessor};
//! use xearthlayer::package::PackageType;
//!
//! // Initialize repository
//! let repo = Repository::init("/path/to/repo")?;
//!
//! // Process Ortho4XP output using the SceneryProcessor trait
//! let processor = Ortho4XPProcessor::new();
//! let scan_result = processor.scan("/path/to/Ortho4XP/Tiles".as_ref())?;
//! let summary = processor.process(&scan_result, "na", PackageType::Ortho, &repo)?;
//!
//! println!("Processed {} tiles", summary.tile_count);
//! ```

mod archive;
mod config;
pub mod coverage;
mod error;
mod library;
mod metadata;
mod processor;
mod region;
mod release;
mod repository;
mod urls;

pub use archive::{
    archive_filename, build_archive, check_required_tools, format_archive_size, ArchiveBuildResult,
    ArchivePart,
};
pub use config::{
    parse_size, read_config, write_config, RepoConfig, DEFAULT_PART_SIZE, MAX_PART_SIZE,
    MIN_PART_SIZE,
};
pub use error::{PublishError, PublishResult};
pub use library::{LibraryManager, DEFAULT_SCOPE, LIBRARY_FILENAME, SPEC_VERSION};
pub use metadata::{
    add_archive_parts, bump_package_version, bump_version, calculate_sha256, create_metadata,
    generate_initial_metadata, has_metadata, read_metadata, update_version, write_metadata,
    VersionBump, METADATA_FILENAME,
};
pub use processor::{
    Ortho4XPProcessor, OverlayProcessor, ProcessSummary, SceneryFormat, SceneryProcessor,
    SceneryScanResult, TileInfo, TileWarning,
};
pub use region::{analyze_tiles, suggest_region, RegionSuggestion, SuggestedRegion};
pub use release::{
    build_package, configure_urls, get_release_status, release_package, validate_repository,
    BuildResult, ReleaseResult, ReleaseStatus, UrlConfigResult,
};
pub use repository::Repository;
pub use urls::{generate_part_urls, validate_url, UrlVerification, UrlVerifier};
