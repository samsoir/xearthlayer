//! Patches management CLI commands.
//!
//! Commands for managing tile patches - pre-built Ortho4XP tiles with custom
//! mesh/elevation that XEL overlays with dynamically generated textures.

use std::path::PathBuf;

use clap::Subcommand;

use xearthlayer::config::ConfigFile;
use xearthlayer::patches::{PatchDiscovery, PatchInfo};

use crate::error::CliError;
use crate::runner::CliRunner;

/// Patches subcommands.
#[derive(Subcommand)]
pub enum PatchesCommands {
    /// List installed patches
    ///
    /// Shows all valid patches in the patches directory, their priority order,
    /// and file counts (DSF, terrain, textures).
    List,

    /// Validate patch structure
    ///
    /// Checks that patches have the required directory structure and files.
    Validate {
        /// Name of a specific patch to validate (optional)
        #[arg(long)]
        name: Option<String>,
    },

    /// Show the patches directory path
    ///
    /// Displays the configured patches directory where patch tiles should be placed.
    Path,
}

/// Run the patches command.
pub fn run(command: PatchesCommands) -> Result<(), CliError> {
    match command {
        PatchesCommands::List => list_patches(),
        PatchesCommands::Validate { name } => validate_patches(name),
        PatchesCommands::Path => show_path(),
    }
}

/// Get the patches directory from config.
fn get_patches_dir(config: &ConfigFile) -> PathBuf {
    config.patches.directory.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".xearthlayer")
            .join("patches")
    })
}

/// List all patches.
fn list_patches() -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    let config = runner.config();
    let patches_dir = get_patches_dir(config);

    println!("Patches directory: {}", patches_dir.display());
    println!();

    if !patches_dir.exists() {
        println!("No patches directory found.");
        println!();
        println!("To add patches, create the directory and place Ortho4XP tiles inside:");
        println!("  mkdir -p {}", patches_dir.display());
        return Ok(());
    }

    let discovery = PatchDiscovery::new(&patches_dir);
    let all_patches = discovery
        .find_patches()
        .map_err(|e| CliError::Config(format!("Failed to scan patches: {}", e)))?;

    if all_patches.is_empty() {
        println!("No patches found.");
        println!();
        println!("Place Ortho4XP tile folders in the patches directory.");
        println!("Each patch folder should contain 'Earth nav data/' with DSF files.");
        return Ok(());
    }

    // Separate valid and invalid patches
    let valid_patches: Vec<_> = all_patches.iter().filter(|p| p.is_valid).collect();
    let invalid_patches: Vec<_> = all_patches.iter().filter(|p| !p.is_valid).collect();

    println!(
        "Found {} patch(es) ({} valid, {} invalid):",
        all_patches.len(),
        valid_patches.len(),
        invalid_patches.len()
    );
    println!();

    // Show valid patches with priority order
    if !valid_patches.is_empty() {
        println!("Valid patches (in priority order):");
        for (i, patch) in valid_patches.iter().enumerate() {
            let priority = i + 1;
            print_patch_info(patch, Some(priority));
        }
        println!();
    }

    // Show invalid patches
    if !invalid_patches.is_empty() {
        println!("Invalid patches:");
        for patch in &invalid_patches {
            print_patch_info(patch, None);
            for error in &patch.validation_errors {
                println!("      ⚠ {}", error);
            }
        }
        println!();
    }

    Ok(())
}

/// Print information about a single patch.
fn print_patch_info(patch: &PatchInfo, priority: Option<usize>) {
    let status = if patch.is_valid { "✓" } else { "✗" };
    let priority_str = priority.map(|p| format!("#{} ", p)).unwrap_or_default();

    println!(
        "  {} {}{} ({} DSF, {} terrain, {} texture files)",
        status, priority_str, patch.name, patch.dsf_count, patch.terrain_count, patch.texture_count
    );
    println!("      Path: {}", patch.path.display());
}

/// Validate patches.
fn validate_patches(name: Option<String>) -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    let config = runner.config();
    let patches_dir = get_patches_dir(config);

    if !patches_dir.exists() {
        return Err(CliError::Config(format!(
            "Patches directory does not exist: {}",
            patches_dir.display()
        )));
    }

    let discovery = PatchDiscovery::new(&patches_dir);
    let all_patches = discovery
        .find_patches()
        .map_err(|e| CliError::Config(format!("Failed to scan patches: {}", e)))?;

    // Filter by name if specified
    let patches_to_validate: Vec<_> = if let Some(ref patch_name) = name {
        all_patches
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(patch_name))
            .collect()
    } else {
        all_patches.iter().collect()
    };

    if patches_to_validate.is_empty() {
        if let Some(patch_name) = name {
            return Err(CliError::Config(format!(
                "Patch '{}' not found in {}",
                patch_name,
                patches_dir.display()
            )));
        } else {
            println!("No patches found to validate.");
            return Ok(());
        }
    }

    let mut all_valid = true;

    for patch in &patches_to_validate {
        println!("Validating: {}", patch.name);
        println!("  Path: {}", patch.path.display());

        // Check directory structure
        let earth_nav_data = patch.path.join("Earth nav data");
        let terrain_dir = patch.path.join("terrain");
        let textures_dir = patch.path.join("textures");

        println!(
            "  Earth nav data/: {}",
            if earth_nav_data.exists() {
                "✓"
            } else {
                "✗"
            }
        );
        println!(
            "  terrain/: {}",
            if terrain_dir.exists() {
                "✓"
            } else {
                "- (optional)"
            }
        );
        println!(
            "  textures/: {}",
            if textures_dir.exists() {
                "✓"
            } else {
                "- (optional)"
            }
        );

        // Show file counts
        println!("  DSF files: {}", patch.dsf_count);
        println!("  Terrain files: {}", patch.terrain_count);
        println!("  Texture files: {}", patch.texture_count);

        // Show validation result
        if patch.is_valid {
            println!("  Status: ✓ Valid");
        } else {
            println!("  Status: ✗ Invalid");
            for error in &patch.validation_errors {
                println!("    Error: {}", error);
            }
            all_valid = false;
        }
        println!();
    }

    if all_valid {
        println!("All patches are valid.");
    } else {
        println!("Some patches have validation errors. Fix them to enable mounting.");
    }

    Ok(())
}

/// Show the patches directory path.
fn show_path() -> Result<(), CliError> {
    let runner = CliRunner::new()?;
    let config = runner.config();
    let patches_dir = get_patches_dir(config);

    println!("{}", patches_dir.display());

    Ok(())
}
