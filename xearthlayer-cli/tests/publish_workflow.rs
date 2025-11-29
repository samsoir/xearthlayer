//! Integration tests for the Publisher workflow.
//!
//! These tests validate the complete publishing pipeline using temporary
//! directories and mock tile data structures that simulate Ortho4XP output.
//!
//! # Running Integration Tests
//!
//! Integration tests are excluded from regular test runs. Use:
//! ```bash
//! make integration-tests
//! ```
//!
//! Or directly:
//! ```bash
//! cargo test --test '*' -- --ignored --nocapture
//! ```

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Helper to create a mock Ortho4XP tile structure.
///
/// Creates minimal DSF and .ter files that represent a valid tile.
struct MockTileBuilder {
    root: PathBuf,
}

impl MockTileBuilder {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Create a mock tile at the given latitude/longitude.
    ///
    /// Creates the expected directory structure:
    /// - `zOrtho4XP_+{lat}{lon}/Earth nav data/+{10lat}+{10lon}/+{lat}+{lon}.dsf`
    /// - `zOrtho4XP_+{lat}{lon}/terrain/+{lat}+{lon}.ter`
    fn create_tile(&self, lat: i32, lon: i32) -> std::io::Result<()> {
        let lat_sign = if lat >= 0 { "+" } else { "" };
        let lon_sign = if lon >= 0 { "+" } else { "" };

        // Calculate 10-degree grid parent
        let lat_10 = (lat / 10) * 10;
        let lon_10 = (lon / 10) * 10;
        let lat_10_sign = if lat_10 >= 0 { "+" } else { "" };
        let lon_10_sign = if lon_10 >= 0 { "+" } else { "" };

        // Directory names
        let tile_dir = format!("zOrtho4XP_{}{:02}{}{:03}", lat_sign, lat, lon_sign, lon);
        let grid_dir = format!(
            "{}{:02}{}{:03}",
            lat_10_sign,
            lat_10.abs(),
            lon_10_sign,
            lon_10.abs()
        );

        // Create DSF directory and file
        let dsf_dir = self
            .root
            .join(&tile_dir)
            .join("Earth nav data")
            .join(&grid_dir);
        fs::create_dir_all(&dsf_dir)?;

        let dsf_file = dsf_dir.join(format!(
            "{}{:02}{}{:03}.dsf",
            lat_sign,
            lat.abs(),
            lon_sign,
            lon.abs()
        ));
        let mut file = File::create(&dsf_file)?;
        // Write minimal DSF header (just enough to be recognized)
        file.write_all(b"XPLNEDSF")?;
        file.write_all(&[0u8; 100])?; // Padding

        // Create terrain directory and .ter files
        let terrain_dir = self.root.join(&tile_dir).join("terrain");
        fs::create_dir_all(&terrain_dir)?;

        // Create a few mock .ter files
        for row in 0..4 {
            for col in 0..4 {
                let ter_file = terrain_dir.join(format!(
                    "{}{:02}{}{:03}_{:02}_{:02}_ZL16.ter",
                    lat_sign,
                    lat.abs(),
                    lon_sign,
                    lon.abs(),
                    row,
                    col
                ));
                let mut file = File::create(&ter_file)?;
                file.write_all(b"A\n800\nTERRAIN\n")?;
            }
        }

        Ok(())
    }

    /// Create multiple tiles for a region.
    fn create_region(&self, tiles: &[(i32, i32)]) -> std::io::Result<()> {
        for (lat, lon) in tiles {
            self.create_tile(*lat, *lon)?;
        }
        Ok(())
    }
}

/// Get the path to the xearthlayer CLI binary.
fn cli_binary() -> PathBuf {
    // Try to find the debug binary first
    let debug_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/debug/xearthlayer");

    if debug_path.exists() {
        return debug_path;
    }

    // Fall back to release binary
    let release_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/release/xearthlayer");

    if release_path.exists() {
        return release_path;
    }

    // If neither exists, use cargo run
    panic!("CLI binary not found. Run `cargo build` first.");
}

/// Run a CLI command and capture output.
fn run_cli(args: &[&str]) -> std::process::Output {
    let binary = cli_binary();
    Command::new(binary)
        .args(args)
        .output()
        .expect("Failed to execute CLI command")
}

/// Assert a command succeeded.
fn assert_success(output: &std::process::Output, context: &str) {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "{} failed:\nstdout: {}\nstderr: {}",
            context, stdout, stderr
        );
    }
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_init_creates_repository_structure() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("test-repo");

    let output = run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    assert_success(&output, "publish init");

    // Verify directory structure
    assert!(
        repo_path.join("packages").exists(),
        "packages/ should exist"
    );
    assert!(repo_path.join("dist").exists(), "dist/ should exist");
    assert!(repo_path.join("staging").exists(), "staging/ should exist");
    assert!(
        repo_path.join(".xearthlayer-repo").exists(),
        "config file should exist"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_init_with_custom_part_size() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("test-repo");

    let output = run_cli(&[
        "publish",
        "init",
        repo_path.to_str().unwrap(),
        "--part-size",
        "200M",
    ]);
    assert_success(&output, "publish init with part-size");

    // Read config and verify part size
    let config_path = repo_path.join(".xearthlayer-repo");
    let config_content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        config_content.contains("part_size = 200M") || config_content.contains("209715200"),
        "Config should contain custom part size"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_scan_discovers_tiles() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tiles
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_region(&[
            (37, -122), // San Francisco area
            (38, -122),
            (37, -123),
        ])
        .expect("Failed to create mock tiles");

    let output = run_cli(&["publish", "scan", "--source", tiles_dir.to_str().unwrap()]);
    assert_success(&output, "publish scan");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should discover 3 tiles (output format: "Tiles:  3")
    assert!(
        stdout.contains("Tiles:  3") || stdout.contains("3 tile") || stdout.contains("Found 3"),
        "Should find 3 tiles, got: {}",
        stdout
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_scan_suggests_region() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock California tiles (should suggest 'na' for North America)
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_region(&[(36, -122), (37, -122), (38, -122)])
        .expect("Failed to create mock tiles");

    let output = run_cli(&["publish", "scan", "--source", tiles_dir.to_str().unwrap()]);
    assert_success(&output, "publish scan");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should suggest North America region
    assert!(
        stdout.to_lowercase().contains("na") || stdout.to_lowercase().contains("north america"),
        "Should suggest NA region for California tiles, got: {}",
        stdout
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_full_workflow_init_scan_add() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tiles
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_region(&[(37, -122), (38, -122)])
        .expect("Failed to create mock tiles");

    // Step 1: Initialize repository
    let output = run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    assert_success(&output, "init");

    // Step 2: Scan tiles
    let output = run_cli(&["publish", "scan", "--source", tiles_dir.to_str().unwrap()]);
    assert_success(&output, "scan");

    // Step 3: Add package
    let output = run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);
    assert_success(&output, "add");

    // Verify package was created
    let package_dir = repo_path.join("packages").join("zzXEL_na_ortho");
    assert!(package_dir.exists(), "Package directory should exist");
    assert!(
        package_dir.join("xearthlayer_scenery_package.txt").exists(),
        "Package metadata should exist"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_status_shows_package_info() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tiles
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_tile(37, -122)
        .expect("Failed to create mock tile");

    // Initialize and add
    run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);

    // Check status
    let output = run_cli(&["publish", "status", repo_path.to_str().unwrap()]);
    assert_success(&output, "status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("na") || stdout.to_lowercase().contains("ortho"),
        "Status should show NA ortho package, got: {}",
        stdout
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_validate_empty_repository() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");

    // Initialize only
    run_cli(&["publish", "init", repo_path.to_str().unwrap()]);

    // Validate (should pass - empty repo is valid)
    let output = run_cli(&["publish", "validate", repo_path.to_str().unwrap()]);
    assert_success(&output, "validate empty repo");
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_init_fails_on_existing_repository() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");

    // First init should succeed
    let output = run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    assert_success(&output, "first init");

    // Second init should fail
    let output = run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "Second init should fail on existing repository"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_add_fails_without_init() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tile
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_tile(37, -122)
        .expect("Failed to create mock tile");

    // Try to add without init
    let output = run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);

    assert!(!output.status.success(), "Add should fail without init");
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_scan_fails_on_empty_directory() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let empty_dir = temp.path().join("empty");
    fs::create_dir_all(&empty_dir).expect("Failed to create empty dir");

    let output = run_cli(&["publish", "scan", "--source", empty_dir.to_str().unwrap()]);

    // Should either fail or report no tiles found
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    assert!(
        !output.status.success()
            || combined.to_lowercase().contains("no tile")
            || combined.contains("0"),
        "Scan should indicate no tiles found in empty directory"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_invalid_version_string_rejected() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tile
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_tile(37, -122)
        .expect("Failed to create mock tile");

    // Initialize
    run_cli(&["publish", "init", repo_path.to_str().unwrap()]);

    // Try to add with invalid version
    let output = run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "invalid-version",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);

    assert!(
        !output.status.success(),
        "Should reject invalid version string"
    );
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_multiple_packages_in_same_repository() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir_na = temp.path().join("tiles_na");
    let tiles_dir_eu = temp.path().join("tiles_eu");
    fs::create_dir_all(&tiles_dir_na).expect("Failed to create NA tiles dir");
    fs::create_dir_all(&tiles_dir_eu).expect("Failed to create EU tiles dir");

    // Create mock NA tiles (California)
    let builder_na = MockTileBuilder::new(&tiles_dir_na);
    builder_na
        .create_tile(37, -122)
        .expect("Failed to create NA tile");

    // Create mock EU tiles (Paris area)
    let builder_eu = MockTileBuilder::new(&tiles_dir_eu);
    builder_eu
        .create_tile(48, 2)
        .expect("Failed to create EU tile");

    // Initialize
    run_cli(&["publish", "init", repo_path.to_str().unwrap()]);

    // Add NA package
    let output = run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir_na.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);
    assert_success(&output, "add NA package");

    // Add EU package
    let output = run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir_eu.to_str().unwrap(),
        "--region",
        "eu",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);
    assert_success(&output, "add EU package");

    // Verify both packages exist
    assert!(
        repo_path.join("packages").join("zzXEL_na_ortho").exists(),
        "NA package should exist"
    );
    assert!(
        repo_path.join("packages").join("zzXEL_eu_ortho").exists(),
        "EU package should exist"
    );

    // Status should show both
    let output = run_cli(&["publish", "status", repo_path.to_str().unwrap()]);
    assert_success(&output, "status");
}

#[test]
#[ignore = "integration test - run with 'make integration-tests'"]
fn test_version_bump_workflow() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().join("repo");
    let tiles_dir = temp.path().join("tiles");
    fs::create_dir_all(&tiles_dir).expect("Failed to create tiles dir");

    // Create mock tile
    let builder = MockTileBuilder::new(&tiles_dir);
    builder
        .create_tile(37, -122)
        .expect("Failed to create mock tile");

    // Initialize and add v0.1.0
    run_cli(&["publish", "init", repo_path.to_str().unwrap()]);
    run_cli(&[
        "publish",
        "add",
        "--source",
        tiles_dir.to_str().unwrap(),
        "--region",
        "na",
        "--type",
        "ortho",
        "--version",
        "0.1.0",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);

    // Bump version to 0.2.0 (minor bump) using "version" subcommand
    let output = run_cli(&[
        "publish",
        "version",
        "--region",
        "na",
        "--type",
        "ortho",
        "--bump",
        "minor",
        "--repo",
        repo_path.to_str().unwrap(),
    ]);
    assert_success(&output, "version bump");

    // Read metadata and verify new version
    let metadata_path = repo_path
        .join("packages")
        .join("zzXEL_na_ortho")
        .join("xearthlayer_scenery_package.txt");
    let metadata = fs::read_to_string(&metadata_path).expect("Failed to read metadata");

    assert!(
        metadata.contains("0.2.0"),
        "Metadata should show bumped version 0.2.0, got: {}",
        metadata
    );
}
