//! Argument types and CLI definitions for packages commands.
//!
//! This module contains the clap-derived argument types and enums used
//! for parsing command-line arguments.

use std::path::PathBuf;

use clap::Subcommand;

use xearthlayer_package::PackageType;

use super::selector::Selection;

/// Help for every `region` argument. They all resolve through
/// `selector::resolve`, so they share one description.
const REGION_HELP: &str =
    "Region code, or the start of one to select a group (e.g. \"NA\", \"NA-CANADA\")";

/// Help for the `regions` of `install` and `remove`, which take several.
const REGIONS_HELP: &str = "One or more region codes, each a code or the start of one to select \
     a group (e.g. \"NA\", \"NA-CANADA\")";

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

    /// Install packages from a library
    Install {
        #[arg(help = REGIONS_HELP, required_unless_present = "all", conflicts_with = "all")]
        regions: Vec<String>,

        /// Install every package of the selected type in the library
        #[arg(long)]
        all: bool,

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

    /// Update installed packages (all of them when no region is given)
    Update {
        #[arg(help = REGION_HELP)]
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

    /// Remove installed packages
    Remove {
        #[arg(help = REGIONS_HELP, required_unless_present = "all", conflicts_with = "all")]
        regions: Vec<String>,

        /// Remove every installed package of the selected type
        #[arg(long)]
        all: bool,

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
        #[arg(help = REGION_HELP)]
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
    pub selection: Selection,
    pub package_type: PackageType,
    pub library_url: String,
    pub install_dir: PathBuf,
    pub temp_dir: PathBuf,
    /// Path to X-Plane Custom Scenery directory for overlay symlinks.
    pub custom_scenery_path: Option<PathBuf>,
    /// Automatically install overlay package when installing ortho for the same region.
    pub auto_install_overlays: bool,
    /// Skip rebuild of the consolidated `yzXEL_overlay/` symlink folder when
    /// true. The package is still downloaded and installed; only the symlink
    /// folder X-Plane reads is left alone. The next `xearthlayer run` will
    /// remove any stale folder. See `packages.disable_overlays` config key.
    pub disable_overlays: bool,
    /// `packages.xworld_pro`: leave the continents X-World Pro covers out of
    /// the consolidated overlay, as `xearthlayer run` does.
    pub xworld_pro: bool,
    /// Number of concurrent part downloads.
    pub concurrent_downloads: usize,
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
    /// See `InstallArgs::disable_overlays`.
    pub disable_overlays: bool,
    /// See `InstallArgs::xworld_pro`.
    pub xworld_pro: bool,
    /// Number of concurrent part downloads.
    pub concurrent_downloads: usize,
}

/// Arguments for the remove command.
pub struct RemoveArgs {
    pub selection: Selection,
    pub package_type: PackageType,
    pub install_dir: PathBuf,
    /// Temp directory that holds the queue lock. A removed package is not
    /// moved here: it is renamed to `.xel-removed-<name>` inside the
    /// packages root and deleted there.
    pub temp_dir: PathBuf,
    pub force: bool,
    /// Path to X-Plane Custom Scenery directory for overlay symlinks.
    pub custom_scenery_path: Option<PathBuf>,
    /// See `InstallArgs::disable_overlays`.
    pub disable_overlays: bool,
    /// See `InstallArgs::xworld_pro`.
    pub xworld_pro: bool,
}

/// Arguments for the info command.
pub struct InfoArgs {
    pub region: String,
    pub package_type: PackageType,
    pub install_dir: PathBuf,
}
