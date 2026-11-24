//! Init command - initialize configuration file.

use std::io::{self, Write};
use std::path::PathBuf;

use xearthlayer::config::{detect_scenery_dir, SceneryDetectionResult};

use crate::error::CliError;

/// Run the init command.
pub fn run() -> Result<(), CliError> {
    // Detect X-Plane scenery directory
    let scenery_dir = match detect_scenery_dir() {
        SceneryDetectionResult::NotFound => {
            println!("X-Plane 12 installation not detected.");
            println!("You can set scenery_dir manually in the config file.");
            println!();
            None
        }
        SceneryDetectionResult::Single(path) => {
            println!("Detected X-Plane 12 Custom Scenery:");
            println!("  {}", path.display());
            println!();
            Some(path)
        }
        SceneryDetectionResult::Multiple(paths) => {
            let selected = prompt_xplane_selection(&paths);
            println!();
            selected
        }
    };

    // Load existing config or create default, then update scenery_dir
    let mut config = super::common::load_config()?;
    if config.xplane.scenery_dir.is_none() {
        config.xplane.scenery_dir = scenery_dir;
    }
    config.save()?;

    let path = xearthlayer::config::config_file_path();
    println!("Configuration file: {}", path.display());
    println!();
    println!("Edit this file to customize XEarthLayer settings.");
    println!("CLI arguments override config file values when specified.");
    Ok(())
}

/// Prompt user to select from multiple X-Plane installations.
fn prompt_xplane_selection(paths: &[PathBuf]) -> Option<PathBuf> {
    println!("Multiple X-Plane 12 installations detected:");
    for (i, path) in paths.iter().enumerate() {
        println!("  [{}] {}", i + 1, path.display());
    }
    println!();
    print!(
        "Select installation (1-{}), or press Enter to skip: ",
        paths.len()
    );
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return None;
    }

    let input = input.trim();
    if input.is_empty() {
        println!("Skipped.");
        return None;
    }

    match input.parse::<usize>() {
        Ok(choice) if choice >= 1 && choice <= paths.len() => {
            let selected = paths[choice - 1].clone();
            println!("Selected: {}", selected.display());
            Some(selected)
        }
        _ => {
            println!("Invalid selection.");
            None
        }
    }
}
