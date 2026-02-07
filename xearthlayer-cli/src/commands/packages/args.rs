//! Argument types and CLI definitions for packages commands.
//!
//! This module contains the clap-derived argument types and enums used
//! for parsing command-line arguments.

use std::path::PathBuf;

use clap::Subcommand;

use xearthlayer::package::PackageType;

/// Package type argument for CLI.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
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

/// Packages subcommands.
#[derive(Subcommand)]
pub enum PackagesCommands {
    /// List installed packages
    List {
        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,

        /// Show detailed information
        #[arg(long, short)]
        verbose: bool,
    },

    /// Check for package updates
    Check {
        /// Library URL to check against (default: from config)
        #[arg(long)]
        library_url: Option<String>,

        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,
    },

    /// Install a package from a library
    Install {
        /// Region code to install (e.g., "na", "eu")
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Library URL to install from (default: from config)
        #[arg(long)]
        library_url: Option<String>,

        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,

        /// Temporary directory for downloads (default: from config or system temp)
        #[arg(long)]
        temp_dir: Option<PathBuf>,
    },

    /// Update installed packages
    Update {
        /// Region code to update (optional, updates all if not specified)
        region: Option<String>,

        /// Package type (optional)
        #[arg(long, value_enum)]
        r#type: Option<PackageTypeArg>,

        /// Library URL to check for updates (default: from config)
        #[arg(long)]
        library_url: Option<String>,

        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,

        /// Temporary directory for downloads (default: from config or system temp)
        #[arg(long)]
        temp_dir: Option<PathBuf>,

        /// Update all packages without prompting
        #[arg(long)]
        all: bool,
    },

    /// Remove an installed package
    Remove {
        /// Region code to remove
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,

        /// Remove without confirmation
        #[arg(long, short)]
        force: bool,
    },

    /// Show detailed information about an installed package
    Info {
        /// Region code
        region: String,

        /// Package type
        #[arg(long, value_enum, default_value = "ortho")]
        r#type: PackageTypeArg,

        /// Installation directory (default: from config)
        #[arg(long)]
        install_dir: Option<PathBuf>,
    },
}

// ============================================================================
// Handler Argument Structs
// ============================================================================

/// Arguments for the list command.
pub struct ListArgs {
    pub install_dir: PathBuf,
    pub verbose: bool,
}

/// Arguments for the check command.
pub struct CheckArgs {
    pub library_url: String,
    pub install_dir: PathBuf,
}

/// Arguments for the install command.
pub struct InstallArgs {
    pub region: String,
    pub package_type: PackageType,
    pub library_url: String,
    pub install_dir: PathBuf,
    pub temp_dir: PathBuf,
    /// Path to X-Plane Custom Scenery directory for overlay symlinks.
    pub custom_scenery_path: Option<PathBuf>,
    /// Automatically install overlay package when installing ortho for the same region.
    pub auto_install_overlays: bool,
}

/// Arguments for the update command.
pub struct UpdateArgs {
    pub region: Option<String>,
    pub package_type: Option<PackageType>,
    pub library_url: String,
    pub install_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub all: bool,
    /// Path to X-Plane Custom Scenery directory for overlay symlinks.
    pub custom_scenery_path: Option<PathBuf>,
}

/// Arguments for the remove command.
pub struct RemoveArgs {
    pub region: String,
    pub package_type: PackageType,
    pub install_dir: PathBuf,
    pub force: bool,
    /// Path to X-Plane Custom Scenery directory for overlay symlinks.
    pub custom_scenery_path: Option<PathBuf>,
}

/// Arguments for the info command.
pub struct InfoArgs {
    pub region: String,
    pub package_type: PackageType,
    pub install_dir: PathBuf,
}
