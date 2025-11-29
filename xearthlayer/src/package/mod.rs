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
//! - **Package Metadata**: Describes a single package (region, version, parts)
//! - **Package Library**: Index of all available packages from a publisher
//! - **Version**: Semantic versioning for packages and specifications
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

mod library;
mod metadata;
mod types;

pub use library::{parse_package_library, serialize_package_library, LibraryEntry, PackageLibrary};
pub use metadata::{parse_package_metadata, serialize_package_metadata, PackageMetadata};
pub use types::{ArchivePart, PackageType};

// Re-export semver::Version for convenience
pub use semver::Version;
