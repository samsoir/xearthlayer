//! CLI error handling with user-friendly messages.
//!
//! Centralizes error handling for the CLI, providing consistent formatting
//! and appropriate exit codes.

use std::fmt;
use std::path::PathBuf;
use std::process;
use xearthlayer::config::ConfigFileError;
use xearthlayer::service::ServiceError;

/// CLI-specific errors with user-friendly messages.
#[derive(Debug)]
pub enum CliError {
    /// Failed to initialize logging
    LoggingInit(String),
    /// Configuration error
    Config(String),
    /// Failed to load config file
    ConfigFile(ConfigFileError),
    /// FUSE server error
    Serve(ServiceError),
    /// Failed to clear cache
    CacheClear(String),
    /// Failed to get cache stats
    CacheStats(String),
    /// Publisher error
    Publish(String),
    /// Package manager error
    Packages(String),
    /// No packages installed
    NoPackages { install_location: PathBuf },
    /// SceneryIndex cache error
    SceneryIndex(String),
    /// XEarthLayer needs initial setup
    NeedsSetup,
}

impl CliError {
    /// Exit the process with an appropriate error message and code.
    pub fn exit(&self) -> ! {
        eprintln!("Error: {}", self);

        // Print additional help for specific errors
        match self {
            CliError::ConfigFile(_) => {
                eprintln!();
                eprintln!("Check your config file at ~/.xearthlayer/config.ini");
                eprintln!("Run 'xearthlayer init' to create a default config file.");
            }
            CliError::Serve(_) => {
                eprintln!();
                eprintln!("Common issues:");
                eprintln!("  1. FUSE not installed: sudo apt install fuse (Linux)");
                eprintln!("  2. Permissions: You may need to add your user to 'fuse' group");
                eprintln!(
                    "  3. Mountpoint in use: Try unmounting with: fusermount -u <mountpoint>"
                );
            }
            CliError::Publish(_) => {
                eprintln!();
                eprintln!("Run 'xearthlayer publish --help' for usage information.");
            }
            CliError::Packages(_) => {
                eprintln!();
                eprintln!("Run 'xearthlayer packages --help' for usage information.");
            }
            CliError::NoPackages { install_location } => {
                eprintln!();
                eprintln!("No ortho packages are installed.");
                eprintln!();
                eprintln!("To get started:");
                eprintln!("  1. View available packages:  xearthlayer packages list");
                eprintln!("  2. Install a region:         xearthlayer packages install <region>");
                eprintln!();
                eprintln!("Example:");
                eprintln!("  xearthlayer packages install na    # Install North America");
                eprintln!();
                eprintln!(
                    "Packages will be installed to: {}",
                    install_location.display()
                );
            }
            CliError::SceneryIndex(_) => {
                eprintln!();
                eprintln!("Run 'xearthlayer scenery-index --help' for usage information.");
            }
            CliError::NeedsSetup => {
                // NeedsSetup is informational, not an error - print welcome message
                println!();
                println!("Welcome to XEarthLayer!");
                println!();
                println!("To get started, run the setup wizard:");
                println!("  xearthlayer setup");
                println!();
                println!("Or manually initialize and install packages:");
                println!("  1. xearthlayer init              # Create config file");
                println!("  2. xearthlayer packages list     # View available packages");
                println!("  3. xearthlayer packages install <region>");
                println!();
                // Exit with code 0 since this is not an error
                process::exit(0);
            }
            _ => {}
        }

        process::exit(1)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::LoggingInit(msg) => write!(f, "Failed to initialize logging: {}", msg),
            CliError::Config(msg) => write!(f, "Configuration error: {}", msg),
            CliError::ConfigFile(e) => write!(f, "Config file error: {}", e),
            CliError::Serve(e) => write!(f, "FUSE server error: {}", e),
            CliError::CacheClear(msg) => write!(f, "Failed to clear cache: {}", msg),
            CliError::CacheStats(msg) => write!(f, "Failed to get cache stats: {}", msg),
            CliError::Publish(msg) => write!(f, "Publisher error: {}", msg),
            CliError::Packages(msg) => write!(f, "Package manager error: {}", msg),
            CliError::NoPackages { .. } => write!(f, "No ortho packages installed"),
            CliError::SceneryIndex(msg) => write!(f, "Scenery index error: {}", msg),
            CliError::NeedsSetup => write!(f, "XEarthLayer needs initial setup"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::ConfigFile(e) => Some(e),
            CliError::Serve(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ConfigFileError> for CliError {
    fn from(e: ConfigFileError) -> Self {
        CliError::ConfigFile(e)
    }
}
