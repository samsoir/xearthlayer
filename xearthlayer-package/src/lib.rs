//! Scenery package format: metadata, library index, naming and the spec gate.
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
//! See `docs/dev/scenery-packages.md` in the repository for detailed format
//! documentation.
//!
//! # Why a separate crate
//!
//! This is the on-disk contract between the publisher, which writes these
//! files, and the runtime, which reads them. It holds format knowledge only:
//! no I/O beyond parsing bytes it is handed, no network, no path discovery.
//! Every statement the format makes lives here rather than in a caller, so
//! the two sides cannot drift. A manifest test pins its dependency list.

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

#[cfg(test)]
mod manifest_tests {
    //! The contract crate holds format knowledge only. Cargo refuses a cycle
    //! but not a creeping dependency, so the manifest is asserted directly.

    /// Names declared under `[dependencies]` in this crate's own manifest.
    fn declared_dependencies() -> Vec<String> {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("crate manifest is readable");
        manifest
            .lines()
            .map(str::trim)
            .skip_while(|line| *line != "[dependencies]")
            .skip(1)
            .take_while(|line| !line.starts_with('['))
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                line.split(['=', '.'])
                    .next()
                    .expect("dependency line has a name")
                    .trim()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn manifest_declares_only_format_dependencies() {
        // semver for package versions, chrono for the timestamps the metadata
        // format carries. Anything else means format knowledge is leaking in
        // from one side of the boundary this crate exists to hold.
        let mut declared = declared_dependencies();
        declared.sort();

        assert_eq!(
            declared,
            ["chrono", "semver"],
            "xearthlayer-package may depend only on the crates the format itself needs"
        );
    }
}
