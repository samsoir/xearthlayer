//! Argument types and CLI definitions for publish commands.
//!
//! This module contains the clap-derived argument types and enums used
//! for parsing command-line arguments.

use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};

use xearthlayer::package::PackageType;
use xearthlayer::publisher::VersionBump;

/// Package type argument for CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PackageTypeArg {
    /// Orthophoto package (satellite imagery)
    Ortho,
    /// Overlay package (roads, buildings, etc.)
    Overlay,
}

impl From<PackageTypeArg> for PackageType {
    fn from(arg: PackageTypeArg) -> Self {
        match arg {
            PackageTypeArg::Ortho => PackageType::Ortho,
            PackageTypeArg::Overlay => PackageType::Overlay,
        }
    }
}

/// Version bump type argument for CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BumpType {
    /// Increment major version (x.0.0)
    Major,
    /// Increment minor version (0.x.0)
    Minor,
    /// Increment patch version (0.0.x)
    Patch,
}

impl From<BumpType> for VersionBump {
    fn from(bump: BumpType) -> Self {
        match bump {
            BumpType::Major => VersionBump::Major,
            BumpType::Minor => VersionBump::Minor,
            BumpType::Patch => VersionBump::Patch,
        }
    }
}

/// Publisher subcommands.
#[derive(Subcommand)]
pub enum PublishCommands {
    /// Initialize a new package repository
    Init {
        /// Path to create repository (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Archive part size (e.g., "500MB", "1GB")
        #[arg(long, default_value = "500M")]
        part_size: String,
    },

    /// Scan Ortho4XP output and report tile information
    Scan {
        /// Path to Ortho4XP Tiles directory
        #[arg(long)]
        source: PathBuf,

        /// Package type to scan for
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,
    },

    /// Process Ortho4XP tiles into a package
    Add {
        /// Path to Ortho4XP Tiles directory
        #[arg(long)]
        source: PathBuf,

        /// Region code (e.g., "na", "eur", "asia")
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Initial version (default: 1.0.0)
        #[arg(long, default_value = "1.0.0")]
        version: String,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// List packages in the repository
    List {
        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,

        /// Show detailed information
        #[arg(long, short)]
        verbose: bool,
    },

    /// Build distributable archives for a package
    Build {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Configure download URLs for a package
    Urls {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Base URL (part filenames will be appended)
        #[arg(long)]
        base_url: String,

        /// Verify URLs are accessible (HEAD request)
        #[arg(long)]
        verify: bool,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Manage package version
    Version {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Bump version (major, minor, or patch)
        #[arg(long, value_enum, conflicts_with = "set")]
        bump: Option<BumpType>,

        /// Set specific version (e.g., "2.0.0")
        #[arg(long, conflicts_with = "bump")]
        set: Option<String>,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Release package to the library index
    Release {
        /// Region code
        #[arg(long)]
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// URL where the metadata file will be hosted
        #[arg(long)]
        metadata_url: String,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Show package release status
    Status {
        /// Region code (optional, shows all if not specified)
        #[arg(long)]
        region: Option<String>,

        /// Package type (optional)
        #[arg(long, value_enum)]
        r#type: Option<PackageTypeArg>,

        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },

    /// Validate repository integrity
    Validate {
        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },

    /// Generate a coverage map image showing tile coverage
    Coverage {
        /// Output file path (PNG or GeoJSON based on --geojson flag)
        #[arg(long, short, default_value = "coverage.png")]
        output: PathBuf,

        /// Image width in pixels (PNG only)
        #[arg(long, default_value = "1200")]
        width: u32,

        /// Image height in pixels (PNG only)
        #[arg(long, default_value = "600")]
        height: u32,

        /// Use dark mode (CartoDB Dark Matter tiles, PNG only)
        #[arg(long)]
        dark: bool,

        /// Generate GeoJSON instead of PNG (for interactive maps)
        #[arg(long)]
        geojson: bool,

        /// Repository path (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },
}

// ============================================================================
// Handler Argument Structs
// ============================================================================

/// Arguments for the init command.
pub struct InitArgs {
    pub path: PathBuf,
    pub part_size: String,
}

/// Arguments for the scan command.
pub struct ScanArgs {
    pub source: PathBuf,
    pub package_type: PackageTypeArg,
}

/// Arguments for the add command.
pub struct AddArgs {
    pub source: PathBuf,
    pub region: String,
    pub package_type: PackageTypeArg,
    pub version: String,
    pub repo: PathBuf,
}

/// Arguments for the list command.
pub struct ListArgs {
    pub repo: PathBuf,
    pub verbose: bool,
}

/// Arguments for the build command.
pub struct BuildArgs {
    pub region: String,
    pub package_type: PackageTypeArg,
    pub repo: PathBuf,
}

/// Arguments for the urls command.
pub struct UrlsArgs {
    pub region: String,
    pub package_type: PackageTypeArg,
    pub base_url: String,
    pub verify: bool,
    pub repo: PathBuf,
}

/// Arguments for the version command.
pub struct VersionArgs {
    pub region: String,
    pub package_type: PackageTypeArg,
    pub bump: Option<BumpType>,
    pub set: Option<String>,
    pub repo: PathBuf,
}

/// Arguments for the release command.
pub struct ReleaseArgs {
    pub region: String,
    pub package_type: PackageTypeArg,
    pub metadata_url: String,
    pub repo: PathBuf,
}

/// Arguments for the status command.
pub struct StatusArgs {
    pub region: Option<String>,
    pub package_type: Option<PackageTypeArg>,
    pub repo: PathBuf,
}

/// Arguments for the validate command.
pub struct ValidateArgs {
    pub repo: PathBuf,
}

/// Arguments for the coverage command.
pub struct CoverageArgs {
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub dark: bool,
    pub geojson: bool,
    pub repo: PathBuf,
}
