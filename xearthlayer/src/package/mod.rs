//! Scenery package management types and parsing.
//!
//! This module provides the core data structures for XEarthLayer's scenery
//! package ecosystem, including package metadata, library index, and version
//! handling.
//!
//! # Overview
//!
//! XEarthLayer scenery packages are distributed as compressed archives containing
//! X-Plane 12 DSF scenery. The package system consists of:
//!
//! - **Package**: Core package identity (region, type, version)
//! - **InstalledPackage**: Extends Package with installation context (path, enabled state)
//! - **Package Metadata**: Full metadata for distribution (checksums, archive parts)
//! - **Package Library**: Index of all available packages from a publisher
//! - **Version**: Semantic versioning for packages and specifications
//!
//! # Type Hierarchy
//!
//! ```text
//! Package (base)                    InstalledPackage (composition)
//! ├── region: String                ├── package: Package  ←── contains
//! ├── package_type: PackageType     ├── path: PathBuf
//! └── version: Version              └── enabled: bool
//! ```
//!
//! `InstalledPackage` uses composition (not inheritance) to extend `Package`.
//! The `Deref` impl allows transparent access to `Package` fields.
//!
//! # File Formats
//!
//! Two text-based file formats are used:
//!
//! - `xearthlayer_scenery_package.txt` - Package metadata (per package)
//! - `xearthlayer_package_library.txt` - Library index (per publisher)
//!
//! See the [Scenery Package Specification](../docs/SCENERY_PACKAGES.md) for
//! detailed format documentation.

mod core;
mod installed;
mod library;
mod metadata;
mod naming;
mod spec;
mod types;

// Core types
pub use core::Package;
pub use installed::InstalledPackage;
pub use types::{ArchivePart, PackageType};

// Specification version policy
pub use spec::{is_supported_spec_major, CURRENT_SPEC_VERSION, SUPPORTED_SPEC_MAJOR};

// Library and metadata
pub use library::{parse_package_library, serialize_package_library, LibraryEntry, PackageLibrary};
pub use metadata::{
    parse_package_metadata, serialize_package_metadata, MetadataValidationError, PackageMetadata,
    ValidationContext,
};

// Naming utilities
pub use naming::{
    archive_filename, archive_part_filename, package_mountpoint, update_archive_version,
};

// Re-export semver::Version for convenience
pub use semver::Version;
